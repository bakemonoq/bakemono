use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde_json::json;
use tauri::webview::{Cookie, PageLoadEvent, WebviewWindowBuilder};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl};

const LOGIN_URL: &str = "https://www.patreon.com/login";
const COOKIE_SCOPE: &str = "https://www.patreon.com";
const SESSION_TTL_SECS: i64 = 63_072_000; // session cookies get a 2-year expiry so gallery-dl keeps them
const MOBILE_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_4 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Mobile/15E148 Safari/604.1";

pub fn open_login(app: &AppHandle) -> Result<()> {
    if let Some(win) = app.get_webview_window("patreon") {
        let _ = win.set_focus();
        return Ok(());
    }
    let url = LOGIN_URL.parse().context("parsing patreon url")?;
    let nav_app = app.clone();
    let load_app = app.clone();
    let mut builder = WebviewWindowBuilder::new(app, "patreon", WebviewUrl::External(url))
        .title("Log in to Patreon")
        .inner_size(414.0, 896.0)
        // patreon serves a blank page to wry's default UA, so pose as mobile Safari for the phone layout
        .user_agent(MOBILE_UA)
        .on_navigation(move |url| {
            let _ = nav_app.emit("patreon-nav", format!("navigating to {url}"));
            true
        })
        .on_page_load(move |_win, payload| {
            let finished = matches!(payload.event(), PageLoadEvent::Finished);
            let url = payload.url().clone();
            let _ = load_app.emit(
                "patreon-nav",
                format!("{} {url}", if finished { "loaded" } else { "loading" }),
            );
            // once the user lands on a logged-in page, grab the session and close the popup
            if finished && is_logged_in_page(&url) {
                let app = load_app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Ok((count, path)) = capture_cookies(&app).await {
                        let _ = app.emit("patreon-captured", json!({"count": count, "path": path}));
                        if let Some(win) = app.get_webview_window("patreon") {
                            let _ = win.close();
                        }
                    }
                });
            }
        });
    // a stable store keeps the login across app restarts, so reopening does not re-prompt
    #[cfg(target_os = "macos")]
    {
        builder = builder.data_store_identifier(PATREON_STORE_ID);
    }
    #[cfg(not(target_os = "macos"))]
    {
        builder = builder.data_directory(webview_data_dir());
    }
    builder.build().context("opening patreon window")?;
    Ok(())
}

#[cfg(target_os = "macos")]
const PATREON_STORE_ID: [u8; 16] = *b"bakemono-patreon";

fn is_logged_in_page(url: &tauri::Url) -> bool {
    let on_patreon = url
        .host_str()
        .map(|h| h.ends_with("patreon.com"))
        .unwrap_or(false);
    if !on_patreon {
        return false;
    }
    let path = url.path();
    !["/login", "/signup", "/register", "/auth", "/oauth"]
        .iter()
        .any(|p| path.starts_with(p))
}

#[cfg(not(target_os = "macos"))]
fn webview_data_dir() -> PathBuf {
    bakemono_engine::data_dir().join("patreon-webview")
}

// pull the logged-in session out of the embedded webview into a cookies.txt gallery-dl can read
pub async fn capture_cookies(app: &AppHandle) -> Result<(usize, String)> {
    let win = app
        .get_webview_window("patreon")
        .context("open the Patreon login window first")?;
    let url = COOKIE_SCOPE.parse().context("parsing cookie scope")?;
    let cookies = win.cookies_for_url(url).context("reading webview cookies")?;
    if cookies.is_empty() {
        bail!("no cookies yet - finish logging in, then capture");
    }

    let path = cookies_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, netscape(&cookies))
        .with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }

    Ok((cookies.len(), path.display().to_string()))
}

// path of a session captured in an earlier run, so the ui can pre-fill it
pub fn saved_cookies_path() -> Option<String> {
    let path = cookies_path();
    path.is_file().then(|| path.display().to_string())
}

fn netscape(cookies: &[Cookie<'static>]) -> String {
    let session_expiry = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64 + SESSION_TTL_SECS)
        .unwrap_or(SESSION_TTL_SECS);
    let mut out = String::from("# Netscape HTTP Cookie File\n");
    for c in cookies {
        let domain = c.domain().unwrap_or(".patreon.com");
        let include_sub = if domain.starts_with('.') {
            "TRUE"
        } else {
            "FALSE"
        };
        let path = c.path().unwrap_or("/");
        let secure = if c.secure().unwrap_or(false) {
            "TRUE"
        } else {
            "FALSE"
        };
        let expires = c
            .expires_datetime()
            .map(|dt| dt.unix_timestamp())
            .unwrap_or(session_expiry);
        out.push_str(&format!(
            "{domain}\t{include_sub}\t{path}\t{secure}\t{expires}\t{}\t{}\n",
            c.name(),
            c.value()
        ));
    }
    out
}

fn cookies_path() -> PathBuf {
    bakemono_engine::data_dir().join("patreon-cookies.txt")
}
