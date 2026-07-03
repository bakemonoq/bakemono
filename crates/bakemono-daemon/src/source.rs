use std::future::Future;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use bakemono_engine::content::{ContentSource, ProgressFn};
use bakemono_engine::seeder::SeederHandle;
use bakemono_scraper::{Cookies, ScrapeRequest};

use bakemono_engine::identity::Identity;
use crate::pipeline::{run_ingest, run_republish, run_scrape, JobContext, Progress};
use crate::scrape::gather_pairs;

// the app's half of the daemon: scrape -> hash -> sign -> publish -> seed what I made
pub struct AppContentSource {
    pub relays: Vec<String>,
    pub identity: Identity,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AppJob {
    Scrape {
        creator: String,
        #[serde(default)]
        limit: Option<u32>,
        #[serde(default)]
        cookies: Option<String>,
        #[serde(default)]
        browser: Option<String>,
        #[serde(default)]
        dest: Option<String>,
    },
    Ingest {
        dir: String,
    },
    Republish {
        #[serde(default)]
        dir: Option<String>,
    },
}

impl ContentSource for AppContentSource {
    fn run(
        &self,
        job: Value,
        seeder: Option<&SeederHandle>,
        cancel: &CancellationToken,
        progress: ProgressFn<'_>,
    ) -> impl Future<Output = Result<Value>> + Send {
        async move {
            let job: AppJob = serde_json::from_value(job).context("parsing job")?;
            let emit = move |p: Progress| progress(serde_json::to_value(&p).unwrap_or(Value::Null));
            let ctx = JobContext {
                relays: &self.relays,
                identity: &self.identity,
                seeder,
                cancel,
                progress: &emit,
            };
            let summary = match job {
                AppJob::Scrape {
                    creator,
                    limit,
                    cookies,
                    browser,
                    dest,
                } => {
                    let dest = dest.map(PathBuf::from).unwrap_or_else(scrape_dest);
                    let mut request = ScrapeRequest::new(creator, dest);
                    request.cookies = resolve_login(browser, cookies)?;
                    run_scrape(request, limit.map(|n| n as usize), &ctx).await?
                }
                AppJob::Ingest { dir } => run_ingest(Path::new(&dir), &ctx).await?,
                AppJob::Republish { dir } => {
                    let dir = dir.map(PathBuf::from).unwrap_or_else(scrape_dest);
                    run_republish(&dir, &ctx).await?
                }
            };
            Ok(serde_json::to_value(summary)?)
        }
    }

    fn seedable(&self, content_dir: &Path) -> Vec<PathBuf> {
        gather_pairs(content_dir)
            .unwrap_or_default()
            .into_iter()
            .map(|(media, _sidecar)| media)
            .collect()
    }

    fn stats(&self, content_dir: &Path) -> Value {
        serde_json::to_value(crate::catalog::stats(content_dir)).unwrap_or(Value::Null)
    }
}

pub fn scrape_dest() -> PathBuf {
    bakemono_engine::data_dir().join("scrape")
}

// a chosen browser wins over a cookies file; either is optional (public posts need neither)
pub fn resolve_login(browser: Option<String>, cookies: Option<String>) -> Result<Option<Cookies>> {
    if let Some(browser) = browser.filter(|b| !b.trim().is_empty()) {
        return Ok(Some(Cookies::Browser(browser)));
    }
    match cookies.filter(|c| !c.trim().is_empty()) {
        Some(raw) => Ok(Some(Cookies::File(resolve_cookies(&raw)?))),
        None => Ok(None),
    }
}

// resolve to an absolute path so gallery-dl finds it regardless of the process's working directory
fn resolve_cookies(raw: &str) -> Result<PathBuf> {
    let path = PathBuf::from(raw);
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    if !absolute.is_file() {
        anyhow::bail!("cookies file not found: {}", absolute.display());
    }
    Ok(absolute)
}
