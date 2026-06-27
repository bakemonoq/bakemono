use anyhow::Result;
use nostr_sdk::prelude::*;
use sqlx::postgres::PgPool;

use bakemono_core::protocol::KIND_MANIFEST;
use bakemono_core::Manifest;

pub async fn run(pool: PgPool, relays: Vec<String>) -> Result<()> {
    let client = Client::new(Keys::generate());
    for relay in &relays {
        client.add_relay(relay).await?;
    }
    client.connect().await;
    client
        .subscribe(Filter::new().kind(Kind::from(KIND_MANIFEST)), None)
        .await?;
    println!("indexer subscribed to {} relay(s)", relays.len());

    client
        .handle_notifications(|notification| {
            let pool = pool.clone();
            async move {
                if let RelayPoolNotification::Event { event, .. } = notification {
                    ingest(&pool, &event).await;
                }
                Ok(false)
            }
        })
        .await?;
    Ok(())
}

async fn ingest(pool: &PgPool, event: &Event) {
    if event.verify().is_err() {
        return;
    }
    let manifest = match Manifest::from_event(event) {
        Ok(manifest) => manifest,
        Err(_) => return,
    };
    if let Err(e) = crate::db::upsert(pool, event, &manifest).await {
        eprintln!("ingest error for {}: {e:#}", event.id.to_hex());
    }
}
