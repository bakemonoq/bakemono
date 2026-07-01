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

// optional narrowing of the seed feed so a keeper can subscribe to just one creator/post/contributor
#[derive(Default)]
pub struct FeedScope {
    pub platform: Option<String>,
    pub creator_id: Option<String>,
    pub post_id: Option<String>,
    pub pubkey: Option<String>,
}

// seed feed: one row per distinct torrent (infohash), newest first, `before` is a created_at cursor so a
// seedbox can page back through the whole catalog instead of only catching the newest window
pub async fn feed(
    pool: &PgPool,
    limit: i64,
    before: Option<i64>,
    scope: &FeedScope,
) -> Result<Vec<ManifestRow>> {
    let rows = sqlx::query_as::<_, ManifestRow>(
        "SELECT * FROM (
             SELECT DISTINCT ON (infohash) * FROM visible_manifests
             WHERE infohash IS NOT NULL
               AND ($2::bigint IS NULL OR created_at < $2)
               AND ($3::text IS NULL OR platform = $3)
               AND ($4::text IS NULL OR creator_id = $4)
               AND ($5::text IS NULL OR post_id = $5)
               AND ($6::text IS NULL OR pubkey = $6)
             ORDER BY infohash, created_at DESC
         ) t ORDER BY created_at DESC, event_id DESC LIMIT $1",
    )
    .bind(limit)
    .bind(before)
    .bind(&scope.platform)
    .bind(&scope.creator_id)
    .bind(&scope.post_id)
    .bind(&scope.pubkey)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// distinct torrents the health prober is responsible for, so it can size each batch to cover them all
// within the recheck window instead of a fixed guess
pub async fn health_catalog_size(pool: &PgPool) -> Result<i64> {
    let n = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(DISTINCT infohash) FROM visible_manifests WHERE infohash IS NOT NULL",
    )
    .fetch_one(pool)
    .await?;
    Ok(n)
}

// visible torrents whose seeder count is unknown or gone stale, least-recently-checked first, so the
// health prober keeps every torrent's count fresh without re-scraping the whole catalog each pass
pub async fn health_batch(pool: &PgPool, limit: i64, recheck_after: i64) -> Result<Vec<String>> {
    let rows = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT m.infohash
         FROM visible_manifests m
         LEFT JOIN torrent_health h ON h.infohash = m.infohash
         WHERE m.infohash IS NOT NULL
           AND (h.checked_at IS NULL OR h.checked_at < EXTRACT(EPOCH FROM now())::bigint - $2)
         ORDER BY m.infohash
         LIMIT $1",
    )
    .bind(limit)
    .bind(recheck_after)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn record_health(pool: &PgPool, infohash: &str, seeders: i32) -> Result<()> {
    sqlx::query(
        "INSERT INTO torrent_health (infohash, seeders, checked_at)
         VALUES ($1, $2, EXTRACT(EPOCH FROM now())::bigint)
         ON CONFLICT (infohash) DO UPDATE SET seeders = EXCLUDED.seeders, checked_at = EXCLUDED.checked_at",
    )
    .bind(infohash)
    .bind(seeders)
    .execute(pool)
    .await?;
    Ok(())
}

// probed torrents ordered by fewest seeders first: the keeper work list. only rows with a known count,
// so a not-yet-probed catalog shows an empty list rather than a misleading one
pub async fn endangered(pool: &PgPool, limit: i64) -> Result<Vec<EndangeredRow>> {
    let rows = sqlx::query_as::<_, EndangeredRow>(
        "SELECT * FROM (
             SELECT DISTINCT ON (m.infohash)
                    m.platform, m.creator_id, m.post_id, m.creator, m.post_title, m.filename,
                    m.magnet, m.infohash, m.event_id, m.created_at, m.size, h.seeders
             FROM visible_manifests m
             JOIN torrent_health h ON h.infohash = m.infohash
             ORDER BY m.infohash, m.created_at DESC
         ) t ORDER BY seeders ASC, created_at DESC LIMIT $1",
    )
    .bind(limit)
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
        "SELECT DISTINCT platform FROM visible_manifests ORDER BY platform",
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
             FROM visible_manifests
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
             FROM visible_manifests
             WHERE ($1 = '' OR creator ILIKE '%' || $1 || '%')
               AND ($4 = '' OR platform = $4)
             GROUP BY platform, creator_id
         ) c
         LEFT JOIN (
             SELECT platform, creator_id, SUM(views) AS views FROM post_views GROUP BY platform, creator_id
         ) v USING (platform, creator_id)
         LEFT JOIN LATERAL (
             SELECT thumb, mime FROM visible_manifests vm
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
             FROM visible_manifests
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
                    LEAD(post_id) OVER w AS prev_id, LEAD(t) OVER w AS prev_title,
                    LAG(post_id)  OVER w AS next_id, LAG(t)  OVER w AS next_title
             FROM (
                 SELECT post_id, MAX(post_title) AS t, MAX(posted_at) AS pa, MAX(created_at) AS ca
                 FROM visible_manifests
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
        "SELECT platform, creator_id, post_id FROM visible_manifests ORDER BY random() LIMIT 1",
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

pub async fn pending_pubkeys(pool: &PgPool, limit: i64) -> Result<Vec<PendingRow>> {
    let rows = sqlx::query_as::<_, PendingRow>(
        "SELECT p.pubkey, COUNT(m.event_id) AS files,
                MAX(m.creator) AS creator, MAX(m.post_title) AS sample,
                (SELECT thumb FROM manifests mm
                 WHERE mm.pubkey = p.pubkey AND mm.thumb IS NOT NULL
                 ORDER BY mm.created_at DESC LIMIT 1) AS thumb
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

const MAX_OPEN_REPORTS: i64 = 5_000;

// tally a report: an existing (post, reason) row always increments and re-opens; a new row is created
// only while the open backlog is under the cap, so a spread-out flood cannot grow the table without bound
pub async fn record_report(
    pool: &PgPool,
    platform: &str,
    creator_id: &str,
    post_id: &str,
    reason: &str,
) -> Result<()> {
    let updated = sqlx::query(
        "UPDATE reports
         SET count = count + 1, last_seen = EXTRACT(EPOCH FROM now())::bigint, status = 'open'
         WHERE platform = $1 AND creator_id = $2 AND post_id = $3 AND reason = $4",
    )
    .bind(platform)
    .bind(creator_id)
    .bind(post_id)
    .bind(reason)
    .execute(pool)
    .await?;
    if updated.rows_affected() == 0 {
        sqlx::query(
            "INSERT INTO reports (platform, creator_id, post_id, reason, count, first_seen, last_seen, status)
             SELECT $1, $2, $3, $4, 1, EXTRACT(EPOCH FROM now())::bigint, EXTRACT(EPOCH FROM now())::bigint, 'open'
             WHERE (SELECT COUNT(*) FROM reports WHERE status = 'open') < $5
             ON CONFLICT (platform, creator_id, post_id, reason) DO NOTHING",
        )
        .bind(platform)
        .bind(creator_id)
        .bind(post_id)
        .bind(reason)
        .bind(MAX_OPEN_REPORTS)
        .execute(pool)
        .await?;
    }
    Ok(())
}

pub async fn post_is_visible(
    pool: &PgPool,
    platform: &str,
    creator_id: &str,
    post_id: &str,
) -> Result<bool> {
    let row: (bool,) = sqlx::query_as(
        "SELECT EXISTS(
             SELECT 1 FROM visible_manifests WHERE platform = $1 AND creator_id = $2 AND post_id = $3
         )",
    )
    .bind(platform)
    .bind(creator_id)
    .bind(post_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

// open reports collapsed to one row per post: severity (csam) first, then most-reported
pub async fn open_reports(pool: &PgPool, limit: i64) -> Result<Vec<ReportGroup>> {
    let rows = sqlx::query_as::<_, ReportGroup>(
        "SELECT r.platform, r.creator_id, r.post_id,
                SUM(r.count)::bigint AS total,
                string_agg(r.reason || ' x' || r.count::text, ' - ' ORDER BY r.count DESC) AS reasons,
                BOOL_OR(r.reason = 'csam') AS has_csam,
                COALESCE(MAX(m.creator), '') AS creator,
                MAX(m.post_title) AS post_title
         FROM reports r
         LEFT JOIN LATERAL (
             SELECT creator, post_title FROM manifests
             WHERE platform = r.platform AND creator_id = r.creator_id AND post_id = r.post_id
             LIMIT 1
         ) m ON true
         WHERE r.status = 'open'
         GROUP BY r.platform, r.creator_id, r.post_id
         ORDER BY BOOL_OR(r.reason = 'csam') DESC, SUM(r.count) DESC, MAX(r.last_seen) DESC
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn resolve_report(
    pool: &PgPool,
    platform: &str,
    creator_id: &str,
    post_id: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE reports SET status = 'dismissed'
         WHERE platform = $1 AND creator_id = $2 AND post_id = $3",
    )
    .bind(platform)
    .bind(creator_id)
    .bind(post_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn open_report_count(pool: &PgPool) -> Result<i64> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM (
             SELECT 1 FROM reports WHERE status = 'open'
             GROUP BY platform, creator_id, post_id
         ) t",
    )
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

#[derive(sqlx::FromRow)]
pub struct ReportGroup {
    pub platform: String,
    pub creator_id: String,
    pub post_id: String,
    pub total: i64,
    pub reasons: Option<String>,
    pub has_csam: bool,
    pub creator: String,
    pub post_title: Option<String>,
}

#[derive(sqlx::FromRow)]
pub struct PendingRow {
    pub pubkey: String,
    pub files: i64,
    pub creator: Option<String>,
    pub sample: Option<String>,
    pub thumb: Option<String>,
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
pub struct EndangeredRow {
    pub platform: String,
    pub creator_id: String,
    pub post_id: String,
    pub creator: String,
    pub post_title: Option<String>,
    pub filename: Option<String>,
    pub magnet: String,
    pub infohash: Option<String>,
    pub event_id: String,
    pub created_at: i64,
    pub size: i64,
    pub seeders: Option<i32>,
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

CREATE TABLE IF NOT EXISTS torrent_health (
    infohash   TEXT PRIMARY KEY,
    seeders    INTEGER NOT NULL,
    checked_at BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS torrent_health_checked ON torrent_health (checked_at);

-- view tally per post, the signal behind the Popular sort; deduped per browser so a refresh does not inflate it
CREATE TABLE IF NOT EXISTS post_views (
    platform   TEXT NOT NULL,
    creator_id TEXT NOT NULL,
    post_id    TEXT NOT NULL,
    views      BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (platform, creator_id, post_id)
);

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

-- user reports of a post, aggregated one row per (post, reason); no free text, so nothing here is
-- attacker-controlled prose the mod panel has to render
CREATE TABLE IF NOT EXISTS reports (
    platform   TEXT NOT NULL,
    creator_id TEXT NOT NULL,
    post_id    TEXT NOT NULL,
    reason     TEXT NOT NULL,
    count      BIGINT NOT NULL DEFAULT 0,
    first_seen BIGINT NOT NULL,
    last_seen  BIGINT NOT NULL,
    status     TEXT NOT NULL DEFAULT 'open',
    PRIMARY KEY (platform, creator_id, post_id, reason)
);
CREATE INDEX IF NOT EXISTS reports_status ON reports (status);

-- the single definition of what the public UI shows: approved pubkeys, minus anything a takedown hides
CREATE OR REPLACE VIEW visible_manifests AS
SELECT m.* FROM manifests m
WHERE m.pubkey IN (SELECT pubkey FROM pubkeys WHERE status = 'approved')
  AND NOT EXISTS (
      SELECT 1 FROM takedowns t WHERE
          (t.target_type = 'e' AND t.target = m.event_id) OR
          (t.target_type = 'x' AND t.target = m.file_hash) OR
          (t.target_type = 'p' AND t.target = m.pubkey) OR
          (t.target_type = 'post' AND t.target = m.platform || ':' || m.creator_id || ':' || m.post_id) OR
          (t.target_type = 'creator' AND t.target = m.platform || ':' || m.creator_id)
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

    #[tokio::test]
    async fn reports_aggregate_and_resolve() {
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
        let creator_id = format!("rep-{}", std::process::id());
        let manifest = sample(&creator_id);
        let event = manifest.to_event_at(&keys, 1_000).unwrap();
        upsert(&pool, &event, &manifest).await.unwrap();
        approve_pubkey(&pool, &keys.public_key().to_hex())
            .await
            .unwrap();

        let (p, c, post) = (
            manifest.platform.as_str(),
            creator_id.as_str(),
            manifest.post_id.as_str(),
        );
        assert!(post_is_visible(&pool, p, c, post).await.unwrap());
        assert!(!post_is_visible(&pool, p, c, "nope").await.unwrap());

        record_report(&pool, p, c, post, "spam").await.unwrap();
        record_report(&pool, p, c, post, "spam").await.unwrap();
        record_report(&pool, p, c, post, "csam").await.unwrap();

        let open = open_reports(&pool, 100).await.unwrap();
        let group = open
            .iter()
            .find(|r| r.post_id == post && r.creator_id == c)
            .expect("reported post present");
        assert_eq!(group.total, 3);
        assert!(group.has_csam);
        let reasons = group.reasons.clone().unwrap_or_default();
        assert!(reasons.contains("spam x2"), "reasons: {reasons}");
        assert!(reasons.contains("csam x1"), "reasons: {reasons}");
        assert_eq!(group.creator, manifest.creator);
        assert!(open_report_count(&pool).await.unwrap() >= 1);

        resolve_report(&pool, p, c, post).await.unwrap();
        assert!(open_reports(&pool, 100)
            .await
            .unwrap()
            .iter()
            .all(|r| !(r.post_id == post && r.creator_id == c)));

        sqlx::query("DELETE FROM reports WHERE creator_id = $1")
            .bind(&creator_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM manifests WHERE creator_id = $1")
            .bind(&creator_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM pubkeys WHERE pubkey = $1")
            .bind(keys.public_key().to_hex())
            .execute(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn post_and_creator_takedowns_hide_via_view() {
        use bakemono_core::Target;
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
        let creator_id = format!("ban-{}", std::process::id());
        let manifest = sample(&creator_id);
        let event = manifest.to_event_at(&keys, 1_000).unwrap();
        upsert(&pool, &event, &manifest).await.unwrap();
        approve_pubkey(&pool, &keys.public_key().to_hex())
            .await
            .unwrap();
        let (p, c, post) = (
            manifest.platform.as_str(),
            creator_id.as_str(),
            manifest.post_id.as_str(),
        );
        assert!(post_is_visible(&pool, p, c, post).await.unwrap());

        for target in [Target::post(p, c, post), Target::creator(p, c)] {
            let td = Takedown {
                target,
                reason: "moderator".into(),
                applied_at: None,
                explanation: String::new(),
            };
            record_takedown(&pool, &td, "local", None).await.unwrap();
            assert!(!post_is_visible(&pool, p, c, post).await.unwrap());
            remove_takedown(&pool, &td.d_tag()).await.unwrap();
            assert!(post_is_visible(&pool, p, c, post).await.unwrap());
        }

        sqlx::query("DELETE FROM manifests WHERE creator_id = $1")
            .bind(&creator_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM pubkeys WHERE pubkey = $1")
            .bind(keys.public_key().to_hex())
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
