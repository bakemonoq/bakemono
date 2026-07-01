use anyhow::Result;
use sqlx::postgres::{PgPool, PgPoolOptions};

use bakemono_core::nostr::Event;
use bakemono_core::{Manifest, Takedown};

pub async fn connect(url: &str) -> Result<PgPool> {
    let pool = PgPoolOptions::new().max_connections(5).connect(url).await?;
    sqlx::raw_sql(SCHEMA).execute(&pool).await?;
    Ok(pool)
}

// cap on pubkeys awaiting review; fresh keys past it are shed until the queue drains or gc frees room
const MAX_PENDING: i64 = 5_000;
// pending pubkeys left unreviewed this long are garbage-collected along with their hidden manifests
pub const PENDING_TTL_SECS: i64 = 14 * 24 * 3_600;

pub async fn upsert(pool: &PgPool, event: &Event, manifest: &Manifest) -> Result<()> {
    let created_at = event.created_at.as_secs() as i64;
    let pubkey = event.pubkey.to_hex();
    match pubkey_status(pool, &pubkey).await?.as_deref() {
        // already rejected: drop instead of storing it hidden
        Some("rejected") => return Ok(()),
        // a known pending or approved pubkey: fall through and store
        Some(_) => {}
        // never seen: enqueue for review only while the queue has room, else shed the flood
        None => {
            if !try_enqueue_pubkey(pool, &pubkey, MAX_PENDING).await? {
                return Ok(());
            }
        }
    }
    // NIP-33: a newer event with the same (pubkey, d) replaces the older one
    sqlx::query("DELETE FROM manifests WHERE pubkey = $1 AND d_tag = $2 AND created_at < $3")
        .bind(event.pubkey.to_hex())
        .bind(manifest.d_tag())
        .bind(created_at)
        .execute(pool)
        .await?;
    sqlx::query(INSERT)
        .bind(event.id.to_hex())
        .bind(event.pubkey.to_hex())
        .bind(created_at)
        .bind(manifest.d_tag())
        .bind(&manifest.file_hash)
        .bind(manifest.size as i64)
        .bind(&manifest.mime)
        .bind(&manifest.magnet)
        .bind(&manifest.platform)
        .bind(&manifest.creator)
        .bind(&manifest.creator_id)
        .bind(&manifest.post_id)
        .bind(manifest.file_index as i32)
        .bind(&manifest.filename)
        .bind(&manifest.post_title)
        .bind(&manifest.posted_at)
        .bind(&manifest.tier)
        .bind(&manifest.content)
        .bind(&manifest.thumb)
        .bind(bakemono_torrent::infohash_from_magnet(&manifest.magnet))
        .execute(pool)
        .await?;
    Ok(())
}

// the gateway only serves infohashes the board actually carries (and that pass moderation), so resolve
// through visible_manifests; an unknown or hidden hash returns None and the route 404s
pub async fn magnet_by_infohash(pool: &PgPool, infohash: &str) -> Result<Option<String>> {
    let magnet = sqlx::query_scalar(
        "SELECT magnet FROM visible_manifests WHERE infohash = $1 LIMIT 1",
    )
    .bind(infohash)
    .fetch_optional(pool)
    .await?;
    Ok(magnet)
}

async fn pubkey_status(pool: &PgPool, pubkey: &str) -> Result<Option<String>> {
    let status = sqlx::query_scalar("SELECT status FROM pubkeys WHERE pubkey = $1")
        .bind(pubkey)
        .fetch_optional(pool)
        .await?;
    Ok(status)
}

// enqueue a never-seen pubkey as pending only while the queue is under the cap; the bool says whether
// it was enqueued, so a flood of fresh keys past the cap is shed rather than filling the queue
pub(crate) async fn try_enqueue_pubkey(pool: &PgPool, pubkey: &str, cap: i64) -> Result<bool> {
    let res = sqlx::query(
        "INSERT INTO pubkeys (pubkey, status, first_seen)
         SELECT $1, 'pending', EXTRACT(EPOCH FROM now())::bigint
         WHERE (SELECT COUNT(*) FROM pubkeys WHERE status = 'pending') < $2
         ON CONFLICT (pubkey) DO NOTHING",
    )
    .bind(pubkey)
    .bind(cap)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() == 1)
}

// drop pending pubkeys never reviewed within the ttl, with their hidden manifests, so an unreviewed
// flood self-heals instead of growing the index forever; returns the number of manifests removed
pub async fn gc_pending(pool: &PgPool, ttl_secs: i64) -> Result<u64> {
    let res = sqlx::query(
        "WITH stale AS (
             DELETE FROM pubkeys
             WHERE status = 'pending'
               AND first_seen < EXTRACT(EPOCH FROM now())::bigint - $1
             RETURNING pubkey
         )
         DELETE FROM manifests WHERE pubkey IN (SELECT pubkey FROM stale)",
    )
    .bind(ttl_secs)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

pub async fn creators(pool: &PgPool) -> Result<Vec<CreatorRow>> {
    let rows = sqlx::query_as::<_, CreatorRow>(
        "SELECT platform, creator_id, MAX(creator) AS creator,
                COUNT(DISTINCT post_id) AS posts, COUNT(DISTINCT file_hash) AS files
         FROM visible_manifests
         GROUP BY platform, creator_id ORDER BY creator",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn search_creators(pool: &PgPool, query: &str) -> Result<Vec<CreatorRow>> {
    let rows = sqlx::query_as::<_, CreatorRow>(
        "SELECT platform, creator_id, MAX(creator) AS creator,
                COUNT(DISTINCT post_id) AS posts, COUNT(DISTINCT file_hash) AS files
         FROM visible_manifests
         GROUP BY platform, creator_id
         HAVING MAX(creator) ILIKE '%' || $1 || '%'
         ORDER BY creator",
    )
    .bind(query)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn stats(pool: &PgPool) -> Result<Stats> {
    let row = sqlx::query_as::<_, Stats>(
        "SELECT
            COUNT(DISTINCT (platform, creator_id, post_id)) AS posts,
            COUNT(DISTINCT (platform, creator_id)) AS authors,
            COUNT(DISTINCT file_hash) AS files,
            COUNT(DISTINCT pubkey) AS contributors
         FROM visible_manifests",
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn posts_by_creator(
    pool: &PgPool,
    platform: &str,
    creator_id: &str,
) -> Result<Vec<PostRow>> {
    let rows = sqlx::query_as::<_, PostRow>(
        "SELECT platform, creator_id, post_id, MAX(creator) AS creator,
                MAX(post_title) AS post_title, MAX(posted_at) AS posted_at,
                COUNT(DISTINCT file_hash) AS files
         FROM visible_manifests
         WHERE platform = $1 AND creator_id = $2
         GROUP BY platform, creator_id, post_id
         ORDER BY MAX(posted_at) DESC NULLS LAST, MAX(created_at) DESC",
    )
    .bind(platform)
    .bind(creator_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn post_files(
    pool: &PgPool,
    platform: &str,
    creator_id: &str,
    post_id: &str,
) -> Result<Vec<ManifestRow>> {
    // dedup by file hash (latest event per identical content), then order for display
    let rows = sqlx::query_as::<_, ManifestRow>(
        "SELECT * FROM (
             SELECT DISTINCT ON (file_hash) * FROM visible_manifests
             WHERE platform = $1 AND creator_id = $2 AND post_id = $3
             ORDER BY file_hash, created_at DESC
         ) t ORDER BY file_index",
    )
    .bind(platform)
    .bind(creator_id)
    .bind(post_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn recent(pool: &PgPool, limit: i64) -> Result<Vec<ManifestRow>> {
    let rows = sqlx::query_as::<_, ManifestRow>(
        "SELECT * FROM (
             SELECT DISTINCT ON (file_hash) * FROM visible_manifests
             ORDER BY file_hash, created_at DESC
         ) t ORDER BY created_at DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn pending_pubkeys(pool: &PgPool, limit: i64) -> Result<Vec<PendingRow>> {
    let rows = sqlx::query_as::<_, PendingRow>(
        "SELECT p.pubkey, COUNT(m.event_id) AS files,
                MAX(m.creator) AS creator, MAX(m.post_title) AS sample
         FROM pubkeys p LEFT JOIN manifests m ON m.pubkey = p.pubkey
         WHERE p.status = 'pending'
         GROUP BY p.pubkey, p.first_seen
         ORDER BY p.first_seen DESC
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// pending keys collapsed by the creator they target, so a flood aimed at one creator reviews as a
// handful of rows the operator can bulk-approve or bulk-reject instead of thousands of single keys
pub async fn pending_groups(pool: &PgPool, limit: i64) -> Result<Vec<PendingGroup>> {
    let rows = sqlx::query_as::<_, PendingGroup>(
        "SELECT m.platform, m.creator_id, MAX(m.creator) AS creator,
                COUNT(DISTINCT m.pubkey) AS pubkeys, COUNT(m.event_id) AS files
         FROM pubkeys p JOIN manifests m ON m.pubkey = p.pubkey
         WHERE p.status = 'pending'
         GROUP BY m.platform, m.creator_id
         ORDER BY COUNT(DISTINCT m.pubkey) DESC, COUNT(m.event_id) DESC
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn approve_pubkey(pool: &PgPool, pubkey: &str) -> Result<()> {
    sqlx::query("UPDATE pubkeys SET status = 'approved' WHERE pubkey = $1")
        .bind(pubkey)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn reject_pubkey(pool: &PgPool, pubkey: &str) -> Result<()> {
    sqlx::query("UPDATE pubkeys SET status = 'rejected' WHERE pubkey = $1")
        .bind(pubkey)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM manifests WHERE pubkey = $1")
        .bind(pubkey)
        .execute(pool)
        .await?;
    Ok(())
}

// approve every still-pending pubkey that posted to this creator (approved keys are left untouched)
pub async fn approve_creator(pool: &PgPool, platform: &str, creator_id: &str) -> Result<u64> {
    let res = sqlx::query(
        "UPDATE pubkeys SET status = 'approved'
         WHERE status = 'pending' AND pubkey IN (
             SELECT DISTINCT pubkey FROM manifests WHERE platform = $1 AND creator_id = $2
         )",
    )
    .bind(platform)
    .bind(creator_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

// reject the still-pending keys that targeted this creator and drop their manifests in one pass
pub async fn reject_creator(pool: &PgPool, platform: &str, creator_id: &str) -> Result<u64> {
    let res = sqlx::query(
        "WITH targeted AS (
             SELECT DISTINCT pubkey FROM manifests WHERE platform = $1 AND creator_id = $2
         ), rejected AS (
             UPDATE pubkeys SET status = 'rejected'
             WHERE status = 'pending' AND pubkey IN (SELECT pubkey FROM targeted)
             RETURNING pubkey
         )
         DELETE FROM manifests WHERE pubkey IN (SELECT pubkey FROM rejected)",
    )
    .bind(platform)
    .bind(creator_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

// hide a target locally; the d_tag is the NIP-33 replaceable id so a fresh decision on the same
// target overwrites the old one. source is "local" or the signer pubkey of a honored peer takedown
pub async fn record_takedown(
    pool: &PgPool,
    takedown: &Takedown,
    source: &str,
    event_id: Option<&str>,
) -> Result<()> {
    let (target_type, target) = takedown.target.parts();
    sqlx::query(UPSERT_TAKEDOWN)
        .bind(takedown.d_tag())
        .bind(target_type)
        .bind(target)
        .bind(&takedown.reason)
        .bind(&takedown.explanation)
        .bind(source)
        .bind(event_id)
        .bind(takedown.applied_at.as_deref().unwrap_or(""))
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn remove_takedown(pool: &PgPool, d_tag: &str) -> Result<()> {
    sqlx::query("DELETE FROM takedowns WHERE d_tag = $1")
        .bind(d_tag)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn takedowns(pool: &PgPool) -> Result<Vec<TakedownRow>> {
    let rows = sqlx::query_as::<_, TakedownRow>(
        "SELECT d_tag, target_type, target, reason, explanation, source, event_id, applied_at
         FROM takedowns ORDER BY applied_at DESC, d_tag",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[derive(sqlx::FromRow)]
#[allow(dead_code)]
pub struct TakedownRow {
    pub d_tag: String,
    pub target_type: String,
    pub target: String,
    pub reason: String,
    pub explanation: String,
    pub source: String,
    pub event_id: Option<String>,
    pub applied_at: String,
}

#[derive(sqlx::FromRow)]
pub struct PendingRow {
    pub pubkey: String,
    pub files: i64,
    pub creator: Option<String>,
    pub sample: Option<String>,
}

#[derive(sqlx::FromRow)]
pub struct PendingGroup {
    pub platform: String,
    pub creator_id: String,
    pub creator: Option<String>,
    pub pubkeys: i64,
    pub files: i64,
}

#[derive(sqlx::FromRow, Default)]
pub struct Stats {
    pub posts: i64,
    pub authors: i64,
    pub files: i64,
    pub contributors: i64,
}

#[derive(sqlx::FromRow)]
pub struct CreatorRow {
    pub platform: String,
    pub creator_id: String,
    pub creator: String,
    pub posts: i64,
    pub files: i64,
}

#[derive(sqlx::FromRow)]
pub struct PostRow {
    pub platform: String,
    pub creator_id: String,
    pub post_id: String,
    pub creator: String,
    pub post_title: Option<String>,
    pub posted_at: Option<String>,
    pub files: i64,
}

#[derive(sqlx::FromRow)]
#[allow(dead_code)]
pub struct ManifestRow {
    pub event_id: String,
    pub pubkey: String,
    pub created_at: i64,
    pub d_tag: String,
    pub file_hash: String,
    pub size: i64,
    pub mime: String,
    pub magnet: String,
    pub platform: String,
    pub creator: String,
    pub creator_id: String,
    pub post_id: String,
    pub file_index: i32,
    pub filename: Option<String>,
    pub post_title: Option<String>,
    pub posted_at: Option<String>,
    pub tier: Option<String>,
    pub content: String,
    pub thumb: Option<String>,
    pub infohash: Option<String>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS manifests (
    event_id   TEXT PRIMARY KEY,
    pubkey     TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    d_tag      TEXT NOT NULL,
    file_hash  TEXT NOT NULL,
    size       BIGINT NOT NULL,
    mime       TEXT NOT NULL,
    magnet     TEXT NOT NULL,
    platform   TEXT NOT NULL,
    creator    TEXT NOT NULL,
    creator_id TEXT NOT NULL,
    post_id    TEXT NOT NULL,
    file_index INTEGER NOT NULL,
    filename   TEXT,
    post_title TEXT,
    posted_at  TEXT,
    tier       TEXT,
    content    TEXT NOT NULL,
    thumb      TEXT,
    infohash   TEXT
);
-- drop the view first so the seeded-thumb columns it expanded via m.* can be dropped below
DROP VIEW IF EXISTS visible_manifests;
-- inline preview lives in the event now; retire the seeded-thumb columns
ALTER TABLE manifests ADD COLUMN IF NOT EXISTS thumb TEXT;
ALTER TABLE manifests DROP COLUMN IF EXISTS thumb_x;
ALTER TABLE manifests DROP COLUMN IF EXISTS thumb_magnet;
ALTER TABLE manifests DROP COLUMN IF EXISTS thumb_infohash;
-- the gateway keys on the v1 btih; carry it as its own column so a lookup is an index hit, not a magnet scan
ALTER TABLE manifests ADD COLUMN IF NOT EXISTS infohash TEXT;
UPDATE manifests SET infohash = lower(substring(magnet from 'xt=urn:btih:([0-9A-Fa-f]{40})'))
WHERE infohash IS NULL AND magnet ~ 'xt=urn:btih:[0-9A-Fa-f]{40}';
CREATE INDEX IF NOT EXISTS manifests_creator ON manifests (platform, creator_id);
CREATE INDEX IF NOT EXISTS manifests_post ON manifests (platform, creator_id, post_id);
CREATE INDEX IF NOT EXISTS manifests_hash ON manifests (file_hash);
CREATE INDEX IF NOT EXISTS manifests_infohash ON manifests (infohash);
CREATE INDEX IF NOT EXISTS manifests_recent ON manifests (created_at DESC);

CREATE TABLE IF NOT EXISTS pubkeys (
    pubkey     TEXT PRIMARY KEY,
    status     TEXT NOT NULL DEFAULT 'pending',
    first_seen BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS pubkeys_status ON pubkeys (status);
-- grandfather pubkeys already indexed into the queue (one-time, idempotent)
INSERT INTO pubkeys (pubkey, status, first_seen)
SELECT pubkey, 'pending', MIN(created_at) FROM manifests GROUP BY pubkey
ON CONFLICT (pubkey) DO NOTHING;

CREATE TABLE IF NOT EXISTS takedowns (
    d_tag       TEXT PRIMARY KEY,
    target_type TEXT NOT NULL,
    target      TEXT NOT NULL,
    reason      TEXT NOT NULL,
    explanation TEXT NOT NULL DEFAULT '',
    source      TEXT NOT NULL,
    event_id    TEXT,
    applied_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS takedowns_target ON takedowns (target_type, target);

-- the single definition of what the public UI shows: approved pubkeys, minus anything a takedown hides
CREATE OR REPLACE VIEW visible_manifests AS
SELECT m.* FROM manifests m
WHERE m.pubkey IN (SELECT pubkey FROM pubkeys WHERE status = 'approved')
  AND NOT EXISTS (
      SELECT 1 FROM takedowns t WHERE
          (t.target_type = 'e' AND t.target = m.event_id) OR
          (t.target_type = 'x' AND t.target = m.file_hash) OR
          (t.target_type = 'p' AND t.target = m.pubkey)
  );
";

const INSERT: &str = "
INSERT INTO manifests (
    event_id, pubkey, created_at, d_tag, file_hash, size, mime, magnet,
    platform, creator, creator_id, post_id, file_index, filename, post_title, posted_at, tier, content,
    thumb, infohash
) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20)
ON CONFLICT (event_id) DO NOTHING
";

const UPSERT_TAKEDOWN: &str = "
INSERT INTO takedowns (d_tag, target_type, target, reason, explanation, source, event_id, applied_at)
VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
ON CONFLICT (d_tag) DO UPDATE SET
    target_type = EXCLUDED.target_type,
    target = EXCLUDED.target,
    reason = EXCLUDED.reason,
    explanation = EXCLUDED.explanation,
    source = EXCLUDED.source,
    event_id = COALESCE(EXCLUDED.event_id, takedowns.event_id),
    applied_at = EXCLUDED.applied_at
";

#[cfg(test)]
mod tests {
    use super::*;
    use bakemono_core::nostr::Keys;
    use bakemono_core::Target;

    // set BAKEMONO_TEST_DB to a Postgres url to run, otherwise skipped
    #[tokio::test]
    async fn ingest_query_and_replace() {
        let Ok(url) = std::env::var("BAKEMONO_TEST_DB") else {
            eprintln!("skipping: BAKEMONO_TEST_DB not set");
            return;
        };
        let pool = match connect(&url).await {
            Ok(pool) => pool,
            Err(e) => {
                eprintln!("skipping: cannot reach test db: {e}");
                return;
            }
        };
        let keys = Keys::generate();
        let creator_id = format!("test-{}", std::process::id());
        let mut manifest = sample(&creator_id);

        let older = manifest.to_event_at(&keys, 1_000).unwrap();
        upsert(&pool, &older, &manifest).await.unwrap();
        approve_pubkey(&pool, &keys.public_key().to_hex())
            .await
            .unwrap();
        let files = post_files(&pool, &manifest.platform, &creator_id, &manifest.post_id)
            .await
            .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].size, manifest.size as i64);

        // newer event, same (pubkey, d) -> replaces the older per NIP-33
        manifest.size = 999;
        let newer = manifest.to_event_at(&keys, 2_000).unwrap();
        upsert(&pool, &newer, &manifest).await.unwrap();
        let files = post_files(&pool, &manifest.platform, &creator_id, &manifest.post_id)
            .await
            .unwrap();
        assert_eq!(files.len(), 1, "only the newest event is kept");
        assert_eq!(files[0].size, 999);
        assert_eq!(files[0].event_id, newer.id.to_hex());

        sqlx::query("DELETE FROM manifests WHERE creator_id = $1")
            .bind(&creator_id)
            .execute(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn dedupes_by_file_hash_across_pubkeys() {
        let Ok(url) = std::env::var("BAKEMONO_TEST_DB") else {
            eprintln!("skipping: BAKEMONO_TEST_DB not set");
            return;
        };
        let pool = match connect(&url).await {
            Ok(pool) => pool,
            Err(e) => {
                eprintln!("skipping: cannot reach test db: {e}");
                return;
            }
        };
        let creator_id = format!("dedup-{}", std::process::id());
        let manifest = sample(&creator_id);

        // same content, two different contributors (pubkeys) -> shown once
        let ka = Keys::generate();
        let kb = Keys::generate();
        let a = manifest.to_event(&ka).unwrap();
        let b = manifest.to_event(&kb).unwrap();
        upsert(&pool, &a, &manifest).await.unwrap();
        upsert(&pool, &b, &manifest).await.unwrap();
        approve_pubkey(&pool, &ka.public_key().to_hex())
            .await
            .unwrap();
        approve_pubkey(&pool, &kb.public_key().to_hex())
            .await
            .unwrap();

        let files = post_files(&pool, &manifest.platform, &creator_id, &manifest.post_id)
            .await
            .unwrap();
        assert_eq!(files.len(), 1);

        sqlx::query("DELETE FROM manifests WHERE creator_id = $1")
            .bind(&creator_id)
            .execute(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn takedown_hides_then_unhides_a_file() {
        let Ok(url) = std::env::var("BAKEMONO_TEST_DB") else {
            eprintln!("skipping: BAKEMONO_TEST_DB not set");
            return;
        };
        let pool = match connect(&url).await {
            Ok(pool) => pool,
            Err(e) => {
                eprintln!("skipping: cannot reach test db: {e}");
                return;
            }
        };
        let keys = Keys::generate();
        let creator_id = format!("takedown-{}", std::process::id());
        let mut manifest = sample(&creator_id);
        let hash = format!("{:0<64}", creator_id.replace('-', ""));
        manifest.file_hash = hash.clone();

        let event = manifest.to_event(&keys).unwrap();
        upsert(&pool, &event, &manifest).await.unwrap();
        approve_pubkey(&pool, &keys.public_key().to_hex())
            .await
            .unwrap();
        assert_eq!(
            post_files(&pool, &manifest.platform, &creator_id, &manifest.post_id)
                .await
                .unwrap()
                .len(),
            1
        );

        let takedown = Takedown {
            target: Target::FileHash(hash.clone()),
            reason: "dmca-us".into(),
            applied_at: Some("2026-06-29T00:00:00+00:00".into()),
            explanation: String::new(),
        };
        record_takedown(&pool, &takedown, "local", None)
            .await
            .unwrap();
        assert!(
            post_files(&pool, &manifest.platform, &creator_id, &manifest.post_id)
                .await
                .unwrap()
                .is_empty(),
            "a file-hash takedown hides the file"
        );

        remove_takedown(&pool, &takedown.d_tag()).await.unwrap();
        assert_eq!(
            post_files(&pool, &manifest.platform, &creator_id, &manifest.post_id)
                .await
                .unwrap()
                .len(),
            1,
            "undoing the takedown brings it back"
        );

        sqlx::query("DELETE FROM manifests WHERE creator_id = $1")
            .bind(&creator_id)
            .execute(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn pending_cap_sheds_fresh_keys_when_full() {
        let Ok(url) = std::env::var("BAKEMONO_TEST_DB") else {
            eprintln!("skipping: BAKEMONO_TEST_DB not set");
            return;
        };
        let pool = match connect(&url).await {
            Ok(pool) => pool,
            Err(e) => {
                eprintln!("skipping: cannot reach test db: {e}");
                return;
            }
        };
        let full = format!("capfull-{}", std::process::id());
        let room = format!("caproom-{}", std::process::id());

        // a zero cap is always full, so a fresh key is shed
        assert!(!try_enqueue_pubkey(&pool, &full, 0).await.unwrap());
        // a generous cap enqueues the fresh key once; a repeat is not a new insert
        assert!(try_enqueue_pubkey(&pool, &room, 1_000_000).await.unwrap());
        assert!(!try_enqueue_pubkey(&pool, &room, 1_000_000).await.unwrap());

        sqlx::query("DELETE FROM pubkeys WHERE pubkey = ANY($1)")
            .bind(vec![full, room])
            .execute(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn gc_drops_stale_pending_with_manifests() {
        let Ok(url) = std::env::var("BAKEMONO_TEST_DB") else {
            eprintln!("skipping: BAKEMONO_TEST_DB not set");
            return;
        };
        let pool = match connect(&url).await {
            Ok(pool) => pool,
            Err(e) => {
                eprintln!("skipping: cannot reach test db: {e}");
                return;
            }
        };
        let stale = Keys::generate();
        let fresh = Keys::generate();
        let creator_id = format!("gc-{}", std::process::id());
        let mut manifest = sample(&creator_id);
        manifest.file_hash =
            format!("{:0<64}", creator_id.replace(|c: char| !c.is_ascii_hexdigit(), ""));

        upsert(&pool, &manifest.to_event(&stale).unwrap(), &manifest)
            .await
            .unwrap();
        upsert(&pool, &manifest.to_event(&fresh).unwrap(), &manifest)
            .await
            .unwrap();

        // backdate one pending key so only it falls outside the ttl window
        sqlx::query("UPDATE pubkeys SET first_seen = 0 WHERE pubkey = $1")
            .bind(stale.public_key().to_hex())
            .execute(&pool)
            .await
            .unwrap();

        gc_pending(&pool, 24 * 3_600).await.unwrap();

        assert_eq!(
            count_manifests(&pool, &stale.public_key().to_hex()).await,
            0,
            "a stale pending key and its manifests are collected"
        );
        assert_eq!(
            count_manifests(&pool, &fresh.public_key().to_hex()).await,
            1,
            "a recently-seen pending key survives gc"
        );

        sqlx::query("DELETE FROM manifests WHERE creator_id = $1")
            .bind(&creator_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM pubkeys WHERE pubkey = $1")
            .bind(fresh.public_key().to_hex())
            .execute(&pool)
            .await
            .unwrap();
    }

    async fn count_manifests(pool: &PgPool, pubkey: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM manifests WHERE pubkey = $1")
            .bind(pubkey)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    fn sample(creator_id: &str) -> Manifest {
        Manifest {
            platform: "patreon".into(),
            creator: "Tester".into(),
            creator_id: creator_id.to_string(),
            post_id: "1".into(),
            file_index: 0,
            file_hash: "a".repeat(64),
            size: 123,
            mime: "image/jpeg".into(),
            magnet: "magnet:?xt=urn:btih:abc".into(),
            post_title: Some("hi".into()),
            content: "body".into(),
            ..Default::default()
        }
    }
}
