use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State};
use tracing_subscriber::prelude::*;

use bakemono_engine::config::AppConfig;
use bakemono_engine::identity::{key_path, Identity};
use bakemono_engine::ipc;

// the GUI is a thin client: the daemon process does the seeding/scraping/publishing, the GUI
// just drives it over the local socket and manages identity/config files on disk
pub fn run() {
    let _log_guard = init_logging();
    let identity = Identity::load_or_generate(&key_path()).expect("loading identity");
    let config = AppConfig::load().unwrap_or_default();
    tracing::info!(
        data_dir = %bakemono_engine::data_dir().display(),
        npub = %identity.npub().unwrap_or_default(),
        "starting bakemono gui"
    );

    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(AppState::new(identity, config))
        .setup(|app| {
            // point the daemon at the bundled sidecars (release only) before it spawns
            let _ = BUNDLED.set(resolve_bundled(app));
            // make sure a daemon is running (spawn one detached if not)
            tauri::async_runtime::spawn(async {
                if let Err(e) = ensure_daemon().await {
                    tracing::error!("could not start daemon: {e:#}");
                }
            });
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move { notify_if_update(handle).await });
            // macOS Cmd+Q (the default Quit item) fires no tauri event, so the daemon never gets
            // the stop signal; install a menu whose Quit we can intercept
            #[cfg(target_os = "macos")]
            install_macos_menu(app)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            identity_npub,
            generate_identity,
            import_identity,
            export_nsec,
            get_config,
            save_settings,
            set_stop_on_exit,
            app_paths,
            open_path,
            sharing_stats,
            start_scrape,
            republish,
            open_patreon_login,
            capture_patreon_cookies,
            saved_patreon_cookies,
            cancel_job,
            daemon_status,
            start_daemon,
            restart_daemon,
            stop_daemon
        ])
        // closing the window quits the gui (no tray); on macOS this does not fire ExitRequested,
        // so stop the daemon here too when the user opted in, then exit the process
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                // closing the patreon login popup must not quit the app
                if window.label() != "main" {
                    return;
                }
                stop_daemon_if_configured();
                window.app_handle().exit(0);
            }
        })
        .build(tauri::generate_context!())
        .expect("building tauri app")
        .run(|_app, event| {
            if let tauri::RunEvent::ExitRequested { .. } = event {
                stop_daemon_if_configured();
            }
        });
}

// the daemon keeps seeding after the gui closes unless the user opted out
fn stop_daemon_if_configured() {
    if AppConfig::load().map(|c| c.stop_daemon_on_exit).unwrap_or(false) {
        shutdown_daemon_blocking();
        tracing::info!("stopped daemon on exit");
    }
}

// replace the default macOS menu so Cmd+Q routes through our own Quit item; keep the Edit submenu
// so copy/paste still works in the webview's text fields
#[cfg(target_os = "macos")]
fn install_macos_menu(app: &tauri::App) -> tauri::Result<()> {
    use tauri::menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder};
    let quit = MenuItemBuilder::with_id("quit", "Quit Bakemono")
        .accelerator("Cmd+Q")
        .build(app)?;
    let app_menu = SubmenuBuilder::new(app, "Bakemono")
        .about(None)
        .separator()
        .hide()
        .hide_others()
        .show_all()
        .separator()
        .item(&quit)
        .build()?;
    let edit_menu = SubmenuBuilder::new(app, "Edit")
        .undo()
        .redo()
        .separator()
        .cut()
        .copy()
        .paste()
        .select_all()
        .build()?;
    let window_menu = SubmenuBuilder::new(app, "Window")
        .minimize()
        .separator()
        .close_window()
        .build()?;
    let menu = MenuBuilder::new(app)
        .items(&[&app_menu, &edit_menu, &window_menu])
        .build()?;
    app.set_menu(menu)?;
    let quit_id = quit.id().clone();
    app.on_menu_event(move |app, event| {
        if event.id() == &quit_id {
            stop_daemon_if_configured();
            app.exit(0);
        }
    });
    Ok(())
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
#[allow(clippy::too_many_arguments)]
fn save_settings(
    relays: Vec<String>,
    trackers: Vec<String>,
    max_up_mbit: u32,
    max_down_mbit: u32,
    ffmpeg_bin: Option<String>,
    gallery_dl_bin: Option<String>,
    state: State<AppState>,
) -> Result<AppConfig, String> {
    let mut guard = state.lock();
    guard.config.relays = relays;
    guard.config.trackers = trackers;
    guard.config.max_up_mbit = max_up_mbit;
    guard.config.max_down_mbit = max_down_mbit;
    guard.config.ffmpeg_bin = clean_path(ffmpeg_bin);
    guard.config.gallery_dl_bin = clean_path(gallery_dl_bin);
    guard.config.save().map_err(stringify)?;
    tracing::info!(relays = guard.config.relays.len(), "saved settings");
    Ok(guard.config.clone())
}

fn clean_path(value: Option<String>) -> Option<String> {
    value.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

#[tauri::command]
fn set_stop_on_exit(value: bool, state: State<AppState>) -> Result<(), String> {
    let mut guard = state.lock();
    guard.config.stop_daemon_on_exit = value;
    guard.config.save().map_err(stringify)
}

#[tauri::command]
async fn start_daemon() -> Result<(), String> {
    ensure_daemon().await.map_err(stringify)
}

#[tauri::command]
fn app_paths() -> Paths {
    let data = bakemono_engine::data_dir();
    Paths {
        data_dir: data.display().to_string(),
        scrape_dir: scrape_dest().display().to_string(),
        log_dir: data.join("logs").display().to_string(),
    }
}

#[tauri::command]
fn open_path(path: String) -> Result<(), String> {
    open_in_file_manager(&path).map_err(stringify)
}

// open a directory in the OS file manager (Finder/Explorer/xdg)
fn open_in_file_manager(path: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(path).ok();
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(program).arg(path).spawn()?;
    Ok(())
}

#[tauri::command]
async fn sharing_stats() -> Result<Value, String> {
    ipc::call(json!({"cmd": "stats"}), |_| {})
        .await
        .map_err(stringify)
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

// re-send the events already saved on disk to the current relay set; use after adding a relay so it
// gets backfilled without a full re-scrape
#[tauri::command]
async fn republish(app: AppHandle) -> Result<Value, String> {
    ensure_daemon().await.map_err(stringify)?;
    let job = json!({"cmd": "run", "job": {"kind": "republish"}});
    ipc::call(job, move |data| {
        let _ = app.emit("progress", data);
    })
    .await
    .map_err(stringify)
}

// open an embedded webview to patreon.com; the user logs in there and we read the
// session straight out of the webview - the credentials never leave the machine
#[tauri::command]
async fn open_patreon_login(app: AppHandle) -> Result<(), String> {
    crate::patreon::open_login(&app).map_err(stringify)
}

#[tauri::command]
async fn capture_patreon_cookies(app: AppHandle) -> Result<CookieCapture, String> {
    let (count, path) = crate::patreon::capture_cookies(&app)
        .await
        .map_err(stringify)?;
    Ok(CookieCapture { count, path })
}

#[derive(Serialize)]
struct CookieCapture {
    count: usize,
    path: String,
}

#[tauri::command]
fn saved_patreon_cookies() -> Option<String> {
    crate::patreon::saved_cookies_path()
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
    apply_sidecar_env(&mut cmd, &AppConfig::load().unwrap_or_default());
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
        "bakemono-daemon.exe"
    } else {
        "bakemono-daemon"
    }
}

// release builds ship gallery-dl and ffmpeg with the app; hand the daemon their paths through the
// env seams the engine already reads. dev builds leave these unset and fall back to PATH
static BUNDLED: OnceLock<Bundled> = OnceLock::new();

#[derive(Default)]
struct Bundled {
    gallery_dl: Option<PathBuf>,
    ffmpeg: Option<PathBuf>,
}

fn resolve_bundled(_app: &tauri::App) -> Bundled {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let next_to_exe = |name: &str| -> Option<PathBuf> {
        let path = exe_dir.as_ref()?.join(bin_name(name));
        path.exists().then_some(path)
    };
    Bundled {
        gallery_dl: next_to_exe("gallery-dl"),
        ffmpeg: next_to_exe("ffmpeg"),
    }
}

// hand the daemon its sidecar paths with precedence: an exported BAKEMONO_* var wins (dev/CI escape
// hatch), else the path configured in the app, else the binary bundled with a release. all unset
// falls back to PATH / the in-repo sidecar
fn apply_sidecar_env(cmd: &mut std::process::Command, config: &AppConfig) {
    let bundled = BUNDLED.get();
    set_sidecar(cmd, "BAKEMONO_GALLERY_DL", config.gallery_dl_bin.as_deref(), bundled.and_then(|b| b.gallery_dl.as_deref()));
    set_sidecar(cmd, "BAKEMONO_FFMPEG", config.ffmpeg_bin.as_deref(), bundled.and_then(|b| b.ffmpeg.as_deref()));
}

fn set_sidecar(cmd: &mut std::process::Command, key: &str, config: Option<&str>, bundled: Option<&Path>) {
    // forward an exported var explicitly rather than trusting the daemon to inherit our environment
    if let Some(value) = std::env::var_os(key) {
        cmd.env(key, value);
        return;
    }
    if let Some(path) = config.map(str::trim).filter(|s| !s.is_empty()) {
        cmd.env(key, path);
        return;
    }
    if let Some(path) = bundled {
        cmd.env(key, path);
    }
}

fn bin_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

async fn notify_if_update(app: AppHandle) {
    use tauri_plugin_updater::UpdaterExt;
    let updater = match app.updater() {
        Ok(updater) => updater,
        Err(e) => {
            tracing::debug!("updater unavailable: {e}");
            return;
        }
    };
    match updater.check().await {
        Ok(Some(update)) => {
            tracing::info!(version = %update.version, "a new version is available");
            let _ = app.emit("update-available", update.version.clone());
        }
        Ok(None) => {}
        Err(e) => tracing::debug!("update check failed: {e}"),
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
    bakemono_engine::data_dir().join("scrape")
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
    let dir = bakemono_engine::data_dir().join("logs");
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
