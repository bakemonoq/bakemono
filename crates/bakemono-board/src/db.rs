use anyhow::Result;
use sqlx::postgres::{PgPool, PgPoolOptions};

pub async fn connect(url: &str) -> Result<PgPool> {
    let pool = PgPoolOptions::new().max_connections(5).connect(url).await?;
    sqlx::raw_sql(SCHEMA).execute(&pool).await?;
    Ok(pool)
}

pub async fn post_files(
    pool: &PgPool,
    platform: &str,
    creator_id: &str,
    post_id: &str,
) -> Result<Vec<ManifestRow>> {
    let rows = sqlx::query_as::<_, ManifestRow>(
        "SELECT * FROM visible_content
         WHERE platform = $1 AND creator_id = $2 AND post_id = $3
         ORDER BY file_index",
    )
    .bind(platform)
    .bind(creator_id)
    .bind(post_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// how a browse grid is ordered. the field enum and desc flag both come from a fixed set, never raw user
// input, so the ORDER BY they build is safe to splice into the query
#[derive(Clone, Copy, PartialEq)]
pub enum SortField {
    Views,
    Created,
    Name,
    Service,
}

impl SortField {
    pub fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some("views") => SortField::Views,
            Some("name") => SortField::Name,
            Some("service") => SortField::Service,
            _ => SortField::Created,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            SortField::Views => "views",
            SortField::Created => "created",
            SortField::Name => "name",
            SortField::Service => "service",
        }
    }
    fn post_order(self, desc: bool) -> String {
        let d = if desc { "DESC" } else { "ASC" };
        match self {
            SortField::Views => format!("views {d}, posted_at DESC NULLS LAST, created_at DESC"),
            SortField::Created => format!("posted_at {d} NULLS LAST, created_at {d}"),
            SortField::Name => format!("lower(post_title) {d} NULLS LAST, created_at DESC"),
            SortField::Service => format!("platform {d}, posted_at DESC NULLS LAST, created_at DESC"),
        }
    }
    fn creator_order(self, desc: bool) -> String {
        let d = if desc { "DESC" } else { "ASC" };
        match self {
            SortField::Views => format!("views {d}, last_at DESC"),
            SortField::Created => format!("last_at {d}"),
            SortField::Name => format!("lower(creator) {d}"),
            SortField::Service => format!("platform {d}, lower(creator) ASC"),
        }
    }
}

// the services present in the catalog, to populate the source filter
pub async fn platforms(pool: &PgPool) -> Result<Vec<String>> {
    let rows = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT platform FROM visible_content ORDER BY platform",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// one card per post; source filters by platform ("" = all). an extra row is fetched so the caller can tell
// there is a next page without a count
pub async fn list_posts(
    pool: &PgPool,
    source: &str,
    sort: SortField,
    desc: bool,
    q: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<PostCard>> {
    let sql = format!(
        "SELECT t.*, COALESCE(pv.views, 0) AS views FROM (
             SELECT DISTINCT ON (platform, creator_id, post_id)
                    platform, creator_id, post_id, creator, post_title, posted_at, created_at,
                    mime, thumb,
                    COUNT(*) OVER (PARTITION BY platform, creator_id, post_id) AS files
             FROM visible_content
             WHERE ($1 = '' OR post_title ILIKE '%' || $1 || '%' OR creator ILIKE '%' || $1 || '%')
               AND ($4 = '' OR platform = $4)
             ORDER BY platform, creator_id, post_id, (thumb IS NULL), file_index
         ) t
         LEFT JOIN post_views pv USING (platform, creator_id, post_id)
         ORDER BY {} LIMIT $2 OFFSET $3",
        sort.post_order(desc)
    );
    let rows = sqlx::query_as::<_, PostCard>(&sql)
        .bind(q)
        .bind(limit)
        .bind(offset)
        .bind(source)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

// one card per creator; views is the summed views over their posts so every sort field works on both tabs
pub async fn list_creators(
    pool: &PgPool,
    source: &str,
    sort: SortField,
    desc: bool,
    q: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<CreatorCard>> {
    let sql = format!(
        "SELECT c.platform, c.creator_id, c.creator, c.posts, c.files, COALESCE(v.views, 0)::bigint AS views,
                cov.thumb, cov.mime
         FROM (
             SELECT platform, creator_id, MAX(creator) AS creator,
                    COUNT(DISTINCT post_id) AS posts, COUNT(DISTINCT file_hash) AS files,
                    MAX(created_at) AS last_at
             FROM visible_content
             WHERE ($1 = '' OR creator ILIKE '%' || $1 || '%')
               AND ($4 = '' OR platform = $4)
             GROUP BY platform, creator_id
         ) c
         LEFT JOIN (
             SELECT platform, creator_id, SUM(views) AS views FROM post_views GROUP BY platform, creator_id
         ) v USING (platform, creator_id)
         LEFT JOIN LATERAL (
             SELECT thumb, mime FROM visible_content vm
             WHERE vm.platform = c.platform AND vm.creator_id = c.creator_id
             ORDER BY (thumb IS NULL), created_at DESC LIMIT 1
         ) cov ON true
         ORDER BY {} LIMIT $2 OFFSET $3",
        sort.creator_order(desc)
    );
    let rows = sqlx::query_as::<_, CreatorCard>(&sql)
        .bind(q)
        .bind(limit)
        .bind(offset)
        .bind(source)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

// one creator's posts as grid cards, newest first
pub async fn creator_posts(
    pool: &PgPool,
    platform: &str,
    creator_id: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<PostCard>> {
    let rows = sqlx::query_as::<_, PostCard>(
        "SELECT t.*, COALESCE(pv.views, 0) AS views FROM (
             SELECT DISTINCT ON (platform, creator_id, post_id)
                    platform, creator_id, post_id, creator, post_title, posted_at, created_at,
                    mime, thumb, infohash,
                    COUNT(*) OVER (PARTITION BY platform, creator_id, post_id) AS files
             FROM visible_content
             WHERE platform = $1 AND creator_id = $2
             ORDER BY platform, creator_id, post_id, (thumb IS NULL), file_index
         ) t
         LEFT JOIN post_views pv USING (platform, creator_id, post_id)
         ORDER BY posted_at DESC NULLS LAST, created_at DESC LIMIT $3 OFFSET $4",
    )
    .bind(platform)
    .bind(creator_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// the older and newer post by the same creator, ordered by post date, for prev/next links on a post page.
// prev is older (further back in time), next is newer
pub async fn adjacent_posts(
    pool: &PgPool,
    platform: &str,
    creator_id: &str,
    post_id: &str,
) -> Result<Option<AdjacentRow>> {
    let row = sqlx::query_as::<_, AdjacentRow>(
        "SELECT prev_id, prev_title, next_id, next_title FROM (
             SELECT post_id,
                    -- window is newest-first, so the left/prev button steps to a newer post (LAG) and the
                    -- right/next button to an older one (LEAD)
                    LAG(post_id)  OVER w AS prev_id, LAG(t)  OVER w AS prev_title,
                    LEAD(post_id) OVER w AS next_id, LEAD(t) OVER w AS next_title
             FROM (
                 SELECT post_id, MAX(post_title) AS t, MAX(posted_at) AS pa, MAX(created_at) AS ca
                 FROM visible_content
                 WHERE platform = $1 AND creator_id = $2
                 GROUP BY post_id
             ) g
             WINDOW w AS (ORDER BY pa DESC NULLS LAST, ca DESC)
         ) r WHERE post_id = $3",
    )
    .bind(platform)
    .bind(creator_id)
    .bind(post_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn random_post(pool: &PgPool) -> Result<Option<(String, String, String)>> {
    let row = sqlx::query_as::<_, (String, String, String)>(
        "SELECT platform, creator_id, post_id FROM visible_content ORDER BY random() LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

// popularity signal for the Popular sort; deduped per browser by the caller so a refresh does not inflate it
pub async fn bump_views(pool: &PgPool, platform: &str, creator_id: &str, post_id: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO post_views (platform, creator_id, post_id, views) VALUES ($1, $2, $3, 1)
         ON CONFLICT (platform, creator_id, post_id) DO UPDATE SET views = post_views.views + 1",
    )
    .bind(platform)
    .bind(creator_id)
    .bind(post_id)
    .execute(pool)
    .await?;
    Ok(())
}

// a post reduced to one grid card: cover thumb + counts, no per-file rows
#[derive(sqlx::FromRow)]
pub struct PostCard {
    pub platform: String,
    pub creator_id: String,
    pub post_id: String,
    pub creator: String,
    pub post_title: Option<String>,
    pub posted_at: Option<String>,
    pub mime: String,
    pub thumb: Option<String>,
    pub files: i64,
    pub views: i64,
}

// prev/next post ids and titles for on-post navigation; prev is older, next is newer
#[derive(sqlx::FromRow)]
pub struct AdjacentRow {
    pub prev_id: Option<String>,
    pub prev_title: Option<String>,
    pub next_id: Option<String>,
    pub next_title: Option<String>,
}

// a creator reduced to one grid card: cover thumb from their newest previewable file + counts
#[derive(sqlx::FromRow)]
pub struct CreatorCard {
    pub platform: String,
    pub creator_id: String,
    pub creator: String,
    pub posts: i64,
    pub files: i64,
    pub views: i64,
    pub thumb: Option<String>,
    pub mime: Option<String>,
}

#[derive(sqlx::FromRow)]
#[allow(dead_code)]
pub struct ManifestRow {
    pub created_at: i64,
    pub size: i64,
    pub mime: String,
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
    pub cid: String,
}

// denylist beats catalog: a revoked cid must not serve even while a stale row still names it.
// returns the mime the board recorded at ingest, never what kubo guesses
pub async fn serveable_file(pool: &PgPool, cid: &str) -> Result<Option<String>> {
    let mime = sqlx::query_scalar(
        "SELECT mime FROM files
         WHERE cid = $1 AND NOT EXISTS (SELECT 1 FROM denylist d WHERE d.cid = $1)",
    )
    .bind(cid)
    .fetch_optional(pool)
    .await?;
    Ok(mime)
}

pub async fn insert_file(
    pool: &PgPool,
    cid: &str,
    sha256: &str,
    size: i64,
    mime: &str,
    filename: Option<&str>,
    thumb_cid: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO files (cid, sha256, size, mime, filename, thumb_cid) VALUES ($1,$2,$3,$4,$5,$6)
         ON CONFLICT (cid) DO UPDATE SET thumb_cid = COALESCE(files.thumb_cid, EXCLUDED.thumb_cid)",
    )
    .bind(cid)
    .bind(sha256)
    .bind(size)
    .bind(mime)
    .bind(filename)
    .bind(thumb_cid)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn upsert_creator(pool: &PgPool, platform: &str, creator_id: &str, name: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO creators (platform, creator_id, creator) VALUES ($1,$2,$3)
         ON CONFLICT (platform, creator_id) DO UPDATE SET creator = EXCLUDED.creator",
    )
    .bind(platform)
    .bind(creator_id)
    .bind(name)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn upsert_post(pool: &PgPool, meta: &crate::scrape::PostMeta) -> Result<()> {
    sqlx::query(
        "INSERT INTO posts (platform, creator_id, post_id, title, body, posted_at, tier)
         VALUES ($1,$2,$3,$4,$5,$6,$7)
         ON CONFLICT (platform, creator_id, post_id) DO UPDATE SET
             title = EXCLUDED.title,
             body = EXCLUDED.body,
             posted_at = EXCLUDED.posted_at,
             tier = EXCLUDED.tier",
    )
    .bind(&meta.platform)
    .bind(&meta.creator_id)
    .bind(&meta.post_id)
    .bind(&meta.title)
    .bind(&meta.body)
    .bind(&meta.posted_at)
    .bind(&meta.tier)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn upsert_post_file(pool: &PgPool, meta: &crate::scrape::PostMeta, cid: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO post_files (platform, creator_id, post_id, file_index, cid)
         VALUES ($1,$2,$3,$4,$5)
         ON CONFLICT (platform, creator_id, post_id, file_index) DO UPDATE SET cid = EXCLUDED.cid",
    )
    .bind(&meta.platform)
    .bind(&meta.creator_id)
    .bind(&meta.post_id)
    .bind(meta.file_index)
    .bind(cid)
    .execute(pool)
    .await?;
    Ok(())
}

// store a sealed contributor cookie and remember which creators it can reach. the plaintext token
// is never persisted; only the RSA-wrapped ciphertext is
pub async fn upsert_cookie(
    pool: &PgPool,
    platform: &str,
    fingerprint: &str,
    sealed: &crate::crypto::Sealed,
    allow_autoimport: bool,
    allow_debug: bool,
) -> Result<i64> {
    let id = sqlx::query_scalar(
        "INSERT INTO cookies (platform, fingerprint, wrapped_key, nonce, ciphertext,
                              allow_autoimport, allow_debug, status, last_ok_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,'live', now())
         ON CONFLICT (fingerprint) DO UPDATE SET
             wrapped_key = EXCLUDED.wrapped_key,
             nonce = EXCLUDED.nonce,
             ciphertext = EXCLUDED.ciphertext,
             allow_autoimport = EXCLUDED.allow_autoimport,
             allow_debug = EXCLUDED.allow_debug,
             status = 'live',
             last_ok_at = now(),
             last_error = NULL
         RETURNING id",
    )
    .bind(platform)
    .bind(fingerprint)
    .bind(&sealed.wrapped_key)
    .bind(&sealed.nonce)
    .bind(&sealed.ciphertext)
    .bind(allow_autoimport)
    .bind(allow_debug)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

// replace a cookie's creator set: mark everything inactive, then reactivate/insert what is current,
// so a creator the contributor unsubscribed from stops being scraped
pub async fn set_cookie_creators(
    pool: &PgPool,
    cookie_id: i64,
    platform: &str,
    creators: &[(String, String, String)],
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE cookie_creators SET active = FALSE WHERE cookie_id = $1")
        .bind(cookie_id)
        .execute(&mut *tx)
        .await?;
    for (id, name, url) in creators {
        sqlx::query(
            "INSERT INTO cookie_creators (cookie_id, platform, creator_id, creator, url, active, discovered_at)
             VALUES ($1,$2,$3,$4,$5,TRUE,now())
             ON CONFLICT (cookie_id, platform, creator_id) DO UPDATE SET
                 creator = EXCLUDED.creator, url = EXCLUDED.url, active = TRUE",
        )
        .bind(cookie_id)
        .bind(platform)
        .bind(id)
        .bind(name)
        .bind(url)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub struct SealedCookie {
    pub id: i64,
    pub platform: String,
    pub sealed: crate::crypto::Sealed,
}

// live cookies opted into auto-import, for a keyed round
pub async fn autoimport_cookies(pool: &PgPool) -> Result<Vec<SealedCookie>> {
    let rows: Vec<(i64, String, String, String, String)> = sqlx::query_as(
        "SELECT id, platform, wrapped_key, nonce, ciphertext FROM cookies
         WHERE status != 'dead' AND allow_autoimport ORDER BY last_ok_at NULLS FIRST",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, platform, wrapped_key, nonce, ciphertext)| SealedCookie {
            id,
            platform,
            sealed: crate::crypto::Sealed { wrapped_key, nonce, ciphertext },
        })
        .collect())
}


pub async fn mark_cookie(pool: &PgPool, cookie_id: i64, status: &str, error: Option<&str>) -> Result<()> {
    let ok_clause = if status == "live" { ", last_ok_at = now()" } else { "" };
    sqlx::query(&format!(
        "UPDATE cookies SET status = $2, last_checked_at = now(), last_error = $3{ok_clause} WHERE id = $1"
    ))
    .bind(cookie_id)
    .bind(status)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

// mod-page view: one row per cookie with its live-creator count, no secrets
pub async fn cookie_overview(pool: &PgPool) -> Result<Vec<(i64, String, String, i64, Option<String>, Option<String>)>> {
    let rows = sqlx::query_as(
        "SELECT c.id, c.platform, c.status,
                (SELECT COUNT(*) FROM cookie_creators cc WHERE cc.cookie_id = c.id AND cc.active),
                c.last_ok_at::text, c.last_error
         FROM cookies c ORDER BY c.created_at DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// sha256 rides along so a later re-scrape of the same bytes is refused before anything re-pins;
// the CID alone dies with the files row on restore, the byte hash does not
pub async fn deny_cid(pool: &PgPool, cid: &str, reason: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO denylist (cid, reason, sha256)
         VALUES ($1, $2, (SELECT sha256 FROM files WHERE cid = $1))
         ON CONFLICT (cid) DO NOTHING",
    )
    .bind(cid)
    .bind(reason)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn deny_restored(
    pool: &PgPool,
    cid: &str,
    reason: &str,
    sha256: Option<&str>,
    revoked_at: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO denylist (cid, reason, sha256, added_at) VALUES ($1,$2,$3,$4::timestamptz)
         ON CONFLICT (cid) DO NOTHING",
    )
    .bind(cid)
    .bind(reason)
    .bind(sha256)
    .bind(revoked_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn sha_denied(pool: &PgPool, sha256: &str) -> Result<Option<String>> {
    let reason = sqlx::query_scalar("SELECT reason FROM denylist WHERE sha256 = $1 LIMIT 1")
        .bind(sha256)
        .fetch_optional(pool)
        .await?;
    Ok(reason)
}

pub async fn thumb_of(pool: &PgPool, cid: &str) -> Result<Option<String>> {
    let thumb = sqlx::query_scalar("SELECT thumb_cid FROM files WHERE cid = $1")
        .bind(cid)
        .fetch_optional(pool)
        .await?;
    Ok(thumb.flatten())
}

pub struct HeadRow {
    pub version: i64,
    pub head_cid: String,
    pub root_cid: String,
}

pub async fn last_head(pool: &PgPool) -> Result<Option<HeadRow>> {
    let row: Option<(i64, String, String)> = sqlx::query_as(
        "SELECT version, head_cid, root_cid FROM manifest_heads ORDER BY version DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(version, head_cid, root_cid)| HeadRow { version, head_cid, root_cid }))
}

pub async fn record_head(
    pool: &PgPool,
    version: i64,
    head_cid: &str,
    root_cid: &str,
    head_json: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO manifest_heads (version, head_cid, root_cid, head_json) VALUES ($1,$2,$3,$4)",
    )
    .bind(version)
    .bind(head_cid)
    .bind(root_cid)
    .bind(head_json)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn latest_head_json(pool: &PgPool) -> Result<Option<String>> {
    let json = sqlx::query_scalar(
        "SELECT head_json FROM manifest_heads ORDER BY version DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(json)
}

// everything the manifest needs about one creator, denylisted content already excluded
pub async fn shard_rows(
    pool: &PgPool,
    platform: &str,
    creator_id: &str,
) -> Result<Vec<ShardRow>> {
    let rows: Vec<ShardRow> = sqlx::query_as(
        "SELECT p.post_id, p.title, p.body, p.posted_at, p.tier,
                pf.file_index, f.cid, f.sha256, f.size, f.mime, f.filename,
                CASE WHEN EXISTS (SELECT 1 FROM denylist d WHERE d.cid = f.thumb_cid)
                     THEN NULL ELSE f.thumb_cid END AS thumb_cid
         FROM posts p
         JOIN post_files pf ON (pf.platform, pf.creator_id, pf.post_id) = (p.platform, p.creator_id, p.post_id)
         JOIN files f ON f.cid = pf.cid
         WHERE p.platform = $1 AND p.creator_id = $2
           AND NOT EXISTS (SELECT 1 FROM denylist d WHERE d.cid = f.cid)
         ORDER BY p.posted_at DESC NULLS LAST, p.post_id DESC, pf.file_index",
    )
    .bind(platform)
    .bind(creator_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[derive(sqlx::FromRow)]
pub struct ShardRow {
    pub post_id: String,
    pub title: String,
    pub body: String,
    pub posted_at: Option<String>,
    pub tier: Option<String>,
    #[allow(dead_code)]
    pub file_index: i32,
    pub cid: String,
    pub sha256: String,
    pub size: i64,
    pub mime: String,
    pub filename: Option<String>,
    pub thumb_cid: Option<String>,
}

pub async fn all_creators(pool: &PgPool) -> Result<Vec<(String, String, String)>> {
    let rows = sqlx::query_as(
        "SELECT platform, creator_id, creator FROM creators ORDER BY platform, creator_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn revoked_entries(pool: &PgPool) -> Result<Vec<(String, Option<String>, String, String)>> {
    let rows = sqlx::query_as(
        "SELECT cid, sha256, reason, to_char(added_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')
         FROM denylist ORDER BY added_at, cid",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[derive(Default)]
pub struct Stats {
    pub posts: i64,
    pub creators: i64,
    pub files: i64,
    pub keepers_hint: i64,
}

pub async fn stats(pool: &PgPool) -> Result<Stats> {
    let (posts, creators, files, version): (i64, i64, i64, Option<i64>) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM posts),
                (SELECT COUNT(*) FROM creators),
                (SELECT COUNT(*) FROM files),
                (SELECT MAX(version) FROM manifest_heads)",
    )
    .fetch_one(pool)
    .await?;
    Ok(Stats { posts, creators, files, keepers_hint: version.unwrap_or(0) })
}

pub async fn denylist(pool: &PgPool) -> Result<Vec<(String, String, String)>> {
    let rows = sqlx::query_as(
        "SELECT cid, reason, added_at::text FROM denylist ORDER BY added_at DESC LIMIT 500",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn cids_for_post(
    pool: &PgPool,
    platform: &str,
    creator_id: &str,
    post_id: &str,
) -> Result<Vec<String>> {
    let rows = sqlx::query_scalar(
        "SELECT cid FROM post_files WHERE platform = $1 AND creator_id = $2 AND post_id = $3",
    )
    .bind(platform)
    .bind(creator_id)
    .bind(post_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn cids_for_creator(pool: &PgPool, platform: &str, creator_id: &str) -> Result<Vec<String>> {
    let rows = sqlx::query_scalar(
        "SELECT cid FROM post_files WHERE platform = $1 AND creator_id = $2",
    )
    .bind(platform)
    .bind(creator_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

const SCHEMA: &str = "
-- pre-launch reset: drop every table the retired nostr/bittorrent stack left behind
DROP VIEW IF EXISTS visible_content;
DROP VIEW IF EXISTS visible_manifests;
DROP TABLE IF EXISTS manifests CASCADE;
DROP TABLE IF EXISTS takedowns CASCADE;
DROP TABLE IF EXISTS torrent_health CASCADE;
DROP TABLE IF EXISTS verified_content CASCADE;
DROP TABLE IF EXISTS reports CASCADE;
DROP TABLE IF EXISTS pubkeys CASCADE;
DROP TABLE IF EXISTS authors CASCADE;
DROP TABLE IF EXISTS sources CASCADE;

CREATE TABLE IF NOT EXISTS files (
    cid       TEXT PRIMARY KEY,
    sha256    TEXT NOT NULL,
    size      BIGINT NOT NULL,
    mime      TEXT NOT NULL,
    filename  TEXT,
    thumb_cid TEXT,
    added_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS files_sha256 ON files (sha256);

CREATE TABLE IF NOT EXISTS denylist (
    cid      TEXT PRIMARY KEY,
    reason   TEXT NOT NULL,
    sha256   TEXT,
    added_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS denylist_sha256 ON denylist (sha256);

CREATE TABLE IF NOT EXISTS creators (
    platform   TEXT NOT NULL,
    creator_id TEXT NOT NULL,
    creator    TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (platform, creator_id)
);

CREATE TABLE IF NOT EXISTS posts (
    platform   TEXT NOT NULL,
    creator_id TEXT NOT NULL,
    post_id    TEXT NOT NULL,
    title      TEXT NOT NULL DEFAULT '',
    body       TEXT NOT NULL DEFAULT '',
    posted_at  TEXT,
    tier       TEXT,
    PRIMARY KEY (platform, creator_id, post_id)
);

CREATE TABLE IF NOT EXISTS post_files (
    platform   TEXT NOT NULL,
    creator_id TEXT NOT NULL,
    post_id    TEXT NOT NULL,
    file_index INTEGER NOT NULL,
    cid        TEXT NOT NULL,
    PRIMARY KEY (platform, creator_id, post_id, file_index)
);
CREATE INDEX IF NOT EXISTS post_files_cid ON post_files (cid);

-- contributor session cookies, stored only as RSA-wrapped ciphertext. the private key lives offline
-- and touches the box only during an import round, so a database dump reveals no usable tokens
CREATE TABLE IF NOT EXISTS cookies (
    id               BIGSERIAL PRIMARY KEY,
    platform         TEXT NOT NULL,
    fingerprint      TEXT NOT NULL UNIQUE,
    wrapped_key      TEXT NOT NULL,
    nonce            TEXT NOT NULL,
    ciphertext       TEXT NOT NULL,
    status           TEXT NOT NULL DEFAULT 'live',
    allow_autoimport BOOLEAN NOT NULL DEFAULT TRUE,
    allow_debug      BOOLEAN NOT NULL DEFAULT FALSE,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_checked_at  TIMESTAMPTZ,
    last_ok_at       TIMESTAMPTZ,
    last_error       TEXT
);

-- creators each cookie can reach, refreshed every round; inactive rows are unsubscribed creators
CREATE TABLE IF NOT EXISTS cookie_creators (
    cookie_id     BIGINT NOT NULL REFERENCES cookies(id) ON DELETE CASCADE,
    platform      TEXT NOT NULL,
    creator_id    TEXT NOT NULL,
    creator       TEXT NOT NULL DEFAULT '',
    url           TEXT NOT NULL,
    active        BOOLEAN NOT NULL DEFAULT TRUE,
    discovered_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (cookie_id, platform, creator_id)
);

-- every published manifest version; head_json is served verbatim at /head.json
CREATE TABLE IF NOT EXISTS manifest_heads (
    version      BIGINT PRIMARY KEY,
    head_cid     TEXT NOT NULL,
    root_cid     TEXT NOT NULL,
    head_json    TEXT NOT NULL,
    published_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- view tally per post, the signal behind the Popular sort; deduped per browser by the caller
CREATE TABLE IF NOT EXISTS post_views (
    platform   TEXT NOT NULL,
    creator_id TEXT NOT NULL,
    post_id    TEXT NOT NULL,
    views      BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (platform, creator_id, post_id)
);

-- the single surface the browse UI reads
CREATE OR REPLACE VIEW visible_content AS
SELECT EXTRACT(EPOCH FROM f.added_at)::bigint AS created_at,
       f.size, f.mime, f.filename,
       p.platform, c.creator, p.creator_id, p.post_id, pf.file_index,
       NULLIF(p.title, '') AS post_title, p.posted_at, p.tier, p.body AS content,
       CASE WHEN f.thumb_cid IS NOT NULL THEN '/f/' || f.thumb_cid END AS thumb,
       f.cid
FROM post_files pf
JOIN posts p ON (p.platform, p.creator_id, p.post_id) = (pf.platform, pf.creator_id, pf.post_id)
JOIN creators c ON (c.platform, c.creator_id) = (p.platform, p.creator_id)
JOIN files f ON f.cid = pf.cid
WHERE NOT EXISTS (SELECT 1 FROM denylist d WHERE d.cid = f.cid);
";
