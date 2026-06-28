use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Result};
use tokio::sync::Mutex;

use bakemono_seeder::{SeedInfo, Seeder};

// one webtorrent sidecar for the whole daemon lifetime: started once, fed files as they arrive,
// torn down only on shutdown so published magnets keep a live seeder behind them
#[derive(Clone, Default)]
pub struct SeederHandle {
    inner: Arc<Mutex<Option<Seeder>>>,
}

impl SeederHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn ensure_started(&self, trackers: &[String], stun: &[String]) -> Result<()> {
        let mut guard = self.inner.lock().await;
        if guard.is_none() {
            // stage on the data volume so hardlinks to scraped files never fall back to copying
            let staging = super::data_dir().join("staging");
            let seeder =
                Seeder::from_env_with(&swarm_env(trackers, stun), Some(&staging)).await?;
            *guard = Some(seeder);
            tracing::info!("webtorrent seeder started");
        }
        Ok(())
    }

    pub async fn seed(&self, file: &Path) -> Result<SeedInfo> {
        let mut guard = self.inner.lock().await;
        match guard.as_mut() {
            Some(seeder) => seeder.seed(file).await,
            None => bail!("seeder not started"),
        }
    }

    pub async fn shutdown(&self) {
        if let Some(seeder) = self.inner.lock().await.take() {
            seeder.shutdown().await.ok();
        }
    }

    pub async fn retain_staging(&self, live_sources: &[PathBuf]) {
        if let Some(seeder) = self.inner.lock().await.as_ref() {
            seeder.retain_staging(live_sources);
        }
    }
}

// config supplies the swarm settings, but a launch-time env var (used for testing) wins
fn swarm_env(trackers: &[String], stun: &[String]) -> Vec<(String, String)> {
    let mut env = Vec::new();
    if std::env::var_os("BAKEMONO_TRACKERS").is_none() && !trackers.is_empty() {
        env.push(("BAKEMONO_TRACKERS".to_string(), trackers.join(",")));
    }
    if std::env::var_os("BAKEMONO_STUN").is_none() && !stun.is_empty() {
        env.push(("BAKEMONO_STUN".to_string(), stun.join(",")));
    }
    env
}
