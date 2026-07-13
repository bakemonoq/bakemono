use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::stream::StreamExt;
use serde_json::Value;
use sqlx::postgres::PgPool;

use bakemono_scraper::ScrapeRequest;

use crate::kubo::Kubo;
use crate::scrape::{self, IngestStats, PostMeta};

// a mirror source is a kemono-style archive (e.g. pawchive): /api/v1/creators enumerates it and
// gallery-dl fetches {base}/{service}/user/{id}. mirrored posts keep their origin platform, so
// they merge and dedupe with directly scraped content

pub async fn run_scheduler(pool: PgPool, kubo: Arc<Kubo>, progress: Arc<Progress>) {
    let bases = bases_from_env();
    if bases.is_empty() {
        tracing::info!("mirror scheduler off (no BAKEMONO_MIRROR_URLS)");
        return;
    }
    let interval = env_u64("BAKEMONO_MIRROR_INTERVAL_SECS", 86_400);
    if interval == 0 {
        return;
    }
    loop {
        for base in &bases {
            match mirror_round(&pool, &kubo, base, &Limits::from_env(), &progress).await {
                Ok(stats) => tracing::info!(
                    base,
                    files = stats.files,
                    posts = stats.posts,
                    skipped = stats.skipped,
                    "mirror round done"
                ),
                Err(e) => tracing::error!(base, "mirror round failed: {e:#}"),
            }
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
    }
}

pub struct Limits {
    pub creators: usize,
    pub posts: u32,
    pub concurrency: usize,
    pub min_free_gb: u64,
}

impl Limits {
    pub fn from_env() -> Self {
        Self {
            creators: env_u64("BAKEMONO_MIRROR_CREATORS", 0) as usize,
            posts: env_u64("BAKEMONO_MIRROR_POSTS", 0) as u32,
            concurrency: (env_u64("BAKEMONO_MIRROR_CONCURRENCY", 2) as usize).max(1),
            min_free_gb: env_u64("BAKEMONO_MIRROR_MIN_FREE_GB", 25),
        }
    }
}

const PUBLISH_EVERY: Duration = Duration::from_secs(900);

pub async fn mirror_round(
    pool: &PgPool,
    kubo: &Kubo,
    base: &str,
    limits: &Limits,
    progress: &Progress,
) -> Result<IngestStats> {
    let base = base.trim_end_matches('/');
    let picked = pick(fetch_creators(base).await?, limits.creators);
    tracing::info!(base, creators = picked.len(), "mirror round starting");
    progress.start_round(host_of(base), picked.len());
    let root = scrape::staging_dir().join("mirror").join(host_of(base));
    std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;

    let full = Arc::new(AtomicBool::new(false));
    let posts = limits.posts;
    let min_free = limits.min_free_gb;
    let jobs = picked.into_iter().map(|creator| {
        let root = root.clone();
        let full = full.clone();
        async move {
            if full.load(Ordering::Relaxed) {
                return IngestStats::default();
            }
            if let Some(free) = free_gb(&root) {
                if free < min_free {
                    if !full.swap(true, Ordering::Relaxed) {
                        tracing::warn!(free, min_free, "mirror pausing: staging disk low");
                    }
                    return IngestStats::default();
                }
            }
            mirror_creator(pool, kubo, base, &root, &creator, posts, progress).await
        }
    });

    let mut stream = futures::stream::iter(jobs).buffer_unordered(limits.concurrency);
    let mut total = IngestStats::default();
    let mut last_publish = Instant::now();
    while let Some(stats) = stream.next().await {
        total.files += stats.files;
        total.posts += stats.posts;
        total.skipped += stats.skipped;
        // the initial fill can run for weeks; publish along the way so keepers and restore always
        // have a recent signed head over what already landed
        if stats.files > 0 && last_publish.elapsed() > PUBLISH_EVERY {
            if let Err(e) = crate::publish::publish_if_changed(pool, kubo).await {
                tracing::error!("mirror publish failed: {e:#}");
            }
            last_publish = Instant::now();
        }
    }
    drop(stream);
    progress.finish_round();
    if total.files > 0 {
        crate::publish::publish_if_changed(pool, kubo).await?;
    }
    Ok(total)
}

async fn mirror_creator(
    pool: &PgPool,
    kubo: &Kubo,
    base: &str,
    root: &Path,
    creator: &Creator,
    posts: u32,
    progress: &Progress,
) -> IngestStats {
    let scope = root.join(format!("{}-{}", creator.service, creator.id));
    if let Err(e) = std::fs::create_dir_all(&scope) {
        tracing::warn!("mirror staging {}: {e}", scope.display());
        return IngestStats { skipped: 1, ..Default::default() };
    }
    let url = format!("{base}/{}/user/{}", creator.service, creator.id);
    let mut request = ScrapeRequest::new(url.clone(), scope.clone());
    // per-creator archive: re-runs skip everything already ingested even though staged files are
    // deleted, and concurrent creators never contend on one sqlite file
    request.archive = Some(scope.join("archive.sqlite3"));
    // posts the mirror itself never got in full only carry previews; keep those out of the archive
    request.options = vec!["image-filter=has_full".to_string()];
    if posts > 0 {
        request.options.push(format!("max-posts={posts}"));
    }
    let key = format!("{}-{}", creator.service, creator.id);
    progress.begin(&key, &creator.name, &creator.service);
    let ingest =
        scrape::stream_ingest(pool, kubo, &mirror_scraper(), &request, &scope, |meta| {
            progress.bump(&key, meta);
        })
        .await;
    match ingest {
        Ok((stats, _)) => {
            if stats.files > 0 {
                tracing::info!(url, files = stats.files, posts = stats.posts, "mirrored creator");
            }
            progress.finish(&key, stats.files, stats.posts, false);
            stats
        }
        Err(e) => {
            tracing::warn!(url, "mirror creator failed: {e:#}");
            progress.finish(&key, 0, 0, true);
            IngestStats { skipped: 1, ..Default::default() }
        }
    }
}

// mirrors get their own gallery-dl (the image pins a master build there while the released
// extractor still dials pawchive's dead .st file host); unset falls back to the stock binary
fn mirror_scraper() -> bakemono_scraper::Scraper {
    match std::env::var_os("BAKEMONO_GALLERY_DL_MIRROR").filter(|s| !s.is_empty()) {
        Some(bin) => bakemono_scraper::Scraper::with_binary(bin),
        None => scrape::scraper(),
    }
}

// live per-creator round state the /mod dashboard reads. shared across the concurrent creator jobs;
// the mirror never persists this, so a restart just starts the next round's view fresh
#[derive(Default)]
pub struct Progress {
    round: Mutex<Option<Round>>,
}

struct Round {
    base: String,
    started: Instant,
    total: usize,
    done: usize,
    files: usize,
    posts: usize,
    active: Vec<Active>,
    recent: VecDeque<Finished>,
    finished: bool,
}

struct Active {
    key: String,
    name: String,
    platform: String,
    started: Instant,
    files: usize,
    posts: HashSet<String>,
}

struct Finished {
    name: String,
    platform: String,
    files: usize,
    posts: usize,
    secs: u64,
    skipped: bool,
}

const RECENT_CAP: usize = 12;

impl Progress {
    fn start_round(&self, base: String, total: usize) {
        *self.round.lock().unwrap() = Some(Round {
            base,
            started: Instant::now(),
            total,
            done: 0,
            files: 0,
            posts: 0,
            active: Vec::new(),
            recent: VecDeque::new(),
            finished: false,
        });
    }

    fn begin(&self, key: &str, name: &str, platform: &str) {
        if let Some(r) = self.round.lock().unwrap().as_mut() {
            r.active.push(Active {
                key: key.to_string(),
                name: name.to_string(),
                platform: platform.to_string(),
                started: Instant::now(),
                files: 0,
                posts: HashSet::new(),
            });
        }
    }

    fn bump(&self, key: &str, meta: &PostMeta) {
        if let Some(r) = self.round.lock().unwrap().as_mut() {
            if let Some(a) = r.active.iter_mut().find(|a| a.key == key) {
                a.files += 1;
                a.posts.insert(meta.post_key());
            }
        }
    }

    fn finish(&self, key: &str, files: usize, posts: usize, skipped: bool) {
        if let Some(r) = self.round.lock().unwrap().as_mut() {
            let secs = r
                .active
                .iter()
                .find(|a| a.key == key)
                .map(|a| a.started.elapsed().as_secs())
                .unwrap_or(0);
            let (name, platform) = r
                .active
                .iter()
                .find(|a| a.key == key)
                .map(|a| (a.name.clone(), a.platform.clone()))
                .unwrap_or_else(|| (key.to_string(), String::new()));
            r.active.retain(|a| a.key != key);
            r.done += 1;
            r.files += files;
            r.posts += posts;
            r.recent.push_front(Finished { name, platform, files, posts, secs, skipped });
            r.recent.truncate(RECENT_CAP);
        }
    }

    fn finish_round(&self) {
        if let Some(r) = self.round.lock().unwrap().as_mut() {
            r.active.clear();
            r.finished = true;
        }
    }

    pub fn snapshot(&self) -> Option<RoundSnapshot> {
        let guard = self.round.lock().unwrap();
        let r = guard.as_ref()?;
        let active: Vec<ActiveSnapshot> = r
            .active
            .iter()
            .map(|a| ActiveSnapshot {
                name: a.name.clone(),
                platform: a.platform.clone(),
                files: a.files,
                posts: a.posts.len(),
                elapsed_secs: a.started.elapsed().as_secs(),
            })
            .collect();
        let live_files: usize = active.iter().map(|a| a.files).sum();
        let live_posts: usize = active.iter().map(|a| a.posts).sum();
        Some(RoundSnapshot {
            base: r.base.clone(),
            elapsed_secs: r.started.elapsed().as_secs(),
            total: r.total,
            done: r.done,
            files: r.files + live_files,
            posts: r.posts + live_posts,
            finished: r.finished,
            active,
            recent: r
                .recent
                .iter()
                .map(|f| FinishedSnapshot {
                    name: f.name.clone(),
                    platform: f.platform.clone(),
                    files: f.files,
                    posts: f.posts,
                    secs: f.secs,
                    skipped: f.skipped,
                })
                .collect(),
        })
    }
}

pub struct RoundSnapshot {
    pub base: String,
    pub elapsed_secs: u64,
    pub total: usize,
    pub done: usize,
    pub files: usize,
    pub posts: usize,
    pub finished: bool,
    pub active: Vec<ActiveSnapshot>,
    pub recent: Vec<FinishedSnapshot>,
}

pub struct ActiveSnapshot {
    pub name: String,
    pub platform: String,
    pub files: usize,
    pub posts: usize,
    pub elapsed_secs: u64,
}

pub struct FinishedSnapshot {
    pub name: String,
    pub platform: String,
    pub files: usize,
    pub posts: usize,
    pub secs: u64,
    pub skipped: bool,
}

struct Creator {
    id: String,
    service: String,
    name: String,
    favorited: i64,
}

// the kemono-standard creators listing (~12 MB on pawchive). ever_imported drops rows that never
// had content; favorited-desc means the most wanted content mirrors first, which is also what
// survives if the disk guard pauses the fill
async fn fetch_creators(base: &str) -> Result<Vec<Creator>> {
    let url = format!("{base}/api/v1/creators");
    let client = reqwest::Client::builder()
        .user_agent("bakemono-mirror")
        .timeout(Duration::from_secs(300))
        .build()?;
    let raw = client
        .get(&url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .with_context(|| format!("fetching {url}"))?
        .bytes()
        .await?;
    parse_creators(&raw)
}

// typed rows, not a Value tree: the listing is ~90k entries and the board box is small
#[derive(serde::Deserialize)]
struct RawCreator {
    #[serde(default)]
    id: Value,
    #[serde(default)]
    service: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    favorited: Option<i64>,
    #[serde(default)]
    ever_imported: bool,
}

fn parse_creators(raw: &[u8]) -> Result<Vec<Creator>> {
    let list: Vec<RawCreator> =
        serde_json::from_slice(raw).context("creators list is not a json array")?;
    let mut out = Vec::new();
    for v in list {
        if !v.ever_imported || v.service.is_empty() {
            continue;
        }
        let Some(id) = stringy(Some(&v.id)) else {
            continue;
        };
        let name = if v.name.is_empty() { id.clone() } else { v.name };
        out.push(Creator { id, service: v.service, name, favorited: v.favorited.unwrap_or(0) });
    }
    Ok(out)
}

fn pick(mut creators: Vec<Creator>, cap: usize) -> Vec<Creator> {
    creators.sort_by(|a, b| b.favorited.cmp(&a.favorited));
    if cap > 0 {
        creators.truncate(cap);
    }
    creators
}

fn stringy(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn host_of(base: &str) -> String {
    base.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .filter(|h| !h.is_empty())
        .unwrap_or("mirror")
        .to_string()
}

fn bases_from_env() -> Vec<String> {
    std::env::var("BAKEMONO_MIRROR_URLS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(unix)]
fn free_gb(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    Some((s.f_bavail as u64).saturating_mul(s.f_frsize as u64) / (1024 * 1024 * 1024))
}

#[cfg(not(unix))]
fn free_gb(_path: &Path) -> Option<u64> {
    None
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_filters_and_ranks_creators() {
        let raw = r#"[
            {"id": "111", "service": "patreon", "name": "a", "favorited": 5, "ever_imported": true},
            {"id": "222", "service": "fanbox", "name": "b", "favorited": 900, "ever_imported": true},
            {"id": "333", "service": "patreon", "name": "never", "favorited": 9999, "ever_imported": false},
            {"id": 444, "service": "patreon", "favorited": 42, "ever_imported": true},
            {"service": "patreon", "ever_imported": true}
        ]"#;
        let picked = pick(parse_creators(raw.as_bytes()).unwrap(), 2);
        assert_eq!(picked.len(), 2);
        assert_eq!(picked[0].id, "222");
        assert_eq!(picked[0].service, "fanbox");
        assert_eq!(picked[1].id, "444");
    }

    #[test]
    fn host_of_strips_scheme_and_path() {
        assert_eq!(host_of("https://pawchive.st"), "pawchive.st");
        assert_eq!(host_of("http://pawchive.st/"), "pawchive.st");
        assert_eq!(host_of(""), "mirror");
    }
}
