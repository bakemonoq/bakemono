mod db;
mod indexer;
mod web;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let database_url = env_or(
        "DATABASE_URL",
        "postgres://postgres:postgres@127.0.0.1:5432/bakemono?sslmode=disable",
    );
    let relays: Vec<String> = env_or("BAKEMONO_RELAYS", "ws://127.0.0.1:8080")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let bind = env_or("BAKEMONO_BIND", "127.0.0.1:3000");

    let pool = db::connect(&database_url).await?;

    let indexer_pool = pool.clone();
    tokio::spawn(async move {
        if let Err(e) = indexer::run(indexer_pool, relays).await {
            eprintln!("indexer stopped: {e:#}");
        }
    });

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    println!("board on http://{bind}");
    axum::serve(listener, web::router(pool)).await?;
    Ok(())
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
