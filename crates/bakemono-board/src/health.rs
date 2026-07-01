use std::time::Duration;

use sqlx::postgres::PgPool;

use crate::db;

// keep the swarm-health signal fresh: on a slow timer, scrape the least-recently-checked torrents' trackers
// for seeder counts so the endangered ranking reflects reality. gentle on trackers by design (bounded batch
// and concurrency), and a tracker that does not answer leaves the count untouched rather than recording zero
pub async fn run(pool: PgPool, trackers: Vec<String>) {
    if std::env::var("BAKEMONO_HEALTH_DISABLE").is_ok() || trackers.is_empty() {
        tracing::info!("swarm health probe disabled");
        return;
    }
    let interval = env_u64("BAKEMONO_HEALTH_INTERVAL_SECS", 900);
    let recheck = env_u64("BAKEMONO_HEALTH_RECHECK_SECS", 3 * 3600) as i64;
    let timeout = Duration::from_secs(env_u64("BAKEMONO_HEALTH_TIMEOUT_SECS", 4));
    let concurrency = env_u64("BAKEMONO_HEALTH_CONCURRENCY", 8).max(1) as usize;
    // BAKEMONO_HEALTH_BATCH pins a fixed batch; unset, the batch auto-sizes off catalog size (min/max clamp)
    let fixed_batch = env_opt_i64("BAKEMONO_HEALTH_BATCH");
    let batch_min = env_u64("BAKEMONO_HEALTH_BATCH_MIN", 20) as i64;
    let batch_max = env_u64("BAKEMONO_HEALTH_BATCH_MAX", 1000) as i64;

    let mut tick = tokio::time::interval(Duration::from_secs(interval));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let batch = match fixed_batch {
            Some(b) => b,
            None => {
                let catalog = db::health_catalog_size(&pool).await.unwrap_or(0);
                auto_batch(catalog, interval, recheck as u64, batch_min, batch_max)
            }
        };
        let hashes = match db::health_batch(&pool, batch, recheck).await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("health batch query failed: {e:#}");
                continue;
            }
        };
        if hashes.is_empty() {
            continue;
        }
        let probed = hashes.len();
        probe_batch(&pool, hashes, &trackers, timeout, concurrency).await;
        tracing::info!(batch, probed, "swarm health tick");
    }
}

// size a batch so the whole catalog is covered once per recheck window (ticks_per_window = recheck/interval),
// clamped so a tiny catalog still cycles and a huge one never floods trackers
fn auto_batch(catalog: i64, interval_secs: u64, recheck_secs: u64, min: i64, max: i64) -> i64 {
    let ticks = (recheck_secs / interval_secs.max(1)).max(1) as i64;
    let per_tick = (catalog + ticks - 1) / ticks;
    per_tick.max(min).min(max)
}

// probe a batch with bounded concurrency so a large auto-sized batch still finishes within the interval
async fn probe_batch(
    pool: &PgPool,
    hashes: Vec<String>,
    trackers: &[String],
    timeout: Duration,
    concurrency: usize,
) {
    for chunk in hashes.chunks(concurrency) {
        let mut set = tokio::task::JoinSet::new();
        for infohash in chunk {
            let infohash = infohash.clone();
            let trackers = trackers.to_vec();
            set.spawn(async move { (infohash.clone(), probe(&infohash, &trackers, timeout).await) });
        }
        while let Some(joined) = set.join_next().await {
            if let Ok((infohash, Some(seeders))) = joined {
                if let Err(e) = db::record_health(pool, &infohash, seeders as i32).await {
                    tracing::warn!("recording health for {infohash} failed: {e:#}");
                }
            }
        }
    }
}

// try each tracker until one answers, keeping the highest seeder count seen; None if none responded
async fn probe(infohash: &str, trackers: &[String], timeout: Duration) -> Option<u32> {
    let mut best = None;
    for tracker in trackers {
        let (tracker, infohash) = (tracker.clone(), infohash.to_string());
        let answered =
            tokio::task::spawn_blocking(move || bakemono_torrent::scrape_seeders(&tracker, &infohash, timeout))
                .await
                .ok()
                .flatten();
        if let Some(n) = answered {
            best = Some(best.map_or(n, |b: u32| b.max(n)));
        }
    }
    best
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(default)
}

fn env_opt_i64(key: &str) -> Option<i64> {
    std::env::var(key).ok().and_then(|s| s.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::auto_batch;

    #[test]
    fn batch_covers_catalog_within_the_recheck_window() {
        // 3000 torrents, 15min tick, 3h window -> 12 ticks -> 250 per tick covers all 3000
        assert_eq!(auto_batch(3000, 900, 10800, 20, 1000), 250);
    }

    #[test]
    fn batch_clamps_small_and_large_catalogs() {
        // tiny catalog floors to the minimum, huge catalog caps to the maximum
        assert_eq!(auto_batch(10, 900, 10800, 20, 1000), 20);
        assert_eq!(auto_batch(1_000_000, 900, 10800, 20, 1000), 1000);
    }
}
