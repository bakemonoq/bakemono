use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::content::{ContentSource, ProgressFn};
use crate::seeder::SeederHandle;

// the head-agnostic core: owns the seeder + config + content set, runs jobs from a ContentSource,
// re-seeds on start, and exposes a small control surface the IPC layer will expose verbatim
pub struct Daemon<C: ContentSource> {
    config: AppConfig,
    content_dir: PathBuf,
    seeder: SeederHandle,
    source: C,
    job: Mutex<Option<CancellationToken>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub running: bool,
    pub seeding: bool,
    pub content_dir: String,
}

impl<C: ContentSource> Daemon<C> {
    pub fn new(config: AppConfig, content_dir: PathBuf, source: C) -> Self {
        Self {
            config,
            content_dir,
            seeder: SeederHandle::new(),
            source,
            job: Mutex::new(None),
        }
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    // bring the seeder up and re-seed the content already on disk
    pub async fn start(&self) -> Result<()> {
        self.reseed().await?;
        Ok(())
    }

    pub async fn reseed(&self) -> Result<usize> {
        self.ensure_seeder().await?;
        let files = self.source.seedable(&self.content_dir);
        let mut count = 0;
        for file in &files {
            match self.seeder.seed(file).await {
                Ok(_) => count += 1,
                Err(e) => tracing::warn!("reseed failed for {}: {e:#}", file.display()),
            }
        }
        self.seeder.retain_staging(&files).await;
        tracing::info!(count, "reseeded content set");
        Ok(count)
    }

    pub async fn run_job(&self, job: Value, progress: ProgressFn<'_>) -> Result<Value> {
        let cancel = self.begin()?;
        let seeder = if self.config.seed {
            self.ensure_seeder().await?;
            Some(&self.seeder)
        } else {
            None
        };
        let result = self.source.run(job, seeder, &cancel, progress).await;
        self.end();
        result
    }

    pub fn cancel(&self) {
        if let Some(token) = self.lock_job().as_ref() {
            tracing::info!("cancel requested");
            token.cancel();
        }
    }

    pub async fn status(&self) -> Status {
        Status {
            running: self.lock_job().is_some(),
            seeding: self.seeder.is_started().await,
            content_dir: self.content_dir.display().to_string(),
        }
    }

    pub async fn shutdown(&self) {
        self.cancel();
        self.seeder.shutdown().await;
    }

    async fn ensure_seeder(&self) -> Result<()> {
        self.seeder
            .ensure_started(&self.config.trackers, &self.config.stun)
            .await
    }

    fn begin(&self) -> Result<CancellationToken> {
        let mut slot = self.lock_job();
        if slot.is_some() {
            bail!("a job is already running");
        }
        let token = CancellationToken::new();
        *slot = Some(token.clone());
        Ok(token)
    }

    fn end(&self) {
        *self.lock_job() = None;
    }

    fn lock_job(&self) -> std::sync::MutexGuard<'_, Option<CancellationToken>> {
        self.job.lock().expect("job slot poisoned")
    }
}
