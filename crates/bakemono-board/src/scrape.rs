use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use futures::stream::StreamExt;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPool;
use tokio_util::sync::CancellationToken;

use bakemono_scraper::{Cookies, ScrapeRequest, Scraper};

use crate::db;
use crate::kubo::Kubo;
use crate::thumb;

// serve only runs auto-import rounds when the operator has accepted keeping the private key on the
// box (BAKEMONO_COOKIE_PRIVKEY). the secure default keeps the key offline and drives rounds
// externally with `bakemono autoimport`
pub async fn run_scheduler(pool: PgPool, kubo: Arc<Kubo>) {
    let interval = env_secs("BAKEMONO_SCRAPE_INTERVAL_SECS", 86_400);
    let privkey = match crate::crypto::load_private_pem() {
        Ok(Some(pem)) => pem,
        Ok(None) => {
            tracing::info!("autoimport scheduler off (no BAKEMONO_COOKIE_PRIVKEY); run `bakemono autoimport` externally");
            return;
        }
        Err(e) => {
            tracing::error!("bad BAKEMONO_COOKIE_PRIVKEY: {e:#}");
            return;
        }
    };
    if interval == 0 {
        return;
    }
    loop {
        if let Err(e) = autoimport_round(&pool, &kubo, &privkey).await {
            tracing::error!("autoimport round failed: {e:#}");
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
    }
}

// one keyed round: decrypt every live cookie, scrape its whole subscription feed, ingest new posts.
// the plaintext token exists only in this function's stack and is dropped as each cookie finishes
pub async fn autoimport_round(pool: &PgPool, kubo: &Kubo, privkey_pem: &str) -> Result<()> {
    let cookies = db::autoimport_cookies(pool).await?;
    tracing::info!(count = cookies.len(), "autoimport round starting");
    // scrape every cookie concurrently, each in its own staging scope, so a huge feed (patreon) never
    // starves a smaller one (fanbox) behind it. bounded so a board with many cookies stays sane
    let futs: Vec<_> =
        cookies.iter().map(|cookie| scrape_cookie(pool, kubo, privkey_pem, cookie)).collect();
    let ingested: usize = futures::stream::iter(futs)
        .buffer_unordered(4)
        .fold(0usize, |sum, n| async move { sum + n })
        .await;
    if ingested > 0 {
        crate::publish::publish_if_changed(pool, kubo).await?;
    }
    tracing::info!(files = ingested, "autoimport round done");
    Ok(())
}

// decrypt one cookie, probe it, scrape its feed into an isolated staging scope; returns files ingested
async fn scrape_cookie(pool: &PgPool, kubo: &Kubo, privkey_pem: &str, cookie: &db::SealedCookie) -> usize {
    let token = match crate::crypto::open(privkey_pem, &cookie.sealed) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) => {
            tracing::error!(cookie = cookie.id, "decrypt failed: {e:#}");
            return 0;
        }
    };
    let scope = cookie.id.to_string();
    if !probe_cookie(&cookie.platform, &token, &scope).await {
        db::mark_cookie(pool, cookie.id, "dead", Some("cookie rejected by the platform")).await.ok();
        return 0;
    }
    match scrape_feed(pool, kubo, &cookie.platform, &token, &scope).await {
        Ok((stats, creators)) => {
            db::set_cookie_creators(pool, cookie.id, &cookie.platform, &creators).await.ok();
            db::mark_cookie(pool, cookie.id, "live", None).await.ok();
            stats.files
        }
        Err(e) => {
            tracing::error!(cookie = cookie.id, "feed scrape failed: {e:#}");
            db::mark_cookie(pool, cookie.id, "live", Some(&format!("{e:#}"))).await.ok();
            0
        }
    }
}

// does this cookie still authenticate? a live cookie lets gallery-dl read the first feed item, a
// dead one hits a login wall. unified across platforms, no per-platform API code
pub async fn probe_cookie(platform: &str, token: &str, scope: &str) -> bool {
    let staging = staging_dir().join(scope);
    if std::fs::create_dir_all(&staging).is_err() {
        return false;
    }
    let (Some(feed_url), Some(cookie_txt)) =
        (crate::platform::feed_url(platform), crate::platform::netscape_cookie(platform, token))
    else {
        return false;
    };
    let Ok(cookie_file) = CookieFile::write(&staging, &cookie_txt) else {
        return false;
    };
    scraper_for(platform)
        .probe(feed_url, Some(&cookie_file.path), platform_proxy(platform).as_deref())
        .await
        .unwrap_or(false)
}

pub type CreatorSeen = (String, String, String);

// scrape the platform's whole subscription feed with the cookie. gallery-dl enumerates every
// creator the cookie can reach and paginates the full history; we ingest and derive the creator
// set from the sidecars. returns ingest stats and the distinct creators seen
pub async fn scrape_feed(
    pool: &PgPool,
    kubo: &Kubo,
    platform: &str,
    token: &str,
    scope: &str,
) -> Result<(IngestStats, Vec<CreatorSeen>)> {
    let staging = staging_dir().join(scope);
    std::fs::create_dir_all(&staging).with_context(|| format!("creating {}", staging.display()))?;
    let feed_url = crate::platform::feed_url(platform).context("platform has no feed")?;
    let cookie_txt = crate::platform::netscape_cookie(platform, token).context("no cookie name")?;
    let cookie_file = CookieFile::write(&staging, &cookie_txt)?;

    let mut request = ScrapeRequest::new(feed_url, staging.clone());
    request.archive = Some(staging.join("archive.sqlite3"));
    request.cookies = Some(Cookies::File(cookie_file.path.clone()));
    request.options = scrape_options(platform);

    stream_ingest(pool, kubo, &scraper_for(platform), &request, &staging, |_| {}).await
}

// ingest each file the moment gallery-dl finishes downloading it, streaming paths over a channel
// to a concurrent consumer. content shows up as it downloads, a restart mid-scrape only loses the
// one file in flight, and deleting each file as it lands keeps staging bounded. on_file fires once
// per ingested file so a caller can surface live progress
pub async fn stream_ingest<F: FnMut(&PostMeta) + Send>(
    pool: &PgPool,
    kubo: &Kubo,
    scraper: &Scraper,
    request: &ScrapeRequest,
    staging: &Path,
    mut on_file: F,
) -> Result<(IngestStats, Vec<CreatorSeen>)> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PathBuf>();
    let scrape = async {
        let r = scraper
            .scrape_streaming(request, CancellationToken::new(), move |p| {
                let _ = tx.send(p);
            })
            .await;
        if let Err(e) = r {
            // partial errors (a gated post, a dead CDN link) are normal for a big run
            tracing::warn!(url = %request.creator, "scrape reported errors: {e:#}");
        }
    };
    let consume = async {
        let mut acc = Accum::default();
        while let Some(media) = rx.recv().await {
            if is_sidecar(&media) || is_thumb(&media) {
                continue;
            }
            if !keep_extension(&media) {
                let _ = std::fs::remove_file(&media);
                let _ = std::fs::remove_file(sidecar_path(&media));
                continue;
            }
            let sidecar = sidecar_path(&media);
            // the metadata sidecar is written right after gallery-dl prints the media path; give it a
            // moment, and if it never lands leave the file for the final sweep
            if !wait_for_sidecar(&sidecar).await {
                continue;
            }
            match ingest_pair(pool, kubo, &media, &sidecar).await {
                Ok(meta) => {
                    acc.record(&meta);
                    on_file(&meta);
                    let _ = std::fs::remove_file(&media);
                    let _ = std::fs::remove_file(&sidecar);
                }
                Err(e) => {
                    tracing::warn!("skipping {}: {e:#}", media.display());
                    acc.stats.skipped += 1;
                }
            }
        }
        acc
    };
    let (_, mut acc) = tokio::join!(scrape, consume);

    // final sweep: files whose sidecar was slow, plus orphans a prior interrupted run left behind
    let (sweep_stats, sweep_creators) = ingest_staging(pool, kubo, staging).await?;
    acc.merge_sweep(sweep_stats, sweep_creators);
    Ok(acc.finish())
}

async fn wait_for_sidecar(sidecar: &Path) -> bool {
    for _ in 0..10 {
        if sidecar.is_file() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    sidecar.is_file()
}

// accumulates ingest stats and the distinct creators seen across a scrape
#[derive(Default)]
struct Accum {
    stats: IngestStats,
    posts_seen: HashSet<String>,
    creators: std::collections::BTreeMap<String, CreatorSeen>,
}

impl Accum {
    fn record(&mut self, meta: &PostMeta) {
        self.stats.files += 1;
        if self.posts_seen.insert(meta.post_key()) {
            self.stats.posts += 1;
        }
        self.creators.entry(meta.creator_id.clone()).or_insert_with(|| {
            (meta.creator_id.clone(), meta.creator.clone(), meta.creator_url.clone().unwrap_or_default())
        });
    }

    fn merge_sweep(&mut self, stats: IngestStats, creators: Vec<CreatorSeen>) {
        self.stats.files += stats.files;
        self.stats.posts += stats.posts;
        self.stats.skipped += stats.skipped;
        for c in creators {
            self.creators.entry(c.0.clone()).or_insert(c);
        }
    }

    fn finish(self) -> (IngestStats, Vec<CreatorSeen>) {
        (self.stats, self.creators.into_values().collect())
    }
}

// one-off operator scrape of a specific URL with a raw cookies.txt; batch ingest, keeps files
pub async fn scrape_source(
    pool: &PgPool,
    kubo: &Kubo,
    url: &str,
    cookies: Option<&str>,
    limit: Option<u32>,
) -> Result<IngestStats> {
    let staging = staging_dir();
    std::fs::create_dir_all(&staging).with_context(|| format!("creating {}", staging.display()))?;

    let mut request = ScrapeRequest::new(url, staging.clone());
    request.archive = Some(staging.join("archive.sqlite3"));
    request.limit = limit;
    let _cookie_file = match cookies {
        Some(text) => {
            let file = CookieFile::write(&staging, text)?;
            request.cookies = Some(Cookies::File(file.path.clone()));
            Some(file)
        }
        None => None,
    };

    let printed = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
    let sink = printed.clone();
    let scraper = scraper();
    scraper
        .scrape_streaming(&request, CancellationToken::new(), move |path| {
            sink.lock().expect("printed paths").push(path);
        })
        .await?;

    let printed = std::mem::take(&mut *printed.lock().expect("printed paths"));
    let (stats, _) = ingest_paths(pool, kubo, &printed, false).await;
    Ok(stats)
}

// ingest every media+sidecar pair under a directory, deleting each after it lands in the archive
async fn ingest_staging(pool: &PgPool, kubo: &Kubo, dir: &Path) -> Result<(IngestStats, Vec<CreatorSeen>)> {
    let mut files = Vec::new();
    walk(dir, &mut files)?;
    Ok(ingest_paths(pool, kubo, &files, true).await)
}

// import a directory of already-scraped media + gallery-dl sidecars (bootstrap dumps); keeps files
pub async fn ingest_dir(pool: &PgPool, kubo: &Kubo, dir: &Path) -> Result<IngestStats> {
    let mut files = Vec::new();
    walk(dir, &mut files)?;
    Ok(ingest_paths(pool, kubo, &files, false).await.0)
}

#[derive(Default)]
pub struct IngestStats {
    pub files: usize,
    pub posts: usize,
    pub skipped: usize,
}

// ingest each media+sidecar pair; when `delete`, remove each file once it is safely in the archive
// so the staging dir stays bounded. also returns the distinct creators seen, keyed platform+id
async fn ingest_paths(
    pool: &PgPool,
    kubo: &Kubo,
    files: &[PathBuf],
    delete: bool,
) -> (IngestStats, Vec<CreatorSeen>) {
    let present: HashSet<&PathBuf> = files.iter().collect();
    let mut stats = IngestStats::default();
    let mut posts_seen = HashSet::new();
    let mut creators: std::collections::BTreeMap<(String, String), CreatorSeen> = Default::default();
    for media in files {
        if is_sidecar(media) || is_thumb(media) {
            continue;
        }
        if !keep_extension(media) {
            if delete {
                let _ = std::fs::remove_file(media);
                let _ = std::fs::remove_file(sidecar_path(media));
            }
            continue;
        }
        let sidecar = sidecar_path(media);
        if !present.contains(&sidecar) && !sidecar.is_file() {
            stats.skipped += 1;
            continue;
        }
        match ingest_pair(pool, kubo, media, &sidecar).await {
            Ok(meta) => {
                stats.files += 1;
                if posts_seen.insert(meta.post_key()) {
                    stats.posts += 1;
                }
                creators
                    .entry((meta.platform.clone(), meta.creator_id.clone()))
                    .or_insert_with(|| {
                        (meta.creator_id.clone(), meta.creator.clone(), meta.creator_url.clone().unwrap_or_default())
                    });
                if delete {
                    let _ = std::fs::remove_file(media);
                    let _ = std::fs::remove_file(&sidecar);
                }
            }
            Err(e) => {
                tracing::warn!("skipping {}: {e:#}", media.display());
                stats.skipped += 1;
            }
        }
    }
    (stats, creators.into_values().collect())
}

async fn ingest_pair(pool: &PgPool, kubo: &Kubo, media: &Path, sidecar: &Path) -> Result<PostMeta> {
    let meta = post_meta(sidecar)?;
    let (sha256, size, mime) = {
        let media = media.to_path_buf();
        tokio::task::spawn_blocking(move || hash_media(&media)).await??
    };
    // revoked bytes must not re-enter the archive through a re-scrape
    if let Some(reason) = db::sha_denied(pool, &sha256).await? {
        anyhow::bail!("revoked content ({reason}), refusing to re-ingest");
    }
    // a thumb is archive content in its own right: it gets a catalog row so /ipfs/{cid} serves it,
    // and later the pinset and shard entries carry it alongside the full file
    let thumb_cid = match thumb::generate(media, &mime).await {
        Some(bytes) => {
            let thumb_sha = hex::encode(Sha256::digest(&bytes));
            let thumb_size = bytes.len() as i64;
            let cid = kubo.add(bytes, "thumb").await?;
            db::insert_file(pool, &cid, &thumb_sha, thumb_size, "image/jpeg", None, None, None).await?;
            kubo.pin_archive(&cid, &format!("thumb {}", meta.post_key())).await?;
            Some(cid)
        }
        None => None,
    };
    let dims = if mime.starts_with("image/") || mime.starts_with("video/") {
        thumb::dimensions(media).await
    } else {
        None
    };
    let cid = kubo.add_path(media).await?;
    kubo.pin_archive(&cid, &meta.post_key()).await?;
    let filename = media.file_name().and_then(|n| n.to_str());
    db::insert_file(pool, &cid, &sha256, size as i64, &mime, filename, thumb_cid.as_deref(), dims).await?;
    db::upsert_creator(pool, &meta.platform, &meta.creator_id, &meta.creator).await?;
    db::upsert_post(pool, &meta).await?;
    db::upsert_post_file(pool, &meta, &cid).await?;
    Ok(meta)
}

pub struct PostMeta {
    pub platform: String,
    pub creator: String,
    pub creator_id: String,
    pub post_id: String,
    pub file_index: i32,
    pub title: String,
    pub body: String,
    pub posted_at: Option<String>,
    pub tier: String,
    pub creator_url: Option<String>,
}

impl PostMeta {
    pub fn post_key(&self) -> String {
        format!("{}:{}:{}", self.platform, self.creator_id, self.post_id)
    }
}

// gallery-dl's per-platform sidecars differ (Patreon nests creator.{id,full_name}; Fanbox uses
// creatorId + user.name; kemono-family mirrors like pawchive put the origin platform in `service`
// and the creator in flat `user`/`username`). read each field through a fallback chain so any
// supported shape maps, and mirrored posts land under their origin platform so they merge and
// dedupe with directly scraped content
fn post_meta(sidecar: &Path) -> Result<PostMeta> {
    let raw = std::fs::read(sidecar).with_context(|| format!("reading {}", sidecar.display()))?;
    let meta: Value =
        serde_json::from_slice(&raw).with_context(|| format!("parsing {}", sidecar.display()))?;
    let platform = string_at(&meta, &["service"]) // kemono-family mirror
        .or_else(|| string_at(&meta, &["category"]))
        .unwrap_or_else(|| "patreon".to_string());
    let creator_id = string_at(&meta, &["creator", "id"]) // patreon
        .or_else(|| string_at(&meta, &["creatorId"])) // fanbox
        .or_else(|| string_at(&meta, &["user", "userId"])) // fanbox numeric
        .or_else(|| string_at(&meta, &["user"])) // kemono-family (plain string, not the fanbox object)
        .context("sidecar has no creator id")?;
    Ok(PostMeta {
        creator: string_at(&meta, &["creator", "full_name"])
            .or_else(|| string_at(&meta, &["creator", "vanity"]))
            .or_else(|| string_at(&meta, &["user", "name"])) // fanbox
            .or_else(|| string_at(&meta, &["username"])) // kemono-family
            .unwrap_or_else(|| creator_id.clone()),
        creator_url: creator_url(&meta, &platform, &creator_id),
        creator_id,
        platform,
        post_id: string_at(&meta, &["id"]).context("sidecar missing id")?,
        file_index: meta
            .get("num")
            .and_then(Value::as_u64)
            .map(|n| n.saturating_sub(1) as i32)
            .unwrap_or(0),
        title: string_at(&meta, &["title"]).map(|t| t.trim().to_string()).unwrap_or_default(),
        body: string_at(&meta, &["content"]).or_else(|| string_at(&meta, &["text"])).unwrap_or_default(),
        posted_at: string_at(&meta, &["published_at"])
            .or_else(|| string_at(&meta, &["publishedDatetime"]))
            .or_else(|| string_at(&meta, &["published"])) // kemono-family
            .or_else(|| string_at(&meta, &["date"])),
        tier: tier_of(&meta),
    })
}

fn creator_url(meta: &Value, platform: &str, creator_id: &str) -> Option<String> {
    if let Some(url) = string_at(meta, &["creator", "url"]) {
        return Some(url);
    }
    // mirrored sidecars carry only the numeric origin id, so fall back to the id-based page
    match platform {
        "patreon" => string_at(meta, &["creator", "vanity"])
            .map(|v| format!("https://www.patreon.com/{v}"))
            .or_else(|| Some(format!("https://www.patreon.com/user?u={creator_id}"))),
        "fanbox" => string_at(meta, &["creatorId"])
            .map(|c| format!("https://{c}.fanbox.cc"))
            .or_else(|| Some(format!("https://www.pixiv.net/fanbox/creator/{creator_id}"))),
        _ => Some(format!("https://{creator_id}")),
    }
}

// one streaming pass: sha256 + byte count + a 12-byte header sniff, never buffering the whole file
fn hash_media(media: &Path) -> Result<(String, u64, String)> {
    let mut file =
        std::fs::File::open(media).with_context(|| format!("reading {}", media.display()))?;
    let mut hasher = Sha256::new();
    let mut header = Vec::with_capacity(12);
    let mut size: u64 = 0;
    let mut buf = vec![0u8; 128 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("reading {}", media.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        if header.len() < 12 {
            let take = (12 - header.len()).min(n);
            header.extend_from_slice(&buf[..take]);
        }
        size += n as u64;
    }
    Ok((hex::encode(hasher.finalize()), size, sniff_mime(&header).to_string()))
}

pub fn sniff_mime(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        "image/png"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        "video/mp4"
    } else if bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        "video/webm"
    } else {
        "application/octet-stream"
    }
}

fn tier_of(meta: &Value) -> String {
    // patreon posts always carry min_cents_pledged_to_view: 0 is a public post, null or a cents
    // threshold means patron-only (it stays null when access is by tier, not a cents floor). is_paid is
    // a rare per-post billing flag, false on almost every post. fanbox: feeRequired (0 = free tier)
    if let Some(cents) = meta.get("min_cents_pledged_to_view") {
        return if cents.as_u64() == Some(0) { "free" } else { "subscriber" }.to_string();
    }
    if let Some(fee) = meta.get("feeRequired").and_then(Value::as_u64) {
        return if fee > 0 { "subscriber" } else { "free" }.to_string();
    }
    "unknown".to_string()
}

fn string_at(value: &Value, path: &[&str]) -> Option<String> {
    let mut cursor = value;
    for key in path {
        cursor = cursor.get(key)?;
    }
    match cursor {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_dir() {
            walk(&entry.path(), out)?;
        } else if meta.is_file() {
            out.push(entry.path());
        }
    }
    Ok(())
}

fn is_sidecar(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("json")
}

fn is_thumb(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(".thumb.jpg") || n.ends_with(".thumbtmp.jpg") || n.ends_with(".thumbtmp.webp"))
}

// operator format policy: BAKEMONO_KEEP_EXTENSIONS is a space/comma list of extensions to keep
// (e.g. "jpg png mp4"); a file outside the list is never ingested. empty/unset keeps everything, so
// this is off by default for other operators
fn keep_extension(media: &Path) -> bool {
    static ALLOWED: OnceLock<Vec<String>> = OnceLock::new();
    let allowed = ALLOWED
        .get_or_init(|| parse_keep_exts(&std::env::var("BAKEMONO_KEEP_EXTENSIONS").unwrap_or_default()));
    ext_allowed(allowed, media)
}

fn parse_keep_exts(raw: &str) -> Vec<String> {
    raw.replace(',', " ").split_whitespace().map(|s| s.to_ascii_lowercase()).collect()
}

fn ext_allowed(allowed: &[String], media: &Path) -> bool {
    if allowed.is_empty() {
        return true;
    }
    match media.extension().and_then(|e| e.to_str()) {
        Some(ext) => allowed.iter().any(|a| a == &ext.to_ascii_lowercase()),
        None => false,
    }
}

fn sidecar_path(media: &Path) -> PathBuf {
    let mut name = media.to_path_buf().into_os_string();
    name.push(".json");
    PathBuf::from(name)
}

pub fn staging_dir() -> PathBuf {
    match std::env::var("BAKEMONO_SCRAPE_DIR").ok().filter(|s| !s.is_empty()) {
        Some(dir) => dir.into(),
        None => std::env::temp_dir().join("bakemono-scrape"),
    }
}

// a fresh staging scope per anonymous submission, monotonic so two concurrent contributors never
// share a cookie file or archive. pair every call with cleanup_scope once the work is done
pub fn new_submit_scope(platform: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("submit-{platform}-{}-{n}", std::process::id())
}

pub fn cleanup_scope(scope: &str) {
    let _ = std::fs::remove_dir_all(staging_dir().join(scope));
}

pub(crate) fn scraper() -> Scraper {
    match std::env::var_os("BAKEMONO_GALLERY_DL").filter(|s| !s.is_empty()) {
        Some(bin) => Scraper::with_binary(bin),
        None => Scraper::new(),
    }
}

// Fanbox needs the curl_cffi fork (firefox impersonation clears Cloudflare); everything else uses
// stock gallery-dl
fn scraper_for(platform: &str) -> Scraper {
    if platform == "fanbox" {
        if let Some(bin) = std::env::var_os("BAKEMONO_GALLERY_DL_FANBOX").filter(|s| !s.is_empty()) {
            return Scraper::with_binary(bin);
        }
        return Scraper::with_binary("gallery-dl-fanbox");
    }
    scraper()
}

// two global scrape proxies: BAKEMONO_SCRAPE_PROXY carries the api, _MEDIA_PROXY the bulk media
// (defaults to the api proxy). they apply only to platforms that need one (Cloudflare-gated), so
// direct-reachable sources like Patreon keep the box's full bandwidth. session rotation only applies
// to a proxy that actually carries a `session-<token>` segment
fn scrape_proxy() -> Option<String> {
    std::env::var("BAKEMONO_SCRAPE_PROXY").ok().filter(|s| !s.trim().is_empty())
}

// proxy for the probe (api only, no downloads); none for platforms that scrape direct
fn platform_proxy(platform: &str) -> Option<String> {
    if !crate::platform::needs_proxy(platform) {
        return None;
    }
    scrape_proxy().map(|t| rotate_session(&t))
}

fn scrape_options(platform: &str) -> Vec<String> {
    if !crate::platform::needs_proxy(platform) {
        return Vec::new();
    }
    let Some(api) = scrape_proxy() else {
        return Vec::new();
    };
    let media = std::env::var("BAKEMONO_SCRAPE_MEDIA_PROXY")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| api.clone());
    // a fresh session for each so the api and media legs ride different IPs
    vec![
        format!("extractor.{platform}.proxy={}", rotate_session(&api)),
        format!("downloader.http.proxy={}", rotate_session(&media)),
    ]
}

fn rotate_session(proxy: &str) -> String {
    let mut token = String::with_capacity(16);
    let mut buf = [0u8; 8];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut buf);
    for b in buf {
        token.push_str(&format!("{b:02x}"));
    }
    // replace the value after `session-` up to the next `_` or `:`
    if let Some(start) = proxy.find("session-") {
        let after = start + "session-".len();
        let end = proxy[after..]
            .find(['_', ':'])
            .map(|i| after + i)
            .unwrap_or(proxy.len());
        format!("{}{}{}", &proxy[..after], token, &proxy[end..])
    } else {
        proxy.to_string()
    }
}

fn env_secs(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(default)
}

// cookies live in postgres; gallery-dl wants a file, so stage one at mode 600 and shred it after
struct CookieFile {
    path: PathBuf,
}

impl CookieFile {
    fn write(dir: &Path, text: &str) -> Result<Self> {
        let path = dir.join(format!(".cookies-{}.txt", std::process::id()));
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(Self { path })
    }
}

impl Drop for CookieFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotate_session_swaps_only_the_token() {
        let p = "http://Morbo_pool-custom_type-high_session-8ab1019500a0feb3_sesstime-90:PASS@proxy.suborbit.al:1337";
        let out = rotate_session(p);
        assert!(out.starts_with("http://Morbo_pool-custom_type-high_session-"));
        assert!(out.ends_with("_sesstime-90:PASS@proxy.suborbit.al:1337"));
        assert!(!out.contains("8ab1019500a0feb3"));
        // two rotations differ (fresh random token each time)
        assert_ne!(rotate_session(p), rotate_session(p));
        // a url without the marker is returned untouched
        assert_eq!(rotate_session("http://u:p@host:1"), "http://u:p@host:1");
    }

    const SIDECAR: &str = r#"{
      "id": 161883250,
      "num": 2,
      "category": "patreon",
      "title": "Lana's Special Delivery ",
      "published_at": "2026-06-23T17:46:49.000+00:00",
      "is_paid": false,
      "min_cents_pledged_to_view": 500,
      "content": "<p>body</p>",
      "creator": {"id": 8360519, "full_name": "BONI", "vanity": "bonifasko"}
    }"#;

    #[test]
    fn maps_sidecar_fields() {
        let dir = std::env::temp_dir().join(format!("bakemono-meta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sidecar = dir.join("x.jpg.json");
        std::fs::write(&sidecar, SIDECAR).unwrap();
        let meta = post_meta(&sidecar).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(meta.platform, "patreon");
        assert_eq!(meta.creator, "BONI");
        assert_eq!(meta.creator_id, "8360519");
        assert_eq!(meta.post_id, "161883250");
        assert_eq!(meta.file_index, 1);
        assert_eq!(meta.tier, "subscriber");
        assert_eq!(meta.title, "Lana's Special Delivery");
    }

    const FANBOX_SIDECAR: &str = r#"{
      "id": "12036006",
      "num": 1,
      "category": "fanbox",
      "title": "Marche Lorraine w/Puppet",
      "publishedDatetime": "2026-06-30T07:45:02+09:00",
      "feeRequired": 500,
      "text": "body text",
      "creatorId": "anna-anon",
      "user": {"userId": "37736420", "name": "Anna Anon"}
    }"#;

    #[test]
    fn maps_fanbox_sidecar() {
        let dir = std::env::temp_dir().join(format!("bakemono-fbmeta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sidecar = dir.join("x.mp4.json");
        std::fs::write(&sidecar, FANBOX_SIDECAR).unwrap();
        let meta = post_meta(&sidecar).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(meta.platform, "fanbox");
        assert_eq!(meta.creator, "Anna Anon");
        assert_eq!(meta.creator_id, "anna-anon");
        assert_eq!(meta.post_id, "12036006");
        assert_eq!(meta.file_index, 0);
        assert_eq!(meta.tier, "subscriber");
        assert_eq!(meta.body, "body text");
        assert_eq!(meta.creator_url.as_deref(), Some("https://anna-anon.fanbox.cc"));
    }

    const PAWCHIVE_SIDECAR: &str = r#"{
      "id": "162854990",
      "num": 2,
      "category": "pawchive",
      "subcategory": "patreon",
      "service": "patreon",
      "user": "9919437",
      "username": "Afrobull",
      "title": "HD pack 100",
      "content": "<p>We made it to 100!</p>",
      "published": "2026-07-04T03:23:31",
      "has_full": true,
      "filename": "HD pack 97",
      "extension": "zip"
    }"#;

    #[test]
    fn maps_kemono_family_sidecar_to_origin_platform() {
        let dir = std::env::temp_dir().join(format!("bakemono-pwmeta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sidecar = dir.join("x.zip.json");
        std::fs::write(&sidecar, PAWCHIVE_SIDECAR).unwrap();
        let meta = post_meta(&sidecar).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(meta.platform, "patreon");
        assert_eq!(meta.creator, "Afrobull");
        assert_eq!(meta.creator_id, "9919437");
        assert_eq!(meta.post_id, "162854990");
        assert_eq!(meta.file_index, 1);
        assert_eq!(meta.posted_at.as_deref(), Some("2026-07-04T03:23:31"));
        assert_eq!(meta.creator_url.as_deref(), Some("https://www.patreon.com/user?u=9919437"));
        assert_eq!(meta.post_key(), "patreon:9919437:162854990");
    }

    #[test]
    fn keep_extension_allowlist() {
        // empty allowlist keeps everything (default for other operators)
        assert!(ext_allowed(&[], Path::new("x.zip")));
        let allow = parse_keep_exts("jpg, png  MP4");
        assert_eq!(allow, vec!["jpg", "png", "mp4"]);
        assert!(ext_allowed(&allow, Path::new("a/b.JPG")));   // case-insensitive
        assert!(ext_allowed(&allow, Path::new("clip.mp4")));
        assert!(!ext_allowed(&allow, Path::new("HD pack.zip")));
        assert!(!ext_allowed(&allow, Path::new("art.psd")));
        assert!(!ext_allowed(&allow, Path::new("noext")));    // no extension: filtered when a list is set
    }

    #[test]
    fn tier_of_classifies() {
        use serde_json::json;
        // patreon: 0 cents is public, null or a threshold is patron-only
        assert_eq!(tier_of(&json!({"min_cents_pledged_to_view": 0})), "free");
        assert_eq!(tier_of(&json!({"min_cents_pledged_to_view": null})), "subscriber");
        assert_eq!(tier_of(&json!({"min_cents_pledged_to_view": 500})), "subscriber");
        // fanbox
        assert_eq!(tier_of(&json!({"feeRequired": 0})), "free");
        assert_eq!(tier_of(&json!({"feeRequired": 500})), "subscriber");
        assert_eq!(tier_of(&json!({"other": 1})), "unknown");
    }

    #[test]
    fn hash_media_sniffs_and_hashes() {
        let dir = std::env::temp_dir().join(format!("bakemono-hash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let media = dir.join("x.jpg");
        std::fs::write(&media, [0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4]).unwrap();
        let (sha, size, mime) = hash_media(&media).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(size, 8);
        assert_eq!(mime, "image/jpeg");
        assert_eq!(sha.len(), 64);
    }
}
