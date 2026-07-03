use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    tracing::info!(
        "indexer subscribed to {} relay(s), honoring takedowns from {} instance(s)",
        relays.len(),
        trusted.len()
    );

    let trusted: Arc<HashSet<String>> = Arc::new(trusted.iter().map(PublicKey::to_hex).collect());
    let limiter = Arc::new(IngestLimiter::default());
    spawn_pending_gc(pool.clone());
    client
        .handle_notifications(|notification| {
            let pool = pool.clone();
            let trusted = trusted.clone();
            let limiter = limiter.clone();
            async move {
                if let RelayPoolNotification::Event { event, .. } = notification {
                    match event.kind.as_u16() {
                        KIND_MANIFEST => ingest_manifest(&pool, &event, &limiter).await,
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

// pending pubkeys never reviewed within the ttl are swept on a timer so an unreviewed flood self-heals
fn spawn_pending_gc(pool: PgPool) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(GC_INTERVAL).await;
            match crate::db::gc_pending(&pool, crate::db::PENDING_TTL_SECS).await {
                Ok(n) if n > 0 => println!("gc: dropped {n} stale pending manifest(s)"),
                Ok(_) => {}
                Err(e) => eprintln!("pending gc error: {e:#}"),
            }
        }
    });
}

async fn ingest_manifest(pool: &PgPool, event: &Event, limiter: &IngestLimiter) {
    if event.verify().is_err() {
        return;
    }
    // NIP-13 floor: drop manifests that never paid the proof-of-work before any DB work
    if !event.id.check_pow(pow_min()) {
        return;
    }
    // rate-limit on the authenticated pubkey so no single key (even an approved one) can flood the index
    if !limiter.allow(&event.pubkey.to_hex(), now_secs()) {
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

fn pow_min() -> u8 {
    static MIN: OnceLock<u8> = OnceLock::new();
    *MIN.get_or_init(|| {
        std::env::var("BAKEMONO_POW_MIN")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(bakemono_core::protocol::POW_DIFFICULTY)
    })
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

const GC_INTERVAL: Duration = Duration::from_secs(3_600);
const RATE_WINDOW_SECS: u64 = 10;
const PER_PUBKEY_MAX: u32 = 600;
const GLOBAL_MAX: u32 = 6_000;
const MAX_TRACKED_PUBKEYS: usize = 100_000;

// fixed-window rate limiter over authenticated pubkeys, with a global ceiling as the flood backstop
struct IngestLimiter {
    window_secs: u64,
    per_pubkey_max: u32,
    global_max: u32,
    state: Mutex<LimiterState>,
}

struct LimiterState {
    pubkeys: HashMap<String, Window>,
    global: Window,
}

struct Window {
    start: u64,
    count: u32,
}

impl Default for IngestLimiter {
    fn default() -> Self {
        Self::new(RATE_WINDOW_SECS, PER_PUBKEY_MAX, GLOBAL_MAX)
    }
}

impl IngestLimiter {
    fn new(window_secs: u64, per_pubkey_max: u32, global_max: u32) -> Self {
        Self {
            window_secs,
            per_pubkey_max,
            global_max,
            state: Mutex::new(LimiterState {
                pubkeys: HashMap::new(),
                global: Window { start: 0, count: 0 },
            }),
        }
    }

    // true while this pubkey is under both its own and the global rate for the current window
    fn allow(&self, pubkey: &str, now: u64) -> bool {
        let mut st = self.state.lock().unwrap();
        if st.pubkeys.len() > MAX_TRACKED_PUBKEYS {
            let window = self.window_secs;
            st.pubkeys.retain(|_, w| now.saturating_sub(w.start) < window);
        }
        let per_ok = {
            let w = st
                .pubkeys
                .entry(pubkey.to_string())
                .or_insert(Window { start: now, count: 0 });
            bump(w, now, self.window_secs, self.per_pubkey_max)
        };
        per_ok && bump(&mut st.global, now, self.window_secs, self.global_max)
    }
}

fn bump(w: &mut Window, now: u64, window: u64, max: u32) -> bool {
    if now.saturating_sub(w.start) >= window {
        w.start = now;
        w.count = 0;
    }
    w.count += 1;
    w.count <= max
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use bakemono_core::{Manifest, Takedown, Target};
    use nostr_relay_builder::MockRelay;

    #[test]
    fn rate_limiter_caps_per_pubkey_and_globally() {
        let limiter = IngestLimiter::new(10, 3, 100);
        assert!(limiter.allow("a", 100));
        assert!(limiter.allow("a", 100));
        assert!(limiter.allow("a", 100));
        assert!(!limiter.allow("a", 101), "a fourth event in the window is dropped");
        assert!(limiter.allow("a", 120), "the next window allows the pubkey again");

        let flood = IngestLimiter::new(10, 100, 4);
        for i in 0..4 {
            assert!(flood.allow(&format!("k{i}"), 0));
        }
        assert!(!flood.allow("k4", 0), "the global window caps a fresh-key flood");
    }

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
        let manifest_event = manifest
            .to_event_pow(&contributor, bakemono_core::protocol::POW_DIFFICULTY)
            .unwrap();
        publisher.send_event(&manifest_event).await.unwrap();

        // the post lands in the review queue on ingest; approve it so the file would be visible
        wait_for(|| async {
            crate::db::approve_pending(&pool, &contributor.public_key().to_hex(), "", "", "")
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
