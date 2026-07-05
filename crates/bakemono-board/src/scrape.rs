use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPool;
use tokio_util::sync::CancellationToken;

use bakemono_scraper::{Cookies, ScrapeRequest, Scraper};

use crate::db;
use crate::kubo::Kubo;
use crate::thumb;

// the serve-mode worker: scrape every due source, one at a time, forever
pub async fn run_scheduler(pool: PgPool, kubo: Arc<Kubo>) {
    let interval = env_secs("BAKEMONO_SCRAPE_INTERVAL_SECS", 21_600);
    if interval == 0 {
        tracing::info!("scrape scheduler disabled");
        return;
    }
    loop {
        let due = match db::due_sources(&pool, interval as i64).await {
            Ok(due) => due,
            Err(e) => {
                tracing::error!("listing due sources: {e:#}");
                Vec::new()
            }
        };
        let mut ingested = 0;
        for source in due {
            tracing::info!(url = %source.url, "scrape starting");
            let result = scrape_source(&pool, &kubo, &source.url, source.cookies.as_deref(), None).await;
            let error = match &result {
                Ok(stats) => {
                    tracing::info!(url = %source.url, files = stats.files, posts = stats.posts, "scrape done");
                    ingested += stats.files;
                    None
                }
                Err(e) => {
                    tracing::error!(url = %source.url, "scrape failed: {e:#}");
                    Some(format!("{e:#}"))
                }
            };
            if let Err(e) = db::mark_scraped(&pool, &source.url, error.as_deref()).await {
                tracing::error!("recording scrape result: {e:#}");
            }
        }
        if ingested > 0 {
            if let Err(e) = crate::publish::publish_if_changed(&pool, &kubo).await {
                tracing::error!("manifest publish failed: {e:#}");
            }
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

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

    // only what this run downloaded; --download-archive keeps re-runs from re-printing old items
    let printed = std::mem::take(&mut *printed.lock().expect("printed paths"));
    ingest_files(pool, kubo, &printed).await
}

// import a directory of already-scraped media + gallery-dl sidecars (bootstrap dumps, old staging)
pub async fn ingest_dir(pool: &PgPool, kubo: &Kubo, dir: &Path) -> Result<IngestStats> {
    let mut files = Vec::new();
    walk(dir, &mut files)?;
    ingest_files(pool, kubo, &files).await
}

#[derive(Default)]
pub struct IngestStats {
    pub files: usize,
    pub posts: usize,
    pub skipped: usize,
}

async fn ingest_files(pool: &PgPool, kubo: &Kubo, files: &[PathBuf]) -> Result<IngestStats> {
    let present: HashSet<&PathBuf> = files.iter().collect();
    let mut stats = IngestStats::default();
    let mut posts_seen = HashSet::new();
    for media in files {
        if is_sidecar(media) || is_thumb(media) {
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
            }
            Err(e) => {
                tracing::warn!("skipping {}: {e:#}", media.display());
                stats.skipped += 1;
            }
        }
    }
    Ok(stats)
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
    // a thumb is archive content in its own right: it gets a catalog row so /f/{cid} serves it,
    // and later the pinset and shard entries carry it alongside the full file
    let thumb_cid = match thumb::generate(media, &mime).await {
        Some(bytes) => {
            let thumb_sha = hex::encode(Sha256::digest(&bytes));
            let thumb_size = bytes.len() as i64;
            let cid = kubo.add(bytes, "thumb").await?;
            db::insert_file(pool, &cid, &thumb_sha, thumb_size, "image/jpeg", None, None).await?;
            kubo.pin_archive(&cid, &format!("thumb {}", meta.post_key())).await?;
            Some(cid)
        }
        None => None,
    };
    let cid = kubo.add_path(media).await?;
    kubo.pin_archive(&cid, &meta.post_key()).await?;
    let filename = media.file_name().and_then(|n| n.to_str());
    db::insert_file(pool, &cid, &sha256, size as i64, &mime, filename, thumb_cid.as_deref()).await?;
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
}

impl PostMeta {
    pub fn post_key(&self) -> String {
        format!("{}:{}:{}", self.platform, self.creator_id, self.post_id)
    }
}

fn post_meta(sidecar: &Path) -> Result<PostMeta> {
    let raw = std::fs::read(sidecar).with_context(|| format!("reading {}", sidecar.display()))?;
    let meta: Value =
        serde_json::from_slice(&raw).with_context(|| format!("parsing {}", sidecar.display()))?;
    Ok(PostMeta {
        platform: string_at(&meta, &["category"]).unwrap_or_else(|| "patreon".to_string()),
        creator: string_at(&meta, &["creator", "full_name"])
            .or_else(|| string_at(&meta, &["creator", "vanity"]))
            .unwrap_or_else(|| "unknown".to_string()),
        creator_id: string_at(&meta, &["creator", "id"]).context("sidecar missing creator.id")?,
        post_id: string_at(&meta, &["id"]).context("sidecar missing id")?,
        file_index: meta
            .get("num")
            .and_then(Value::as_u64)
            .map(|n| n.saturating_sub(1) as i32)
            .unwrap_or(0),
        title: string_at(&meta, &["title"]).map(|t| t.trim().to_string()).unwrap_or_default(),
        body: string_at(&meta, &["content"]).unwrap_or_default(),
        posted_at: string_at(&meta, &["published_at"]).or_else(|| string_at(&meta, &["date"])),
        tier: tier_of(&meta),
    })
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
    match meta.get("is_paid").and_then(Value::as_bool) {
        Some(true) => "subscriber",
        Some(false) => "free",
        None => "unknown",
    }
    .to_string()
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

fn scraper() -> Scraper {
    match std::env::var_os("BAKEMONO_GALLERY_DL").filter(|s| !s.is_empty()) {
        Some(bin) => Scraper::with_binary(bin),
        None => Scraper::new(),
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

    const SIDECAR: &str = r#"{
      "id": 161883250,
      "num": 2,
      "category": "patreon",
      "title": "Lana's Special Delivery ",
      "published_at": "2026-06-23T17:46:49.000+00:00",
      "is_paid": true,
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
