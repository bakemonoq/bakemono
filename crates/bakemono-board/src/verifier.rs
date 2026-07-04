use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use sqlx::postgres::PgPool;
use tokio::io::AsyncReadExt;

use bakemono_torrent::Gateway;

use crate::db;
use crate::web::hex_lower;

const TICK_SECS: u64 = 30;
const BATCH: i64 = 8;

// required verification: every new manifest's bytes are pulled from the swarm and sha256'd before it can
// reach a mod. bytes matching their claimed hash whose content is already approved go public; matching-but-
// new go to the queue; a mismatch is quarantined. a file with no reachable seeder is retried next tick
pub async fn run(pool: PgPool, gateway: Arc<Gateway>) {
    let secs = std::env::var("BAKEMONO_VERIFY_SECS")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(TICK_SECS);
    let mut tick = tokio::time::interval(Duration::from_secs(secs));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        match db::unverified_batch(&pool, BATCH).await {
            Ok(batch) => {
                for job in batch {
                    verify_one(&pool, &gateway, job).await;
                }
            }
            Err(e) => tracing::warn!("verifier: batch query failed: {e:#}"),
        }
    }
}

async fn verify_one(pool: &PgPool, gateway: &Gateway, job: db::VerifyJob) {
    let file_index = job.file_index as usize;
    let (sha256, size) = match fetch_and_hash(gateway, &job.magnet, file_index).await {
        Ok(v) => v,
        Err(_) => return, // no reachable seeder yet: leave it unverified and retry next tick
    };
    let ok = sha256 == job.file_hash && size == job.size as u64;
    if let Err(e) = db::record_verification(pool, &job.infohash, file_index, &sha256, ok, size).await {
        tracing::warn!("verifier: record failed: {e:#}");
        return;
    }
    match db::apply_verification(pool, &job.infohash, file_index, ok).await {
        Ok(_) if !ok => {
            tracing::warn!(infohash = %job.infohash, "verifier: bytes do not match claimed hash, quarantined")
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("verifier: status update failed: {e:#}"),
    }
}

async fn fetch_and_hash(gateway: &Gateway, magnet: &str, file_index: usize) -> anyhow::Result<(String, u64)> {
    let mut file = gateway.open(magnet, file_index).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 128 * 1024];
    let mut size = 0u64;
    loop {
        let n = file.stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((hex_lower(hasher.finalize()), size))
}
