use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use maud::{html, PreEscaped};
use serde::Serialize;
use sqlx::postgres::PgPool;

use crate::db;
use crate::web::AppState;

// read-only JSON index. file references are IPFS CIDs, never bytes: a consumer reads structure here and
// fetches content from any IPFS node, so browsing us over HTML and proxying media through our gateway
// both fall away once other keepers hold the pinset
const API_PAGE: i64 = 100;
const GUIDE: &str = "https://docs.ipfs.tech/how-to/command-line-quick-start/";
const NOTICE: &str = "Please don't scrape the browsing pages - use this API instead. It hands out IPFS CIDs, not bytes, so content comes straight from IPFS rather than through this board. Any node or public gateway serves the same CID, which keeps load off this server, and the more you pull over IPFS the less any single host matters";
const ENDPOINTS: &[&str] = &[
    "/api/posts?source=&tier=&q=&sort=&dir=&page=",
    "/api/posts/{platform}/{creator_id}/{post_id}",
    "/api/creators?source=&sort=&dir=&page=",
    "/api/creators/{platform}/{creator_id}?tier=&page=",
];
const BOORU_ENDPOINTS: &[&str] = &[
    "/index.php?page=dapi&s=post&q=index&tags=&pid=&limit=&json=1",
    "/index.php?page=dapi&s=tag&q=index&name_pattern=&limit=",
    "/autocomplete.php?q=",
];
const ICON_WARN: &str = "<svg viewBox='0 0 24 24' fill='none' stroke='currentColor' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'><path d='M10.29 3.86 1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z'/><path d='M12 9v4'/><path d='M12 17h.01'/></svg>";

pub fn routes() -> axum::Router<AppState> {
    use axum::routing::get;
    axum::Router::new()
        .route("/api", get(index))
        .route("/api/posts", get(posts))
        .route("/api/posts/{platform}/{creator_id}/{post_id}", get(post))
        .route("/api/creators", get(creators))
        .route("/api/creators/{platform}/{creator_id}", get(creator))
}

// browsers get a human page with the notice and guide, tools get the JSON index; both carry the head and
// root CIDs so a consumer can walk the whole signed manifest (head -> root -> shards) straight from IPFS
async fn index(State(pool): State<PgPool>, headers: HeaderMap) -> Response {
    let (head, root) =
        db::last_head(&pool).await.ok().flatten().map(|h| (h.head_cid, h.root_cid)).unzip();
    if wants_html(&headers) {
        return page(head.as_deref(), root.as_deref()).into_response();
    }
    Json(ApiIndex {
        board: crate::config::get().name.clone(),
        notice: NOTICE,
        guide: GUIDE,
        head,
        root,
        endpoints: ENDPOINTS,
        booru: BOORU_ENDPOINTS,
    })
    .into_response()
}

fn wants_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("text/html"))
}

fn page(head: Option<&str>, root: Option<&str>) -> Html<String> {
    crate::web::render(
        "API",
        html! {
            section.keepwrap {
                div.contribintro {
                    h1 { "API" }
                    p { "A read-only JSON index of everything this board holds. Every file reference is an IPFS CID, not bytes - read the structure here and fetch content from any IPFS node" }
                }
                div.warnbox {
                    (PreEscaped(ICON_WARN))
                    p { (NOTICE) }
                }
                div.panel {
                    h3 { "New to IPFS?" }
                    p {
                        "Fetch a CID with " code { "ipfs get <cid>" } ", or open "
                        code { "https://dweb.link/ipfs/<cid>" } " in a browser. Here is "
                        a href=(GUIDE) rel="noopener noreferrer" target="_blank" { "a short how-to" }
                    }
                }
                div.panel {
                    h3 { "Manifest" }
                    p {
                        "The whole signed index lives in IPFS as head -> root -> per-creator shards. Start from "
                        a href="/head.json" { "/head.json" } " and walk it; no per-post request touches this board"
                    }
                    @if let Some(h) = head { p.cidline { "head " code { (h) } } }
                    @if let Some(r) = root { p.cidline { "root " code { (r) } } }
                }
                div.panel {
                    h3 { "Endpoints" }
                    p { "Every file reference is a CID plus an " code { "ipfs://<cid>" } " URI; bytes come from IPFS, not from us" }
                    ul.list {
                        @for e in ENDPOINTS { li { code { (e) } } }
                    }
                }
                div.panel {
                    h3 { "Booru clients" }
                    p {
                        "The board also speaks the Gelbooru 0.2 API, so booru explorer apps can browse it: "
                        "add this site as a Gelbooru-compatible source. Tags are derived - creator name, platform, "
                        code { "free" } "/" code { "paid" } ", " code { "video" } "/" code { "gif" } "/" code { "animated" }
                        " - and any other search term matches post titles"
                    }
                    ul.list {
                        @for e in BOORU_ENDPOINTS { li { code { (e) } } }
                    }
                }
            }
        },
    )
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

async fn creator(State(state): State<AppState>, Path((platform, creator_id)): Path<(String, String)>, Query(q): Query<CreatorQuery>) -> Response {
    let tier = tier_db(q.tier.as_deref());
    let page = q.page.unwrap_or(0).max(0);
    let total = db::creator_post_count(&state.pool, &platform, &creator_id, "").await.unwrap_or(0);
    let mut rows = db::creator_posts(&state.pool, &platform, &creator_id, tier, API_PAGE + 1, page * API_PAGE).await.unwrap_or_default();
    if rows.is_empty() && page == 0 && total == 0 {
        return StatusCode::NOT_FOUND.into_response();
    }
    let has_next = rows.len() as i64 > API_PAGE;
    rows.truncate(API_PAGE as usize);
    let name = rows.first().map(|p| p.creator.clone()).unwrap_or_else(|| creator_id.clone());
    let shard_cid = shard_cid(&state, &platform, &creator_id).await;
    Json(CreatorDetail {
        platform,
        creator_id,
        creator: name,
        posts: total,
        shard_cid,
        page,
        has_next,
        items: rows.iter().map(ApiPost::from_card).collect(),
    })
    .into_response()
}

// a creator's whole post index is one IPFS object (their shard); its CID lives in the signed Root. cache
// the Root's shard map per manifest version so a lookup is a map hit, not a kubo fetch, past the first call
async fn shard_cid(state: &AppState, platform: &str, creator_id: &str) -> Option<String> {
    let root_cid = db::last_head(&state.pool).await.ok().flatten()?.root_cid;
    let key = format!("{platform}:{creator_id}");
    let cache = SHARD_CACHE.get_or_init(|| Mutex::new((String::new(), BTreeMap::new())));
    if let Ok(g) = cache.lock() {
        if g.0 == root_cid {
            return g.1.get(&key).cloned();
        }
    }
    let bytes = state.kubo.cat(&root_cid).await.ok()?;
    let root: bakemono_core::Root = serde_json::from_slice(&bytes).ok()?;
    let map: BTreeMap<String, String> = root.shards.into_iter().map(|(k, v)| (k, v.cid)).collect();
    let found = map.get(&key).cloned();
    if let Ok(mut g) = cache.lock() {
        *g = (root_cid, map);
    }
    found
}

static SHARD_CACHE: OnceLock<Mutex<(String, BTreeMap<String, String>)>> = OnceLock::new();

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
    thumb.and_then(|t| t.strip_prefix("/ipfs/")).map(str::to_string)
}

#[derive(Serialize)]
struct ApiIndex {
    board: String,
    notice: &'static str,
    guide: &'static str,
    head: Option<String>,
    root: Option<String>,
    endpoints: &'static [&'static str],
    booru: &'static [&'static str],
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
    // the creator's whole post index as a single IPFS object; fetch it instead of paging this endpoint
    shard_cid: Option<String>,
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
