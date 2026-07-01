use std::collections::HashMap;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context, Result};
use librqbit::api::TorrentIdOrHash;
use librqbit::limits::LimitsConfig;
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
    cache: Arc<Cache>,
    // cap on resolving a cold torrent's metadata, so a request with no reachable peers fails instead of hanging
    fetch_timeout: Duration,
}

impl Gateway {
    pub async fn new(
        cache_dir: PathBuf,
        listen_port: Option<u16>,
        initial_peers: Vec<SocketAddr>,
        budget_bytes: u64,
    ) -> Result<Self> {
        let cache = Arc::new(Cache::load(cache_dir.clone(), budget_bytes)?);
        let opts = SessionOptions {
            persistence: None,
            // reuse of the stored DHT port collides when several sessions run on one host; take a fresh port
            disable_dht_persistence: true,
            listen_port_range: listen_port.map(|p| p..p + 1),
            ..Default::default()
        };
        let session = Session::new_with_opts(cache_dir, opts)
            .await
            .context("starting torrent session")?;
        let fetch_timeout = Duration::from_secs(
            std::env::var("BAKEMONO_FETCH_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(20),
        );
        let drop_idle_interval = Duration::from_secs(
            std::env::var("BAKEMONO_DROP_IDLE_SECS")
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(60),
        );
        let gateway = Self {
            session,
            initial_peers,
            cache,
            fetch_timeout,
        };
        gateway.spawn_drop_idle(drop_idle_interval);
        Ok(gateway)
    }

    // a board that only ever leeches accumulates one librqbit torrent per post viewed; each keeps
    // announcing to DHT/trackers forever. sweep completed downloads out of the session on a timer (their
    // bytes stay on disk and serve from there), so the session only ever holds in-flight downloads
    fn spawn_drop_idle(&self, interval: Duration) {
        if interval.is_zero() {
            return;
        }
        let session = Arc::downgrade(&self.session);
        let cache = Arc::downgrade(&self.cache);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let (Some(session), Some(cache)) = (session.upgrade(), cache.upgrade()) else {
                    break; // gateway dropped, stop sweeping
                };
                cache.drop_idle(&session).await;
            }
        });
    }

    pub async fn meta(&self, magnet: &str) -> Result<TorrentMeta> {
        let infohash =
            infohash_from_magnet(magnet).ok_or_else(|| anyhow!("magnet carries no infohash"))?;
        // cache hit: describe the file from disk, no peer needed
        if let Some((path, size)) = self.cache.complete_file(&infohash) {
            let name = file_name(&path);
            self.cache.touch(&infohash, size, None);
            return Ok(TorrentMeta {
                files: vec![FileMeta {
                    index: 0,
                    mime: mime_for(&name),
                    path: name.clone(),
                    size,
                }],
                name,
                info_hash: infohash,
            });
        }
        let (handle, _) = self.add(magnet, vec![0]).await?;
        let meta = meta_from_handle(&handle)?;
        let size = meta.files.first().map(|f| f.size).unwrap_or(0);
        self.cache.touch(&infohash, size, Some(handle.id().into()));
        self.cache.evict(&self.session).await;
        Ok(meta)
    }

    pub async fn open(&self, magnet: &str, file_index: usize) -> Result<OpenFile> {
        let infohash =
            infohash_from_magnet(magnet).ok_or_else(|| anyhow!("magnet carries no infohash"))?;
        // cache hit: a fully-downloaded file serves straight from disk over HTTP, no torrent, no peer.
        // scoped to single-file torrents (index 0), which is what our manifests produce
        if file_index == 0 {
            if let Some((path, size)) = self.cache.complete_file(&infohash) {
                let file = tokio::fs::File::open(&path)
                    .await
                    .with_context(|| format!("opening cached {}", path.display()))?;
                let name = file_name(&path);
                self.cache.touch(&infohash, size, None);
                return Ok(OpenFile {
                    stream: Box::pin(file),
                    size,
                    mime: mime_for(&name),
                    name,
                });
            }
        }
        // miss: pull from the swarm (needs a live seeder), streaming pieces as they arrive
        let (handle, _) = self.add(magnet, vec![file_index]).await?;
        let (name, size) = file_entry(&handle, file_index)?;
        self.cache.touch(&infohash, size, Some(handle.id().into()));
        self.cache.evict(&self.session).await;
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

    // each torrent downloads into cache_dir/<infohash>/ so files never collide by name and eviction is
    // a single directory remove; returns the handle plus the infohash the cache keys on
    async fn add(&self, magnet: &str, only_files: Vec<usize>) -> Result<(Arc<ManagedTorrent>, String)> {
        let infohash =
            infohash_from_magnet(magnet).ok_or_else(|| anyhow!("magnet carries no infohash"))?;
        let dir = self.cache.dir.join(&infohash);
        let opts = AddTorrentOptions {
            only_files: Some(only_files),
            overwrite: true,
            output_folder: Some(dir.to_string_lossy().into_owned()),
            initial_peers: (!self.initial_peers.is_empty()).then(|| self.initial_peers.clone()),
            ..Default::default()
        };
        // both add_torrent and wait_until_initialized block on metadata from a peer for a cold torrent;
        // bound the whole thing so a request with no reachable seeder fails instead of hanging forever
        let added = tokio::time::timeout(self.fetch_timeout, async {
            let resp = self
                .session
                .add_torrent(AddTorrent::from_url(magnet), Some(opts))
                .await
                .context("adding torrent to session")?;
            let handle = resp
                .into_handle()
                .ok_or_else(|| anyhow!("torrent produced no handle"))?;
            handle
                .wait_until_initialized()
                .await
                .context("waiting for torrent metadata")?;
            Ok::<_, anyhow::Error>(handle)
        })
        .await;
        let handle = match added {
            Ok(Ok(handle)) => handle,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                // drop the stuck torrent so it does not linger in the session
                if let Ok(id) = TorrentIdOrHash::parse(&infohash) {
                    let _ = self.session.delete(id, false).await;
                }
                return Err(anyhow!(
                    "no seeders reachable for {infohash} (timed out after {}s)",
                    self.fetch_timeout.as_secs()
                ));
            }
        };
        // when the selected file finishes, drop a .done marker so later requests serve it from disk
        // with no peer (offline cache hit) and survive a board restart
        let h = handle.clone();
        tokio::spawn(async move {
            if h.wait_until_completed().await.is_ok() {
                let _ = std::fs::write(dir.join(".done"), b"");
            }
        });
        Ok((handle, infohash))
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// on-SSD file cache keyed by infohash, bounded by a size budget with LRU eviction ("download, display,
// delete"). the librqbit session writes each torrent under cache_dir/<infohash>/, so evicting is a
// session.delete plus a directory remove
struct Cache {
    dir: PathBuf,
    budget: u64, // bytes; 0 disables eviction (unlimited)
    state: StdMutex<CacheState>,
}

#[derive(Default)]
struct CacheState {
    total: u64,
    entries: HashMap<String, Entry>,
}

struct Entry {
    size: u64,
    last: SystemTime,
    // Some once the torrent is managed this run; None for dirs found on disk at startup
    id: Option<TorrentIdOrHash>,
}

// don't evict something touched within this window, so an in-flight stream is never yanked
const EVICT_GRACE: Duration = Duration::from_secs(120);

impl Cache {
    fn load(dir: PathBuf, budget: u64) -> Result<Self> {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating cache dir {}", dir.display()))?;
        let mut entries = HashMap::new();
        let mut total = 0;
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                if !e.path().is_dir() {
                    continue;
                }
                let infohash = e.file_name().to_string_lossy().into_owned();
                let size = dir_size(&e.path());
                let last = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                total += size;
                entries.insert(infohash, Entry { size, last, id: None });
            }
        }
        let cache = Cache {
            dir,
            budget,
            state: StdMutex::new(CacheState { total, entries }),
        };
        // trim leftovers from previous runs down to budget before serving (nothing is in-flight yet)
        cache.evict_cold();
        Ok(cache)
    }

    fn touch(&self, infohash: &str, size: u64, id: Option<TorrentIdOrHash>) {
        let mut st = self.state.lock().unwrap();
        match st.entries.get_mut(infohash) {
            Some(e) => {
                e.last = SystemTime::now();
                // keep a known session id; a cache hit (id None) must not clear it
                if id.is_some() {
                    e.id = id;
                }
            }
            None => {
                st.total += size;
                st.entries.insert(
                    infohash.to_string(),
                    Entry {
                        size,
                        last: SystemTime::now(),
                        id,
                    },
                );
            }
        }
    }

    // the cached file for an infohash, but only if its download finished (a .done marker is present),
    // so it serves from disk with no peer. single-file torrents, so returns the one non-marker file
    fn complete_file(&self, infohash: &str) -> Option<(PathBuf, u64)> {
        let dir = self.dir.join(infohash);
        if !dir.join(".done").is_file() {
            return None;
        }
        for e in std::fs::read_dir(&dir).ok()?.flatten() {
            let p = e.path();
            if p.is_file() && p.file_name().map(|n| n != ".done").unwrap_or(false) {
                let size = e.metadata().ok()?.len();
                return Some((p, size));
            }
        }
        None
    }

    async fn evict(&self, session: &Session) {
        if self.budget == 0 {
            return;
        }
        loop {
            let victim = {
                let mut st = self.state.lock().unwrap();
                if st.total <= self.budget {
                    break;
                }
                let now = SystemTime::now();
                let pick = st
                    .entries
                    .iter()
                    .filter(|(_, e)| now.duration_since(e.last).map(|d| d > EVICT_GRACE).unwrap_or(true))
                    .min_by_key(|(_, e)| e.last)
                    .map(|(ih, _)| ih.clone());
                match pick {
                    Some(ih) => {
                        let e = st.entries.remove(&ih).unwrap();
                        st.total -= e.size;
                        Some((ih, e.id))
                    }
                    None => None, // everything is within the grace window; try again later
                }
            };
            match victim {
                Some((infohash, id)) => {
                    if let Some(id) = id {
                        let _ = session.delete(id, true).await;
                    }
                    let _ = std::fs::remove_dir_all(self.dir.join(&infohash));
                }
                None => break,
            }
        }
    }

    // drop completed, idle torrents from the session while keeping their files, so the session tracks only
    // in-flight downloads; a dropped entry then serves purely from disk via complete_file
    async fn drop_idle(&self, session: &Session) {
        let victims = self.idle_complete_victims();
        let dropped = victims.len();
        for (infohash, id) in victims {
            let _ = session.delete(id, false).await; // false = keep the downloaded files
            if let Some(e) = self.state.lock().unwrap().entries.get_mut(&infohash) {
                e.id = None; // now pure disk cache; a later evict just removes the dir
            }
        }
        if dropped > 0 {
            tracing::info!(dropped, "released completed torrents from the session");
        }
    }

    // entries that finished downloading (a .done marker), are still managed in the session, and have not
    // been touched within the grace window, so no stream is mid-flight
    fn idle_complete_victims(&self) -> Vec<(String, TorrentIdOrHash)> {
        let now = SystemTime::now();
        let st = self.state.lock().unwrap();
        st.entries
            .iter()
            .filter_map(|(ih, e)| {
                let id = e.id?;
                let idle = now
                    .duration_since(e.last)
                    .map(|d| d > EVICT_GRACE)
                    .unwrap_or(true);
                let done = self.dir.join(ih).join(".done").is_file();
                (idle && done).then_some((ih.clone(), id))
            })
            .collect()
    }

    // startup-only: evict oldest dirs over budget without a session (nothing is managed yet)
    fn evict_cold(&self) {
        if self.budget == 0 {
            return;
        }
        let mut st = self.state.lock().unwrap();
        while st.total > self.budget {
            let Some(ih) = st
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last)
                .map(|(ih, _)| ih.clone())
            else {
                break;
            };
            let e = st.entries.remove(&ih).unwrap();
            st.total -= e.size;
            let _ = std::fs::remove_dir_all(self.dir.join(&ih));
        }
    }
}

fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let Ok(rd) = std::fs::read_dir(path) else {
        return 0;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            total += dir_size(&p);
        } else if let Ok(m) = e.metadata() {
            total += m.len();
        }
    }
    total
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

// holds a file in the swarm over classic BT: creates the torrent, announces to trackers, serves peers
pub struct Seeder {
    session: Arc<Session>,
    trackers: Vec<String>,
}

impl Seeder {
    pub async fn start(
        session_dir: PathBuf,
        trackers: Vec<String>,
        listen_port: Option<u16>,
        up_bps: Option<u32>,
        down_bps: Option<u32>,
    ) -> Result<Self> {
        std::fs::create_dir_all(&session_dir)
            .with_context(|| format!("creating session dir {}", session_dir.display()))?;
        let opts = SessionOptions {
            persistence: None,
            // reuse of the stored DHT port collides when several sessions run on one host; take a fresh port
            disable_dht_persistence: true,
            listen_port_range: listen_port.map(|p| p..p + 1),
            // session-wide rate caps in bytes/sec; None (or 0) is unlimited
            ratelimits: LimitsConfig {
                upload_bps: up_bps.and_then(NonZeroU32::new),
                download_bps: down_bps.and_then(NonZeroU32::new),
            },
            ..Default::default()
        };
        let session = Session::new_with_opts(session_dir, opts)
            .await
            .context("starting seed session")?;
        Ok(Self { session, trackers })
    }

    // seed the file in place; for a complete torrent librqbit only reads it, it never rewrites content
    pub async fn seed(&self, file: &Path) -> Result<SeedInfo> {
        let file = file
            .canonicalize()
            .with_context(|| format!("resolving {}", file.display()))?;
        let created = librqbit::create_torrent(
            &file,
            CreateTorrentOptions {
                name: None,
                piece_length: None,
            },
        )
        .await
        .with_context(|| format!("creating torrent for {}", file.display()))?;
        let info_hash = created.info_hash().as_string();
        let torrent = created.as_bytes().context("serializing torrent")?;
        let output_folder = file.parent().map(|p| p.to_string_lossy().into_owned());
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
    fn drop_idle_picks_only_completed_idle_managed_entries() {
        let base = std::env::temp_dir().join(format!("bmdropidle-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        // load on an empty dir, then wire up entries by hand so the selection logic is isolated
        let cache = Cache::load(base.clone(), 0).unwrap();
        let id = TorrentIdOrHash::parse(&"a".repeat(40)).unwrap();
        let stale = SystemTime::now() - EVICT_GRACE - Duration::from_secs(60);
        let done = |ih: &str| {
            let d = base.join(ih);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(".done"), b"").unwrap();
        };
        done("aaaa"); // done + managed + idle -> victim
        done("bbbb"); // done + managed but fresh -> keep (stream may be live)
        std::fs::create_dir_all(base.join("cccc")).unwrap(); // managed + idle but still downloading
        done("dddd"); // done + idle but already unmanaged (id None)
        {
            let mut st = cache.state.lock().unwrap();
            let e = |last, id| Entry { size: 0, last, id };
            st.entries.insert("aaaa".into(), e(stale, Some(id)));
            st.entries.insert("bbbb".into(), e(SystemTime::now(), Some(id)));
            st.entries.insert("cccc".into(), e(stale, Some(id)));
            st.entries.insert("dddd".into(), e(stale, None));
        }
        let victims: Vec<String> = cache
            .idle_complete_victims()
            .into_iter()
            .map(|(ih, _)| ih)
            .collect();
        assert_eq!(victims, vec!["aaaa".to_string()]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn cache_evicts_cold_dirs_over_budget() {
        let base = std::env::temp_dir().join(format!("bmcache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for ih in ["aaaa", "bbbb", "cccc"] {
            let d = base.join(ih);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("f.bin"), vec![0u8; 100]).unwrap();
        }
        // budget 150 with 3x100 on disk -> must trim down to a single dir
        let cache = Cache::load(base.clone(), 150).unwrap();
        {
            let st = cache.state.lock().unwrap();
            assert!(st.total <= 150, "total {} still over budget", st.total);
            assert_eq!(st.entries.len(), 1, "should keep one dir under budget");
        }
        let dirs = std::fs::read_dir(&base)
            .unwrap()
            .flatten()
            .filter(|e| e.path().is_dir())
            .count();
        assert_eq!(dirs, 1);
        let _ = std::fs::remove_dir_all(&base);
    }
}
