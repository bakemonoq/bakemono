use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use nostr_relay_builder::MockRelay;
use nostr_sdk::prelude::*;
use tokio_util::sync::CancellationToken;

use bakemono_app::core::identity::Identity;
use bakemono_app::core::pipeline::{reseed, run_ingest, run_scrape, JobContext, Progress, RunSummary};
use bakemono_app::core::seeder::SeederHandle;
use bakemono_core::protocol::KIND_MANIFEST;
use bakemono_core::Manifest;
use bakemono_scraper::{Cookies, ScrapeRequest};

#[tokio::main]
async fn main() -> Result<()> {
    let opts = Opts::parse(std::env::args().skip(1).collect())?;

    // keep webtorrent off DHT/LSD so the local test swarm stays clean
    if std::env::var_os("BAKEMONO_ISOLATE").is_none() {
        std::env::set_var("BAKEMONO_ISOLATE", "1");
    }

    // reseed exercises the launch path (seed from disk + prune orphaned staging), no relay needed
    if let Mode::Reseed { dir } = &opts.mode {
        let seeder = SeederHandle::new();
        seeder
            .ensure_started(&opts.trackers, &[])
            .await
            .context("starting seeder")?;
        let count = reseed(&seeder, dir).await;
        seeder.shutdown().await;
        println!("reseeded {count} file(s) from {}", dir.display());
        return Ok(());
    }

    let relay = MockRelay::run().await.context("starting embedded relay")?;
    let url = relay.url().await.to_string();
    eprintln!("embedded relay at {url}");

    let identity = Identity::generate();
    eprintln!("identity {}", identity.npub()?);

    let relays = vec![url.clone()];
    let summary = run(&opts, &relays, &identity).await?;

    verify(&url, &summary).await?;
    println!(
        "\nPASS: {} event(s) published and verified on the relay",
        summary.event_ids.len()
    );
    Ok(())
}

async fn run(opts: &Opts, relays: &[String], identity: &Identity) -> Result<RunSummary> {
    let progress = |p: Progress| println!("  {}", render(&p));
    let cancel = CancellationToken::new();
    let seeder = if opts.seed {
        let handle = SeederHandle::new();
        // mirrors the GUI: --tracker stands in for config; a launch-time env var still wins
        handle
            .ensure_started(&opts.trackers, &[])
            .await
            .context("starting seeder")?;
        Some(handle)
    } else {
        None
    };

    let ctx = JobContext {
        relays,
        identity,
        seeder: seeder.as_ref(),
        cancel: &cancel,
        progress: &progress,
    };

    let result = match &opts.mode {
        Mode::Reseed { .. } => unreachable!("reseed is handled before run()"),
        Mode::Ingest { dir } => {
            eprintln!("ingesting {}", dir.display());
            run_ingest(dir, &ctx).await
        }
        Mode::Scrape {
            creator,
            limit,
            cookies,
            dest,
        } => {
            let dest = dest
                .clone()
                .unwrap_or_else(|| std::env::temp_dir().join("bakemono-scrapetest"));
            let mut request = ScrapeRequest::new(creator.clone(), &dest);
            request.cookies = cookies.clone().map(Cookies::File);
            eprintln!("scraping {creator} into {}", dest.display());
            run_scrape(request, limit.map(|n| n as usize), &ctx).await
        }
    };

    if let Some(seeder) = &seeder {
        seeder.shutdown().await;
    }
    result
}

async fn verify(url: &str, summary: &RunSummary) -> Result<()> {
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

    for id_hex in &summary.event_ids {
        let event = events
            .iter()
            .find(|e| e.id.to_hex() == *id_hex)
            .with_context(|| format!("event {id_hex} not found on relay"))?;
        if event.verify().is_err() {
            bail!("event {id_hex} failed signature verification");
        }
        Manifest::from_event(event)
            .with_context(|| format!("event {id_hex} did not parse back into a manifest"))?;
    }
    Ok(())
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

const USAGE: &str = "usage:\n  scrapetest [ingest [DIR]] [--tracker URL]... [--no-seed]\n  scrapetest scrape <creator> [--limit N] [--cookies FILE] [--dest DIR] [--tracker URL]... [--no-seed]";
