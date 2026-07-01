use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Result};
use tokio::sync::Mutex;

use bakemono_torrent::{SeedInfo, Seeder};

// one librqbit seed session for the daemon lifetime: started once, fed files as they arrive,
// so published magnets keep a live seeder behind them
#[derive(Clone, Default)]
pub struct SeederHandle {
    inner: Arc<Mutex<Option<Seeder>>>,
}

impl SeederHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn ensure_started(&self, trackers: &[String]) -> Result<()> {
        let mut guard = self.inner.lock().await;
        if guard.is_none() {
            // stage on the data volume so hardlinks to scraped files never fall back to copying
            let staging = super::data_dir().join("staging");
            // wss trackers were WebRTC-only; classic BT announces to udp/http trackers plus DHT
            let trackers: Vec<String> = trackers
                .iter()
                .filter(|t| !t.starts_with("wss://"))
                .cloned()
                .collect();
            // seed on a fixed TCP port by default so a gateway can pin us; librqbit opens no listener
            // without one, which leaves the seeder undialable
            let port = std::env::var("BAKEMONO_SEED_PORT")
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .or(Some(4250));
            let seeder = Seeder::start(staging, trackers, port).await?;
            *guard = Some(seeder);
            tracing::info!("seeder started");
        }
        Ok(())
    }

    pub async fn seed(&self, file: &Path) -> Result<SeedInfo> {
        let guard = self.inner.lock().await;
        match guard.as_ref() {
            Some(seeder) => seeder.seed(file).await,
            None => bail!("seeder not started"),
        }
    }

    pub async fn is_started(&self) -> bool {
        self.inner.lock().await.is_some()
    }

    pub async fn shutdown(&self) {
        // dropping the session ends seeding; there is no external process to reap
        let _ = self.inner.lock().await.take();
    }

    pub async fn retain_staging(&self, live_sources: &[PathBuf]) {
        if let Some(seeder) = self.inner.lock().await.as_ref() {
            seeder.retain_staging(live_sources);
        }
    }
}
