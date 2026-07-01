use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use librqbit::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, ManagedTorrent, Session, SessionOptions,
};
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncSeek};

// librqbit's FileStream is public but not re-exported at a nameable path, so box it behind a trait
// the HTTP layer can range over
pub trait SeekableRead: AsyncRead + AsyncSeek + Send {}
impl<T: AsyncRead + AsyncSeek + Send> SeekableRead for T {}

#[derive(Debug, Clone, Serialize)]
pub struct FileMeta {
    pub index: usize,
    pub path: String,
    pub size: u64,
    pub mime: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TorrentMeta {
    pub name: String,
    pub info_hash: String,
    pub files: Vec<FileMeta>,
}

pub struct OpenFile {
    pub stream: Pin<Box<dyn SeekableRead>>,
    pub size: u64,
    pub mime: String,
    pub name: String,
}

// leech-only swarm endpoint: one librqbit session pulls bytes over classic BT (TCP/uTP + DHT + trackers)
pub struct Gateway {
    session: Arc<Session>,
    // operator-pinned seeders tried directly, so a known seedbox works without waiting on tracker/DHT
    initial_peers: Vec<SocketAddr>,
}

impl Gateway {
    pub async fn new(
        output_dir: PathBuf,
        listen_port: Option<u16>,
        initial_peers: Vec<SocketAddr>,
    ) -> Result<Self> {
        let opts = SessionOptions {
            persistence: None,
            // reuse of the stored DHT port collides when several sessions run on one host; take a fresh port
            disable_dht_persistence: true,
            listen_port_range: listen_port.map(|p| p..p + 1),
            ..Default::default()
        };
        let session = Session::new_with_opts(output_dir, opts)
            .await
            .context("starting torrent session")?;
        Ok(Self {
            session,
            initial_peers,
        })
    }

    pub async fn meta(&self, magnet: &str) -> Result<TorrentMeta> {
        let handle = self.add(magnet, vec![0]).await?;
        meta_from_handle(&handle)
    }

    pub async fn open(&self, magnet: &str, file_index: usize) -> Result<OpenFile> {
        let handle = self.add(magnet, vec![file_index]).await?;
        let (name, size) = file_entry(&handle, file_index)?;
        let stream = handle
            .clone()
            .stream(file_index)
            .context("opening file stream")?;
        Ok(OpenFile {
            stream: Box::pin(stream),
            size,
            mime: mime_for(&name),
            name,
        })
    }

    async fn add(&self, magnet: &str, only_files: Vec<usize>) -> Result<Arc<ManagedTorrent>> {
        let initial_peers =
            (!self.initial_peers.is_empty()).then(|| self.initial_peers.clone());
        let resp = self
            .session
            .add_torrent(
                AddTorrent::from_url(magnet),
                Some(AddTorrentOptions {
                    only_files: Some(only_files),
                    overwrite: true,
                    initial_peers,
                    ..Default::default()
                }),
            )
            .await
            .context("adding torrent to session")?;
        let handle = resp
            .into_handle()
            .ok_or_else(|| anyhow!("torrent produced no handle"))?;
        handle
            .wait_until_initialized()
            .await
            .context("waiting for torrent metadata")?;
        Ok(handle)
    }
}

fn meta_from_handle(handle: &Arc<ManagedTorrent>) -> Result<TorrentMeta> {
    let info_hash = handle.info_hash().as_string();
    let name = handle.name().unwrap_or_default();
    let files = handle.with_metadata(|m| {
        m.file_infos
            .iter()
            .enumerate()
            .map(|(i, fi)| {
                let path = fi.relative_filename.to_string_lossy().into_owned();
                FileMeta {
                    index: i,
                    mime: mime_for(&path),
                    path,
                    size: fi.len,
                }
            })
            .collect::<Vec<_>>()
    })?;
    Ok(TorrentMeta {
        name,
        info_hash,
        files,
    })
}

fn file_entry(handle: &Arc<ManagedTorrent>, idx: usize) -> Result<(String, u64)> {
    handle
        .with_metadata(|m| {
            m.file_infos
                .get(idx)
                .map(|fi| (fi.relative_filename.to_string_lossy().into_owned(), fi.len))
        })?
        .ok_or_else(|| anyhow!("file index {idx} out of range"))
}

#[derive(Debug, Clone)]
pub struct SeedInfo {
    pub magnet: String,
    pub info_hash: String,
}

// holds a file in the swarm over classic BT: creates the torrent, announces to trackers, serves peers.
// staging survives across runs (a file already linked is skipped), so relaunch cost is O(new files)
pub struct Seeder {
    session: Arc<Session>,
    staging: PathBuf,
    trackers: Vec<String>,
}

impl Seeder {
    pub async fn start(
        staging_root: PathBuf,
        trackers: Vec<String>,
        listen_port: Option<u16>,
    ) -> Result<Self> {
        std::fs::create_dir_all(&staging_root)
            .with_context(|| format!("creating staging dir {}", staging_root.display()))?;
        let opts = SessionOptions {
            persistence: None,
            // reuse of the stored DHT port collides when several sessions run on one host; take a fresh port
            disable_dht_persistence: true,
            listen_port_range: listen_port.map(|p| p..p + 1),
            ..Default::default()
        };
        let session = Session::new_with_opts(staging_root.clone(), opts)
            .await
            .context("starting seed session")?;
        Ok(Self {
            session,
            staging: staging_root,
            trackers,
        })
    }

    // webtorrent mis-hashed pieces from odd source paths, so keep seeding a sanitized hardlink
    pub async fn seed(&self, file: &Path) -> Result<SeedInfo> {
        let staged = self.stage(file)?;
        let created = librqbit::create_torrent(
            &staged,
            CreateTorrentOptions {
                name: None,
                piece_length: None,
            },
        )
        .await
        .with_context(|| format!("creating torrent for {}", staged.display()))?;
        let info_hash = created.info_hash().as_string();
        let torrent = created.as_bytes().context("serializing torrent")?;
        let output_folder = staged
            .parent()
            .map(|p| p.to_string_lossy().into_owned());
        self.session
            .add_torrent(
                AddTorrent::from_bytes(torrent),
                Some(AddTorrentOptions {
                    output_folder,
                    overwrite: true,
                    trackers: Some(self.trackers.clone()),
                    ..Default::default()
                }),
            )
            .await
            .context("adding torrent to seed session")?;
        Ok(SeedInfo {
            magnet: synth_magnet(&info_hash, &self.trackers),
            info_hash,
        })
    }

    // drop staged links whose source is gone, so deleted files stop pinning disk
    pub fn retain_staging(&self, live_sources: &[PathBuf]) {
        let keep: std::collections::HashSet<String> = live_sources
            .iter()
            .filter_map(|p| p.canonicalize().ok())
            .map(|p| staging_key(&p))
            .collect();
        let Ok(entries) = std::fs::read_dir(&self.staging) else {
            return;
        };
        for entry in entries.flatten() {
            if !keep.contains(entry.file_name().to_string_lossy().as_ref()) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    fn stage(&self, file: &Path) -> Result<PathBuf> {
        let src = file
            .canonicalize()
            .with_context(|| format!("resolving {}", file.display()))?;
        let dir = self.staging.join(staging_key(&src));
        std::fs::create_dir_all(&dir)?;
        let staged = dir.join(safe_filename(&src));
        if !staged.exists() && std::fs::hard_link(&src, &staged).is_err() {
            std::fs::copy(&src, &staged).with_context(|| format!("staging {}", src.display()))?;
        }
        Ok(staged)
    }
}

// pull the v1 btih out of magnet:?xt=urn:btih:<40 hex>&...; lowercased so it matches our stored hashes
pub fn infohash_from_magnet(magnet: &str) -> Option<String> {
    let after = magnet.split("xt=urn:btih:").nth(1)?;
    let raw = after.split('&').next()?;
    let hash = raw.trim().to_ascii_lowercase();
    (hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hash)
}

// build a minimal magnet from a bare infohash plus trackers, for fetching content the catalog points at
pub fn synth_magnet(infohash: &str, trackers: &[String]) -> String {
    let mut magnet = format!("magnet:?xt=urn:btih:{}", infohash.to_ascii_lowercase());
    for tr in trackers {
        magnet.push_str("&tr=");
        magnet.push_str(&urlencode(tr));
    }
    magnet
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn staging_key(canonical_src: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    canonical_src.to_string_lossy().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn safe_filename(src: &Path) -> String {
    let raw = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "file".to_string()
    } else {
        cleaned
    }
}

fn mime_for(path: &str) -> String {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        _ => "application/octet-stream",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_btih_from_magnet() {
        let m = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=x&tr=udp://t";
        assert_eq!(
            infohash_from_magnet(m).as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        assert_eq!(infohash_from_magnet("magnet:?xt=urn:btih:short"), None);
    }

    #[test]
    fn synthesizes_magnet_with_trackers() {
        let m = synth_magnet(
            "0123456789ABCDEF0123456789abcdef01234567",
            &["udp://tracker.example:1337/announce".to_string()],
        );
        assert!(m.starts_with("magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567"));
        assert!(m.contains("&tr=udp%3A%2F%2Ftracker.example%3A1337%2Fannounce"));
    }

    #[test]
    fn sanitizes_odd_filenames() {
        assert_eq!(safe_filename(Path::new("/x/a b#1.jpg")), "a_b_1.jpg");
        assert_eq!(safe_filename(Path::new("/x/")), "x");
    }
}
