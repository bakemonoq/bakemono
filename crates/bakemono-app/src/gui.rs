use std::path::PathBuf;
use std::sync::Mutex;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::prelude::*;

use bakemono_scraper::{Cookies, ScrapeRequest};

use crate::core::catalog::{self, CatalogStats};
use crate::core::config::AppConfig;
use crate::core::identity::{key_path, Identity};
use crate::core::pipeline::{reseed, run_scrape, JobContext, Progress, RunSummary};
use crate::core::seeder::SeederHandle;

pub fn run() {
    let _log_guard = init_logging();
    let identity = Identity::load_or_generate(&key_path()).expect("loading identity");
    let config = AppConfig::load().unwrap_or_default();
    tracing::info!(
        data_dir = %crate::core::data_dir().display(),
        npub = %identity.npub().unwrap_or_default(),
        "starting bakemono"
    );

    tauri::Builder::default()
        .manage(AppState::new(identity, config))
        .setup(|app| {
            let state = app.state::<AppState>();
            let seeder = state.seeder.clone();
            let (trackers, stun) = {
                let guard = state.lock();
                (guard.config.trackers.clone(), guard.config.stun.clone())
            };
            let dir = scrape_dest();
            // start the seeder and re-seed everything on disk, so restarts resume seeding
            tauri::async_runtime::spawn(async move {
                if let Err(e) = seeder.ensure_started(&trackers, &stun).await {
                    tracing::error!("seeder failed to start: {e:#}");
                    return;
                }
                reseed(&seeder, &dir).await;
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            identity_npub,
            generate_identity,
            import_identity,
            export_nsec,
            get_config,
            save_settings,
            app_paths,
            sharing_stats,
            start_scrape,
            cancel_job
        ])
        .run(tauri::generate_context!())
        .expect("running tauri application");
}

#[tauri::command]
fn identity_npub(state: State<AppState>) -> Result<String, String> {
    state.lock().identity.npub().map_err(stringify)
}

#[tauri::command]
fn generate_identity(state: State<AppState>) -> Result<String, String> {
    let identity = Identity::generate();
    identity.save(&key_path()).map_err(stringify)?;
    let npub = identity.npub().map_err(stringify)?;
    state.lock().identity = identity;
    tracing::info!(%npub, "generated identity");
    Ok(npub)
}

#[tauri::command]
fn import_identity(nsec: String, state: State<AppState>) -> Result<String, String> {
    let identity = Identity::import(&nsec).map_err(stringify)?;
    identity.save(&key_path()).map_err(stringify)?;
    let npub = identity.npub().map_err(stringify)?;
    state.lock().identity = identity;
    tracing::info!(%npub, "imported identity");
    Ok(npub)
}

#[tauri::command]
fn export_nsec(state: State<AppState>) -> Result<String, String> {
    state.lock().identity.nsec().map_err(stringify)
}

#[tauri::command]
fn get_config(state: State<AppState>) -> AppConfig {
    state.lock().config.clone()
}

#[tauri::command]
fn save_settings(
    relays: Vec<String>,
    trackers: Vec<String>,
    stun: Vec<String>,
    state: State<AppState>,
) -> Result<AppConfig, String> {
    let mut guard = state.lock();
    guard.config.relays = relays;
    guard.config.trackers = trackers;
    guard.config.stun = stun;
    guard.config.save().map_err(stringify)?;
    tracing::info!(
        relays = guard.config.relays.len(),
        trackers = guard.config.trackers.len(),
        stun = guard.config.stun.len(),
        "saved settings"
    );
    Ok(guard.config.clone())
}

#[tauri::command]
fn app_paths() -> Paths {
    let data = crate::core::data_dir();
    Paths {
        data_dir: data.display().to_string(),
        scrape_dir: scrape_dest().display().to_string(),
        log_dir: data.join("logs").display().to_string(),
    }
}

#[tauri::command]
fn sharing_stats() -> CatalogStats {
    catalog::stats(&scrape_dest())
}

#[tauri::command]
fn cancel_job(state: State<AppState>) -> Result<(), String> {
    state.cancel_current();
    Ok(())
}

#[tauri::command]
async fn start_scrape(
    creator: String,
    limit: Option<u32>,
    cookies: Option<String>,
    browser: Option<String>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<RunSummary, String> {
    let job = state.snapshot();
    let token = state.begin_job()?;
    if job.seed {
        state
            .seeder
            .ensure_started(&job.trackers, &job.stun)
            .await
            .map_err(stringify)?;
    }
    let mut request = ScrapeRequest::new(creator, scrape_dest());
    request.cookies = resolve_login(browser, cookies)?;
    let callback = emitter(app);
    let ctx = JobContext {
        relays: &job.relays,
        identity: &job.identity,
        seeder: job.seed.then_some(&state.seeder),
        cancel: &token,
        progress: &callback,
    };
    let result = run_scrape(request, limit.map(|n| n as usize), &ctx).await;
    state.end_job();
    result.map_err(stringify)
}

pub struct AppState {
    inner: Mutex<Inner>,
    job: Mutex<Option<CancellationToken>>,
    seeder: SeederHandle,
}

struct Inner {
    identity: Identity,
    config: AppConfig,
}

struct Job {
    identity: Identity,
    relays: Vec<String>,
    trackers: Vec<String>,
    stun: Vec<String>,
    seed: bool,
}

impl AppState {
    fn new(identity: Identity, config: AppConfig) -> Self {
        Self {
            inner: Mutex::new(Inner { identity, config }),
            job: Mutex::new(None),
            seeder: SeederHandle::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("app state poisoned")
    }

    // copy out what a job needs so the lock is never held across an await
    fn snapshot(&self) -> Job {
        let guard = self.lock();
        Job {
            identity: guard.identity.clone(),
            relays: guard.config.relays.clone(),
            trackers: guard.config.trackers.clone(),
            stun: guard.config.stun.clone(),
            seed: guard.config.seed,
        }
    }

    fn begin_job(&self) -> Result<CancellationToken, String> {
        let mut slot = self.job.lock().expect("job slot poisoned");
        if slot.is_some() {
            return Err("a job is already running".into());
        }
        let token = CancellationToken::new();
        *slot = Some(token.clone());
        Ok(token)
    }

    fn end_job(&self) {
        *self.job.lock().expect("job slot poisoned") = None;
    }

    fn cancel_current(&self) {
        if let Some(token) = self.job.lock().expect("job slot poisoned").as_ref() {
            tracing::info!("cancel requested");
            token.cancel();
        }
    }
}

fn emitter(app: AppHandle) -> impl Fn(Progress) + Send + Sync {
    move |progress| {
        let _ = app.emit("progress", progress);
    }
}

fn scrape_dest() -> PathBuf {
    crate::core::data_dir().join("scrape")
}

// a chosen browser wins over a cookies file; either is optional (public posts need neither)
fn resolve_login(
    browser: Option<String>,
    cookies: Option<String>,
) -> Result<Option<Cookies>, String> {
    if let Some(browser) = browser.filter(|b| !b.trim().is_empty()) {
        return Ok(Some(Cookies::Browser(browser)));
    }
    match cookies.filter(|c| !c.trim().is_empty()) {
        Some(raw) => Ok(Some(Cookies::File(resolve_cookies(&raw)?))),
        None => Ok(None),
    }
}

// resolve to an absolute path so gallery-dl finds it regardless of the app's working directory
fn resolve_cookies(raw: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(raw);
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(path)
    };
    if !absolute.is_file() {
        return Err(format!("cookies file not found: {}", absolute.display()));
    }
    Ok(absolute)
}

#[derive(Serialize)]
struct Paths {
    data_dir: String,
    scrape_dir: String,
    log_dir: String,
}

fn init_logging() -> tracing_appender::non_blocking::WorkerGuard {
    let dir = crate::core::data_dir().join("logs");
    std::fs::create_dir_all(&dir).ok();
    let (file_writer, guard) = tracing_appender::non_blocking(tracing_appender::rolling::daily(
        &dir,
        "bakemono.log",
    ));
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(file_writer),
        )
        .init();
    guard
}

fn stringify(err: anyhow::Error) -> String {
    format!("{err:#}")
}
