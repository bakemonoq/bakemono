use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use bakemono_engine::config::AppConfig;
use bakemono_engine::daemon::Daemon;
use bakemono_engine::logging;

mod source;
use source::FarmContentSource;

// the seednode is config-driven and autonomous: no IPC, no CLI - it runs a loop under systemd
#[tokio::main]
async fn main() -> Result<()> {
    let _log_guard = logging::init("seednode");
    bakemono_engine::version::spawn_log_check(env!("CARGO_PKG_VERSION"));
    let config = AppConfig::load().unwrap_or_default();
    let content_dir = bakemono_engine::data_dir().join("cache");
    std::fs::create_dir_all(&content_dir).ok();

    let daemon = Arc::new(Daemon::new(config, content_dir, FarmContentSource));
    daemon.start().await?; // seeder up + re-seed the existing cache

    tracing::warn!(
        "auto-fetch not implemented yet (needs the board demand + blocklist feeds); \
         seednode is only re-seeding its existing cache for now"
    );

    loop {
        // TODO: pull the board demand+health feed and fetch popular/endangered hashes here
        // TODO: enforce the disk budget via a RetentionPolicy (evict by frecency, pin low-replica)
        // TODO: honor the board blocklist (CSAM/takedown), export Prometheus metrics
        tracing::info!(stats = %daemon.stats(), "seednode tick");
        tokio::time::sleep(Duration::from_secs(300)).await;
    }
}
