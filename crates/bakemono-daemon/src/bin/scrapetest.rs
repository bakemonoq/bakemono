use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use nostr_relay_builder::MockRelay;
use nostr_sdk::prelude::*;
use serde_json::{json, Value};

use bakemono_engine::identity::Identity;
use bakemono_daemon::pipeline::{Progress, RunSummary};
use bakemono_daemon::source::AppContentSource;
use bakemono_core::protocol::KIND_MANIFEST;
use bakemono_core::Manifest;
use bakemono_engine::config::AppConfig;
use bakemono_engine::daemon::Daemon;
use bakemono_engine::ipc;

#[tokio::main]
async fn main() -> Result<()> {
    let opts = Opts::parse(std::env::args().skip(1).collect())?;

    let mut config = AppConfig::default();
    config.seed = opts.seed;
    if !opts.trackers.is_empty() {
        config.trackers = opts.trackers.clone();
    }

    // reseed exercises the launch path (seed from disk + prune orphaned staging), no relay needed
    if let Mode::Reseed { dir } = &opts.mode {
        let source = AppContentSource {
            relays: Vec::new(),
            identity: Identity::generate(),
        };
        let daemon = Daemon::new(config, dir.clone(), source);
        let count = daemon.reseed().await?;
        daemon.shutdown().await;
        println!("reseeded {count} file(s) from {}", dir.display());
        return Ok(());
    }

    // ipc drives a real daemon over the socket, end to end, in one process
    if let Mode::Ipc { dir } = &opts.mode {
        return run_ipc_test(dir.clone(), config).await;
    }

    // status connects to an already-running daemon process and prints its status
    if let Mode::DaemonStatus = &opts.mode {
        let result = ipc::call(json!({"cmd": "status"}), |_| {}).await?;
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }

    let relay = MockRelay::run().await.context("starting embedded relay")?;
    let url = relay.url().await.to_string();
    eprintln!("embedded relay at {url}");

    let identity = Identity::generate();
    eprintln!("identity {}", identity.npub()?);

    let source = AppContentSource {
        relays: vec![url.clone()],
        identity,
    };
    let (content_dir, job) = job_from_mode(&opts.mode);
    let daemon = Daemon::new(config, content_dir.clone(), source);

    let progress = |event: Value| {
        if let Ok(p) = serde_json::from_value::<Progress>(event) {
            println!("  {}", render(&p));
        }
    };
    let result = daemon.run_job(job, &progress).await;
    daemon.shutdown().await;
    let summary: RunSummary = serde_json::from_value(result?)?;

    verify(&url, &summary, &content_dir, opts.seed).await?;
    println!(
        "\nPASS: {} event(s) published and verified on the relay",
        summary.event_ids.len()
    );
    Ok(())
}

async fn run_ipc_test(dir: PathBuf, config: AppConfig) -> Result<()> {
    // isolate the socket + staging + config under a throwaway data dir
    let tmp = std::env::temp_dir().join(format!("bakemono-ipc-{}", std::process::id()));
    std::env::set_var("BAKEMONO_DATA_DIR", &tmp);

    let relay = MockRelay::run().await.context("starting embedded relay")?;
    let url = relay.url().await.to_string();
    eprintln!("embedded relay at {url}");

    let source = AppContentSource {
        relays: vec![url.clone()],
        identity: Identity::generate(),
    };
    let seed = config.seed;
    let daemon = Arc::new(Daemon::new(config, dir.clone(), source));
    let server = {
        let daemon = daemon.clone();
        tokio::spawn(async move {
            let _ = ipc::serve(daemon).await;
        })
    };

    // wait for the daemon to bind its socket
    let sock = ipc::socket_path();
    for _ in 0..100 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    eprintln!("daemon socket at {}", sock.display());

    let progress = |event: Value| {
        if let Ok(p) = serde_json::from_value::<Progress>(event) {
            println!("  {}", render(&p));
        }
    };
    let job = json!({"cmd": "run", "job": {"kind": "ingest", "dir": dir.to_string_lossy()}});
    let result = ipc::call(job, progress).await?;
    let summary: RunSummary = serde_json::from_value(result)?;

    verify(&url, &summary, &dir, seed).await?;
    let _ = ipc::call(json!({"cmd": "shutdown"}), |_| {}).await;
    let _ = server.await;
    let _ = std::fs::remove_dir_all(&tmp);
    println!(
        "\nPASS: {} event(s) published and verified over IPC",
        summary.event_ids.len()
    );
    Ok(())
}

fn job_from_mode(mode: &Mode) -> (PathBuf, Value) {
    match mode {
        Mode::Ipc { .. } => unreachable!("ipc is handled before this"),
        Mode::DaemonStatus => unreachable!("status is handled before this"),
        Mode::Reseed { .. } => unreachable!("reseed is handled before this"),
        Mode::Ingest { dir } => (
            dir.clone(),
            json!({"kind": "ingest", "dir": dir.to_string_lossy()}),
        ),
        Mode::Scrape {
            creator,
            limit,
            cookies,
            dest,
        } => {
            let dest = dest
                .clone()
                .unwrap_or_else(|| std::env::temp_dir().join("bakemono-scrapetest"));
            let job = json!({
                "kind": "scrape",
                "creator": creator,
                "limit": limit,
                "cookies": cookies.as_ref().map(|p| p.to_string_lossy().into_owned()),
                "dest": dest.to_string_lossy(),
            });
            (dest, job)
        }
    }
}

async fn verify(url: &str, summary: &RunSummary, content_dir: &Path, seed: bool) -> Result<()> {
    let client = Client::new(Keys::generate());
    client.add_relay(url).await?;
    client.connect().await;
    let events = client
        .fetch_events(
            Filter::new().kind(Kind::from(KIND_MANIFEST)),
            Duration::from_secs(10),
        )
        .await?;
    client.disconnect().await;

    let mut manifests = Vec::new();
    for id_hex in &summary.event_ids {
        let event = events
            .iter()
            .find(|e| e.id.to_hex() == *id_hex)
            .with_context(|| format!("event {id_hex} not found on relay"))?;
        if event.verify().is_err() {
            bail!("event {id_hex} failed signature verification");
        }
        let manifest = Manifest::from_event(event)
            .with_context(|| format!("event {id_hex} did not parse back into a manifest"))?;
        manifests.push(manifest);
    }
    verify_thumbnails(content_dir, &manifests, seed).await
}

// end to end preview check: with seeding + ffmpeg on, every image/video manifest must reference a
// seeded thumbnail whose on-disk bytes hash to the thumb_x the signed event carries
async fn verify_thumbnails(content_dir: &Path, manifests: &[Manifest], seed: bool) -> Result<()> {
    if !seed {
        println!("thumbnails: skipped (--no-seed)");
        return Ok(());
    }
    if !ffmpeg_available().await {
        println!("thumbnails: skipped (ffmpeg not found; set BAKEMONO_FFMPEG to require previews)");
        return Ok(());
    }
    let mut verified = 0;
    for m in manifests {
        if !(m.mime.starts_with("image/") || m.mime.starts_with("video/")) {
            continue;
        }
        let thumb_x = m.thumb_x.as_deref().with_context(|| {
            format!("{}: no thumb_x, but ffmpeg is present and mime is {}", m.d_tag(), m.mime)
        })?;
        let magnet = m
            .thumb_magnet
            .as_deref()
            .with_context(|| format!("{}: thumb_x set but thumb_magnet missing", m.d_tag()))?;
        if !magnet.starts_with("magnet:?") {
            bail!("{}: thumb_magnet is not a magnet uri: {magnet}", m.d_tag());
        }
        let filename = m
            .filename
            .as_deref()
            .with_context(|| format!("{}: manifest has no filename to locate its thumbnail", m.d_tag()))?;
        let thumb = find_thumb(content_dir, filename).with_context(|| {
            format!("thumbnail file {filename}.thumb.jpg not found under {}", content_dir.display())
        })?;
        let got = hash_hex(&std::fs::read(&thumb)?);
        if got != thumb_x {
            bail!("{filename}: thumbnail file hash {got} != signed thumb_x {thumb_x}");
        }
        verified += 1;
    }
    if verified == 0 {
        println!("thumbnails: no image/video manifests in this run");
    } else {
        println!("thumbnails: verified {verified} seeded preview(s); on-disk bytes match signed thumb_x");
    }
    Ok(())
}

async fn ffmpeg_available() -> bool {
    let bin = std::env::var_os("BAKEMONO_FFMPEG").unwrap_or_else(|| "ffmpeg".into());
    tokio::process::Command::new(bin)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

fn find_thumb(dir: &Path, media_filename: &str) -> Option<PathBuf> {
    let target = format!("{media_filename}.thumb.jpg");
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if entry.file_name().to_string_lossy() == target {
                return Some(path);
            }
        }
    }
    None
}

fn hash_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

fn render(p: &Progress) -> String {
    match p {
        Progress::Scraping { creator, dest } => format!("scraping {creator} -> {dest}"),
        Progress::ScrapePost { posts, file } => format!("post #{posts}: {file}"),
        Progress::Scraped { files, posts } => {
            format!("scraped {files} file(s) across {posts} post(s)")
        }
        Progress::Pairs { count } => format!("{count} media+sidecar pair(s)"),
        Progress::SeederReady => "seeder ready".to_string(),
        Progress::Manifest {
            index,
            total,
            file,
            hash,
            size,
        } => format!("[{index}/{total}] {file} {} ({size} bytes)", &hash[..16]),
        Progress::Seeded { file, magnet } => format!("seeded {file} -> {magnet}"),
        Progress::Thumbnailed { file, magnet } => format!("thumb {file} -> {magnet}"),
        Progress::Skipped { file, reason } => format!("skip {file}: {reason}"),
        Progress::Publishing { relays, count } => {
            format!("publishing {count} event(s) to {}", relays.join(", "))
        }
        Progress::Published { event_ids } => format!("published {} event(s)", event_ids.len()),
        Progress::Cancelled => "cancelled".to_string(),
        Progress::Done { manifests } => format!("done, {manifests} manifest(s)"),
        Progress::Failed { error } => format!("failed: {error}"),
    }
}

struct Opts {
    mode: Mode,
    seed: bool,
    trackers: Vec<String>,
}

enum Mode {
    Ingest {
        dir: PathBuf,
    },
    Reseed {
        dir: PathBuf,
    },
    Ipc {
        dir: PathBuf,
    },
    DaemonStatus,
    Scrape {
        creator: String,
        limit: Option<u32>,
        cookies: Option<PathBuf>,
        dest: Option<PathBuf>,
    },
}

impl Opts {
    fn parse(args: Vec<String>) -> Result<Self> {
        let mut seed = true;
        let mut rest = Vec::new();
        let mut it = args.into_iter();
        let mut mode_word = None;
        let mut limit = None;
        let mut cookies = None;
        let mut dest = None;
        let mut trackers = Vec::new();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--no-seed" => seed = false,
                "--limit" => {
                    limit = Some(
                        it.next()
                            .context("--limit expects a value")?
                            .parse()
                            .context("--limit number")?,
                    )
                }
                "--cookies" => {
                    cookies = Some(PathBuf::from(it.next().context("--cookies expects a path")?))
                }
                "--dest" => dest = Some(PathBuf::from(it.next().context("--dest expects a path")?)),
                "--tracker" => trackers.push(it.next().context("--tracker expects a url")?),
                "-h" | "--help" => {
                    eprintln!("{USAGE}");
                    std::process::exit(0);
                }
                flag if flag.starts_with('-') => bail!("unknown flag {flag}\n{USAGE}"),
                _ if mode_word.is_none() => mode_word = Some(arg),
                _ => rest.push(arg),
            }
        }

        let mode = match mode_word.as_deref() {
            None | Some("ingest") => Mode::Ingest {
                dir: rest.first().map(PathBuf::from).unwrap_or_else(default_dir),
            },
            Some("reseed") => Mode::Reseed {
                dir: rest.first().map(PathBuf::from).unwrap_or_else(default_dir),
            },
            Some("ipc") => Mode::Ipc {
                dir: rest.first().map(PathBuf::from).unwrap_or_else(default_dir),
            },
            Some("status") => Mode::DaemonStatus,
            Some("scrape") => Mode::Scrape {
                creator: rest.first().cloned().context("scrape needs a creator")?,
                limit,
                cookies,
                dest,
            },
            Some(other) => bail!("unknown mode `{other}`\n{USAGE}"),
        };
        Ok(Self {
            mode,
            seed,
            trackers,
        })
    }
}

fn default_dir() -> PathBuf {
    PathBuf::from("out")
}

const USAGE: &str = "usage:\n  scrapetest [ingest [DIR]] [--tracker URL]... [--no-seed]\n  scrapetest reseed [DIR]\n  scrapetest ipc [DIR]\n  scrapetest scrape <creator> [--limit N] [--cookies FILE] [--dest DIR] [--tracker URL]... [--no-seed]";
