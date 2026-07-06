use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use sqlx::postgres::PgPool;

use crate::db;

// read-only JSON index. file references are IPFS CIDs, never bytes: a scraper reads structure here and
// fetches content from any IPFS node, so browsing us over HTML and proxying media through our gateway
// both drop off once other keepers hold the pinset
const API_PAGE: i64 = 100;

pub fn routes() -> axum::Router<crate::web::AppState> {
    use axum::routing::get;
    axum::Router::new()
        .route("/api", get(index))
        .route("/api/posts", get(posts))
        .route("/api/posts/{platform}/{creator_id}/{post_id}", get(post))
        .route("/api/creators", get(creators))
        .route("/api/creators/{platform}/{creator_id}", get(creator))
}

async fn index() -> Json<ApiIndex> {
    let cfg = crate::config::get();
    Json(ApiIndex {
        board: cfg.name.clone(),
        note: "file references are IPFS CIDs, not bytes; fetch content from any IPFS node or gateway, e.g. ipfs://<cid>",
        head: "/head.json",
        endpoints: &[
            "/api/posts?source=&tier=&q=&sort=&dir=&page=",
            "/api/posts/{platform}/{creator_id}/{post_id}",
            "/api/creators?source=&sort=&dir=&page=",
            "/api/creators/{platform}/{creator_id}?tier=&page=",
        ],
    })
}

async fn posts(State(pool): State<PgPool>, Query(q): Query<Browse>) -> Response {
    let (source, sort, desc, query, tier, page) = q.parts();
    let mut rows = match db::list_posts(&pool, &source, sort, desc, &query, tier, API_PAGE + 1, page * API_PAGE).await {
        Ok(r) => r,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let has_next = rows.len() as i64 > API_PAGE;
    rows.truncate(API_PAGE as usize);
    Json(PostList { page, has_next, posts: rows.iter().map(ApiPost::from_card).collect() }).into_response()
}

async fn post(State(pool): State<PgPool>, Path((platform, creator_id, post_id)): Path<(String, String, String)>) -> Response {
    let files = db::post_files(&pool, &platform, &creator_id, &post_id).await.unwrap_or_default();
    let Some(first) = files.first() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    Json(ApiPostDetail {
        platform: first.platform.clone(),
        creator_id: first.creator_id.clone(),
        post_id: first.post_id.clone(),
        title: first.post_title.clone(),
        creator: first.creator.clone(),
        posted_at: first.posted_at.clone(),
        tier: first.tier.clone(),
        body: first.content.clone(),
        files: files.iter().map(ApiFile::from_row).collect(),
    })
    .into_response()
}

async fn creators(State(pool): State<PgPool>, Query(q): Query<Browse>) -> Response {
    let (source, sort, desc, query, _tier, page) = q.parts();
    let mut rows = match db::list_creators(&pool, &source, sort, desc, &query, API_PAGE + 1, page * API_PAGE).await {
        Ok(r) => r,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let has_next = rows.len() as i64 > API_PAGE;
    rows.truncate(API_PAGE as usize);
    Json(CreatorList { page, has_next, creators: rows.iter().map(ApiCreator::from_card).collect() }).into_response()
}

async fn creator(State(pool): State<PgPool>, Path((platform, creator_id)): Path<(String, String)>, Query(q): Query<CreatorQuery>) -> Response {
    let tier = tier_db(q.tier.as_deref());
    let page = q.page.unwrap_or(0).max(0);
    let total = db::creator_post_count(&pool, &platform, &creator_id).await.unwrap_or(0);
    let mut rows = db::creator_posts(&pool, &platform, &creator_id, tier, API_PAGE + 1, page * API_PAGE).await.unwrap_or_default();
    if rows.is_empty() && page == 0 && total == 0 {
        return StatusCode::NOT_FOUND.into_response();
    }
    let has_next = rows.len() as i64 > API_PAGE;
    rows.truncate(API_PAGE as usize);
    let name = rows.first().map(|p| p.creator.clone()).unwrap_or_else(|| creator_id.clone());
    Json(CreatorDetail {
        platform,
        creator_id,
        creator: name,
        posts: total,
        page,
        has_next,
        items: rows.iter().map(ApiPost::from_card).collect(),
    })
    .into_response()
}

// browse params shared by the posts and creators listings; tier ("free"/"paid") only bites on posts
#[derive(serde::Deserialize)]
struct Browse {
    source: Option<String>,
    sort: Option<String>,
    dir: Option<String>,
    q: Option<String>,
    tier: Option<String>,
    page: Option<i64>,
}

impl Browse {
    fn parts(self) -> (String, db::SortField, bool, String, &'static str, i64) {
        (
            self.source.unwrap_or_default().trim().to_string(),
            db::SortField::parse(self.sort.as_deref()),
            self.dir.as_deref() != Some("asc"),
            self.q.unwrap_or_default().trim().to_string(),
            tier_db(self.tier.as_deref()),
            self.page.unwrap_or(0).max(0),
        )
    }
}

#[derive(serde::Deserialize)]
struct CreatorQuery {
    tier: Option<String>,
    page: Option<i64>,
}

// the UI tier value maps to the stored one; "paid" is stored as "subscriber", anything else means all
fn tier_db(raw: Option<&str>) -> &'static str {
    match raw.map(str::trim) {
        Some("free") => "free",
        Some("paid") => "subscriber",
        _ => "",
    }
}

fn cover_cid(thumb: Option<&str>) -> Option<String> {
    thumb.and_then(|t| t.strip_prefix("/f/")).map(str::to_string)
}

#[derive(Serialize)]
struct ApiIndex {
    board: String,
    note: &'static str,
    head: &'static str,
    endpoints: &'static [&'static str],
}

#[derive(Serialize)]
struct PostList {
    page: i64,
    has_next: bool,
    posts: Vec<ApiPost>,
}

#[derive(Serialize)]
struct CreatorList {
    page: i64,
    has_next: bool,
    creators: Vec<ApiCreator>,
}

#[derive(Serialize)]
struct CreatorDetail {
    platform: String,
    creator_id: String,
    creator: String,
    posts: i64,
    page: i64,
    has_next: bool,
    items: Vec<ApiPost>,
}

#[derive(Serialize)]
struct ApiPost {
    platform: String,
    creator_id: String,
    post_id: String,
    title: Option<String>,
    creator: String,
    posted_at: Option<String>,
    tier: Option<String>,
    files: i64,
    views: i64,
    cover_cid: Option<String>,
}

impl ApiPost {
    fn from_card(p: &db::PostCard) -> Self {
        Self {
            platform: p.platform.clone(),
            creator_id: p.creator_id.clone(),
            post_id: p.post_id.clone(),
            title: p.post_title.clone(),
            creator: p.creator.clone(),
            posted_at: p.posted_at.clone(),
            tier: p.tier.clone(),
            files: p.files,
            views: p.views,
            cover_cid: cover_cid(p.thumb.as_deref()),
        }
    }
}

#[derive(Serialize)]
struct ApiPostDetail {
    platform: String,
    creator_id: String,
    post_id: String,
    title: Option<String>,
    creator: String,
    posted_at: Option<String>,
    tier: Option<String>,
    body: String,
    files: Vec<ApiFile>,
}

#[derive(Serialize)]
struct ApiFile {
    cid: String,
    mime: String,
    size: i64,
    filename: Option<String>,
    thumb_cid: Option<String>,
    ipfs: String,
}

impl ApiFile {
    fn from_row(r: &db::ManifestRow) -> Self {
        Self {
            cid: r.cid.clone(),
            mime: r.mime.clone(),
            size: r.size,
            filename: r.filename.clone(),
            thumb_cid: cover_cid(r.thumb.as_deref()),
            ipfs: format!("ipfs://{}", r.cid),
        }
    }
}

#[derive(Serialize)]
struct ApiCreator {
    platform: String,
    creator_id: String,
    creator: String,
    posts: i64,
    files: i64,
    views: i64,
    cover_cid: Option<String>,
}

impl ApiCreator {
    fn from_card(c: &db::CreatorCard) -> Self {
        Self {
            platform: c.platform.clone(),
            creator_id: c.creator_id.clone(),
            creator: c.creator.clone(),
            posts: c.posts,
            files: c.files,
            views: c.views,
            cover_cid: cover_cid(c.thumb.as_deref()),
        }
    }
}
