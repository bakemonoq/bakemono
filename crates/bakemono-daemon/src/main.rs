use std::sync::Arc;

use anyhow::Result;

use bakemono_engine::identity::{key_path, Identity};
use bakemono_daemon::source::{scrape_dest, AppContentSource};
use bakemono_engine::config::AppConfig;
use bakemono_engine::daemon::Daemon;
use bakemono_engine::{ipc, logging};

#[tokio::main]
async fn main() -> Result<()> {
    let _log_guard = logging::init("daemon");
    // one daemon per machine: if another is already serving, do nothing
    if ipc::is_running().await {
        tracing::info!("a daemon is already running, exiting");
        return Ok(());
    }

    let config = AppConfig::load().unwrap_or_default();
    let identity = Identity::load_or_generate(&key_path())?;
    tracing::info!(npub = %identity.npub().unwrap_or_default(), "starting bakemono daemon");

    let source = AppContentSource {
        relays: config.relays.clone(),
        identity,
    };
    let daemon = Arc::new(Daemon::new(config, scrape_dest(), source));

    // re-seed in the background so the socket binds immediately and the daemon is
    // controllable right away even while a large content set is still being re-seeded
    let warmup = daemon.clone();
    tokio::spawn(async move {
        if let Err(e) = warmup.start().await {
            tracing::error!("startup reseed failed: {e:#}");
        }
    });

    ipc::serve(daemon).await
}
