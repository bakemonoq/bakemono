use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, State};
use tracing_subscriber::prelude::*;

use bakemono_daemon::ipc;

use crate::core::catalog::{self, CatalogStats};
use crate::core::config::AppConfig;
use crate::core::identity::{key_path, Identity};

// the GUI is a thin client: the daemon process does the seeding/scraping/publishing, the GUI
// just drives it over the local socket and manages identity/config files on disk
pub fn run() {
    let _log_guard = init_logging();
    let identity = Identity::load_or_generate(&key_path()).expect("loading identity");
    let config = AppConfig::load().unwrap_or_default();
    tracing::info!(
        data_dir = %crate::core::data_dir().display(),
        npub = %identity.npub().unwrap_or_default(),
        "starting bakemono gui"
    );

    tauri::Builder::default()
        .manage(AppState::new(identity, config))
        .setup(|_app| {
            // make sure a daemon is running (spawn one detached if not)
            tauri::async_runtime::spawn(async {
                if let Err(e) = ensure_daemon().await {
                    tracing::error!("could not start daemon: {e:#}");
                }
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
            cancel_job,
            daemon_status,
            restart_daemon,
            stop_daemon
        ])
        .build(tauri::generate_context!())
        .expect("building tauri app")
        .run(|_app, event| {
            if let tauri::RunEvent::ExitRequested { .. } = event {
                // daemon keeps seeding after the window closes unless the user opted out
                if AppConfig::load().map(|c| c.stop_daemon_on_exit).unwrap_or(false) {
                    shutdown_daemon_blocking();
                    tracing::info!("stopped daemon on exit");
                }
            }
        });
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
    tracing::info!(relays = guard.config.relays.len(), "saved settings");
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
async fn start_scrape(
    creator: String,
    limit: Option<u32>,
    cookies: Option<String>,
    browser: Option<String>,
    app: AppHandle,
) -> Result<Value, String> {
    ensure_daemon().await.map_err(stringify)?;
    // resolve a cookies file path against the GUI's cwd before handing it to the daemon
    let cookies = match cookies.filter(|c| !c.trim().is_empty()) {
        Some(raw) => Some(resolve_cookies(&raw)?),
        None => None,
    };
    let job = json!({
        "cmd": "run",
        "job": {"kind": "scrape", "creator": creator, "limit": limit, "cookies": cookies, "browser": browser}
    });
    let app = app.clone();
    ipc::call(job, move |data| {
        let _ = app.emit("progress", data);
    })
    .await
    .map_err(stringify)
}

#[tauri::command]
async fn cancel_job() -> Result<(), String> {
    ipc::call(json!({"cmd": "cancel"}), |_| {})
        .await
        .map_err(stringify)?;
    Ok(())
}

#[tauri::command]
async fn daemon_status() -> Result<Value, String> {
    ipc::call(json!({"cmd": "status"}), |_| {})
        .await
        .map_err(stringify)
}

#[tauri::command]
async fn stop_daemon() -> Result<(), String> {
    let _ = ipc::call(json!({"cmd": "shutdown"}), |_| {}).await;
    Ok(())
}

#[tauri::command]
async fn restart_daemon() -> Result<(), String> {
    let _ = ipc::call(json!({"cmd": "shutdown"}), |_| {}).await;
    let sock = ipc::socket_path();
    for _ in 0..40 {
        if !sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    ensure_daemon().await.map_err(stringify)
}

pub struct AppState {
    inner: Mutex<Inner>,
}

struct Inner {
    identity: Identity,
    config: AppConfig,
}

impl AppState {
    fn new(identity: Identity, config: AppConfig) -> Self {
        Self {
            inner: Mutex::new(Inner { identity, config }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("app state poisoned")
    }
}

async fn ensure_daemon() -> anyhow::Result<()> {
    if daemon_alive().await {
        return Ok(());
    }
    spawn_daemon()?;
    for _ in 0..100 {
        if daemon_alive().await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("daemon did not come up");
}

async fn daemon_alive() -> bool {
    ipc::call(json!({"cmd": "status"}), |_| {}).await.is_ok()
}

// the daemon binary ships next to the gui binary
fn spawn_daemon() -> std::io::Result<()> {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(daemon_bin_name())))
        .unwrap_or_else(|| PathBuf::from(daemon_bin_name()));
    let mut cmd = std::process::Command::new(exe);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // own process group so the daemon survives the gui and ignores its signals
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()?;
    tracing::info!("spawned daemon");
    Ok(())
}

fn daemon_bin_name() -> &'static str {
    if cfg!(windows) {
        "bakemono-app-daemon.exe"
    } else {
        "bakemono-app-daemon"
    }
}

#[cfg(unix)]
fn shutdown_daemon_blocking() {
    use std::io::Write;
    if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(ipc::socket_path()) {
        let _ = stream.write_all(b"{\"cmd\":\"shutdown\"}\n");
        let _ = stream.flush();
    }
}

#[cfg(not(unix))]
fn shutdown_daemon_blocking() {}

fn scrape_dest() -> PathBuf {
    crate::core::data_dir().join("scrape")
}

// resolve to an absolute path so the daemon (with its own cwd) finds the cookies file
fn resolve_cookies(raw: &str) -> Result<String, String> {
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
    Ok(absolute.to_string_lossy().into_owned())
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
    let (file_writer, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::daily(&dir, "gui.log"));
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
