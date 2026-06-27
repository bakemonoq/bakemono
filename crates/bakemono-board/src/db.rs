use anyhow::Result;
use sqlx::postgres::{PgPool, PgPoolOptions};

use bakemono_core::nostr::Event;
use bakemono_core::Manifest;

pub async fn connect(url: &str) -> Result<PgPool> {
    let pool = PgPoolOptions::new().max_connections(5).connect(url).await?;
    sqlx::raw_sql(SCHEMA).execute(&pool).await?;
    Ok(pool)
}

pub async fn upsert(pool: &PgPool, event: &Event, manifest: &Manifest) -> Result<()> {
    let created_at = event.created_at.as_secs() as i64;
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
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn creators(pool: &PgPool) -> Result<Vec<CreatorRow>> {
    let rows = sqlx::query_as::<_, CreatorRow>(
        "SELECT platform, creator_id, MAX(creator) AS creator,
                COUNT(DISTINCT post_id) AS posts, COUNT(*) AS files
         FROM manifests GROUP BY platform, creator_id ORDER BY creator",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn posts_by_creator(
    pool: &PgPool,
    platform: &str,
    creator_id: &str,
) -> Result<Vec<PostRow>> {
    let rows = sqlx::query_as::<_, PostRow>(
        "SELECT platform, creator_id, post_id, MAX(creator) AS creator,
                MAX(post_title) AS post_title, MAX(posted_at) AS posted_at, COUNT(*) AS files
         FROM manifests WHERE platform = $1 AND creator_id = $2
         GROUP BY platform, creator_id, post_id ORDER BY MAX(created_at) DESC",
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
    let rows = sqlx::query_as::<_, ManifestRow>(
        "SELECT * FROM manifests WHERE platform = $1 AND creator_id = $2 AND post_id = $3
         ORDER BY file_index",
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
        "SELECT * FROM manifests ORDER BY created_at DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
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
    content    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS manifests_creator ON manifests (platform, creator_id);
CREATE INDEX IF NOT EXISTS manifests_post ON manifests (platform, creator_id, post_id);
CREATE INDEX IF NOT EXISTS manifests_hash ON manifests (file_hash);
CREATE INDEX IF NOT EXISTS manifests_recent ON manifests (created_at DESC);
";

const INSERT: &str = "
INSERT INTO manifests (
    event_id, pubkey, created_at, d_tag, file_hash, size, mime, magnet,
    platform, creator, creator_id, post_id, file_index, filename, post_title, posted_at, tier, content
) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18)
ON CONFLICT (event_id) DO NOTHING
";

#[cfg(test)]
mod tests {
    use super::*;
    use bakemono_core::nostr::Keys;

    // set BAKEMONO_TEST_DB to a Postgres url to run, otherwise skipped
    #[tokio::test]
    async fn ingest_query_and_replace() {
        let Ok(url) = std::env::var("BAKEMONO_TEST_DB") else {
            eprintln!("skipping: BAKEMONO_TEST_DB not set");
            return;
        };
        let pool = connect(&url).await.unwrap();
        let keys = Keys::generate();
        let creator_id = format!("test-{}", std::process::id());
        let mut manifest = sample(&creator_id);

        let older = manifest.to_event_at(&keys, 1_000).unwrap();
        upsert(&pool, &older, &manifest).await.unwrap();
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
