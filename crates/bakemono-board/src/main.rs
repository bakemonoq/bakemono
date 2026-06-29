mod db;
mod indexer;
mod instance;
mod web;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let database_url = env_or(
        "DATABASE_URL",
        "postgres://postgres:postgres@127.0.0.1:5432/bakemono?sslmode=disable",
    );
    let relays = relays();
    let bind = env_or("BAKEMONO_BIND", "127.0.0.1:3000");
    let signer = instance::load();
    let trusted = instance::trusted(signer.as_ref());

    let pool = db::connect(&database_url).await?;

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
    };
    axum::serve(listener, web::router(state)).await?;
    Ok(())
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
