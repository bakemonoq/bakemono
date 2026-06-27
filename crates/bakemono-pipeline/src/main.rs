use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use nostr_sdk::prelude::*;

use bakemono_pipeline::{gather_pairs, manifest_from_files, publish_manifests};
use bakemono_scraper::{Cookies, ScrapeRequest, Scraper};
use bakemono_seeder::Seeder;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().context(USAGE)?;
    let rest: Vec<String> = args.collect();
    match mode.as_str() {
        "scrape" => scrape_and_publish(rest).await,
        "ingest" => ingest_and_publish(rest).await,
        "-h" | "--help" => {
            eprintln!("{USAGE}");
            Ok(())
        }
        other => bail!("unknown mode `{other}`\n{USAGE}"),
    }
}

async fn scrape_and_publish(args: Vec<String>) -> Result<()> {
    let mut creator = None;
    let mut dest = None;
    let mut relays = Vec::new();
    let mut limit = None;
    let mut cookies = None;
    let mut seed = true;

    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--relay" => relays.push(value(&mut it, "--relay")?),
            "--limit" => {
                limit = Some(
                    value(&mut it, "--limit")?
                        .parse()
                        .context("--limit number")?,
                )
            }
            "--cookies" => cookies = Some(Cookies::File(value(&mut it, "--cookies")?.into())),
            "--browser" => cookies = Some(Cookies::Browser(value(&mut it, "--browser")?)),
            "--no-seed" => seed = false,
            flag if flag.starts_with('-') => bail!("unknown flag {flag}"),
            positional if creator.is_none() => creator = Some(positional.to_string()),
            positional if dest.is_none() => dest = Some(PathBuf::from(positional)),
            other => bail!("unexpected argument {other}"),
        }
    }

    let creator = creator.context(SCRAPE_USAGE)?;
    let dest = dest.context(SCRAPE_USAGE)?;
    let keys = load_keys()?;
    let scraper = match std::env::var_os("BAKEMONO_GALLERY_DL") {
        Some(path) => Scraper::with_binary(path),
        None => Scraper::new(),
    };
    let mut request = ScrapeRequest::new(creator, &dest);
    request.cookies = cookies.or_else(cookies_from_env);
    request.limit = limit;

    eprintln!(
        "using {}",
        scraper.version().context("gallery-dl not found")?
    );
    let outcome = scraper.scrape(&request)?;
    eprintln!(
        "scraped {} files into {}",
        outcome.files.len(),
        outcome.dest.display()
    );

    build_and_publish(gather_pairs(&outcome.dest)?, &keys, &or_local(relays), seed).await
}

async fn ingest_and_publish(args: Vec<String>) -> Result<()> {
    let mut dir = None;
    let mut relays = Vec::new();
    let mut seed = true;

    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--relay" => relays.push(value(&mut it, "--relay")?),
            "--no-seed" => seed = false,
            flag if flag.starts_with('-') => bail!("unknown flag {flag}"),
            positional if dir.is_none() => dir = Some(PathBuf::from(positional)),
            other => bail!("unexpected argument {other}"),
        }
    }

    let dir = dir.context(INGEST_USAGE)?;
    let keys = load_keys()?;
    let pairs = gather_pairs(&dir)?;
    eprintln!("found {} media files under {}", pairs.len(), dir.display());

    build_and_publish(pairs, &keys, &or_local(relays), seed).await
}

async fn build_and_publish(
    pairs: Vec<(PathBuf, PathBuf)>,
    keys: &Keys,
    relays: &[String],
    seed: bool,
) -> Result<()> {
    if pairs.is_empty() {
        bail!("no media+sidecar pairs found");
    }
    let mut seeder = if seed {
        Some(
            Seeder::from_env()
                .await
                .context("starting webtorrent sidecar")?,
        )
    } else {
        None
    };

    let mut manifests = Vec::new();
    for (media, sidecar) in &pairs {
        let mut manifest = match manifest_from_files(media, sidecar) {
            Ok(manifest) => manifest,
            Err(e) => {
                eprintln!("  skip {}: {e:#}", media.display());
                continue;
            }
        };
        if let Some(seeder) = seeder.as_mut() {
            manifest.magnet = seeder.seed(media).await.context("seeding file")?.magnet;
        }
        println!(
            "  {} {} {}:{} #{} ({} bytes)\n    {}",
            manifest.file_hash,
            manifest.creator,
            manifest.platform,
            manifest.post_id,
            manifest.file_index,
            manifest.size,
            manifest.magnet
        );
        manifests.push(manifest);
    }
    if manifests.is_empty() {
        bail!("no manifests built");
    }

    let ids = publish_manifests(relays, keys, &manifests).await?;
    println!("published {} events to {}", ids.len(), relays.join(", "));

    if let Some(seeder) = seeder {
        println!(
            "seeding {} files over BT v1 + WebRTC, ctrl-c to stop",
            manifests.len()
        );
        tokio::signal::ctrl_c().await?;
        seeder.shutdown().await?;
    }
    Ok(())
}

// stable identity across restarts: env override, else a persisted key file, else generate and save
fn load_keys() -> Result<Keys> {
    if let Ok(nsec) = std::env::var("BAKEMONO_NSEC") {
        return Ok(Keys::parse(&nsec)?);
    }
    let path = key_file_path();
    if path.exists() {
        let nsec = std::fs::read_to_string(&path)?;
        return Ok(Keys::parse(nsec.trim())?);
    }
    let keys = Keys::generate();
    save_key(&path, &keys.secret_key().to_bech32()?)?;
    eprintln!(
        "generated identity {}, saved to {}",
        keys.public_key().to_bech32()?,
        path.display()
    );
    Ok(keys)
}

fn key_file_path() -> PathBuf {
    if let Ok(p) = std::env::var("BAKEMONO_KEY_FILE") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config/bakemono/identity.nsec")
}

fn save_key(path: &Path, nsec: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, nsec)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn cookies_from_env() -> Option<Cookies> {
    if let Some(path) = std::env::var_os("BAKEMONO_COOKIES") {
        Some(Cookies::File(PathBuf::from(path)))
    } else {
        std::env::var("BAKEMONO_COOKIES_BROWSER")
            .ok()
            .map(Cookies::Browser)
    }
}

fn or_local(relays: Vec<String>) -> Vec<String> {
    if relays.is_empty() {
        vec!["ws://127.0.0.1:8080".to_string()]
    } else {
        relays
    }
}

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    args.next()
        .with_context(|| format!("{flag} expects a value"))
}

const SCRAPE_USAGE: &str =
    "usage: bakemono-pipeline scrape <creator> <dest> [--relay URL] [--limit N] [--cookies FILE | --browser NAME]";
const INGEST_USAGE: &str = "usage: bakemono-pipeline ingest <dir> [--relay URL]";
const USAGE: &str = "usage: bakemono-pipeline <scrape|ingest> ...";
