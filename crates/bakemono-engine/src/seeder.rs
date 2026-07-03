use std::path::Path;
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

    pub async fn ensure_started(
        &self,
        trackers: &[String],
        max_up_mbit: u32,
        max_down_mbit: u32,
    ) -> Result<()> {
        let mut guard = self.inner.lock().await;
        if guard.is_none() {
            let session_dir = super::data_dir().join("seed-session");
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
            let seeder = Seeder::start(
                session_dir,
                trackers,
                port,
                mbit_to_bps(max_up_mbit),
                mbit_to_bps(max_down_mbit),
            )
            .await?;
            *guard = Some(seeder);
            tracing::info!("seeder started");
        }
        Ok(())
    }

    pub async fn seed(&self, file: &Path, sha256: Option<&str>) -> Result<SeedInfo> {
        let guard = self.inner.lock().await;
        match guard.as_ref() {
            Some(seeder) => seeder.seed(file, sha256).await,
            None => bail!("seeder not started"),
        }
    }

    pub async fn seed_in_place(&self, file: &Path) -> Result<SeedInfo> {
        let guard = self.inner.lock().await;
        match guard.as_ref() {
            Some(seeder) => seeder.seed_in_place(file).await,
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
}

// Mbit/s -> bytes/s for librqbit's caps; 0 means unlimited (None), and the clamp keeps the multiply in u32
fn mbit_to_bps(mbit: u32) -> Option<u32> {
    if mbit == 0 {
        return None;
    }
    Some(((mbit as u64) * 1_000_000 / 8).min(u32::MAX as u64) as u32)
}
