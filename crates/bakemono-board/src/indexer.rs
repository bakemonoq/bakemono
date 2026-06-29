use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use nostr_sdk::prelude::*;
use sqlx::postgres::PgPool;

use bakemono_core::protocol::{KIND_MANIFEST, KIND_TAKEDOWN};
use bakemono_core::{Manifest, Takedown};

pub async fn run(pool: PgPool, relays: Vec<String>, trusted: Vec<PublicKey>) -> Result<()> {
    let client = Client::new(Keys::generate());
    for relay in &relays {
        client.add_relay(relay).await?;
    }
    client.connect().await;
    client
        .subscribe(Filter::new().kind(Kind::from(KIND_MANIFEST)), None)
        .await?;
    if !trusted.is_empty() {
        client
            .subscribe(
                Filter::new()
                    .kind(Kind::from(KIND_TAKEDOWN))
                    .authors(trusted.clone()),
                None,
            )
            .await?;
    }
    println!(
        "indexer subscribed to {} relay(s), honoring takedowns from {} instance(s)",
        relays.len(),
        trusted.len()
    );

    let trusted: Arc<HashSet<String>> = Arc::new(trusted.iter().map(PublicKey::to_hex).collect());
    client
        .handle_notifications(|notification| {
            let pool = pool.clone();
            let trusted = trusted.clone();
            async move {
                if let RelayPoolNotification::Event { event, .. } = notification {
                    match event.kind.as_u16() {
                        KIND_MANIFEST => ingest_manifest(&pool, &event).await,
                        KIND_TAKEDOWN => ingest_takedown(&pool, &event, &trusted).await,
                        _ => {}
                    }
                }
                Ok(false)
            }
        })
        .await?;
    Ok(())
}

async fn ingest_manifest(pool: &PgPool, event: &Event) {
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

// honor a takedown only from a trusted operator; the relay author filter is advisory, so re-check here
async fn ingest_takedown(pool: &PgPool, event: &Event, trusted: &HashSet<String>) {
    if event.verify().is_err() || !trusted.contains(&event.pubkey.to_hex()) {
        return;
    }
    let takedown = match Takedown::from_event(event) {
        Ok(takedown) => takedown,
        Err(_) => return,
    };
    let source = event.pubkey.to_hex();
    if let Err(e) =
        crate::db::record_takedown(pool, &takedown, &source, Some(&event.id.to_hex())).await
    {
        eprintln!("takedown ingest error for {}: {e:#}", event.id.to_hex());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use bakemono_core::{Manifest, Takedown, Target};
    use nostr_relay_builder::MockRelay;

    // set BAKEMONO_TEST_DB to a Postgres url to run, otherwise skipped
    #[tokio::test]
    async fn honors_a_takedown_from_a_trusted_instance() {
        let Ok(url) = std::env::var("BAKEMONO_TEST_DB") else {
            eprintln!("skipping: BAKEMONO_TEST_DB not set");
            return;
        };
        let pool = match crate::db::connect(&url).await {
            Ok(pool) => pool,
            Err(e) => {
                eprintln!("skipping: cannot reach test db: {e}");
                return;
            }
        };

        let relay = MockRelay::run().await.unwrap();
        let relay_url = relay.url().await.to_string();
        let operator = Keys::generate();
        let contributor = Keys::generate();
        let creator_id = format!("peer-td-{}", std::process::id());
        let hash = format!("{:0<64}", creator_id.replace(|c: char| !c.is_ascii_hexdigit(), ""));

        let indexer = tokio::spawn(run(
            pool.clone(),
            vec![relay_url.clone()],
            vec![operator.public_key()],
        ));
        tokio::time::sleep(Duration::from_millis(300)).await;

        let publisher = Client::new(Keys::generate());
        publisher.add_relay(&relay_url).await.unwrap();
        publisher.connect().await;

        let mut manifest = Manifest {
            platform: "patreon".into(),
            creator: "Peer".into(),
            creator_id: creator_id.clone(),
            post_id: "1".into(),
            mime: "image/jpeg".into(),
            magnet: "magnet:?xt=urn:btih:abc".into(),
            content: "body".into(),
            ..Default::default()
        };
        manifest.file_hash = hash.clone();
        manifest.size = 1;
        let manifest_event = manifest.to_event(&contributor).unwrap();
        publisher.send_event(&manifest_event).await.unwrap();

        // the contributor lands in the queue on first sight; approve so the file would be visible
        wait_for(|| async {
            crate::db::approve_pubkey(&pool, &contributor.public_key().to_hex())
                .await
                .ok();
            visible(&pool, &creator_id).await == 1
        })
        .await;

        let takedown = Takedown {
            target: Target::FileHash(hash.clone()),
            reason: "dmca-us".into(),
            applied_at: None,
            explanation: String::new(),
        };

        // an untrusted operator's takedown must be ignored, otherwise anyone could hide anything
        let stranger = Keys::generate();
        publisher
            .send_event(&takedown.to_event(&stranger).unwrap())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            visible(&pool, &creator_id).await,
            1,
            "an untrusted takedown is not honored"
        );

        publisher
            .send_event(&takedown.to_event(&operator).unwrap())
            .await
            .unwrap();

        let hidden = wait_for(|| async { visible(&pool, &creator_id).await == 0 }).await;
        indexer.abort();
        sqlx::query("DELETE FROM manifests WHERE creator_id = $1")
            .bind(&creator_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM takedowns WHERE target = $1")
            .bind(&hash)
            .execute(&pool)
            .await
            .unwrap();
        assert!(hidden, "a trusted instance takedown should hide the file");
    }

    async fn visible(pool: &PgPool, creator_id: &str) -> usize {
        crate::db::post_files(pool, "patreon", creator_id, "1")
            .await
            .unwrap()
            .len()
    }

    async fn wait_for<F, Fut>(mut cond: F) -> bool
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        for _ in 0..50 {
            if cond().await {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }
}
