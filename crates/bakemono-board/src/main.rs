mod config;
mod db;
mod health;
mod indexer;
mod instance;
mod kubo;
mod publish;
mod ratelimit;
mod restore;
mod sanitize;
mod scrape;
mod thumb;
mod trusted_proxy;
mod verifier;
mod web;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{bail, Result};

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        // no-arg default stays `serve` so existing docker entrypoints keep working
        None | Some("serve") => serve().await,
        Some("add") => cmd_add(args.collect()).await,
        Some("ingest") => cmd_ingest(args.next()).await,
        Some("scrape") => cmd_scrape(args.collect()).await,
        Some("source") => cmd_source(args.collect()).await,
        Some("restore") => cmd_restore(args.next()).await,
        Some(other) => {
            bail!("unknown command `{other}` (expected serve, add, ingest, scrape, source or restore)")
        }
    }
}

async fn serve() -> Result<()> {
    let database_url = database_url();
    let relays = relays();
    let bind = env_or("BAKEMONO_BIND", "127.0.0.1:3000");
    let signer = instance::load();
    let trusted = instance::trusted(signer.as_ref());

    let pool = db::connect(&database_url).await?;
    let gateway = Arc::new(
        bakemono_torrent::Gateway::new(
            gateway_dir(),
            gateway_port(),
            gateway_peers(),
            gateway_budget(),
        )
        .await?,
    );

    let indexer_pool = pool.clone();
    let indexer_relays = relays.clone();
    tokio::spawn(async move {
        if let Err(e) = indexer::run(indexer_pool, indexer_relays, trusted).await {
            tracing::error!("indexer stopped: {e:#}");
        }
    });

    let health_pool = pool.clone();
    tokio::spawn(health::run(health_pool, bakemono_core::default_trackers()));

    tokio::spawn(verifier::run(pool.clone(), gateway.clone()));

    let kubo = Arc::new(kubo::Kubo::from_env());
    tokio::spawn(scrape::run_scheduler(pool.clone(), kubo.clone()));

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("board on http://{bind}");
    let state = web::AppState {
        pool,
        relays,
        signer,
        gateway,
        kubo,
        cold_limiter: Arc::new(ratelimit::ColdLimiter::from_env()),
    };
    axum::serve(
        listener,
        web::router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

// hand-feed one file into the new-stack catalog; superseded by `ingest` once the scrape worker lands
async fn cmd_add(paths: Vec<String>) -> Result<()> {
    if paths.is_empty() {
        bail!("usage: bakemono-board add <file>...");
    }
    let pool = db::connect(&database_url()).await?;
    let kubo = kubo::Kubo::from_env();
    for path in paths {
        let bytes = tokio::fs::read(&path).await?;
        let sha256 = {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(&bytes))
        };
        let size = bytes.len() as i64;
        let mime = scrape::sniff_mime(&bytes);
        let filename = std::path::Path::new(&path)
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_owned);
        let cid = kubo.add(bytes, &path).await?;
        db::insert_file(&pool, &cid, &sha256, size, mime, filename.as_deref(), None).await?;
        println!("{cid}  {path}");
    }
    publish_and_report(&pool, &kubo).await
}

async fn cmd_ingest(dir: Option<String>) -> Result<()> {
    let Some(dir) = dir else {
        bail!("usage: bakemono-board ingest <dir>");
    };
    let pool = db::connect(&database_url()).await?;
    let kubo = kubo::Kubo::from_env();
    let stats = scrape::ingest_dir(&pool, &kubo, std::path::Path::new(&dir)).await?;
    println!("{} files across {} posts ({} skipped)", stats.files, stats.posts, stats.skipped);
    publish_and_report(&pool, &kubo).await
}

// one-off headless scrape: url, then optional cookies file and item limit in any order
async fn cmd_scrape(args: Vec<String>) -> Result<()> {
    let mut url = None;
    let mut cookies = None;
    let mut limit = None;
    for arg in args {
        if let Ok(n) = arg.parse::<u32>() {
            limit = Some(n);
        } else if std::path::Path::new(&arg).is_file() {
            cookies = Some(std::fs::read_to_string(&arg)?);
        } else if url.is_none() {
            url = Some(arg);
        } else {
            bail!("unexpected argument `{arg}`");
        }
    }
    let Some(url) = url else {
        bail!("usage: bakemono-board scrape <url> [cookies.txt] [limit]");
    };
    let pool = db::connect(&database_url()).await?;
    let kubo = kubo::Kubo::from_env();
    let stats = scrape::scrape_source(&pool, &kubo, &url, cookies.as_deref(), limit).await?;
    println!("{} files across {} posts ({} skipped)", stats.files, stats.posts, stats.skipped);
    publish_and_report(&pool, &kubo).await
}

async fn cmd_restore(head_cid: Option<String>) -> Result<()> {
    let Some(head_cid) = head_cid else {
        bail!("usage: bakemono-board restore <head-cid>");
    };
    let pool = db::connect(&database_url()).await?;
    let kubo = kubo::Kubo::from_env();
    restore::restore(&pool, &kubo, head_cid.trim()).await
}

async fn publish_and_report(pool: &sqlx::postgres::PgPool, kubo: &kubo::Kubo) -> Result<()> {
    match publish::publish_if_changed(pool, kubo).await? {
        Some(head) => println!("manifest v{} published, head {}", head.version, head_cid_of(pool).await?),
        None => println!("manifest unchanged"),
    }
    Ok(())
}

async fn head_cid_of(pool: &sqlx::postgres::PgPool) -> Result<String> {
    Ok(db::last_head(pool).await?.map(|h| h.head_cid).unwrap_or_default())
}

// the scheduler's work list: `source add <url> [cookies.txt]` / `source ls`
async fn cmd_source(args: Vec<String>) -> Result<()> {
    let pool = db::connect(&database_url()).await?;
    let mut args = args.into_iter();
    match args.next().as_deref() {
        Some("add") => {
            let Some(url) = args.next() else {
                bail!("usage: bakemono-board source add <url> [cookies.txt]");
            };
            let cookies = match args.next() {
                Some(path) => Some(std::fs::read_to_string(&path)?),
                None => None,
            };
            db::add_source(&pool, &url, cookies.as_deref()).await?;
            println!("added {url}");
        }
        Some("ls") | None => {
            for (url, enabled, scraped_at, error) in db::list_sources(&pool).await? {
                let status = if enabled { "on " } else { "off" };
                let scraped = scraped_at.unwrap_or_else(|| "never".into());
                let error = error.map(|e| format!("  ERR {e}")).unwrap_or_default();
                println!("{status}  {scraped}  {url}{error}");
            }
        }
        Some(other) => bail!("unknown source command `{other}` (expected add or ls)"),
    }
    Ok(())
}

fn database_url() -> String {
    env_or(
        "DATABASE_URL",
        "postgres://postgres:postgres@127.0.0.1:5432/bakemono?sslmode=disable",
    )
}

// stdout logs so `docker logs` shows the gateway/indexer; librqbit's own chatter is pinned to warn by
// default (RUST_LOG overrides) so cache/session lines stay legible
fn init_tracing() {
    use tracing_subscriber::prelude::*;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(
            "info,librqbit=warn,librqbit_dht=warn,librqbit_utp=warn,librqbit_upnp=warn",
        )
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
        .init();
}

// persistent cache dir so downloads survive a restart (warm); in Docker point BAKEMONO_GATEWAY_DIR at a volume
fn gateway_dir() -> std::path::PathBuf {
    if let Some(dir) = std::env::var("BAKEMONO_GATEWAY_DIR").ok().filter(|s| !s.is_empty()) {
        return dir.into();
    }
    std::env::temp_dir().join("bakemono-gateway")
}

// BAKEMONO_CACHE_GB caps the on-disk cache; over budget, least-recently-used content is evicted. 0 = unlimited
fn gateway_budget() -> u64 {
    std::env::var("BAKEMONO_CACHE_GB")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(10)
        .saturating_mul(1_000_000_000)
}

fn gateway_port() -> Option<u16> {
    std::env::var("BAKEMONO_GATEWAY_PORT")
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

// operator-pinned seeders (comma-separated ip:port) the gateway dials directly, bypassing tracker/DHT;
// the reliable path for a local seeder on the same host or a known seedbox
fn gateway_peers() -> Vec<std::net::SocketAddr> {
    std::env::var("BAKEMONO_GATEWAY_PEERS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect()
}

// BAKEMONO_RELAYS overrides; otherwise our embedded relay first, then the shared public set
fn relays() -> Vec<String> {
    if let Ok(raw) = std::env::var("BAKEMONO_RELAYS") {
        return raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    std::iter::once("ws://127.0.0.1:8080".to_string())
        .chain(bakemono_core::default_relays())
        .collect()
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
