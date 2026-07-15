use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use sqlx::postgres::PgPool;
use sqlx::{Postgres, QueryBuilder};

use crate::config;

// Gelbooru 0.2 dapi facade so stock booru clients can browse the board. There is no tag table:
// tags are derived per file (artist slug, platform, tier, media kind) and an unknown search token
// falls back to a title match, so title words search fine without polluting the tag list
pub fn routes() -> axum::Router<crate::web::AppState> {
    use axum::routing::get;
    axum::Router::new()
        .route("/index.php", get(dapi))
        .route("/autocomplete.php", get(autocomplete))
        // danbooru REST surface: clients like Anime Boxes probe /posts.json to validate a site, and
        // some prefix it with index.php. same query layer, danbooru-shaped output
        .route("/posts.json", get(danbooru_posts))
        .route("/index.php/posts.json", get(danbooru_posts))
        // matchit forbids a literal ".json" suffix beside a path param, so capture the whole segment
        // ("123.json") and strip it in the handler
        .route("/posts/{id}", get(danbooru_post))
        .route("/tags.json", get(danbooru_tags))
        .route("/index.php/tags.json", get(danbooru_tags))
        .layer(axum::middleware::from_fn(cors))
}

// third-party web explorers fetch this cross-origin; media itself is served by the gateway
async fn cors(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    res.headers_mut().insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    res
}

// clients differ on how they probe index.php: some hit it bare to check the site exists, some send
// page=post&s=list (old gallery), some the canonical page=dapi&s=post&q=index. dispatch on `s` alone
// and default to posts, so any of those validates instead of 404ing
async fn dapi(State(pool): State<PgPool>, headers: HeaderMap, Query(p): Query<Dapi>) -> Response {
    if p.page.as_deref() == Some("autocomplete2") {
        return complete(&pool, p.term.as_deref().unwrap_or_default()).await;
    }
    let json = p.json.as_deref() == Some("1");
    let result = match p.s.as_deref() {
        Some("tag") => tags(&pool, &p, json).await,
        Some("comment") => Ok(xml_response(format!("{XML_HEAD}<comments type=\"array\"/>\n"))),
        _ => posts(&pool, &headers, &p, json).await,
    };
    result.unwrap_or_else(|e| {
        tracing::warn!("booru query failed: {e:#}");
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    })
}

async fn posts(pool: &PgPool, headers: &HeaderMap, p: &Dapi, json: bool) -> anyhow::Result<Response> {
    let limit = p.limit.unwrap_or(100).clamp(1, 100);
    let offset = p.pid.unwrap_or(0).max(0) * limit;
    let Some(search) = Search::parse(p.tags.as_deref().unwrap_or(""), p.id) else {
        return Ok(render_posts(Vec::new(), 0, offset, limit, json));
    };
    let mut count = QueryBuilder::new("SELECT COUNT(*) FROM visible_content vc WHERE ");
    search.push_where(&mut count);
    let total: i64 = count.build_query_scalar().fetch_one(pool).await?;
    let rows = fetch_rows(pool, &search, limit, offset).await?;
    let base = base_url(headers);
    let out = rows.iter().map(|r| BooruPost::from_row(r, &base)).collect();
    Ok(render_posts(out, total, offset, limit, json))
}

// shared post query for both the gelbooru and danbooru surfaces; denylisted content is already
// absent from visible_content, so takedowns drop out of every api at once
async fn fetch_rows(pool: &PgPool, search: &Search, limit: i64, offset: i64) -> anyhow::Result<Vec<FileRow>> {
    let mut qb = QueryBuilder::new(
        "SELECT vc.file_id, vc.cid, vc.thumb_cid, vc.sha256, vc.mime, vc.filename,
                vc.width, vc.height, vc.size, vc.platform, vc.creator, vc.creator_id, vc.post_id,
                vc.posted_at, vc.tier, vc.created_at, COALESCE(pv.views, 0)::bigint AS views
         FROM visible_content vc
         LEFT JOIN post_views pv
             ON (pv.platform, pv.creator_id, pv.post_id) = (vc.platform, vc.creator_id, vc.post_id)
         WHERE ",
    );
    search.push_where(&mut qb);
    qb.push(" ORDER BY ").push(search.order_sql());
    qb.push(" LIMIT ").push_bind(limit).push(" OFFSET ").push_bind(offset);
    Ok(qb.build_query_as().fetch_all(pool).await?)
}

fn render_posts(posts: Vec<BooruPost>, count: i64, offset: i64, limit: i64, json: bool) -> Response {
    if json {
        // gelbooru wraps the array in @attributes + a `post` key; a bare array trips stricter clients
        return Json(PostsJson { attributes: Attributes { limit, offset, count }, post: posts })
            .into_response();
    }
    let mut xml = String::with_capacity(posts.len() * 700 + 128);
    xml.push_str(XML_HEAD);
    xml.push_str(&format!("<posts count=\"{count}\" offset=\"{offset}\">\n"));
    for p in &posts {
        p.write_xml(&mut xml);
    }
    xml.push_str("</posts>\n");
    xml_response(xml)
}

async fn tags(pool: &PgPool, p: &Dapi, json: bool) -> anyhow::Result<Response> {
    let limit = p.limit.unwrap_or(100).clamp(1, 1000);
    let pattern = p.name_pattern.clone().unwrap_or_default();
    let names = match (&p.name, &p.names) {
        (Some(n), _) => n.clone(),
        (None, Some(n)) => n.clone(),
        _ => String::new(),
    };
    let order = if p.orderby.as_deref() == Some("name") { "name" } else { "count DESC, name" };
    let sql = format!(
        "{TAG_UNION} WHERE count > 0
           AND ($1 = '' OR name ILIKE $1)
           AND ($2 = '' OR name = ANY(string_to_array($2, ' ')))
         ORDER BY {order} LIMIT $3"
    );
    let rows: Vec<(String, i32, i64)> =
        sqlx::query_as(&sql).bind(&pattern).bind(&names).bind(limit).fetch_all(pool).await?;
    if json {
        let tag: Vec<BooruTag> = rows
            .iter()
            .map(|(name, kind, count)| BooruTag {
                id: tag_id(name),
                name: name.clone(),
                count: *count,
                kind: *kind,
                ambiguous: 0,
            })
            .collect();
        let attributes = Attributes { limit, offset: 0, count: tag.len() as i64 };
        return Ok(Json(TagsJson { attributes, tag }).into_response());
    }
    let mut xml = String::from(XML_HEAD);
    xml.push_str("<tags type=\"array\">\n");
    for (name, kind, count) in &rows {
        xml.push_str(&format!(
            "<tag id=\"{}\" name=\"{}\" count=\"{count}\" type=\"{kind}\" ambiguous=\"false\"/>\n",
            tag_id(name),
            xesc(name)
        ));
    }
    xml.push_str("</tags>\n");
    Ok(xml_response(xml))
}

async fn autocomplete(State(pool): State<PgPool>, Query(p): Query<Ac>) -> Response {
    complete(&pool, p.q.as_deref().unwrap_or_default()).await
}

async fn complete(pool: &PgPool, term: &str) -> Response {
    let term = term.trim().to_lowercase();
    if term.is_empty() {
        return Json(Vec::<AcEntry>::new()).into_response();
    }
    let sql = format!(
        "{TAG_UNION} WHERE count > 0 AND name LIKE $1 || '%' ORDER BY count DESC, name LIMIT 15"
    );
    let rows: Vec<(String, i32, i64)> = match sqlx::query_as(&sql).bind(&term).fetch_all(pool).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("booru autocomplete failed: {e:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let out: Vec<AcEntry> = rows
        .into_iter()
        .map(|(name, kind, count)| AcEntry {
            kind: "tag",
            label: format!("{name} ({count})"),
            value: name,
            post_count: count.to_string(),
            category: category(kind),
        })
        .collect();
    Json(out).into_response()
}

// danbooru v2 REST surface: clients like Anime Boxes take the site url as-is and append /posts.json
// to validate and browse. bare json array, danbooru field names and single-letter rating
async fn danbooru_posts(State(pool): State<PgPool>, headers: HeaderMap, Query(q): Query<DanbooruQuery>) -> Response {
    let limit = q.limit.unwrap_or(20).clamp(1, 200);
    let (offset, before, after) = q.paginate(limit);
    let Some(mut search) = Search::parse(q.tags.as_deref().unwrap_or(""), None) else {
        return Json(Vec::<DanbooruPost>::new()).into_response();
    };
    search.before = before;
    search.after = after;
    match fetch_rows(&pool, &search, limit, offset).await {
        Ok(rows) => {
            let base = base_url(&headers);
            let out: Vec<DanbooruPost> = rows.iter().map(|r| DanbooruPost::from_row(r, &base)).collect();
            Json(out).into_response()
        }
        Err(e) => {
            tracing::warn!("danbooru posts failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn danbooru_post(State(pool): State<PgPool>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    let Ok(file_id) = id.trim_end_matches(".json").parse::<i64>() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(mut search) = Search::parse("", None) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    search.id = Some(file_id);
    match fetch_rows(&pool, &search, 1, 0).await {
        Ok(rows) => match rows.first() {
            Some(r) => Json(DanbooruPost::from_row(r, &base_url(&headers))).into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        },
        Err(e) => {
            tracing::warn!("danbooru post failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn danbooru_tags(State(pool): State<PgPool>, Query(q): Query<DanbooruTagQuery>) -> Response {
    let limit = q.limit.unwrap_or(50).clamp(1, 1000);
    let pattern = q.name_matches.as_deref().unwrap_or("").replace('*', "%");
    let order = if q.order.as_deref() == Some("name") { "name" } else { "count DESC, name" };
    let sql = format!(
        "{TAG_UNION} WHERE count > 0 AND ($1 = '' OR name LIKE $1) ORDER BY {order} LIMIT $2"
    );
    let rows: Vec<(String, i32, i64)> = match sqlx::query_as(&sql).bind(&pattern).bind(limit).fetch_all(&pool).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("danbooru tags failed: {e:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let out: Vec<DanbooruTag> = rows
        .into_iter()
        .map(|(name, kind, count)| DanbooruTag { id: tag_id(&name), name, post_count: count, category: kind })
        .collect();
    Json(out).into_response()
}

// each search token is one condition; a plain word matches platform, artist slug or title so a
// user typing either a derived tag or a title fragment gets results
struct Search {
    tokens: Vec<(bool, Token)>,
    id: Option<i64>,
    order: Order,
    // danbooru id-cursor pagination: file_id strictly below `before` / above `after`
    before: Option<i64>,
    after: Option<i64>,
}

enum Token {
    Video,
    Gif,
    Animated,
    Free,
    Paid,
    Word(String),
}

enum Order {
    IdDesc,
    IdAsc,
    ScoreDesc,
    ScoreAsc,
}

impl Search {
    // None = a rating: token excludes the board's single rating, so nothing can match
    fn parse(tags: &str, id: Option<i64>) -> Option<Self> {
        let mut tokens = Vec::new();
        let mut order = Order::IdDesc;
        for raw in tags.split_whitespace() {
            let (neg, t) = match raw.strip_prefix('-') {
                Some(rest) => (true, rest),
                None => (false, raw),
            };
            let t = t.to_lowercase();
            if let Some(v) = t.strip_prefix("rating:") {
                if rating_matches(v) == neg {
                    return None;
                }
                continue;
            }
            if let Some(v) = t.strip_prefix("sort:").or_else(|| t.strip_prefix("order:")) {
                order = parse_order(v);
                continue;
            }
            if t.is_empty() || t == "all" {
                continue;
            }
            let token = match t.as_str() {
                "video" => Token::Video,
                "gif" => Token::Gif,
                "animated" => Token::Animated,
                "free" => Token::Free,
                "paid" => Token::Paid,
                _ => Token::Word(t),
            };
            tokens.push((neg, token));
        }
        Some(Search { tokens, id, order, before: None, after: None })
    }

    fn push_where(&self, qb: &mut QueryBuilder<Postgres>) {
        qb.push("(vc.mime LIKE 'image/%' OR vc.mime LIKE 'video/%')");
        if let Some(id) = self.id {
            qb.push(" AND vc.file_id = ").push_bind(id);
        }
        if let Some(b) = self.before {
            qb.push(" AND vc.file_id < ").push_bind(b);
        }
        if let Some(a) = self.after {
            qb.push(" AND vc.file_id > ").push_bind(a);
        }
        for (neg, token) in &self.tokens {
            qb.push(if *neg { " AND NOT " } else { " AND " });
            match token {
                Token::Video => qb.push("(vc.mime LIKE 'video/%')"),
                Token::Gif => qb.push("(vc.mime = 'image/gif')"),
                Token::Animated => qb.push("(vc.mime LIKE 'video/%' OR vc.mime = 'image/gif')"),
                Token::Free => qb.push("(vc.tier = 'free')"),
                Token::Paid => qb.push("(vc.tier = 'subscriber')"),
                Token::Word(w) => qb
                    .push("(vc.platform = ")
                    .push_bind(w.clone())
                    .push(" OR ")
                    .push(ARTIST_SLUG)
                    .push(" = ")
                    .push_bind(w.clone())
                    .push(" OR vc.post_title ILIKE '%' || ")
                    .push_bind(w.replace('_', " "))
                    .push(" || '%')"),
            };
        }
    }

    fn order_sql(&self) -> &'static str {
        match self.order {
            Order::IdDesc => "vc.file_id DESC",
            Order::IdAsc => "vc.file_id ASC",
            Order::ScoreDesc => "views DESC, vc.file_id DESC",
            Order::ScoreAsc => "views ASC, vc.file_id ASC",
        }
    }
}

fn parse_order(v: &str) -> Order {
    let mut it = v.split(':');
    let field = it.next().unwrap_or("");
    let asc = it.next() == Some("asc");
    match (field, asc) {
        ("score", false) => Order::ScoreDesc,
        ("score", true) => Order::ScoreAsc,
        (_, true) => Order::IdAsc,
        _ => Order::IdDesc,
    }
}

fn rating_matches(query: &str) -> bool {
    rating_class(query) == rating_class(rating())
}

// old scheme's "safe" folds into "general"; everything else compares by first letter
fn rating_class(r: &str) -> char {
    if r.starts_with("safe") {
        return 'g';
    }
    r.chars().next().unwrap_or('e')
}

fn rating() -> &'static str {
    match config::get().rating.as_deref() {
        Some(r) if !r.is_empty() => r,
        _ => "explicit",
    }
}

#[derive(Serialize)]
struct BooruPost {
    id: i64,
    parent_id: i64,
    md5: String,
    hash: String,
    image: String,
    directory: String,
    owner: String,
    tags: String,
    rating: String,
    source: String,
    score: i64,
    file_url: String,
    width: i32,
    height: i32,
    sample: bool,
    sample_url: String,
    sample_width: i32,
    sample_height: i32,
    preview_url: String,
    preview_width: i32,
    preview_height: i32,
    change: i64,
    created_at: String,
    status: String,
    has_notes: bool,
    has_comments: bool,
    has_children: bool,
    comment_count: i64,
}

impl BooruPost {
    fn from_row(r: &FileRow, base: &str) -> Self {
        let (w, h) = (r.width.unwrap_or(0), r.height.unwrap_or(0));
        let image = format!("{}.{}", r.file_id, ext(&r.mime, r.filename.as_deref()));
        let file_url = format!("{base}/ipfs/{}?filename={image}", r.cid);
        let (preview_url, (pw, ph)) = match &r.thumb_cid {
            Some(t) => (format!("{base}/ipfs/{t}?filename={}_thumb.jpg", r.file_id), scaled(w, h, 400)),
            None => (file_url.clone(), (w, h)),
        };
        let (created_at, change) = booru_time(r.posted_at.as_deref(), r.created_at);
        let md5 = r.sha256.get(..32).unwrap_or(&r.sha256).to_string();
        Self {
            id: r.file_id,
            parent_id: 0,
            hash: md5.clone(),
            md5,
            image,
            directory: String::new(),
            owner: slug(&r.creator),
            tags: tag_string(r),
            rating: rating().to_string(),
            source: format!("{base}/p/{}/{}/{}", r.platform, r.creator_id, r.post_id),
            score: r.views,
            sample_url: file_url.clone(),
            sample_width: w,
            sample_height: h,
            sample: false,
            file_url,
            width: w,
            height: h,
            preview_url,
            preview_width: pw,
            preview_height: ph,
            change,
            created_at,
            status: "active".to_string(),
            has_notes: false,
            has_comments: false,
            has_children: false,
            comment_count: 0,
        }
    }

    fn write_xml(&self, out: &mut String) {
        out.push_str("<post");
        for (k, v) in [
            ("id", self.id.to_string()),
            ("md5", self.md5.clone()),
            ("file_url", self.file_url.clone()),
            ("width", self.width.to_string()),
            ("height", self.height.to_string()),
            ("sample_url", self.sample_url.clone()),
            ("sample_width", self.sample_width.to_string()),
            ("sample_height", self.sample_height.to_string()),
            ("preview_url", self.preview_url.clone()),
            ("preview_width", self.preview_width.to_string()),
            ("preview_height", self.preview_height.to_string()),
            ("rating", self.rating.clone()),
            ("tags", self.tags.clone()),
            ("source", self.source.clone()),
            ("score", self.score.to_string()),
            ("parent_id", String::new()),
            ("change", self.change.to_string()),
            ("created_at", self.created_at.clone()),
            ("creator_id", "1".to_string()),
            ("status", self.status.clone()),
            ("has_notes", "false".to_string()),
            ("has_comments", "false".to_string()),
            ("has_children", "false".to_string()),
        ] {
            out.push(' ');
            out.push_str(k);
            out.push_str("=\"");
            out.push_str(&xesc(&v));
            out.push('"');
        }
        out.push_str("/>\n");
    }
}

fn tag_string(r: &FileRow) -> String {
    let mut tags = vec![slug(&r.creator), r.platform.clone()];
    match r.tier.as_deref() {
        Some("free") => tags.push("free".to_string()),
        Some("subscriber") => tags.push("paid".to_string()),
        _ => {}
    }
    if r.mime.starts_with("video/") {
        tags.push("video".to_string());
        tags.push("animated".to_string());
    } else if r.mime == "image/gif" {
        tags.push("gif".to_string());
        tags.push("animated".to_string());
    }
    tags.join(" ")
}

fn slug(name: &str) -> String {
    let s = name.to_lowercase().split_whitespace().collect::<Vec<_>>().join("_");
    if s.is_empty() {
        "unknown".to_string()
    } else {
        s
    }
}

fn ext(mime: &str, filename: Option<&str>) -> String {
    if let Some(e) = filename.and_then(|f| f.rsplit_once('.')).map(|(_, e)| e) {
        if !e.is_empty() && e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric()) {
            return e.to_lowercase();
        }
    }
    match mime.split('/').nth(1).unwrap_or("bin") {
        "jpeg" => "jpg",
        "svg+xml" => "svg",
        "quicktime" => "mov",
        "x-matroska" => "mkv",
        "x-msvideo" => "avi",
        "mpeg" => "mpg",
        other => other.trim_start_matches("x-"),
    }
    .to_string()
}

fn scaled(w: i32, h: i32, max: i32) -> (i32, i32) {
    let m = w.max(h);
    if m <= max {
        return (w.max(0), h.max(0));
    }
    ((w as i64 * max as i64 / m as i64) as i32, (h as i64 * max as i64 / m as i64) as i32)
}

// gelbooru's ruby-style timestamp plus the unix `change` field; platform dates come as ISO-8601
// text in a few shapes, ingest time is the fallback
fn booru_time(posted_at: Option<&str>, ingest_epoch: i64) -> (String, i64) {
    use chrono::{DateTime, NaiveDateTime, Utc};
    if let Some(s) = posted_at {
        if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
            return (dt.format(BOORU_DATE).to_string(), dt.timestamp());
        }
        for f in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
            if let Ok(n) = NaiveDateTime::parse_from_str(s, f) {
                let dt = n.and_utc();
                return (dt.format(BOORU_DATE).to_string(), dt.timestamp());
            }
        }
    }
    let dt = DateTime::<Utc>::from_timestamp(ingest_epoch, 0).unwrap_or_default();
    (dt.format(BOORU_DATE).to_string(), ingest_epoch)
}

fn base_url(headers: &HeaderMap) -> String {
    if let Some(u) = &config::get().public_url {
        return u.trim_end_matches('/').to_string();
    }
    let proto =
        headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()).unwrap_or("http");
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("127.0.0.1:3000");
    format!("{proto}://{host}")
}

fn category(kind: i32) -> &'static str {
    match kind {
        1 => "artist",
        3 => "copyright",
        5 => "metadata",
        _ => "general",
    }
}

// tags have no table, so the id is a stable hash of the name; clients only need it unique-ish
fn tag_id(name: &str) -> i64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in name.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    (h & 0x7fff_ffff) as i64
}

fn xesc(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

fn xml_response(xml: String) -> Response {
    ([(header::CONTENT_TYPE, "text/xml; charset=utf-8")], xml).into_response()
}

const XML_HEAD: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n";
const BOORU_DATE: &str = "%a %b %d %H:%M:%S %z %Y";
const ARTIST_SLUG: &str = "regexp_replace(lower(vc.creator), '\\s+', '_', 'g')";

// the whole derived tag vocabulary as one relation: creators as artist tags, platforms as
// copyright, tier/media kinds as metadata. gelbooru type codes: 1 artist, 3 copyright, 5 meta
const TAG_UNION: &str = "
SELECT name, type, count FROM (
    SELECT regexp_replace(lower(creator), '\\s+', '_', 'g') AS name, 1 AS type, COUNT(*)::bigint AS count
      FROM visible_content WHERE mime LIKE 'image/%' OR mime LIKE 'video/%' GROUP BY 1
    UNION ALL
    SELECT platform, 3, COUNT(*)::bigint
      FROM visible_content WHERE mime LIKE 'image/%' OR mime LIKE 'video/%' GROUP BY 1
    UNION ALL
    SELECT 'free', 5, COUNT(*)::bigint
      FROM visible_content WHERE tier = 'free' AND (mime LIKE 'image/%' OR mime LIKE 'video/%')
    UNION ALL
    SELECT 'paid', 5, COUNT(*)::bigint
      FROM visible_content WHERE tier = 'subscriber' AND (mime LIKE 'image/%' OR mime LIKE 'video/%')
    UNION ALL
    SELECT 'video', 5, COUNT(*)::bigint FROM visible_content WHERE mime LIKE 'video/%'
    UNION ALL
    SELECT 'gif', 5, COUNT(*)::bigint FROM visible_content WHERE mime = 'image/gif'
    UNION ALL
    SELECT 'animated', 5, COUNT(*)::bigint
      FROM visible_content WHERE mime LIKE 'video/%' OR mime = 'image/gif'
) t";

#[derive(sqlx::FromRow)]
struct FileRow {
    file_id: i64,
    cid: String,
    thumb_cid: Option<String>,
    sha256: String,
    mime: String,
    filename: Option<String>,
    width: Option<i32>,
    height: Option<i32>,
    size: i64,
    platform: String,
    creator: String,
    creator_id: String,
    post_id: String,
    posted_at: Option<String>,
    tier: Option<String>,
    created_at: i64,
    views: i64,
}

#[derive(Serialize)]
struct BooruTag {
    id: i64,
    name: String,
    count: i64,
    #[serde(rename = "type")]
    kind: i32,
    ambiguous: i32,
}

// gelbooru's json envelope: a metadata object plus the array under a type-named key
#[derive(Serialize)]
struct PostsJson {
    #[serde(rename = "@attributes")]
    attributes: Attributes,
    post: Vec<BooruPost>,
}

#[derive(Serialize)]
struct TagsJson {
    #[serde(rename = "@attributes")]
    attributes: Attributes,
    tag: Vec<BooruTag>,
}

#[derive(Serialize)]
struct Attributes {
    limit: i64,
    offset: i64,
    count: i64,
}

#[derive(Serialize)]
struct AcEntry {
    #[serde(rename = "type")]
    kind: &'static str,
    label: String,
    value: String,
    post_count: String,
    category: &'static str,
}

#[derive(serde::Deserialize)]
struct Dapi {
    page: Option<String>,
    s: Option<String>,
    id: Option<i64>,
    tags: Option<String>,
    pid: Option<i64>,
    limit: Option<i64>,
    json: Option<String>,
    name: Option<String>,
    names: Option<String>,
    name_pattern: Option<String>,
    orderby: Option<String>,
    term: Option<String>,
}

#[derive(serde::Deserialize)]
struct Ac {
    q: Option<String>,
}

#[derive(serde::Deserialize)]
struct DanbooruQuery {
    tags: Option<String>,
    page: Option<String>,
    limit: Option<i64>,
}

impl DanbooruQuery {
    // danbooru pages are 1-indexed, or a "b<id>"/"a<id>" cursor for deep paging past the offset wall.
    // returns (offset, before_id, after_id)
    fn paginate(&self, limit: i64) -> (i64, Option<i64>, Option<i64>) {
        match self.page.as_deref() {
            Some(p) if p.starts_with('b') => (0, p[1..].parse().ok(), None),
            Some(p) if p.starts_with('a') => (0, None, p[1..].parse().ok()),
            Some(p) => ((p.parse::<i64>().unwrap_or(1).max(1) - 1) * limit, None, None),
            None => (0, None, None),
        }
    }
}

#[derive(serde::Deserialize)]
struct DanbooruTagQuery {
    #[serde(rename = "search[name_matches]")]
    name_matches: Option<String>,
    #[serde(rename = "search[order]")]
    order: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize)]
struct DanbooruTag {
    id: i64,
    name: String,
    post_count: i64,
    // danbooru category ints line up with our tag kinds: 1 artist, 3 copyright, 5 meta
    category: i32,
}

#[derive(Serialize)]
struct DanbooruPost {
    id: i64,
    created_at: String,
    updated_at: String,
    score: i64,
    up_score: i64,
    down_score: i64,
    fav_count: i64,
    rating: &'static str,
    source: String,
    md5: String,
    file_url: String,
    large_file_url: String,
    preview_file_url: String,
    file_ext: String,
    file_size: i64,
    image_width: i32,
    image_height: i32,
    tag_string: String,
    tag_string_general: String,
    tag_string_artist: String,
    tag_string_character: String,
    tag_string_copyright: String,
    tag_string_meta: String,
    parent_id: Option<i64>,
    has_children: bool,
    is_deleted: bool,
    is_banned: bool,
    is_flagged: bool,
    is_pending: bool,
    pixiv_id: Option<i64>,
}

impl DanbooruPost {
    fn from_row(r: &FileRow, base: &str) -> Self {
        let ext = ext(&r.mime, r.filename.as_deref());
        let image = format!("{}.{ext}", r.file_id);
        let file_url = format!("{base}/ipfs/{}?filename={image}", r.cid);
        let preview_file_url = match &r.thumb_cid {
            Some(t) => format!("{base}/ipfs/{t}?filename={}_thumb.jpg", r.file_id),
            None => file_url.clone(),
        };
        let created = iso_time(r.posted_at.as_deref(), r.created_at);
        let md5 = r.sha256.get(..32).unwrap_or(&r.sha256).to_string();
        let t = categorized_tags(r);
        Self {
            id: r.file_id,
            updated_at: created.clone(),
            created_at: created,
            score: r.views,
            up_score: r.views,
            down_score: 0,
            fav_count: 0,
            rating: danbooru_rating(),
            source: format!("{base}/p/{}/{}/{}", r.platform, r.creator_id, r.post_id),
            md5,
            large_file_url: file_url.clone(),
            file_url,
            preview_file_url,
            file_ext: ext,
            file_size: r.size,
            image_width: r.width.unwrap_or(0),
            image_height: r.height.unwrap_or(0),
            tag_string: t.all,
            tag_string_general: String::new(),
            tag_string_artist: t.artist,
            tag_string_character: String::new(),
            tag_string_copyright: t.copyright,
            tag_string_meta: t.meta,
            parent_id: None,
            has_children: false,
            is_deleted: false,
            is_banned: false,
            is_flagged: false,
            is_pending: false,
            pixiv_id: None,
        }
    }
}

struct Categorized {
    all: String,
    artist: String,
    copyright: String,
    meta: String,
}

fn categorized_tags(r: &FileRow) -> Categorized {
    let artist = slug(&r.creator);
    let copyright = r.platform.clone();
    let mut meta = Vec::new();
    match r.tier.as_deref() {
        Some("free") => meta.push("free"),
        Some("subscriber") => meta.push("paid"),
        _ => {}
    }
    if r.mime.starts_with("video/") {
        meta.push("video");
        meta.push("animated");
    } else if r.mime == "image/gif" {
        meta.push("gif");
        meta.push("animated");
    }
    let meta = meta.join(" ");
    let all = [artist.as_str(), copyright.as_str(), meta.as_str()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    Categorized { all, artist, copyright, meta }
}

// danbooru rating is a single letter; fold the board's one rating down to it
fn danbooru_rating() -> &'static str {
    match rating_class(rating()) {
        'g' => "g",
        's' => "s",
        'q' => "q",
        _ => "e",
    }
}

fn iso_time(posted_at: Option<&str>, ingest_epoch: i64) -> String {
    use chrono::{DateTime, NaiveDateTime, Utc};
    if let Some(s) = posted_at {
        if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
            return dt.to_rfc3339();
        }
        for f in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
            if let Ok(n) = NaiveDateTime::parse_from_str(s, f) {
                return n.and_utc().to_rfc3339();
            }
        }
    }
    DateTime::<Utc>::from_timestamp(ingest_epoch, 0).unwrap_or_default().to_rfc3339()
}
