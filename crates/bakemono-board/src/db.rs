use anyhow::Result;
use sqlx::postgres::{PgPool, PgPoolOptions};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub async fn connect(url: &str) -> Result<PgPool> {
    let pool = build_pool(url, 16).await?;
    sqlx::raw_sql(SCHEMA).execute(&pool).await?;
    Ok(pool)
}

// a separate small pool for the serve process's background tasks (card refresher, scrape scheduler, dims
// backfill), so their long-held connections stay off the web pool - a 20s+ REFRESH can no longer make a
// page load wait out the acquire timeout. schema is already ensured by connect
pub async fn background_pool(url: &str, max: u32) -> Result<PgPool> {
    build_pool(url, max).await
}

async fn build_pool(url: &str, max: u32) -> Result<PgPool> {
    Ok(PgPoolOptions::new().max_connections(max).connect(url).await?)
}

static CARDS_DIRTY: AtomicBool = AtomicBool::new(false);

// browse reads a materialized snapshot; an ingest only flips a dirty flag, and this task collapses any
// burst of inserts into one refresh per interval. so a scrape inserting rows every second still costs a
// single refresh per cycle, never one per row. runs in the serve process; one-off cli commands that
// mutate then exit refresh inline instead
pub async fn run_card_refresher(pool: PgPool) {
    let secs = std::env::var("BAKEMONO_CARD_REFRESH_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(180);
    let interval = Duration::from_secs(secs);
    loop {
        tokio::time::sleep(interval).await;
        if CARDS_DIRTY.swap(false, Ordering::Relaxed) {
            if let Err(e) = refresh_cards(&pool).await {
                CARDS_DIRTY.store(true, Ordering::Relaxed);
                tracing::warn!("card refresh failed: {e:#}");
            }
        }
    }
}

// concurrent so browse reads never block on the rebuild; the two statements run separately because
// refresh concurrently cannot sit in a transaction
pub async fn refresh_cards(pool: &PgPool) -> Result<()> {
    sqlx::query("REFRESH MATERIALIZED VIEW CONCURRENTLY post_cards").execute(pool).await?;
    sqlx::query("REFRESH MATERIALIZED VIEW CONCURRENTLY creator_cards").execute(pool).await?;
    Ok(())
}

pub fn mark_cards_dirty() {
    CARDS_DIRTY.store(true, Ordering::Relaxed);
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

// the services present in the catalog, to populate the source filter. read off creator_cards (one row per
// creator) rather than post_cards (one per post), since the distinct set is identical but far smaller to scan
pub async fn platforms(pool: &PgPool) -> Result<Vec<String>> {
    let rows = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT platform FROM creator_cards ORDER BY platform",
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
    tier: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<PostCard>> {
    let sql = format!(
        "SELECT platform, creator_id, post_id, pc.creator, pc.post_title, pc.posted_at,
                pc.mime, pc.thumb, pc.tier, pc.files, pc.created_at, COALESCE(pv.views, 0) AS views
         FROM post_cards pc
         LEFT JOIN post_views pv USING (platform, creator_id, post_id)
         WHERE ($1 = '' OR pc.post_title ILIKE '%' || $1 || '%' OR pc.creator ILIKE '%' || $1 || '%')
           AND ($4 = '' OR pc.platform = $4)
           AND ($5 = '' OR pc.tier = $5)
         ORDER BY {} LIMIT $2 OFFSET $3",
        sort.post_order(desc)
    );
    let rows = sqlx::query_as::<_, PostCard>(&sql)
        .bind(q)
        .bind(limit)
        .bind(offset)
        .bind(source)
        .bind(tier)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

// top posts by views logged in the trailing window; ranked on the daily buckets, display count stays the
// running total so the same post reads the same everywhere. inner join to post_cards drops revoked posts
pub async fn list_popular(pool: &PgPool, days: i64, limit: i64) -> Result<Vec<PostCard>> {
    let rows = sqlx::query_as::<_, PostCard>(
        "SELECT pc.platform, pc.creator_id, pc.post_id, pc.creator, pc.post_title, pc.posted_at,
                pc.mime, pc.thumb, pc.tier, pc.files, pc.created_at, COALESCE(pv.views, 0) AS views
         FROM (
             SELECT platform, creator_id, post_id, SUM(views) AS recent
             FROM post_views_daily
             WHERE day >= CURRENT_DATE - ($1::text || ' days')::interval
             GROUP BY platform, creator_id, post_id
         ) r
         JOIN post_cards pc USING (platform, creator_id, post_id)
         LEFT JOIN post_views pv USING (platform, creator_id, post_id)
         ORDER BY r.recent DESC, pc.posted_at DESC NULLS LAST, pc.created_at DESC
         LIMIT $2",
    )
    .bind(days)
    .bind(limit)
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
        "SELECT platform, creator_id, cc.creator, cc.posts, cc.files,
                COALESCE(v.views, 0)::bigint AS views, cc.thumb, cc.mime
         FROM creator_cards cc
         LEFT JOIN (
             SELECT platform, creator_id, SUM(views) AS views FROM post_views GROUP BY platform, creator_id
         ) v USING (platform, creator_id)
         WHERE ($1 = '' OR cc.creator ILIKE '%' || $1 || '%')
           AND ($4 = '' OR cc.platform = $4)
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
    tier: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<PostCard>> {
    let rows = sqlx::query_as::<_, PostCard>(
        "SELECT t.*, COALESCE(pv.views, 0) AS views FROM (
             SELECT DISTINCT ON (platform, creator_id, post_id)
                    platform, creator_id, post_id, creator, post_title, posted_at, created_at,
                    mime, thumb, tier,
                    COUNT(*) OVER (PARTITION BY platform, creator_id, post_id) AS files
             FROM visible_content
             WHERE platform = $1 AND creator_id = $2
               AND ($3 = '' OR tier = $3)
             ORDER BY platform, creator_id, post_id, (thumb IS NULL), file_index
         ) t
         LEFT JOIN post_views pv USING (platform, creator_id, post_id)
         ORDER BY posted_at DESC NULLS LAST, created_at DESC LIMIT $4 OFFSET $5",
    )
    .bind(platform)
    .bind(creator_id)
    .bind(tier)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn creator_post_count(pool: &PgPool, platform: &str, creator_id: &str, tier: &str) -> Result<i64> {
    let n = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT post_id) FROM visible_content
         WHERE platform = $1 AND creator_id = $2 AND ($3 = '' OR tier = $3)",
    )
    .bind(platform)
    .bind(creator_id)
    .bind(tier)
    .fetch_one(pool)
    .await?;
    Ok(n)
}

// how many distinct posts match a browse filter, for the "N posts" count next to the pager
pub async fn count_posts(pool: &PgPool, source: &str, q: &str, tier: &str) -> Result<i64> {
    let n = sqlx::query_scalar(
        "SELECT COUNT(*) FROM post_cards
         WHERE ($1 = '' OR post_title ILIKE '%' || $1 || '%' OR creator ILIKE '%' || $1 || '%')
           AND ($2 = '' OR platform = $2)
           AND ($3 = '' OR tier = $3)",
    )
    .bind(q)
    .bind(source)
    .bind(tier)
    .fetch_one(pool)
    .await?;
    Ok(n)
}

pub async fn count_creators(pool: &PgPool, source: &str, q: &str) -> Result<i64> {
    let n = sqlx::query_scalar(
        "SELECT COUNT(*) FROM creator_cards
         WHERE ($1 = '' OR creator ILIKE '%' || $1 || '%')
           AND ($2 = '' OR platform = $2)",
    )
    .bind(q)
    .bind(source)
    .fetch_one(pool)
    .await?;
    Ok(n)
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

// one crawlable URL for the sitemap: post_id is None for a creator row. lastmod is the newest file
// added_at in the group, as a unix epoch - a proper timestamp, unlike the free-form posted_at string
#[derive(sqlx::FromRow)]
pub struct SitemapRow {
    pub platform: String,
    pub creator_id: String,
    pub post_id: Option<String>,
    pub lastmod: i64,
}

pub async fn sitemap_post_count(pool: &PgPool) -> Result<i64> {
    let n = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM (SELECT DISTINCT platform, creator_id, post_id FROM visible_content) t",
    )
    .fetch_one(pool)
    .await?;
    Ok(n)
}

// stable ordering (platform, creator_id, post_id) so a given offset maps to the same slice across the
// sitemap index and its chunk files even as rows are added
pub async fn sitemap_posts(pool: &PgPool, limit: i64, offset: i64) -> Result<Vec<SitemapRow>> {
    let rows = sqlx::query_as::<_, SitemapRow>(
        "SELECT platform, creator_id, post_id, MAX(created_at) AS lastmod
         FROM visible_content
         GROUP BY platform, creator_id, post_id
         ORDER BY platform, creator_id, post_id
         LIMIT $1 OFFSET $2",
    )
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn sitemap_creators(pool: &PgPool) -> Result<Vec<SitemapRow>> {
    let rows = sqlx::query_as::<_, SitemapRow>(
        "SELECT platform, creator_id, NULL::text AS post_id, MAX(created_at) AS lastmod
         FROM visible_content
         GROUP BY platform, creator_id
         ORDER BY platform, creator_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// posts whose newest file landed at or after `since` (unix epoch), for an IndexNow ping after a publish
pub async fn posts_since(pool: &PgPool, since: i64, limit: i64) -> Result<Vec<(String, String, String)>> {
    let rows = sqlx::query_as::<_, (String, String, String)>(
        "SELECT platform, creator_id, post_id FROM visible_content
         GROUP BY platform, creator_id, post_id
         HAVING MAX(created_at) >= $1
         ORDER BY MAX(created_at) DESC
         LIMIT $2",
    )
    .bind(since)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// popularity signal for the Popular sort; deduped per browser by the caller so a refresh does not inflate it.
// the running total drives sort/display; the daily bucket feeds the trailing-window Popular Now section
pub async fn bump_views(pool: &PgPool, platform: &str, creator_id: &str, post_id: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO post_views (platform, creator_id, post_id, views) VALUES ($1, $2, $3, 1)
         ON CONFLICT (platform, creator_id, post_id) DO UPDATE SET views = post_views.views + 1",
    )
    .bind(platform)
    .bind(creator_id)
    .bind(post_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO post_views_daily (platform, creator_id, post_id, day, views)
         VALUES ($1, $2, $3, CURRENT_DATE, 1)
         ON CONFLICT (platform, creator_id, post_id, day) DO UPDATE SET views = post_views_daily.views + 1",
    )
    .bind(platform)
    .bind(creator_id)
    .bind(post_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
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
    pub tier: Option<String>,
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


pub async fn insert_file(
    pool: &PgPool,
    cid: &str,
    sha256: &str,
    size: i64,
    mime: &str,
    filename: Option<&str>,
    thumb_cid: Option<&str>,
    dims: Option<(i32, i32)>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO files (cid, sha256, size, mime, filename, thumb_cid, width, height)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
         ON CONFLICT (cid) DO UPDATE SET
             thumb_cid = COALESCE(files.thumb_cid, EXCLUDED.thumb_cid),
             width = COALESCE(files.width, EXCLUDED.width),
             height = COALESCE(files.height, EXCLUDED.height)",
    )
    .bind(cid)
    .bind(sha256)
    .bind(size)
    .bind(mime)
    .bind(filename)
    .bind(thumb_cid)
    .bind(dims.map(|d| d.0))
    .bind(dims.map(|d| d.1))
    .execute(pool)
    .await?;
    mark_cards_dirty();
    Ok(())
}

// files the booru facade surfaces but whose dimensions were never probed (pre-dating the column,
// or restored from a manifest that carries no byte facts)
pub async fn files_missing_dims(pool: &PgPool, limit: i64) -> Result<Vec<String>> {
    let rows = sqlx::query_scalar(
        "SELECT f.cid FROM files f
         WHERE f.width IS NULL AND (f.mime LIKE 'image/%' OR f.mime LIKE 'video/%')
           AND EXISTS (SELECT 1 FROM post_files pf WHERE pf.cid = f.cid)
           AND NOT EXISTS (SELECT 1 FROM denylist d WHERE d.cid = f.cid)
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn set_dims(pool: &PgPool, cid: &str, width: i32, height: i32) -> Result<()> {
    sqlx::query("UPDATE files SET width = $2, height = $3 WHERE cid = $1")
        .bind(cid)
        .bind(width)
        .bind(height)
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
    mark_cards_dirty();
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
    mark_cards_dirty();
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
    mark_cards_dirty();
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

#[derive(sqlx::FromRow)]
pub struct CookieRow {
    pub id: i64,
    pub platform: String,
    pub status: String,
    pub creators: i64,
    pub allow_autoimport: bool,
    pub allow_debug: bool,
    pub last_ok: Option<String>,
    pub last_checked: Option<String>,
    pub added: Option<String>,
    pub error: Option<String>,
}

// mod-page view: one row per contributor key with its live-creator count, never any secret material
pub async fn cookie_overview(pool: &PgPool) -> Result<Vec<CookieRow>> {
    let rows = sqlx::query_as(
        "SELECT c.id, c.platform, c.status,
                (SELECT COUNT(*) FROM cookie_creators cc WHERE cc.cookie_id = c.id AND cc.active) AS creators,
                c.allow_autoimport, c.allow_debug,
                c.last_ok_at::text AS last_ok, c.last_checked_at::text AS last_checked,
                c.created_at::text AS added, c.last_error AS error
         FROM cookies c ORDER BY c.created_at DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// every key's reachable creators for the mod-page expandable list; inactive rows are creators the
// contributor has since unsubscribed from
pub async fn cookie_creator_rows(pool: &PgPool) -> Result<Vec<(i64, String, String, String, bool)>> {
    let rows = sqlx::query_as(
        "SELECT cookie_id, creator_id, creator, url, active FROM cookie_creators
         ORDER BY cookie_id, active DESC, creator",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// drop a contributor key: deletes the sealed token outright (cookie_creators cascades), so the board
// keeps nothing for it and every future round skips it
pub async fn delete_cookie(pool: &PgPool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM cookies WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

// pause or resume a key without dropping it: an off cookie stays sealed but no round decrypts it
pub async fn set_cookie_autoimport(pool: &PgPool, id: i64, on: bool) -> Result<()> {
    sqlx::query("UPDATE cookies SET allow_autoimport = $2 WHERE id = $1")
        .bind(id)
        .bind(on)
        .execute(pool)
        .await?;
    Ok(())
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
    mark_cards_dirty();
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
    mark_cards_dirty();
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

// published_at of the newest head as a unix epoch; the IndexNow ping uses it to scope "new since last
// publish" before the fresh head lands
pub async fn last_head_published_epoch(pool: &PgPool) -> Result<Option<i64>> {
    let epoch = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT EXTRACT(EPOCH FROM published_at)::bigint FROM manifest_heads ORDER BY version DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(epoch.flatten())
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

#[derive(sqlx::FromRow)]
pub struct ShardDigest {
    pub platform: String,
    pub creator_id: String,
    pub creator: String,
    pub digest: String,
    pub posts: i64,
    pub bytes: i64,
}

// one grouped pass yields every creator's content digest plus the counts a ShardRef needs. the digest
// spans each field that lands in the shard, in the order shard_rows emits them, so it flips exactly when
// the shard's bytes would - which lets publish rebuild only the changed shards. creators with no visible
// file produce no row, matching publish skipping empty shards
pub async fn shard_digests(pool: &PgPool) -> Result<Vec<ShardDigest>> {
    let rows = sqlx::query_as(
        "SELECT p.platform, p.creator_id, MAX(c.creator) AS creator,
                md5(COALESCE(MAX(c.creator), '') || string_agg(
                    concat_ws(E'\\x1f',
                        p.post_id, p.title, p.body, COALESCE(p.posted_at, ''), COALESCE(p.tier, ''),
                        pf.file_index::text, f.cid, f.sha256, f.size::text, f.mime, COALESCE(f.filename, ''),
                        CASE WHEN f.thumb_cid IS NULL
                                  OR EXISTS (SELECT 1 FROM denylist dt WHERE dt.cid = f.thumb_cid)
                             THEN '' ELSE f.thumb_cid END),
                    E'\\x1e' ORDER BY p.posted_at DESC NULLS LAST, p.post_id DESC, pf.file_index)) AS digest,
                COUNT(DISTINCT p.post_id)::bigint AS posts,
                COALESCE(SUM(f.size), 0)::bigint AS bytes
         FROM posts p
         JOIN post_files pf ON (pf.platform, pf.creator_id, pf.post_id) = (p.platform, p.creator_id, p.post_id)
         JOIN creators c ON (c.platform, c.creator_id) = (p.platform, p.creator_id)
         JOIN files f ON f.cid = pf.cid
         WHERE NOT EXISTS (SELECT 1 FROM denylist d WHERE d.cid = f.cid)
         GROUP BY p.platform, p.creator_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// the shard state carried over from the last publish, keyed (platform, creator_id): (digest, cid, posts, bytes)
pub async fn stored_shards(pool: &PgPool) -> Result<Vec<(String, String, String, String, i64, i64)>> {
    let rows = sqlx::query_as(
        "SELECT platform, creator_id, digest, cid, posts, bytes FROM manifest_shards",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn upsert_shard(
    pool: &PgPool,
    platform: &str,
    creator_id: &str,
    digest: &str,
    cid: &str,
    posts: i64,
    bytes: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO manifest_shards (platform, creator_id, digest, cid, posts, bytes)
         VALUES ($1,$2,$3,$4,$5,$6)
         ON CONFLICT (platform, creator_id) DO UPDATE SET
             digest = EXCLUDED.digest, cid = EXCLUDED.cid,
             posts = EXCLUDED.posts, bytes = EXCLUDED.bytes",
    )
    .bind(platform)
    .bind(creator_id)
    .bind(digest)
    .bind(cid)
    .bind(posts)
    .bind(bytes)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_shard(pool: &PgPool, platform: &str, creator_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM manifest_shards WHERE platform = $1 AND creator_id = $2")
        .bind(platform)
        .bind(creator_id)
        .execute(pool)
        .await?;
    Ok(())
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
    -- pixel dimensions; NULL = not probed yet, 0 = probe failed (so the backfill never retries it)
    width     INTEGER,
    height    INTEGER,
    added_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS files_sha256 ON files (sha256);
ALTER TABLE files ADD COLUMN IF NOT EXISTS width INTEGER;
ALTER TABLE files ADD COLUMN IF NOT EXISTS height INTEGER;

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

-- id is the stable integer handle the booru facade exposes; ingest order keeps it roughly newest-last
CREATE TABLE IF NOT EXISTS post_files (
    id         BIGSERIAL,
    platform   TEXT NOT NULL,
    creator_id TEXT NOT NULL,
    post_id    TEXT NOT NULL,
    file_index INTEGER NOT NULL,
    cid        TEXT NOT NULL,
    PRIMARY KEY (platform, creator_id, post_id, file_index)
);
CREATE INDEX IF NOT EXISTS post_files_cid ON post_files (cid);
ALTER TABLE post_files ADD COLUMN IF NOT EXISTS id BIGSERIAL;
CREATE UNIQUE INDEX IF NOT EXISTS post_files_id ON post_files (id);

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

-- last published shard per creator: digest lets a publish reuse the stored CID for unchanged creators
-- instead of re-adding every shard, so the cost tracks what changed, not the whole catalog
CREATE TABLE IF NOT EXISTS manifest_shards (
    platform   TEXT NOT NULL,
    creator_id TEXT NOT NULL,
    digest     TEXT NOT NULL,
    cid        TEXT NOT NULL,
    posts      BIGINT NOT NULL,
    bytes      BIGINT NOT NULL,
    PRIMARY KEY (platform, creator_id)
);

-- view tally per post, the signal behind the Popular sort; deduped per browser by the caller
CREATE TABLE IF NOT EXISTS post_views (
    platform   TEXT NOT NULL,
    creator_id TEXT NOT NULL,
    post_id    TEXT NOT NULL,
    views      BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (platform, creator_id, post_id)
);

-- per-day view buckets so Popular Now can rank on a trailing window; the all-time total lives in post_views
CREATE TABLE IF NOT EXISTS post_views_daily (
    platform   TEXT NOT NULL,
    creator_id TEXT NOT NULL,
    post_id    TEXT NOT NULL,
    day        DATE NOT NULL,
    views      BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (platform, creator_id, post_id, day)
);
CREATE INDEX IF NOT EXISTS post_views_daily_day ON post_views_daily (day);

-- the single surface the browse UI reads
CREATE OR REPLACE VIEW visible_content AS
SELECT EXTRACT(EPOCH FROM f.added_at)::bigint AS created_at,
       f.size, f.mime, f.filename,
       p.platform, c.creator, p.creator_id, p.post_id, pf.file_index,
       NULLIF(p.title, '') AS post_title, p.posted_at, p.tier, p.body AS content,
       CASE WHEN f.thumb_cid IS NOT NULL THEN '/ipfs/' || f.thumb_cid END AS thumb,
       f.cid, pf.id AS file_id, f.sha256, f.thumb_cid, f.width, f.height
FROM post_files pf
JOIN posts p ON (p.platform, p.creator_id, p.post_id) = (pf.platform, pf.creator_id, pf.post_id)
JOIN creators c ON (c.platform, c.creator_id) = (p.platform, p.creator_id)
JOIN files f ON f.cid = pf.cid
WHERE NOT EXISTS (SELECT 1 FROM denylist d WHERE d.cid = f.cid);

-- browse read model: one indexable row per post / per creator, so a listing is an index range scan
-- instead of a full rescan of the file table. built on the base tables (not visible_content) so the
-- schema's DROP VIEW at the top has no dependency to trip over. refresh_cards rebuilds them after every
-- ingest round and takedown. views are excluded (they change on every page view) and joined at read time
CREATE MATERIALIZED VIEW IF NOT EXISTS post_cards AS
SELECT DISTINCT ON (pf.platform, pf.creator_id, pf.post_id)
       pf.platform, pf.creator_id, pf.post_id,
       c.creator,
       NULLIF(p.title, '') AS post_title,
       p.posted_at, p.tier,
       EXTRACT(EPOCH FROM f.added_at)::bigint AS created_at,
       f.mime,
       CASE WHEN f.thumb_cid IS NOT NULL THEN '/ipfs/' || f.thumb_cid END AS thumb,
       COUNT(*) OVER (PARTITION BY pf.platform, pf.creator_id, pf.post_id) AS files
FROM post_files pf
JOIN posts p ON (p.platform, p.creator_id, p.post_id) = (pf.platform, pf.creator_id, pf.post_id)
JOIN creators c ON (c.platform, c.creator_id) = (pf.platform, pf.creator_id)
JOIN files f ON f.cid = pf.cid
WHERE NOT EXISTS (SELECT 1 FROM denylist d WHERE d.cid = f.cid)
ORDER BY pf.platform, pf.creator_id, pf.post_id, (f.thumb_cid IS NULL), pf.file_index;
CREATE UNIQUE INDEX IF NOT EXISTS post_cards_pk ON post_cards (platform, creator_id, post_id);
CREATE INDEX IF NOT EXISTS post_cards_recent ON post_cards (posted_at DESC NULLS LAST, created_at DESC);
CREATE INDEX IF NOT EXISTS post_cards_platform ON post_cards (platform);
CREATE EXTENSION IF NOT EXISTS pg_trgm;
-- substring search for the browse/search ILIKE; two single-column gin indexes so title OR creator becomes a
-- bitmap-or of index scans instead of a full seq scan of the whole view
CREATE INDEX IF NOT EXISTS post_cards_title_trgm ON post_cards USING gin (post_title gin_trgm_ops);
CREATE INDEX IF NOT EXISTS post_cards_creator_trgm ON post_cards USING gin (creator gin_trgm_ops);

CREATE MATERIALIZED VIEW IF NOT EXISTS creator_cards AS
SELECT c.platform, c.creator_id, c.creator, c.posts, c.files, c.last_at, cov.thumb, cov.mime
FROM (
    SELECT pf.platform, pf.creator_id, MAX(cr.creator) AS creator,
           COUNT(DISTINCT pf.post_id) AS posts, COUNT(DISTINCT pf.cid) AS files,
           MAX(EXTRACT(EPOCH FROM f.added_at)::bigint) AS last_at
    FROM post_files pf
    JOIN posts p ON (p.platform, p.creator_id, p.post_id) = (pf.platform, pf.creator_id, pf.post_id)
    JOIN creators cr ON (cr.platform, cr.creator_id) = (pf.platform, pf.creator_id)
    JOIN files f ON f.cid = pf.cid
    WHERE NOT EXISTS (SELECT 1 FROM denylist d WHERE d.cid = f.cid)
    GROUP BY pf.platform, pf.creator_id
) c
LEFT JOIN LATERAL (
    SELECT CASE WHEN f.thumb_cid IS NOT NULL THEN '/ipfs/' || f.thumb_cid END AS thumb, f.mime
    FROM post_files pf
    JOIN posts p ON (p.platform, p.creator_id, p.post_id) = (pf.platform, pf.creator_id, pf.post_id)
    JOIN files f ON f.cid = pf.cid
    WHERE pf.platform = c.platform AND pf.creator_id = c.creator_id
      AND NOT EXISTS (SELECT 1 FROM denylist d WHERE d.cid = f.cid)
    -- newest post by content date, deterministic tiebreak, so a re-scrape never reshuffles the cover
    ORDER BY (f.thumb_cid IS NULL), p.posted_at DESC NULLS LAST, pf.post_id DESC, pf.file_index
    LIMIT 1
) cov ON true;
CREATE UNIQUE INDEX IF NOT EXISTS creator_cards_pk ON creator_cards (platform, creator_id);
CREATE INDEX IF NOT EXISTS creator_cards_recent ON creator_cards (last_at DESC);
CREATE INDEX IF NOT EXISTS creator_cards_creator_trgm ON creator_cards USING gin (creator gin_trgm_ops);
";
