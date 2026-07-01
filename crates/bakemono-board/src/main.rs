mod db;
mod indexer;
mod instance;
mod web;

use std::sync::Arc;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let database_url = env_or(
        "DATABASE_URL",
        "postgres://postgres:postgres@127.0.0.1:5432/bakemono?sslmode=disable",
    );
    let relays = relays();
    let bind = env_or("BAKEMONO_BIND", "127.0.0.1:3000");
    let signer = instance::load();
    let trusted = instance::trusted(signer.as_ref());

    let pool = db::connect(&database_url).await?;
    let gateway = Arc::new(
        bakemono_torrent::Gateway::new(gateway_dir(), gateway_port(), gateway_peers()).await?,
    );

    let indexer_pool = pool.clone();
    let indexer_relays = relays.clone();
    tokio::spawn(async move {
        if let Err(e) = indexer::run(indexer_pool, indexer_relays, trusted).await {
            eprintln!("indexer stopped: {e:#}");
        }
    });

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    println!("board on http://{bind}");
    let state = web::AppState {
        pool,
        relays,
        signer,
        gateway,
    };
    axum::serve(listener, web::router(state)).await?;
    Ok(())
}

// a fresh per-process dir keeps every start cold (no warm pieces from a prior run); override to persist
fn gateway_dir() -> std::path::PathBuf {
    if let Some(dir) = std::env::var("BAKEMONO_GATEWAY_DIR").ok().filter(|s| !s.is_empty()) {
        return dir.into();
    }
    std::env::temp_dir().join(format!("bakemono-gw-{}", std::process::id()))
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
