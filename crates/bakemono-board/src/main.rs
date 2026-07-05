mod config;
mod db;
mod kubo;
mod publish;
mod restore;
mod sanitize;
mod scrape;
mod thumb;
mod web;

use std::sync::Arc;

use anyhow::{bail, Result};

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        // no-arg default stays `serve` so docker entrypoints need no arguments
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
    let bind = env_or("BAKEMONO_BIND", "127.0.0.1:3000");
    let pool = db::connect(&database_url()).await?;
    let kubo = Arc::new(kubo::Kubo::from_env());

    tokio::spawn(scrape::run_scheduler(pool.clone(), kubo.clone()));

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("board on http://{bind}");
    let state = web::AppState { pool, kubo };
    axum::serve(listener, web::router(state)).await?;
    Ok(())
}

// hand-feed one file into the catalog; unlike ingest it links no post, so the file is served
// at /f/{cid} but never enters the manifest
async fn cmd_add(paths: Vec<String>) -> Result<()> {
    if paths.is_empty() {
        bail!("usage: bakemono add <file>...");
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
    Ok(())
}

async fn cmd_ingest(dir: Option<String>) -> Result<()> {
    let Some(dir) = dir else {
        bail!("usage: bakemono ingest <dir>");
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
        bail!("usage: bakemono scrape <url> [cookies.txt] [limit]");
    };
    let pool = db::connect(&database_url()).await?;
    let kubo = kubo::Kubo::from_env();
    let stats = scrape::scrape_source(&pool, &kubo, &url, cookies.as_deref(), limit).await?;
    println!("{} files across {} posts ({} skipped)", stats.files, stats.posts, stats.skipped);
    publish_and_report(&pool, &kubo).await
}

// the scheduler's work list: `source add <url> [cookies.txt]` / `source ls`
async fn cmd_source(args: Vec<String>) -> Result<()> {
    let pool = db::connect(&database_url()).await?;
    let mut args = args.into_iter();
    match args.next().as_deref() {
        Some("add") => {
            let Some(url) = args.next() else {
                bail!("usage: bakemono source add <url> [cookies.txt]");
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

async fn cmd_restore(head_cid: Option<String>) -> Result<()> {
    let Some(head_cid) = head_cid else {
        bail!("usage: bakemono restore <head-cid>");
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

fn database_url() -> String {
    env_or(
        "DATABASE_URL",
        "postgres://postgres:postgres@127.0.0.1:5432/bakemono?sslmode=disable",
    )
}

fn init_tracing() {
    use tracing_subscriber::prelude::*;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
        .init();
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
