use std::path::PathBuf;
use std::time::Duration;

pub const RELEASES_URL: &str = "https://github.com/bakemonoq/bakemono/releases/latest";

// the same updater manifest the desktop app reads; its `version` field is the latest published release
const LATEST_JSON: &str =
    "https://github.com/bakemonoq/bakemono/releases/latest/download/latest.json";

// long-running services call this once at startup: log if a newer release exists and cache the
// latest version so the short-lived cli can report it without its own network call. set
// BAKEMONO_NO_UPDATE_CHECK to skip the network entirely
pub fn spawn_log_check(current: &'static str) {
    if std::env::var_os("BAKEMONO_NO_UPDATE_CHECK").is_some() {
        return;
    }
    tokio::spawn(async move {
        let Some(latest) = latest_version().await else {
            return;
        };
        let _ = std::fs::write(cache_path(), &latest);
        if is_newer(&latest, current) {
            tracing::info!(
                current,
                latest = %latest,
                url = RELEASES_URL,
                "a newer bakemono release is available"
            );
        }
    });
}

// no network: read the version a running service last cached, returned only if newer than `current`
pub fn cached_newer(current: &str) -> Option<String> {
    let latest = std::fs::read_to_string(cache_path()).ok()?;
    let latest = latest.trim();
    is_newer(latest, current).then(|| latest.to_string())
}

async fn latest_version() -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?;
    let body = client.get(LATEST_JSON).send().await.ok()?.text().await.ok()?;
    let json: serde_json::Value = serde_json::from_str(&body).ok()?;
    json.get("version")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn cache_path() -> PathBuf {
    crate::data_dir().join("latest-version")
}

fn is_newer(latest: &str, current: &str) -> bool {
    parts(latest) > parts(current)
}

fn parts(v: &str) -> Vec<u64> {
    v.trim()
        .trim_start_matches('v')
        .split('.')
        .map(|p| p.parse().unwrap_or(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("v0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.1"));
    }
}
