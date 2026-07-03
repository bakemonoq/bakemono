use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
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
            match self.seeder.seed(file, None).await {
                Ok(info) => {
                    count += 1;
                    self.reseed_legacy(file, &info.info_hash).await;
                }
                Err(e) => tracing::warn!("reseed failed for {}: {e:#}", file.display()),
            }
        }
        tracing::info!(count, "reseeded content set");
        Ok(count)
    }

    // a magnet published before deterministic construction points at a different infohash; seed that
    // torrent too so the already-published events stay fetchable
    async fn reseed_legacy(&self, file: &Path, deterministic: &str) {
        let Some(magnet) = self.source.published_magnet(file) else {
            return;
        };
        match bakemono_torrent::infohash_from_magnet(&magnet) {
            Some(old) if old != deterministic => {
                if let Err(e) = self.seeder.seed_in_place(file).await {
                    tracing::warn!("legacy reseed failed for {}: {e:#}", file.display());
                }
            }
            _ => {}
        }
    }

    pub async fn run_job(&self, job: Value, progress: ProgressFn<'_>) -> Result<Value> {
        // the guard releases the job slot on every exit path; an early error (e.g. the seeder
        // sidecar failing to start) must not leave the daemon wedged in 'a job is already running'
        let guard = self.begin()?;
        tracing::info!("job started");
        let result = self.run_guarded(job, guard.token(), progress).await;
        match &result {
            Ok(_) => tracing::info!("job finished"),
            Err(e) => tracing::warn!("job failed: {e:#}"),
        }
        result
    }

    async fn run_guarded(
        &self,
        job: Value,
        cancel: &CancellationToken,
        progress: ProgressFn<'_>,
    ) -> Result<Value> {
        let seeder = if self.config.seed {
            self.ensure_seeder().await.context("starting the seeder")?;
            Some(&self.seeder)
        } else {
            None
        };
        self.source.run(job, seeder, cancel, progress).await
    }

    pub fn cancel(&self) {
        if let Some(token) = self.lock_job().as_ref() {
            tracing::info!("cancel requested");
            token.cancel();
        }
    }

    pub async fn status(&self) -> Status {
        let running = self.lock_job().is_some();
        let seeding = self.seeder.is_started().await;
        Status {
            running,
            seeding,
            content_dir: self.content_dir.display().to_string(),
        }
    }

    pub fn stats(&self) -> Value {
        self.source.stats(&self.content_dir)
    }

    pub async fn shutdown(&self) {
        self.cancel();
        self.seeder.shutdown().await;
    }

    async fn ensure_seeder(&self) -> Result<()> {
        self.seeder
            .ensure_started(
                &self.config.trackers,
                self.config.max_up_mbit,
                self.config.max_down_mbit,
            )
            .await
    }

    fn begin(&self) -> Result<JobGuard<'_>> {
        let token = CancellationToken::new();
        {
            let mut slot = self.lock_job();
            if slot.is_some() {
                bail!("a job is already running");
            }
            *slot = Some(token.clone());
        }
        Ok(JobGuard {
            slot: &self.job,
            token,
        })
    }

    fn lock_job(&self) -> std::sync::MutexGuard<'_, Option<CancellationToken>> {
        self.job.lock().expect("job slot poisoned")
    }
}

// releases the daemon's single job slot when the running job ends, errors out, or panics
struct JobGuard<'a> {
    slot: &'a Mutex<Option<CancellationToken>>,
    token: CancellationToken,
}

impl JobGuard<'_> {
    fn token(&self) -> &CancellationToken {
        &self.token
    }
}

impl Drop for JobGuard<'_> {
    fn drop(&mut self) {
        *self.slot.lock().expect("job slot poisoned") = None;
        tracing::debug!("job slot released");
    }
}
