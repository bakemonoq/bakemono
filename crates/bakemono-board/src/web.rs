use std::sync::{Arc, OnceLock};

use axum::body::Body;
use axum::extract::{Form, FromRef, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use maud::{html, Markup, PreEscaped, DOCTYPE};
use chrono::Utc;
use sqlx::postgres::PgPool;

use crate::config;
use crate::db;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub kubo: Arc<crate::kubo::Kubo>,
}

// lets handlers that only need the pool keep extracting State<PgPool> unchanged
impl FromRef<AppState> for PgPool {
    fn from_ref(state: &AppState) -> Self {
        state.pool.clone()
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(home))
        .route("/style.css", get(style_css))
        .route("/static/{file}", get(static_file))
        .route("/assets/{file}", get(asset_file))
        .route("/posts", get(posts_index))
        .route("/creators", get(creators_index))
        .route("/search", get(search_index))
        .route("/random", get(random_redirect))
        .route("/keepers", get(keepers))
        .route("/contribute", get(contribute).post(contribute_submit))
        .route("/info", get(info_page))
        .route("/c/{platform}/{creator_id}", get(creator_page))
        .route("/p/{platform}/{creator_id}/{post_id}", get(post_page))
        .route("/mod", get(mod_page))
        .route("/mod/deny-cid", post(mod_deny_cid))
        .route("/mod/remove-post", post(mod_remove_post))
        .route("/mod/remove-creator", post(mod_remove_creator))
        .route("/f/{cid}", get(ipfs_file))
        .route("/head.json", get(head_json))
        .route("/follower.json", get(follower_json))
        .merge(crate::api::routes())
        .with_state(state)
}

// how many cards a browse page shows; one extra is fetched to detect a next page without a count query
const PAGE: i64 = 36;

async fn home(State(pool): State<PgPool>) -> Html<String> {
    // 12 keeps Recent to two rows on a wide screen
    let posts = db::list_posts(&pool, "", db::SortField::Created, true, "", "", 12, 0)
        .await
        .unwrap_or_default();
    let creators = db::list_creators(&pool, "", db::SortField::Views, true, "", 12, 0)
        .await
        .unwrap_or_default();
    let cfg = config::get();
    render(
        "",
        html! {
            (welcome(cfg))
            section.block {
                div.blockhead { h2 { "Recent" } a.more href="/posts" { "all posts" } }
                @if posts.is_empty() {
                    p.muted { "Nothing indexed yet. Publish some manifests to a relay the board subscribes to" }
                }
                (posts_grid(&posts))
            }
            @if !creators.is_empty() {
                section.block {
                    div.blockhead { h2 { "Creators" } a.more href="/creators" { "all creators" } }
                    (creators_grid(&creators))
                }
            }
        },
    )
}

async fn posts_index(State(pool): State<PgPool>, Query(query): Query<BrowseQuery>) -> Html<String> {
    let (q, source, sort, desc, tier, page) = query.parts();
    let platforms = db::platforms(&pool).await.unwrap_or_default();
    let mut posts = db::list_posts(&pool, &source, sort, desc, &q, tier_db(&tier), PAGE + 1, page * PAGE)
        .await
        .unwrap_or_default();
    let has_next = posts.len() as i64 > PAGE;
    posts.truncate(PAGE as usize);
    let total = db::count_posts(&pool, &source, &q, tier_db(&tier)).await.ok();
    render(
        "posts",
        html! {
            h1.pagetitle { "Posts" }
            (filter_bar("/posts", &q, &source, sort, desc, &tier, &platforms, None))
            (tier_tabs("/posts", &q, &source, sort, desc, &tier))
            (pager("/posts", &q, &source, sort, desc, &tier, None, page, has_next, true, total, "post"))
            @if posts.is_empty() { p.muted { "No posts match" } }
            (posts_grid(&posts))
            (pager("/posts", &q, &source, sort, desc, &tier, None, page, has_next, false, total, "post"))
        },
    )
}

async fn creators_index(
    State(pool): State<PgPool>,
    Query(query): Query<BrowseQuery>,
) -> Html<String> {
    let (q, source, sort, desc, _tier, page) = query.parts();
    let platforms = db::platforms(&pool).await.unwrap_or_default();
    let mut creators = db::list_creators(&pool, &source, sort, desc, &q, PAGE + 1, page * PAGE)
        .await
        .unwrap_or_default();
    let has_next = creators.len() as i64 > PAGE;
    creators.truncate(PAGE as usize);
    let total = db::count_creators(&pool, &source, &q).await.ok();
    render(
        "creators",
        html! {
            h1.pagetitle { "Creators" }
            (filter_bar("/creators", &q, &source, sort, desc, "", &platforms, None))
            (pager("/creators", &q, &source, sort, desc, "", None, page, has_next, true, total, "creator"))
            @if creators.is_empty() { p.muted { "No creators match" } }
            (creators_grid(&creators))
            (pager("/creators", &q, &source, sort, desc, "", None, page, has_next, false, total, "creator"))
        },
    )
}

async fn random_redirect(State(pool): State<PgPool>) -> Redirect {
    match db::random_post(&pool).await {
        Ok(Some((platform, creator_id, post_id))) => {
            Redirect::to(&format!("/p/{platform}/{creator_id}/{post_id}"))
        }
        _ => Redirect::to("/posts"),
    }
}

// one query, two result sets: posts and creators live under their own tabs since either can be large
async fn search_index(State(pool): State<PgPool>, Query(query): Query<SearchQuery>) -> Html<String> {
    let q = query.q.unwrap_or_default().trim().to_string();
    let source = query.source.unwrap_or_default().trim().to_string();
    let sort = db::SortField::parse(query.sort.as_deref());
    let desc = query.dir.as_deref() != Some("asc");
    let creators_tab = query.tab.as_deref() == Some("creators");
    let page = query.page.unwrap_or(0).max(0);
    let tab = if creators_tab { "creators" } else { "posts" };
    // tier is a post-only filter, so it never rides the creators tab
    let tier = if creators_tab { String::new() } else { tier_param(query.tier.as_deref()) };
    let platforms = db::platforms(&pool).await.unwrap_or_default();

    let mut posts = Vec::new();
    let mut creators = Vec::new();
    let mut has_next = false;
    if !q.is_empty() && creators_tab {
        creators = db::list_creators(&pool, &source, sort, desc, &q, PAGE + 1, page * PAGE)
            .await
            .unwrap_or_default();
        has_next = creators.len() as i64 > PAGE;
        creators.truncate(PAGE as usize);
    } else if !q.is_empty() {
        posts = db::list_posts(&pool, &source, sort, desc, &q, tier_db(&tier), PAGE + 1, page * PAGE)
            .await
            .unwrap_or_default();
        has_next = posts.len() as i64 > PAGE;
        posts.truncate(PAGE as usize);
    }
    let (total, noun) = if q.is_empty() {
        (None, "post")
    } else if creators_tab {
        (db::count_creators(&pool, &source, &q).await.ok(), "creator")
    } else {
        (db::count_posts(&pool, &source, &q, tier_db(&tier)).await.ok(), "post")
    };

    render(
        "search",
        html! {
            h1.pagetitle { "Search" }
            div.tabs {
                a.tab.active[!creators_tab] href=(browse_url("/search", &q, &source, sort, desc, &tier, Some("posts"), 0)) { "Posts" }
                a.tab.active[creators_tab] href=(browse_url("/search", &q, &source, sort, desc, "", Some("creators"), 0)) { "Creators" }
            }
            (filter_bar("/search", &q, &source, sort, desc, &tier, &platforms, Some(tab)))
            @if !creators_tab { (tier_tabs("/search", &q, &source, sort, desc, &tier)) }
            @if !q.is_empty() { (pager("/search", &q, &source, sort, desc, &tier, Some(tab), page, has_next, true, total, noun)) }
            @if q.is_empty() {
                p.muted { "Type something to search posts and creators" }
            } @else if creators_tab {
                @if creators.is_empty() { p.muted { "No creators match \"" (q) "\"" } }
                (creators_grid(&creators))
            } @else {
                @if posts.is_empty() { p.muted { "No posts match \"" (q) "\"" } }
                (posts_grid(&posts))
            }
            @if !q.is_empty() { (pager("/search", &q, &source, sort, desc, &tier, Some(tab), page, has_next, false, total, noun)) }
        },
    )
}

#[derive(serde::Deserialize)]
struct SearchQuery {
    q: Option<String>,
    source: Option<String>,
    sort: Option<String>,
    dir: Option<String>,
    tier: Option<String>,
    tab: Option<String>,
    page: Option<i64>,
}

#[derive(serde::Deserialize)]
struct BrowseQuery {
    q: Option<String>,
    source: Option<String>,
    sort: Option<String>,
    dir: Option<String>,
    tier: Option<String>,
    page: Option<i64>,
}

impl BrowseQuery {
    // (query, source, sort field, desc, tier, page); desc unless dir=asc. tier is "" | "free" | "paid"
    fn parts(self) -> (String, String, db::SortField, bool, String, i64) {
        (
            self.q.unwrap_or_default().trim().to_string(),
            self.source.unwrap_or_default().trim().to_string(),
            db::SortField::parse(self.sort.as_deref()),
            self.dir.as_deref() != Some("asc"),
            tier_param(self.tier.as_deref()),
            self.page.unwrap_or(0).max(0),
        )
    }
}

// the UI tier value ("free"/"paid"), or "" for all; anything else is ignored
fn tier_param(raw: Option<&str>) -> String {
    match raw.map(str::trim) {
        Some("free") => "free".to_string(),
        Some("paid") => "paid".to_string(),
        _ => String::new(),
    }
}

// map the UI tier to the stored tier value; "paid" is stored as "subscriber"
fn tier_db(tier: &str) -> &str {
    match tier {
        "free" => "free",
        "paid" => "subscriber",
        _ => "",
    }
}

// All / Free / Paid pills for a post listing, preserving the current query and sort
fn tier_tabs(base: &str, q: &str, source: &str, sort: db::SortField, desc: bool, tier: &str) -> Markup {
    html! {
        div.tabs {
            @for (val, label) in [("", "All"), ("free", "Free"), ("paid", "Paid")] {
                a.tab.active[tier == val] href=(browse_url(base, q, source, sort, desc, val, None, 0)) { (label) }
            }
        }
    }
}

// the free/paid chip for a post card, or nothing when the tier is unknown
fn tier_badge(tier: Option<&str>) -> Option<Markup> {
    match tier {
        Some("free") => Some(html! { span.chip.ok { "free" } }),
        Some("subscriber") => Some(html! { span.chip.paid { "paid" } }),
        _ => None,
    }
}

fn posts_grid(posts: &[db::PostCard]) -> Markup {
    html! {
        div.grid {
            @for p in posts { (post_card(p)) }
        }
    }
}

fn post_card(p: &db::PostCard) -> Markup {
    let href = format!("/p/{}/{}/{}", p.platform, p.creator_id, p.post_id);
    let title = p.post_title.clone().unwrap_or_else(|| p.post_id.clone());
    html! {
        a.card href=(href) {
            div.cardthumb { (card_thumb(p.thumb.as_deref(), &p.mime)) }
            div.cardmeta {
                div.cardtitle { (title) }
                div.cardsub {
                    span.strong { (p.creator) }
                    @if let Some(b) = tier_badge(p.tier.as_deref()) { " " (b) }
                    br;
                    (p.files) @if p.files == 1 { " file" } @else { " files" }
                    @if let Some(at) = &p.posted_at { " - " (pretty_date(at)) }
                    br;
                    (p.views) @if p.views == 1 { " view" } @else { " views" }
                }
            }
        }
    }
}

fn creators_grid(creators: &[db::CreatorCard]) -> Markup {
    html! {
        div.grid.wide {
            @for c in creators { (creator_card(c)) }
        }
    }
}

fn creator_card(c: &db::CreatorCard) -> Markup {
    let href = format!("/c/{}/{}", c.platform, c.creator_id);
    html! {
        a.card href=(href) {
            div.cardthumb.banner { (card_thumb(c.thumb.as_deref(), c.mime.as_deref().unwrap_or(""))) }
            div.cardmeta {
                div.cardtitle { (c.creator) }
                div.cardsub {
                    span.chip.platform { (pretty_platform(&c.platform)) }
                    " " (c.posts) " posts - " (c.files) " files"
                    br;
                    (c.views) @if c.views == 1 { " view" } @else { " views" }
                }
            }
        }
    }
}

// the thumbnail area: the inline preview paints instantly with zero fetch. no preview shows a placeholder
// rather than pulling a full file into a grid cell, so a page of cards stays cheap
fn card_thumb(thumb: Option<&str>, mime: &str) -> Markup {
    html! {
        @match thumb {
            Some(t) => { img.cover src=(t) loading="lazy" alt=""; }
            None => { (placeholder(mime)) }
        }
        @if mime.starts_with("video/") { span.playbadge {} }
    }
}

fn placeholder(mime: &str) -> Markup {
    let label = mime.rsplit('/').next().unwrap_or("file").to_uppercase();
    html! { div.placeholder { (PreEscaped(ICON_IMAGE)) span { (label) } } }
}

// source filter + sort field + direction, applied over a search box; selects auto-submit, the direction
// toggle is a plain link carrying the current filters so it works without waiting on the selects
fn filter_bar(
    base: &str,
    q: &str,
    source: &str,
    sort: db::SortField,
    desc: bool,
    tier: &str,
    platforms: &[String],
    tab: Option<&str>,
) -> Markup {
    html! {
        form.filters method="get" action=(base) {
            @if let Some(t) = tab { input type="hidden" name="tab" value=(t); }
            @if !tier.is_empty() { input type="hidden" name="tier" value=(tier); }
            input type="hidden" name="dir" value=(if desc { "desc" } else { "asc" });
            div.searchfield {
                span.sicon { (PreEscaped(ICON_SEARCH)) }
                input type="search" name="q" value=(q) placeholder="Search";
            }
            span.filtersel {
                select name="source" aria-label="Source" onchange="this.form.submit()" {
                    option value="" selected[source.is_empty()] { "All sources" }
                    @for p in platforms {
                        option value=(p) selected[source == p.as_str()] { (pretty_platform(p)) }
                    }
                }
            }
            span.filtersel {
                select name="sort" aria-label="Sort" onchange="this.form.submit()" {
                    @for (val, label) in [("views", "Views"), ("created", "Created"), ("name", "Alphabetic"), ("service", "Service")] {
                        option value=(val) selected[sort.as_str() == val] { (label) }
                    }
                }
            }
            a.dirtoggle href=(browse_url(base, q, source, sort, !desc, tier, tab, 0))
                title=(if desc { "Sorted descending" } else { "Sorted ascending" })
                aria-label="Toggle sort direction" {
                (PreEscaped(if desc { ICON_SORT_DESC } else { ICON_SORT_ASC }))
            }
        }
    }
}

fn pager(
    base: &str,
    q: &str,
    source: &str,
    sort: db::SortField,
    desc: bool,
    tier: &str,
    tab: Option<&str>,
    page: i64,
    has_next: bool,
    top: bool,
    total: Option<i64>,
    noun: &str,
) -> Markup {
    let paged = page > 0 || has_next;
    html! {
        @if paged || total.is_some() {
            div.pager.top[top] {
                @if paged && page > 0 {
                    a.btn.ghost href=(browse_url(base, q, source, sort, desc, tier, tab, page - 1)) { "prev" }
                } @else if paged {
                    span.btn.ghost.off { "prev" }
                }
                span.muted {
                    @if paged { "page " (page + 1) }
                    @if paged && total.is_some() { " - " }
                    @if let Some(t) = total { (t) " " (noun) @if t != 1 { "s" } }
                }
                @if paged && has_next {
                    a.btn.ghost href=(browse_url(base, q, source, sort, desc, tier, tab, page + 1)) { "next" }
                } @else if paged {
                    span.btn.ghost.off { "next" }
                }
            }
        }
    }
}

fn browse_url(
    base: &str,
    q: &str,
    source: &str,
    sort: db::SortField,
    desc: bool,
    tier: &str,
    tab: Option<&str>,
    page: i64,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !q.is_empty() {
        parts.push(format!("q={}", qs_encode(q)));
    }
    if !source.is_empty() {
        parts.push(format!("source={}", qs_encode(source)));
    }
    parts.push(format!("sort={}", sort.as_str()));
    if !desc {
        parts.push("dir=asc".to_string());
    }
    if !tier.is_empty() {
        parts.push(format!("tier={}", qs_encode(tier)));
    }
    if let Some(t) = tab {
        if t != "posts" {
            parts.push(format!("tab={t}"));
        }
    }
    if page > 0 {
        parts.push(format!("page={page}"));
    }
    format!("{base}?{}", parts.join("&"))
}

// a service id rendered for humans; unknown ids are just capitalized
fn pretty_platform(p: &str) -> String {
    match p {
        "patreon" => "Patreon".to_string(),
        "fanbox" => "Pixiv Fanbox".to_string(),
        "fantia" => "Fantia".to_string(),
        "gumroad" => "Gumroad".to_string(),
        "subscribestar" => "SubscribeStar".to_string(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

fn welcome(cfg: &config::BoardConfig) -> Markup {
    html! {
        @if cfg.mascot.is_some() || cfg.welcome_html.is_some() {
            section.welcome {
                @if let Some(m) = &cfg.mascot { img.mascot src=(m) alt=""; }
                div.welcometext {
                    h1 { (cfg.name) }
                    @if let Some(t) = &cfg.tagline { p.tagline { (t) } }
                    // operator-authored html, rendered raw on purpose (same trust level as the binary)
                    @if let Some(body) = &cfg.welcome_html { div.welcomebody { (PreEscaped(body)) } }
                }
            }
        }
    }
}

async fn style_css() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        STYLE,
    )
        .into_response()
}

// operator assets (mascot, favicon) self-hosted from a configured dir so a board makes no external request.
// flat filenames only - any separator or traversal is rejected, so this never escapes the static dir
async fn static_file(Path(file): Path<String>) -> Response {
    let Some(dir) = config::get().static_dir.as_deref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if file.is_empty() || file.contains('/') || file.contains('\\') || file.contains("..") {
        return StatusCode::NOT_FOUND.into_response();
    }
    match tokio::fs::read(std::path::Path::new(dir).join(&file)).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, content_type_for(&file)),
                (header::CACHE_CONTROL, "public, max-age=3600"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

// guide images bundled into the binary so every board serves them with no operator setup
async fn asset_file(Path(file): Path<String>) -> Response {
    let bytes: &'static [u8] = match file.as_str() {
        "devtools_pick_application.png" => include_bytes!("../../../assets/devtools_pick_application.png"),
        "copy_cookie_patreon.png" => include_bytes!("../../../assets/copy_cookie_patreon.png"),
        "cookies_extension.png" => include_bytes!("../../../assets/cookies_extension.png"),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        bytes,
    )
        .into_response()
}

fn content_type_for(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "css" => "text/css",
        _ => "application/octet-stream",
    }
}

// absolute links for feed readers: honor the proxy's forwarded scheme (Cloudflare sets it), else assume http
fn base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http");
    format!("{proto}://{host}")
}

fn qs_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}


async fn creator_page(
    State(pool): State<PgPool>,
    Path((platform, creator_id)): Path<(String, String)>,
    Query(query): Query<BrowseQuery>,
    headers: HeaderMap,
) -> Html<String> {
    let page = query.page.unwrap_or(0).max(0);
    let tier = tier_param(query.tier.as_deref());
    let mut posts = db::creator_posts(&pool, &platform, &creator_id, tier_db(&tier), PAGE + 1, page * PAGE)
        .await
        .unwrap_or_default();
    let has_next = posts.len() as i64 > PAGE;
    posts.truncate(PAGE as usize);
    let total = db::creator_post_count(&pool, &platform, &creator_id, "").await.unwrap_or(0);
    let matching = if tier.is_empty() {
        total
    } else {
        db::creator_post_count(&pool, &platform, &creator_id, tier_db(&tier)).await.unwrap_or(0)
    };
    let name = posts
        .first()
        .map(|p| p.creator.clone())
        .unwrap_or_else(|| creator_id.clone());
    let base = format!("/c/{platform}/{creator_id}");
    render(
        &name,
        html! {
            div.crumbs { a href="/creators" { "Creators" } " / " span { (name) } }
            div.creatorhead {
                h1 { (name) }
                span.chip.platform { (pretty_platform(&platform)) }
                span.muted { (total) @if total == 1 { " post" } @else { " posts" } }
            }
            @if is_mod(&headers) { (mod_bar_creator(&platform, &creator_id)) }
            (tier_tabs(&base, "", "", db::SortField::Created, true, &tier))
            @if posts.is_empty() { p.muted { "Nothing here yet" } }
            (posts_grid(&posts))
            (pager(&base, "", "", db::SortField::Created, true, &tier, None, page, has_next, false, Some(matching), "post"))
        },
    )
}

async fn post_page(
    State(pool): State<PgPool>,
    Path((platform, creator_id, post_id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let files = db::post_files(&pool, &platform, &creator_id, &post_id)
        .await
        .unwrap_or_default();
    let first = files.first();
    let title = first
        .and_then(|f| f.post_title.clone())
        .unwrap_or_else(|| post_id.clone());
    let body = first.map(|f| crate::sanitize::body(&f.content)).unwrap_or_default();
    let creator = first.map(|f| f.creator.clone());
    let date = first.and_then(|f| f.posted_at.clone());
    let adjacent = db::adjacent_posts(&pool, &platform, &creator_id, &post_id)
        .await
        .ok()
        .flatten();

    // count one view per browser: a rolling cookie holds the last post opened, so a refresh does not re-count
    let token = view_token(&platform, &creator_id, &post_id);
    let repeat = cookie_value(&headers, "lastview").as_deref() == Some(token.as_str());
    if !repeat && !files.is_empty() {
        let _ = db::bump_views(&pool, &platform, &creator_id, &post_id).await;
    }

    let page = render(
        &title,
        html! {
            div.posthead {
                (nav_btn(&platform, &creator_id,
                    adjacent.as_ref().and_then(|a| a.prev_id.as_deref()),
                    adjacent.as_ref().and_then(|a| a.prev_title.as_deref()), true))
                div.postheadmid {
                    h1.posttitle { (title) }
                    div.postmeta {
                        @if let Some(c) = &creator {
                            "by " a.strong href=(format!("/c/{platform}/{creator_id}")) { (c) }
                        }
                        @if let Some(d) = &date { " - " (pretty_date(d)) }
                        @if !files.is_empty() { " - " (files.len()) @if files.len() == 1 { " file" } @else { " files" } }
                    }
                }
                (nav_btn(&platform, &creator_id,
                    adjacent.as_ref().and_then(|a| a.next_id.as_deref()),
                    adjacent.as_ref().and_then(|a| a.next_title.as_deref()), false))
            }
            @if files.is_empty() { p.muted { "This post has no files, or they are hidden" } }
            (carousel(&files))
            @if !body.is_empty() { div.body { (PreEscaped(&body)) } }
            @if is_mod(&headers) { (mod_bar_post(&platform, &creator_id, &post_id)) }
            script { (PreEscaped(CAROUSEL_JS)) }
        },
    );

    let mut resp = page.into_response();
    if !repeat {
        if let Ok(v) = header::HeaderValue::from_str(&format!(
            "lastview={token}; Path=/; Max-Age=1800; SameSite=Lax"
        )) {
            resp.headers_mut().insert(header::SET_COOKIE, v);
        }
    }
    resp
}

// a compact prev/next button flanking the post title; an absent neighbor keeps its slot so the title stays
// centered, and the neighbor's title rides along as a hover tooltip
fn nav_btn(
    platform: &str,
    creator_id: &str,
    target: Option<&str>,
    tip: Option<&str>,
    is_prev: bool,
) -> Markup {
    let (icon, fallback) = if is_prev { (ICON_PREV, "Prev") } else { (ICON_NEXT, "Next") };
    let label = tip.unwrap_or(fallback);
    html! {
        @match target {
            Some(id) => a.pnav href=(format!("/p/{platform}/{creator_id}/{id}")) title=[tip] {
                @if is_prev { (PreEscaped(icon)) span.ptitle { (label) } }
                @else { span.ptitle { (label) } (PreEscaped(icon)) }
            }
            None => span.pnav.off {
                @if is_prev { (PreEscaped(icon)) span.ptitle { (label) } }
                @else { span.ptitle { (label) } (PreEscaped(icon)) }
            }
        }
    }
}

// full media (not the tiny preview) served straight from the local IPFS gateway (/ipfs/{cid}), one at a
// time and centered - content is the point of the page. loading direct from Kubo keeps the board out of
// the byte path; each item loads only when shown, so a many-image post does not fetch it all at once
fn carousel(files: &[db::ManifestRow]) -> Markup {
    let items: Vec<String> = files
        .iter()
        .map(|f| {
            let video = f.mime.starts_with("video/");
            format!("{{\"u\":\"/ipfs/{}\",\"v\":{video}}}", f.cid)
        })
        .collect();
    if items.is_empty() {
        return html! {};
    }
    let multi = items.len() > 1;
    let json = format!("[{}]", items.join(","));
    html! {
        div.carousel data-items=(json) {
            @if multi { button.cprev type="button" aria-label="Previous image" { (PreEscaped(ICON_PREV)) } }
            div.cstage {}
            @if multi { button.cnext type="button" aria-label="Next image" { (PreEscaped(ICON_NEXT)) } }
            @if multi { div.ccount {} }
        }
    }
}

async fn contribute() -> Html<String> {
    if crate::crypto::load_public_pem().is_none() {
        return render("contribute", html! {
            section.contribwrap {
                div.contribintro {
                    h1 { "Contribute" }
                    p { "Contributions are closed on this board right now" }
                }
            }
        });
    }
    render("contribute", contribute_body(None))
}

fn contribute_body(error: Option<&str>) -> Markup {
    html! {
        section.contribwrap {
            div.contribintro {
                h1 { "Contribute your subscriptions" }
                p { "Paste one session cookie from a site you subscribe to. The board fetches every post you can already see and saves it to a shared archive - you never upload anything yourself, and your cookie is encrypted the moment it arrives" }
            }
            @if let Some(e) = error { p.err { (e) } }
            form.contribform method="post" action="/contribute" {
                label { "Platform"
                    select name="platform" {
                        @for p in crate::platform::live_platforms() {
                            option value=(p.id) { (p.label) }
                        }
                    }
                }
                label { "Session cookie value"
                    textarea name="token" rows="2" maxlength=(crate::platform::MAX_COOKIE_PASTE) placeholder="paste the cookie value, or the whole cookies.txt" required autocomplete="off" spellcheck="false" {}
                }
                label.check { input type="checkbox" name="allow_autoimport" value="1" checked; span { "Keep importing my new posts every day " span.hint { "(stores the cookie encrypted)" } } }
                label.check { input type="checkbox" name="allow_debug" value="1"; span { "Let the operator use my session to debug import problems" } }
                button type="submit" { "Import my subscriptions" }
                p.formnote { "Encrypted with RSA-4096 on arrival. The key that could unlock stored cookies stays offline, so a database breach can't expose it. Without daily import checked, the cookie is discarded right after the first run" }
            }
            section.faq {
                h2 { "Questions" }
                (faq_item("How does it work?", html! {
                    p { "Your browser proves you are subscribed with a small session cookie. You hand the board a copy, it signs in as you just long enough to download the posts you already have access to, and adds the files to an archive mirrored across many machines" }
                    p { "The archive never contains your cookie - it is encrypted the instant it reaches the server and only ever used to fetch on your behalf" }
                }))
                (faq_item("What do I need to do?", html! {
                    p { "Three things: pick the platform, paste the cookie value, press Import. No account, no upload, nothing to install" }
                    p { "Leave \"keep importing my new posts\" checked if you want new posts pulled in automatically. Otherwise it is a one-time import and the cookie is thrown away" }
                }))
                (faq_item("How do I find my cookie?", html! {
                    p { "Sign in to the site in your browser first, then pick either route below:" }
                    (browser_spoiler("Easiest: the Get cookies.txt LOCALLY add-on", true, html! {
                        ol {
                            li { "Install it for "
                                a href="https://addons.mozilla.org/en-US/firefox/addon/get-cookies-txt-locally/" rel="noopener noreferrer" target="_blank" { "Firefox" }
                                " or "
                                a href="https://chromewebstore.google.com/detail/get-cookiestxt-locally/cclelndahbckbenkjhflpdbgdldlbecc" rel="noopener noreferrer" target="_blank" { "Chrome, Edge, Brave" }
                            }
                            li { "Open a tab on the site, click the add-on, then press " strong { "Copy" }
                                img.guideimg src="/assets/cookies_extension.png" alt="Copying cookies with the Get cookies.txt LOCALLY add-on" loading="lazy";
                            }
                            li { "Paste that into the form above - we keep only the one cookie we need and drop the rest" }
                        }
                        p.muted { "\"LOCALLY\" is the point: it reads your cookies in the browser and never sends them anywhere itself" }
                    }))
                    (browser_spoiler("Or do it by hand in your browser", false, html! {
                        p { "Copy the value of the cookie named for your platform, then paste it into the form above:" }
                        ul.cookienames {
                            @for p in crate::platform::live_platforms() {
                                li { (p.label) ": " code { (p.cookie_name) } }
                            }
                        }
                        p.muted { "Open the cookie panel in your browser:" }
                        (browser_spoiler("Chrome, Edge, Brave", false, html! {
                            ol {
                                li { "Press " code { "F12" } " to open developer tools, or right-click the page and choose Inspect" }
                                li { "In the row of tabs at the top open " strong { "Application" } " - click " code { ">>" } " if you do not see it"
                                    img.guideimg src="/assets/devtools_pick_application.png" alt="Opening the Application tab in developer tools" loading="lazy";
                                }
                                li { "In the left sidebar open " strong { "Storage -> Cookies" } " and select the site's address" }
                                li { "Click the cookie named for your platform, then copy its " strong { "Cookie Value" } " from the panel at the bottom"
                                    img.guideimg src="/assets/copy_cookie_patreon.png" alt="Copying the session_id cookie value on Patreon" loading="lazy";
                                }
                            }
                        }))
                        (browser_spoiler("Safari", false, html! {
                            ol {
                                li { "Turn on developer features: " strong { "Settings -> Advanced" } ", check " strong { "Show features for web developers" } }
                                li { "Open the Web Inspector with " code { "Cmd+Option+I" } }
                                li { "Go to " strong { "Storage -> Cookies" } " and pick the site" }
                                li { "Double-click the cookie named for your platform and copy its " strong { "Value" } }
                            }
                        }))
                        (browser_spoiler("Firefox", false, html! {
                            ol {
                                li { "Press " code { "F12" } " and open the " strong { "Storage" } " tab" }
                                li { "Expand " strong { "Cookies" } " and pick the site" }
                                li { "Right-click the cookie named for your platform and choose " strong { "Copy" } }
                            }
                        }))
                    }))
                }))
            }
        }
    }
}

fn faq_item(question: &str, body: Markup) -> Markup {
    html! {
        div.faqitem {
            h3 { (question) }
            (body)
        }
    }
}

fn browser_spoiler(name: &str, open: bool, body: Markup) -> Markup {
    html! {
        details.spoiler open[open] {
            summary { (name) }
            div.spbody { (body) }
        }
    }
}

#[derive(serde::Deserialize)]
struct ContributeForm {
    platform: String,
    token: String,
    #[serde(default)]
    allow_autoimport: Option<String>,
    #[serde(default)]
    allow_debug: Option<String>,
}

async fn contribute_submit(
    State(state): State<AppState>,
    Form(form): Form<ContributeForm>,
) -> Response {
    let Some(pubkey) = crate::crypto::load_public_pem() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "contributions closed").into_response();
    };
    let platform = form.platform.trim();
    let raw = form.token.trim();
    if !crate::platform::is_live(platform) || raw.is_empty() {
        return render("contribute", contribute_body(Some("Pick a platform and paste a cookie value"))).into_response();
    }
    if raw.len() > crate::platform::MAX_COOKIE_PASTE {
        return render("contribute", contribute_body(Some("That is far more than a cookie - paste just the cookie value or your cookies.txt, not a whole page"))).into_response();
    }
    // people paste the whole cookies.txt, a Cookie header, or `name=value`; pull out the one value we need
    let Some(token) = crate::platform::extract_token(platform, raw) else {
        return render("contribute", contribute_body(Some("We couldn't find your session cookie in that. Paste just the cookie value, or the whole cookies.txt file you exported"))).into_response();
    };
    let token = token.as_str();

    // validate while we still hold the plaintext: a quick feed probe tells live from dead. anything
    // past here treats the cookie as a live secret to protect
    if !crate::scrape::probe_cookie(platform, token, &format!("submit-{platform}")).await {
        return render("contribute", contribute_body(Some("That cookie was rejected by the site - make sure you are signed in and copied the whole value"))).into_response();
    }

    let allow_autoimport = form.allow_autoimport.is_some();
    let allow_debug = form.allow_debug.is_some();

    // persist the sealed cookie only if the contributor opted into daily import; either way the
    // first import runs now in the background off the plaintext, no private key needed
    let stored = if allow_autoimport || allow_debug {
        match seal_and_store(&state, &pubkey, platform, token, allow_autoimport, allow_debug).await {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::error!("storing cookie failed: {e:#}");
                return (StatusCode::INTERNAL_SERVER_ERROR, "could not store the cookie").into_response();
            }
        }
    } else {
        None
    };
    spawn_import(&state, stored, platform.to_string(), token.to_string());

    render("contribute", html! {
        section.contribwrap {
            div.contribintro {
                h1 { "Thanks!" }
                p { "Your cookie checks out. The board is importing everything you are subscribed to now - it will show up in the archive shortly" }
            }
            div.panel.confirm {
                @if allow_autoimport { p { "Your cookie is stored encrypted and new posts will be imported every day" } }
                @else { p { "Nothing was stored - this was a one-time import" } }
                p { a.btn href="/" { "Back to the board" } }
            }
        }
    }).into_response()
}

async fn seal_and_store(
    state: &AppState,
    pubkey: &str,
    platform: &str,
    token: &str,
    allow_autoimport: bool,
    allow_debug: bool,
) -> anyhow::Result<i64> {
    let sealed = crate::crypto::seal(pubkey, token.as_bytes())?;
    let fp = crate::crypto::fingerprint(platform, token);
    db::upsert_cookie(&state.pool, platform, &fp, &sealed, allow_autoimport, allow_debug).await
}

// scrape the whole feed with the plaintext; the token lives only in this task. if the cookie was
// stored, record which creators it reached for the mod page
fn spawn_import(state: &AppState, cookie_id: Option<i64>, platform: String, token: String) {
    let (pool, kubo) = (state.pool.clone(), state.kubo.clone());
    tokio::spawn(async move {
        let scope = format!("submit-{platform}");
        match crate::scrape::scrape_feed(&pool, &kubo, &platform, &token, &scope).await {
            Ok((stats, creators)) => {
                if let Some(id) = cookie_id {
                    db::set_cookie_creators(&pool, id, &platform, &creators).await.ok();
                }
                if stats.files > 0 {
                    if let Err(e) = crate::publish::publish_if_changed(&pool, &kubo).await {
                        tracing::error!("publish after import failed: {e:#}");
                    }
                }
            }
            Err(e) => tracing::error!(platform, "import failed: {e:#}"),
        }
    });
}

async fn keepers(headers: HeaderMap) -> Html<String> {
    let base = base_url(&headers);
    let script_url = REPO.replace("github.com", "raw.githubusercontent.com") + "/main/scripts/keeper-setup.sh";
    let quick_cmd = format!("curl -fsSL {script_url} | sudo bash -s -- {base}");
    render(
        "keepers",
        html! {
            section.keepwrap {
                div.contribintro {
                    h1 { "Become a keeper" }
                    p { "Help the archive outlive any single server. A keeper mirrors everything this board holds - every file, thumbnail, and the signed manifest history. If the board dies, it is rebuilt from any keeper" }
                }
                div.panel {
                    h3 { "Quick setup (Linux)" }
                    p.muted { "One command on a fresh Linux box. It installs IPFS and the cluster follower, wires up this board, and runs both under systemd:" }
                    pre { code { (quick_cmd) } }
                    p.muted.small { "Want to read it first? Source: " a href=(script_url) rel="noopener noreferrer" target="_blank" { "keeper-setup.sh" } }
                }
                div.panel {
                    h3 { "Manual setup" }
                    p.muted { "Prefer to install by hand, or not on Linux? Install kubo (docs.ipfs.tech/install) and ipfs-cluster-follow, then:" }
                    pre { code {
                        "ipfs init\n"
                        "ipfs config --json Bitswap.ServerEnabled true\n"
                        "ipfs config --json Internal.Bitswap.BroadcastControl.Enable false\n"
                        "ipfs config --json Reprovider.Strategy '\"roots\"'\n"
                        "# gateway serves only blocks you hold, never fetches for strangers\n"
                        "ipfs config --json Gateway.NoFetch true\n"
                        "ipfs daemon --enable-gc &\n\n"
                        "ipfs-cluster-follow bakemono init " (base) "/follower.json\n"
                        "ipfs-cluster-follow bakemono run"
                    } }
                    p.muted.small { "The quick-setup script also installs a timer that keeps the takedown denylist current (see below); by hand, run periodically: " code { "ipfs cat /ipfs/$(ipfs cat " (base) "/head.json | jq -r .root | xargs -I@ ipfs cat /ipfs/@ | jq -r .denylist) > $IPFS_PATH/denylists/bakemono.deny" } }
                }
                section.faq {
                    h2 { "Questions" }
                    (faq_item("How does it work?", html! {
                        p { "IPFS addresses every file by a hash of its contents. This board publishes a signed list of those hashes; your keeper follows the list and pulls down each file, so you end up with a full copy of the archive" }
                        p { "If this server ever dies, the whole board can be rebuilt from any keeper's copy" }
                    }))
                    (faq_item("What do I need to do?", html! {
                        p { "On Linux: paste the quick-setup command and leave it. It installs everything, points the follower at this board, and keeps both running across reboots. There is no board software, no account, and nothing to maintain by hand" }
                        p { "You mirror the entire archive. Following just one creator or platform is not supported yet" }
                    }))
                    (faq_item("What happens over time?", html! {
                        ul {
                            li { "New posts replicate to you automatically" }
                            li { "Removed content is unpinned automatically - you host a moderated mirror, not a write-once dump" }
                            li { "Run kubo with " code { "--enable-gc" } " so unpinned files are actually freed from your disk" }
                            li { "Stopping is safe; nothing depends on your machine specifically" }
                        }
                    }))
                    (faq_item("How is a takedown enforced on my node?", html! {
                        p { "Two layers, because they solve different problems. Unpinning drops the file from the pinset, so your node stops offering it and GC eventually frees the disk - but GC is not prompt (kubo only runs it near the storage cap), so the block can linger for a long time" }
                        p { "So the block would still be reachable by its direct hash in the meantime. To stop that, your gateway runs " code { "NoFetch" } " (serves only what you hold, never fetches arbitrary hashes for others) and loads the board's signed takedown list into nopfs - a revoked hash returns 410 at once, independent of GC" }
                        p { "That list lives in IPFS (referenced from the signed manifest), so it survives the board dying: the sync timer just pulls the latest one your node already replicates. This matters most for the categorically-illegal content the board actively moderates" }
                    }))
                }
                div.panel {
                    h3 { "Pointers" }
                    ul.list {
                        li { a href="/follower.json" { "follower.json" } " - the cluster config your follower reads" }
                        li { a href="/head.json" { "head.json" } " - the signed manifest head (the archive index)" }
                    }
                }
            }
        },
    )
}

async fn info_page(State(state): State<AppState>) -> Html<String> {
    let stats = db::stats(&state.pool).await.unwrap_or_default();
    let board_pubkey = crate::publish::board_pubkey().ok();
    let head = db::last_head(&state.pool).await.ok().flatten();
    render(
        "info",
        html! {
            h2 { (board_name()) }
            // operator-authored board description, rendered raw (same trust level as the binary)
            @if let Some(about) = &config::get().about_html {
                div.aboutblock { (PreEscaped(about)) }
            }
            div.stats {
                (stat_card(stats.posts, "posts"))
                (stat_card(stats.creators, "creators"))
                (stat_card(stats.files, "files"))
                (stat_card(stats.keepers_hint, "manifest version"))
            }

            h3 { "Archive identity" }
            @match &board_pubkey {
                Some(key) => {
                    p { "This board signs its manifest with the key below. Peer boards and keepers verify against it:" }
                    p { code { (key) } }
                }
                None => p.muted { "No board key yet; it is generated on first publish" }
            }
            @if let Some(h) = &head {
                p { "Current manifest: version " (h.version) ", head " a href="/head.json" { code { (h.head_cid) } } }
            }

            h3 { "Keeping" }
            p { "Anyone can replicate this archive with stock IPFS tools: " a href="/keepers" { "become a keeper" } }

            h3 { "Source" }
            p { a href=(REPO) { (REPO) } }

            h3 { "DMCA and contact" }
            p { "Removals are recorded in the signed manifest's revoked list - a public, hash-linked transparency log that keepers replicate" }
            @match dmca_contact() {
                Some(email) => p { "DMCA notices: " a href=(format!("mailto:{email}")) { (email) } }
                None => p.muted { "No DMCA contact configured for this board" }
            }
            @if let Some(email) = contact() {
                p { "General contact: " a href=(format!("mailto:{email}")) { (email) } }
            }
        },
    )
}

fn stat_card(num: i64, label: &str) -> Markup {
    html! {
        div.stat {
            div.num { (num) }
            div.label { (label) }
        }
    }
}

async fn mod_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(denied) = require_mod(&headers) {
        return denied;
    }
    let denied = db::denylist(&state.pool).await.unwrap_or_default();
    let cookies = db::cookie_overview(&state.pool).await.unwrap_or_default();
    let live = cookies.iter().filter(|c| c.2 == "live").count();
    let page = render(
        "mod",
        html! {
            h1 { "Moderation" }
            h3 { "Take down a file" }
            p.muted { "Denies the CID (and its preview), unpins it fleet-wide, republishes the manifest with a revoked entry. Post and creator removal buttons live on their pages" }
            form method="post" action="/mod/deny-cid" {
                input type="text" name="cid" placeholder="bafy..." required;
                input type="text" name="reason" placeholder="reason (dmca-us, csam, wrong-content...)";
                button type="submit" { "Take down" }
            }
            h3 { "Revoked (" (denied.len()) ")" }
            @if denied.is_empty() { p.muted { "Nothing revoked" } }
            @for d in &denied {
                p { code { (d.0) } " - " (d.1) " - " (pretty_date(&d.2)) }
            }
            h3 { "Contributor cookies (" (live) " live / " (cookies.len()) ")" }
            p.muted { "Tokens are encrypted; run an import round with `bakemono autoimport < cookie-private.pem`" }
            @if cookies.is_empty() { p.muted { "No cookies submitted yet" } }
            @for (id, platform, status, creators, last_ok, error) in &cookies {
                p {
                    @if status == "live" { span.chip { "live" } } @else { span.chip.muted { (status) } }
                    " #" (id) " " (crate::platform::label(platform))
                    " - " (creators) " creators"
                    " - last ok: " (last_ok.clone().unwrap_or_else(|| "never".into()))
                    @if let Some(e) = error { span.muted { " - " (e) } }
                }
            }
        },
    );
    with_mod_cookie(page.into_response())
}

fn mod_bar_post(platform: &str, creator_id: &str, post_id: &str) -> Markup {
    html! {
        div.modbar {
            form method="post" action="/mod/remove-post" {
                input type="hidden" name="platform" value=(platform);
                input type="hidden" name="creator_id" value=(creator_id);
                input type="hidden" name="post_id" value=(post_id);
                input type="text" name="reason" placeholder="reason";
                button type="submit" { "Take down post" }
            }
        }
    }
}

fn mod_bar_creator(platform: &str, creator_id: &str) -> Markup {
    html! {
        div.modbar {
            form method="post" action="/mod/remove-creator" {
                input type="hidden" name="platform" value=(platform);
                input type="hidden" name="creator_id" value=(creator_id);
                input type="text" name="reason" placeholder="reason";
                button type="submit" { "Take down creator" }
            }
        }
    }
}

#[derive(serde::Deserialize)]
struct RemovePostForm {
    platform: String,
    creator_id: String,
    post_id: String,
    #[serde(default)]
    reason: Option<String>,
}

async fn mod_remove_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<RemovePostForm>,
) -> Response {
    if let Err(denied) = require_mod(&headers) {
        return denied;
    }
    let reason = clean_reason(form.reason.as_deref());
    match crate::publish::revoke_post(&state.pool, &state.kubo, &form.platform, &form.creator_id, &form.post_id, &reason).await {
        Ok(()) => Redirect::to(&format!("/c/{}/{}", form.platform, form.creator_id)).into_response(),
        Err(e) => {
            tracing::error!("remove-post failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(serde::Deserialize)]
struct RemoveCreatorForm {
    platform: String,
    creator_id: String,
    #[serde(default)]
    reason: Option<String>,
}

async fn mod_remove_creator(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<RemoveCreatorForm>,
) -> Response {
    if let Err(denied) = require_mod(&headers) {
        return denied;
    }
    let reason = clean_reason(form.reason.as_deref());
    match crate::publish::revoke_creator(&state.pool, &state.kubo, &form.platform, &form.creator_id, &reason).await {
        Ok(()) => Redirect::to("/creators").into_response(),
        Err(e) => {
            tracing::error!("remove-creator failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn clean_reason(raw: Option<&str>) -> String {
    raw.map(str::trim).filter(|r| !r.is_empty()).unwrap_or("unspecified").to_string()
}

// the new-stack gateway: catalog + denylist gate, then proxy the local kubo gateway.
// kubo runs NoFetch, so only blocks this board already pinned can ever leave this route
async fn ipfs_file(
    State(state): State<AppState>,
    Path(cid): Path<String>,
    headers: HeaderMap,
) -> Response {
    if cid.is_empty() || !cid.chars().all(|c| c.is_ascii_alphanumeric()) {
        return (StatusCode::BAD_REQUEST, "bad cid").into_response();
    }
    let mime = match db::serveable_file(&state.pool, &cid).await {
        Ok(Some(mime)) => mime,
        Ok(None) => return (StatusCode::NOT_FOUND, "unknown cid").into_response(),
        Err(e) => {
            tracing::error!("file lookup failed: {e:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let upstream = match state.kubo.fetch(&cid, range).await {
        Ok(resp) => resp,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("kubo error: {e:#}")).into_response(),
    };
    let status = StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if status != StatusCode::OK && status != StatusCode::PARTIAL_CONTENT {
        return (StatusCode::BAD_GATEWAY, format!("kubo gateway status {status}")).into_response();
    }
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, mime)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable");
    for name in [header::CONTENT_LENGTH, header::CONTENT_RANGE] {
        if let Some(value) = upstream.headers().get(name.as_str()) {
            if let Ok(value) = axum::http::HeaderValue::from_bytes(value.as_bytes()) {
                builder = builder.header(name, value);
            }
        }
    }
    match builder.body(Body::from_stream(upstream.bytes_stream())) {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!("ipfs proxy response build failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(serde::Deserialize)]
struct DenyCidForm {
    cid: String,
    #[serde(default)]
    reason: Option<String>,
}

// new-stack takedown by CID: denylist + unpin + republish with the revoked entry
async fn mod_deny_cid(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<DenyCidForm>,
) -> Response {
    if let Err(denied) = require_mod(&headers) {
        return denied;
    }
    let cid = form.cid.trim();
    if cid.is_empty() || !cid.chars().all(|c| c.is_ascii_alphanumeric()) {
        return (StatusCode::BAD_REQUEST, "bad cid").into_response();
    }
    let reason = form.reason.as_deref().map(str::trim).filter(|r| !r.is_empty()).unwrap_or("unspecified");
    match crate::publish::revoke_cid(&state.pool, &state.kubo, cid, reason).await {
        Ok(()) => Redirect::to("/mod").into_response(),
        Err(e) => {
            tracing::error!("deny-cid failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// what a volunteer keeper points ipfs-cluster-follow at: the board's own cluster config with
// follower-appropriate edits. built from the mounted cluster config dir; identity.json is read
// for its public peer id ONLY - the private key never leaves this function
async fn follower_json() -> Response {
    let Some(dir) = std::env::var("BAKEMONO_CLUSTER_CONFIG_DIR").ok().filter(|s| !s.is_empty())
    else {
        return (StatusCode::NOT_FOUND, "no cluster configured").into_response();
    };
    let public_addr = std::env::var("BAKEMONO_CLUSTER_PUBLIC_ADDR").unwrap_or_default();
    match build_follower_config(&dir, &public_addr).await {
        Ok(json) => (
            [(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-cache")],
            json,
        )
            .into_response(),
        Err(e) => {
            tracing::error!("follower config build failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn build_follower_config(dir: &str, public_addr: &str) -> anyhow::Result<String> {
    use serde_json::{json, Value};
    let mut service: Value =
        serde_json::from_slice(&tokio::fs::read(format!("{dir}/service.json")).await?)?;
    let identity: Value =
        serde_json::from_slice(&tokio::fs::read(format!("{dir}/identity.json")).await?)?;
    let peer_id = identity["id"].as_str().unwrap_or_default().to_string();
    anyhow::ensure!(!peer_id.is_empty(), "cluster identity has no id");

    service["cluster"]["peername"] = json!("keeper");
    service["cluster"]["listen_multiaddress"] = json!(["/ip4/0.0.0.0/tcp/9096"]);
    service["cluster"]["peer_addresses"] = if public_addr.is_empty() {
        json!([])
    } else {
        json!([format!("{public_addr}/p2p/{peer_id}")])
    };
    service["consensus"]["crdt"]["trusted_peers"] = json!([peer_id]);
    // a follower replicates; it does not expose APIs or proxy its kubo
    service["api"] = json!({});
    service["ipfs_connector"]["ipfshttp"]["node_multiaddress"] = json!("/ip4/127.0.0.1/tcp/5001");
    Ok(service.to_string())
}

// the signed pointer to the current manifest version, served verbatim as published
async fn head_json(State(pool): State<PgPool>) -> Response {
    match db::latest_head_json(&pool).await {
        Ok(Some(json)) => (
            [(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-cache")],
            json,
        )
            .into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no manifest published yet").into_response(),
        Err(e) => {
            tracing::error!("head lookup failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}



fn now_secs() -> i64 {
    Utc::now().timestamp()
}

// derived from the mod token so it is stable across restarts with no rng dep; it salts the ip hash
// and keys the anti-replay token
fn session_secret() -> &'static [u8; 32] {
    static SECRET: OnceLock<[u8; 32]> = OnceLock::new();
    SECRET.get_or_init(|| {
        let token = std::env::var("BAKEMONO_MOD_TOKEN").unwrap_or_default();
        let mut h = Sha256::new();
        h.update(b"bakemono-report-secret-v1");
        h.update(token.as_bytes());
        let out = h.finalize();
        let mut key = [0u8; 32];
        key.copy_from_slice(&out);
        key
    })
}

type HmacSha256 = Hmac<Sha256>;


// the mod routes require HTTP Basic auth with the password set in BAKEMONO_MOD_TOKEN
fn require_mod(headers: &HeaderMap) -> Result<(), Response> {
    if std::env::var("BAKEMONO_MOD_TOKEN")
        .unwrap_or_default()
        .is_empty()
    {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "mod queue disabled; set BAKEMONO_MOD_TOKEN on the board",
        )
            .into_response());
    }
    if is_mod(headers) {
        return Ok(());
    }
    Err((
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"bakemono mod\"")],
        "authentication required",
    )
        .into_response())
}

// true when the request carries the mod token (Basic auth) or a live mod-session cookie; gates the
// mod actions and decides whether public post/creator pages render in-context ban controls
fn is_mod(headers: &HeaderMap) -> bool {
    let token = std::env::var("BAKEMONO_MOD_TOKEN").unwrap_or_default();
    if token.is_empty() {
        return false;
    }
    basic_auth_password(headers).as_deref() == Some(token.as_str())
        || valid_mod_cookie(headers, now_secs())
}

const MOD_SESSION_TTL: i64 = 8 * 3600;

// a signed, expiring cookie minted when a mod loads /mod with the token, so ban buttons work on the
// public pages without re-prompting; keyed on session_secret (mod-token-derived) so rotating the token
// invalidates every outstanding session
fn mod_session_cookie() -> String {
    let expiry = now_secs() + MOD_SESSION_TTL;
    let sig = mod_session_sig(expiry);
    format!("modsession={expiry}.{sig}; Path=/; Max-Age={MOD_SESSION_TTL}; HttpOnly; SameSite=Strict")
}

fn with_mod_cookie(mut resp: Response) -> Response {
    if let Ok(v) = header::HeaderValue::from_str(&mod_session_cookie()) {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    resp
}

fn mod_session_sig(expiry: i64) -> String {
    let mut mac = HmacSha256::new_from_slice(session_secret()).expect("hmac key");
    mac.update(format!("modsession:{expiry}").as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&mac.finalize().into_bytes()[..16])
}

fn valid_mod_cookie(headers: &HeaderMap, now: i64) -> bool {
    let Some(raw) = cookie_value(headers, "modsession") else {
        return false;
    };
    let Some((exp, sig_b64)) = raw.split_once('.') else {
        return false;
    };
    let Ok(expiry) = exp.parse::<i64>() else {
        return false;
    };
    if expiry < now {
        return false;
    }
    let Ok(sig) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(sig_b64) else {
        return false;
    };
    let mut mac = HmacSha256::new_from_slice(session_secret()).expect("hmac key");
    mac.update(format!("modsession:{expiry}").as_bytes());
    mac.verify_truncated_left(&sig).is_ok()
}

fn basic_auth_password(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let encoded = value.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let creds = String::from_utf8(decoded).ok()?;
    creds.split_once(':').map(|(_, pass)| pass.to_string())
}

fn board_name() -> String {
    config::get().name.clone()
}

fn dmca_contact() -> Option<String> {
    config::get().dmca_contact.clone()
}

fn contact() -> Option<String> {
    config::get().contact.clone()
}


// a stable per-post cookie token so a refresh does not re-count a view; hashed so odd ids stay cookie-safe
fn view_token(platform: &str, creator_id: &str, post_id: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (platform, creator_id, post_id).hash(&mut h);
    format!("{:016x}", h.finish())
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k.trim() == name).then(|| v.trim().to_string())
    })
}

// posted_at is ISO-8601 (2026-06-23T17:46:49.000+00:00); show a humane "Jun 23, 2026"
fn pretty_date(raw: &str) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let date = raw.get(..10).unwrap_or(raw);
    if let [year, month, day] = date.split('-').collect::<Vec<_>>()[..] {
        if let Ok(m) = month.parse::<usize>() {
            if (1..=12).contains(&m) {
                return format!(
                    "{} {}, {}",
                    MONTHS[m - 1],
                    day.trim_start_matches('0'),
                    year
                );
            }
        }
    }
    raw.to_string()
}

pub(crate) fn render(title: &str, body: Markup) -> Html<String> {
    let cfg = config::get();
    let tab = if title.is_empty() {
        cfg.name.clone()
    } else {
        format!("{title} - {}", cfg.name)
    };
    Html(
        html! {
            (DOCTYPE)
            html lang="en" {
                head {
                    meta charset="utf-8";
                    meta name="viewport" content="width=device-width, initial-scale=1";
                    meta name="referrer" content="no-referrer";
                    title { (tab) }
                    link rel="stylesheet" href=(concat!("/style.css?v=", env!("CARGO_PKG_VERSION")));
                    // operator accent override; the base sheet stays static and cacheable
                    @if let Some(accent) = &cfg.accent {
                        style { (PreEscaped(format!(":root{{--accent:{accent}}}"))) }
                    }
                }
                body {
                    header.topbar {
                        a.brand href="/" {
                            @if let Some(m) = &cfg.mascot { img.brandmascot src=(m) alt=""; }
                            span { (cfg.name) }
                        }
                        // the search field is a full bar on desktop; on mobile it collapses and this checkbox
                        // (toggled by the search icon button) reveals it as a row under the top bar
                        input #searchtoggle.searchtoggle type="checkbox" hidden;
                        form.topsearch method="get" action="/search" {
                            button.searchicon type="submit" aria-label="Search" { (PreEscaped(ICON_SEARCH)) }
                            input type="search" name="q" placeholder="Search" aria-label="Search";
                        }
                        div.topactions {
                            label.iconbtn.searchopen for="searchtoggle" title="Search" aria-label="Search" { (PreEscaped(ICON_SEARCH)) }
                            a.iconbtn.shuffle href="/random" title="Random post" aria-label="Random post" { (PreEscaped(ICON_SHUFFLE)) }
                            nav {
                                a href="/creators" { (PreEscaped(ICON_CREATORS)) span { "Creators" } }
                                a href="/posts" { (PreEscaped(ICON_POSTS)) span { "Posts" } }
                                a href="/contribute" { (PreEscaped(ICON_CONTRIBUTE)) span { "Contribute" } }
                                a href="/keepers" { (PreEscaped(ICON_KEEPERS)) span { "Keepers" } }
                            }
                        }
                    }
                    main { (body) }
                    (footer(cfg))
                    script { (PreEscaped(SHELL_JS)) }
                }
            }
        }
        .into_string(),
    )
}

fn footer(cfg: &config::BoardConfig) -> Markup {
    html! {
        footer.foot {
            div.footinner {
                @if !cfg.community.is_empty() {
                    div.social {
                        @for l in &cfg.community {
                            a.soc href=(l.url) title=(l.label) rel="noopener noreferrer" target="_blank" {
                                (PreEscaped(community_icon(&l.label))) span { (l.label) }
                            }
                        }
                    }
                }
                nav.footlinks {
                    a href="/info" { (PreEscaped(ICON_INFO)) span { "Info" } }
                    a href="/api" { (PreEscaped(ICON_API)) span { "API" } }
                    a href="/keepers" { (PreEscaped(ICON_KEEPERS)) span { "Keepers" } }
                    a href="/contribute" { (PreEscaped(ICON_CONTRIBUTE)) span { "Contribute" } }
                    @if let Some(email) = &cfg.contact { a href=(format!("mailto:{email}")) { (PreEscaped(ICON_MAIL)) span { "Contact" } } }
                }
                p.small.muted {
                    "Files are served from a peer swarm, not stored here. "
                    a href="/info" { "How this works" }
                }
            }
        }
    }
}

// map a toml community link label to a brand mark; unknown networks get a generic link glyph
fn community_icon(label: &str) -> &'static str {
    let l = label.to_ascii_lowercase();
    if l.contains("telegram") {
        ICON_TELEGRAM
    } else if l.contains("discord") {
        ICON_DISCORD
    } else if l.contains("reddit") {
        ICON_REDDIT
    } else if l.contains("youtube") {
        ICON_YOUTUBE
    } else if l.contains("github") {
        ICON_GITHUB
    } else if l.contains("twitter") || l == "x" || l.contains("x.com") {
        ICON_X
    } else {
        ICON_LINK
    }
}

const REPO: &str = env!("CARGO_PKG_REPOSITORY");

// inline SVGs so the page pulls no icon font or external asset. shared attrs are per-svg to stay standalone
const ICON_SHUFFLE: &str = "<svg viewBox='0 0 24 24' width='18' height='18' fill='none' stroke='currentColor' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'><path d='M16 3h5v5'/><path d='M4 20 21 3'/><path d='M21 16v5h-5'/><path d='M15 15l6 6'/><path d='M4 4l5 5'/></svg>";
const ICON_SEARCH: &str = "<svg viewBox='0 0 24 24' width='18' height='18' fill='none' stroke='currentColor' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'><circle cx='11' cy='11' r='7'/><path d='M21 21l-4.3-4.3'/></svg>";
const ICON_PREV: &str = "<svg viewBox='0 0 24 24' width='20' height='20' fill='none' stroke='currentColor' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'><path d='M15 18l-6-6 6-6'/></svg>";
const ICON_NEXT: &str = "<svg viewBox='0 0 24 24' width='20' height='20' fill='none' stroke='currentColor' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'><path d='M9 18l6-6-6-6'/></svg>";
const ICON_CREATORS: &str = "<svg viewBox='0 0 24 24' width='18' height='18' fill='none' stroke='currentColor' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'><path d='M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2'/><circle cx='9' cy='7' r='4'/><path d='M23 21v-2a4 4 0 0 0-3-3.87'/><path d='M16 3.13a4 4 0 0 1 0 7.75'/></svg>";
const ICON_POSTS: &str = "<svg viewBox='0 0 24 24' width='18' height='18' fill='none' stroke='currentColor' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'><rect x='3' y='3' width='7' height='7' rx='1'/><rect x='14' y='3' width='7' height='7' rx='1'/><rect x='3' y='14' width='7' height='7' rx='1'/><rect x='14' y='14' width='7' height='7' rx='1'/></svg>";
const ICON_CONTRIBUTE: &str = "<svg viewBox='0 0 24 24' width='18' height='18' fill='none' stroke='currentColor' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'><path d='M20.8 4.6a5.5 5.5 0 0 0-7.8 0L12 5.6l-1-1a5.5 5.5 0 1 0-7.8 7.8l1 1L12 21l7.8-7.6 1-1a5.5 5.5 0 0 0 0-7.8z'/></svg>";
const ICON_IMAGE: &str = "<svg viewBox='0 0 24 24' width='30' height='30' fill='none' stroke='currentColor' stroke-width='1.6' stroke-linecap='round' stroke-linejoin='round'><rect x='3' y='3' width='18' height='18' rx='2'/><circle cx='8.5' cy='8.5' r='1.5'/><path d='M21 15l-5-5L5 21'/></svg>";
const ICON_INFO: &str = "<svg viewBox='0 0 24 24' fill='none' stroke='currentColor' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'><circle cx='12' cy='12' r='9'/><path d='M12 16v-4M12 8h.01'/></svg>";
const ICON_API: &str = "<svg viewBox='0 0 24 24' width='18' height='18' fill='none' stroke='currentColor' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'><path d='M8 6l-6 6 6 6'/><path d='M16 6l6 6-6 6'/></svg>";
const ICON_KEEPERS: &str = "<svg viewBox='0 0 24 24' fill='none' stroke='currentColor' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'><path d='M12 3l7 3v5c0 4.5-3 7.5-7 9-4-1.5-7-4.5-7-9V6z'/></svg>";
const ICON_MAIL: &str = "<svg viewBox='0 0 24 24' fill='none' stroke='currentColor' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'><rect x='3' y='5' width='18' height='14' rx='2'/><path d='M3 7l9 6 9-6'/></svg>";
const ICON_SORT_DESC: &str = "<svg viewBox='0 0 24 24' fill='none' stroke='currentColor' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'><path d='M12 5v14M6 13l6 6 6-6'/></svg>";
const ICON_SORT_ASC: &str = "<svg viewBox='0 0 24 24' fill='none' stroke='currentColor' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'><path d='M12 19V5M6 11l6-6 6 6'/></svg>";

// brand marks for footer social links (self-hosted, no icon font); matched from the toml link label
const ICON_TELEGRAM: &str = "<svg viewBox='0 0 24 24' fill='currentColor'><path d='M11.944 0A12 12 0 0 0 0 12a12 12 0 0 0 12 12 12 12 0 0 0 12-12A12 12 0 0 0 12 0a12 12 0 0 0-.056 0zm4.962 7.224c.1-.002.321.023.465.14a.506.506 0 0 1 .171.325c.016.093.036.306.02.472-.18 1.898-.962 6.502-1.36 8.627-.168.9-.499 1.201-.82 1.23-.696.065-1.225-.46-1.9-.902-1.056-.693-1.653-1.124-2.678-1.8-1.185-.78-.417-1.21.258-1.91.177-.184 3.247-2.977 3.307-3.23.007-.032.014-.15-.056-.212s-.174-.041-.249-.024c-.106.024-1.793 1.14-5.061 3.345-.48.33-.913.49-1.302.48-.428-.008-1.252-.241-1.865-.44-.752-.245-1.349-.374-1.297-.789.027-.216.325-.437.893-.663 3.498-1.524 5.83-2.529 6.998-3.014 3.332-1.386 4.025-1.627 4.476-1.635z'/></svg>";
const ICON_DISCORD: &str = "<svg viewBox='0 0 24 24' fill='currentColor'><path d='M20.317 4.3698a19.7913 19.7913 0 0 0-4.8851-1.5152.0741.0741 0 0 0-.0785.0371c-.211.3753-.4447.8648-.6083 1.2495-1.8447-.2762-3.68-.2762-5.4868 0-.1636-.3933-.4058-.8742-.6177-1.2495a.077.077 0 0 0-.0785-.037 19.7363 19.7363 0 0 0-4.8852 1.515.0699.0699 0 0 0-.0321.0277C.5334 9.0458-.319 13.5799.0992 18.0578a.0824.0824 0 0 0 .0312.0561c2.0528 1.5076 4.0413 2.4228 5.9929 3.0294a.0777.0777 0 0 0 .0842-.0276c.4616-.6304.8731-1.2952 1.226-1.9942a.076.076 0 0 0-.0416-.1057c-.6528-.2476-1.2743-.5495-1.8722-.8923a.077.077 0 0 1-.0076-.1277c.1258-.0943.2517-.1923.3718-.2914a.0743.0743 0 0 1 .0776-.0105c3.9278 1.7933 8.18 1.7933 12.0614 0a.0739.0739 0 0 1 .0785.0095c.1202.099.246.1981.3728.2924a.077.077 0 0 1-.0066.1276 12.2986 12.2986 0 0 1-1.873.8914.0766.0766 0 0 0-.0407.1067c.3604.698.7719 1.3628 1.225 1.9932a.076.076 0 0 0 .0842.0286c1.961-.6067 3.9495-1.5219 6.0023-3.0294a.077.077 0 0 0 .0313-.0552c.5004-5.177-.8382-9.6739-3.5485-13.6604a.061.061 0 0 0-.0312-.0286zM8.02 15.3312c-1.1825 0-2.1569-1.0857-2.1569-2.419 0-1.3332.9555-2.4189 2.157-2.4189 1.2108 0 2.1757 1.0952 2.1568 2.419 0 1.3332-.9555 2.4189-2.1569 2.4189zm7.9748 0c-1.1825 0-2.1569-1.0857-2.1569-2.419 0-1.3332.9554-2.4189 2.1569-2.4189 1.2108 0 2.1757 1.0952 2.1568 2.419 0 1.3332-.946 2.4189-2.1568 2.4189z'/></svg>";
const ICON_REDDIT: &str = "<svg viewBox='0 0 24 24' fill='currentColor'><path d='M12 0A12 12 0 0 0 0 12a12 12 0 0 0 12 12 12 12 0 0 0 12-12A12 12 0 0 0 12 0zm5.01 4.744c.688 0 1.25.561 1.25 1.249a1.25 1.25 0 0 1-2.498.056l-2.597-.547-.8 3.747c1.824.07 3.48.632 4.674 1.488.308-.309.73-.491 1.207-.491.968 0 1.754.786 1.754 1.754 0 .716-.435 1.333-1.01 1.614a3.111 3.111 0 0 1 .042.52c0 2.694-3.13 4.87-7.004 4.87-3.874 0-7.004-2.176-7.004-4.87 0-.183.015-.366.043-.534A1.748 1.748 0 0 1 4.028 12c0-.968.786-1.754 1.754-1.754.463 0 .898.196 1.207.49 1.207-.883 2.878-1.43 4.744-1.487l.885-4.182a.342.342 0 0 1 .14-.197.35.35 0 0 1 .238-.042l2.906.617a1.214 1.214 0 0 1 1.108-.701zM9.25 12c-.688 0-1.25.561-1.25 1.25 0 .687.561 1.248 1.25 1.248.687 0 1.248-.561 1.248-1.249 0-.688-.561-1.249-1.249-1.249zm5.5 0c-.687 0-1.248.561-1.248 1.25 0 .687.561 1.248 1.249 1.248.688 0 1.249-.561 1.249-1.249 0-.687-.562-1.249-1.25-1.249zm-5.466 3.99a.327.327 0 0 0-.231.094.33.33 0 0 0 0 .463c.842.842 2.484.913 2.961.913.477 0 2.105-.056 2.961-.913a.361.361 0 0 0 .029-.463.33.33 0 0 0-.464 0c-.547.533-1.684.73-2.512.73-.828 0-1.979-.196-2.512-.73a.326.326 0 0 0-.232-.095z'/></svg>";
const ICON_YOUTUBE: &str = "<svg viewBox='0 0 24 24' fill='currentColor'><path d='M23.498 6.186a3.016 3.016 0 0 0-2.122-2.136C19.505 3.545 12 3.545 12 3.545s-7.505 0-9.377.505A3.017 3.017 0 0 0 .502 6.186C0 8.07 0 12 0 12s0 3.93.502 5.814a3.016 3.016 0 0 0 2.122 2.136c1.871.505 9.376.505 9.376.505s7.505 0 9.377-.505a3.015 3.015 0 0 0 2.122-2.136C24 15.93 24 12 24 12s0-3.93-.502-5.814zM9.545 15.568V8.432L15.818 12z'/></svg>";
const ICON_GITHUB: &str = "<svg viewBox='0 0 24 24' fill='currentColor'><path d='M12 .297c-6.63 0-12 5.373-12 12 0 5.303 3.438 9.8 8.205 11.385.6.113.82-.258.82-.577 0-.285-.01-1.04-.015-2.04-3.338.724-4.042-1.61-4.042-1.61C4.422 18.07 3.633 17.7 3.633 17.7c-1.087-.744.084-.729.084-.729 1.205.084 1.838 1.236 1.838 1.236 1.07 1.835 2.809 1.305 3.495.998.108-.776.417-1.305.76-1.605-2.665-.3-5.466-1.332-5.466-5.93 0-1.31.465-2.38 1.235-3.22-.135-.303-.54-1.523.105-3.176 0 0 1.005-.322 3.3 1.23.96-.267 1.98-.399 3-.405 1.02.006 2.04.138 3 .405 2.28-1.552 3.285-1.23 3.285-1.23.645 1.653.24 2.873.12 3.176.765.84 1.23 1.91 1.23 3.22 0 4.61-2.805 5.625-5.475 5.92.42.36.81 1.096.81 2.22 0 1.606-.015 2.896-.015 3.286 0 .315.21.69.825.57C20.565 22.092 24 17.592 24 12.297c0-6.627-5.373-12-12-12'/></svg>";
const ICON_X: &str = "<svg viewBox='0 0 24 24' fill='currentColor'><path d='M18.244 2.25h3.308l-7.227 8.26 8.502 11.24H16.17l-5.214-6.817L4.99 21.75H1.68l7.73-8.835L1.254 2.25H8.08l4.713 6.231zm-1.161 17.52h1.833L7.084 4.126H5.117z'/></svg>";
const ICON_LINK: &str = "<svg viewBox='0 0 24 24' fill='none' stroke='currentColor' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'><circle cx='12' cy='12' r='9'/><path d='M3 12h18'/><path d='M12 3a15 15 0 0 1 0 18 15 15 0 0 1 0-18z'/></svg>";

// Catppuccin Mocha, self-hosted and static: no external font, no CDN, no third-party request from any page
const STYLE: &str = "
:root {
  --base:#1e1e2e; --mantle:#181825; --crust:#11111b;
  --surface0:#313244; --surface1:#45475a; --surface2:#585b70;
  --overlay0:#6c7086; --overlay1:#7f849c;
  --text:#cdd6f4; --subtext1:#bac2de; --subtext0:#a6adc8;
  --mauve:#cba6f7; --red:#f38ba8; --green:#a6e3a1; --yellow:#f9e2af;
  --accent:var(--mauve);
  color-scheme: dark;
}
* { box-sizing: border-box }
body { margin:0; background:var(--base); color:var(--text); line-height:1.5;
  font-family: system-ui,-apple-system,Segoe UI,Roboto,Helvetica,Arial,sans-serif }
a { color:var(--accent); text-decoration:none }
a:hover { text-decoration:underline }
img { display:block }
h1,h2,h3 { line-height:1.2 }
button,input,select { font:inherit }

.topbar { position:sticky; top:0; z-index:20; display:flex; align-items:center; gap:1rem;
  padding:.6rem 1.1rem; background:color-mix(in srgb, var(--mantle) 92%, transparent);
  backdrop-filter:blur(10px); border-bottom:1px solid var(--surface0) }
.brand { display:flex; align-items:center; gap:.5rem; font-weight:800; font-size:1.15rem; color:var(--text); white-space:nowrap }
.brand:hover { text-decoration:none }
.brandmascot { width:28px; height:28px; border-radius:7px; object-fit:cover }
.topsearch { flex:1; position:relative; display:flex; align-items:center; max-width:520px; margin:0 auto }
.topsearch input { flex:1; height:40px; padding:0 .9rem 0 2.4rem; border-radius:10px; border:1px solid var(--surface1);
  background:var(--surface0); color:var(--text) }
.topsearch input:focus { outline:none; border-color:var(--accent) }
.searchicon { position:absolute; left:0; top:0; height:40px; width:2.4rem; display:grid; place-items:center;
  background:none; border:none; color:var(--subtext0); cursor:pointer; padding:0 }
.searchicon:hover { color:var(--accent) }
.topactions { display:flex; align-items:center; gap:1rem }
.iconbtn { flex:none; display:grid; place-items:center; width:40px; height:40px; border-radius:10px;
  background:var(--surface0); color:var(--subtext1); border:1px solid var(--surface1); cursor:pointer }
.iconbtn:hover { color:var(--crust); background:var(--accent); border-color:var(--accent); text-decoration:none }
.iconbtn svg { width:18px; height:18px }
.searchopen { display:none }
.topbar nav { display:flex; gap:1.1rem; align-items:center }
.topbar nav a { display:inline-flex; align-items:center; gap:.4rem; color:var(--subtext1); font-weight:600 }
.topbar nav a:hover { color:var(--text); text-decoration:none }
.topbar nav a svg { width:17px; height:17px }

main { max-width:1240px; margin:0 auto; padding:1.4rem 1.1rem 3rem }
.pagetitle { font-size:1.6rem; margin:.2rem 0 1rem }
.modhead { display:flex; align-items:baseline; justify-content:space-between; gap:1rem; flex-wrap:wrap }
.block { margin:1.8rem 0; scroll-margin-top:72px }
.blockhead { display:flex; align-items:baseline; justify-content:space-between; margin-bottom:.8rem }
.blockhead h2 { margin:0; font-size:1.25rem }
.more { font-size:.85rem; font-weight:600 }
.crumbs { color:var(--subtext0); font-size:.85rem; margin-bottom:.6rem }
.crumbs a { color:var(--subtext1) }
.creatorhead { display:flex; flex-wrap:wrap; align-items:center; gap:.7rem; margin-bottom:1.2rem }
.creatorhead h1 { margin:0; font-size:1.6rem }

.grid { display:grid; grid-template-columns:repeat(auto-fill,minmax(170px,1fr)); gap:14px }
.grid.wide { grid-template-columns:repeat(auto-fill,minmax(230px,1fr)) }
.card { display:flex; flex-direction:column; background:var(--surface0); border:1px solid var(--surface0);
  border-radius:10px; overflow:hidden; color:var(--text);
  transition:transform .12s ease, border-color .12s ease, box-shadow .12s ease }
.card:hover { transform:translateY(-3px); border-color:var(--accent); box-shadow:0 8px 24px #0006; text-decoration:none }
.cardthumb { position:relative; aspect-ratio:3/4; background:var(--crust); overflow:hidden }
.cardthumb.banner { aspect-ratio:16/10 }
.cover { width:100%; height:100%; object-fit:cover }
.placeholder { position:absolute; inset:0; display:flex; flex-direction:column; gap:.3rem; align-items:center;
  justify-content:center; color:var(--overlay1); font-size:.72rem; letter-spacing:.08em;
  background:linear-gradient(135deg,var(--surface0),var(--crust)) }
.playbadge { position:absolute; top:8px; left:8px; padding:.15rem .45rem; border-radius:6px;
  background:#000a; color:#fff; font-size:.65rem; font-weight:700; letter-spacing:.06em }
.playbadge::before { content:'VIDEO' }
.cardmeta { padding:.55rem .6rem .65rem }
.cardtitle { font-weight:600; font-size:.9rem; display:-webkit-box; -webkit-line-clamp:2;
  -webkit-box-orient:vertical; overflow:hidden }
.cardsub { margin-top:.3rem; color:var(--subtext0); font-size:.76rem }
.cardsub .strong { color:var(--subtext1); font-weight:600 }

.chip { display:inline-block; padding:.12rem .5rem; border-radius:8px; background:var(--surface1);
  color:var(--subtext1); font-size:.72rem; font-weight:600 }
.chip.platform { background:color-mix(in srgb, var(--accent) 22%, var(--surface1)); color:var(--text) }
.chip.danger { background:color-mix(in srgb, var(--red) 26%, var(--surface1)); color:var(--text); font-weight:700 }
.chip.ok { background:color-mix(in srgb, var(--green) 22%, var(--surface1)); color:var(--text) }
.chip.paid { background:color-mix(in srgb, var(--yellow) 26%, var(--surface1)); color:var(--text) }

.filters { display:flex; flex-wrap:wrap; align-items:center; gap:.6rem; margin:0 0 1rem }
.searchfield { position:relative; display:flex; align-items:center; flex:1; min-width:220px }
.searchfield .sicon { position:absolute; left:.75rem; display:grid; place-items:center; color:var(--subtext0); pointer-events:none }
.searchfield .sicon svg { width:17px; height:17px }
.searchfield input { flex:1; height:40px; padding:0 .9rem 0 2.3rem; border-radius:10px; border:1px solid var(--surface1); background:var(--surface0); color:var(--text) }
.searchfield input:focus { outline:none; border-color:var(--accent) }
.filtersel { position:relative; display:inline-flex }
.filtersel select { height:40px; padding:0 2.1rem 0 .85rem; border-radius:10px; border:1px solid var(--surface1); background:var(--surface0); color:var(--text); font-weight:600; appearance:none; -webkit-appearance:none; cursor:pointer }
.filtersel select:focus { outline:none; border-color:var(--accent) }
.filtersel::after { content:''; position:absolute; right:.9rem; top:50%; width:.5rem; height:.5rem; border-right:2px solid var(--subtext0); border-bottom:2px solid var(--subtext0); transform:translateY(-70%) rotate(45deg); pointer-events:none }
.dirtoggle { flex:none; display:grid; place-items:center; width:40px; height:40px; border-radius:10px; background:var(--surface0); border:1px solid var(--surface1); color:var(--subtext1) }
.dirtoggle:hover { background:var(--accent); color:var(--crust); border-color:var(--accent); text-decoration:none }
.dirtoggle svg { width:18px; height:18px }
.tabs { display:inline-flex; gap:.3rem; background:var(--surface0); padding:.25rem; border-radius:10px; margin-bottom:.9rem }
.tab { padding:.4rem .9rem; border-radius:8px; color:var(--subtext1); font-weight:600; font-size:.9rem }
.tab:hover { color:var(--text); text-decoration:none }
.tab.active { background:var(--accent); color:var(--crust) }

.btn { display:inline-block; padding:.5rem .9rem; border-radius:10px; background:var(--accent);
  color:var(--crust); font-weight:700; border:none; cursor:pointer }
.btn:hover { filter:brightness(1.08); text-decoration:none }
.btn.ghost { background:var(--surface0); color:var(--text); border:1px solid var(--surface1) }
.btn.ghost.off { opacity:.4; pointer-events:none }
.search button, .takedown button { padding:.5rem .8rem; border-radius:10px; background:var(--accent);
  color:var(--crust); font-weight:700; border:none; cursor:pointer }
.pager { display:flex; gap:1rem; align-items:center; justify-content:center; margin:1.1rem 0 }
.pager.top { margin:.2rem 0 1.2rem }

.welcome { display:flex; gap:1.4rem; align-items:center; background:var(--mantle); border:1px solid var(--surface0);
  border-radius:12px; padding:1.4rem 1.6rem; margin-bottom:1.6rem }
.mascot { width:150px; height:auto; border-radius:10px; flex:none }
.welcometext h1 { margin:0 0 .3rem }
.tagline { color:var(--subtext0); margin:.2rem 0 .6rem }
.welcomebody { color:var(--subtext1) }

.posthead { display:flex; align-items:center; gap:1rem; margin:.4rem 0 1rem }
.postheadmid { flex:1; min-width:0; text-align:center }
.posttitle { margin:0; font-size:1.4rem; overflow-wrap:anywhere }
.postmeta { margin:.2rem 0 0; color:var(--subtext0) }
.postmeta a { color:var(--subtext1); font-weight:600 }
.pnav { flex:none; max-width:22%; display:inline-flex; align-items:center; gap:.35rem; color:var(--subtext1); font-weight:600;
  background:var(--surface0); border:1px solid var(--surface1); padding:.45rem .7rem; border-radius:10px }
.pnav svg { flex:none }
.pnav .ptitle { overflow:hidden; text-overflow:ellipsis; white-space:nowrap }
.pnav:hover { border-color:var(--accent); text-decoration:none }
.pnav.off { visibility:hidden }
.carousel { position:relative; display:flex; align-items:center; justify-content:center; gap:.6rem; margin:.4rem auto 1.4rem; max-width:960px }
.cstage { flex:1; display:flex; align-items:center; justify-content:center; height:min(70vh,760px); background:var(--crust); border-radius:12px; overflow:hidden }
.cmedia { max-width:100%; max-height:100%; object-fit:contain; display:block }
.cload { color:var(--subtext0); font-size:.9rem; letter-spacing:.04em }
.cprev, .cnext { flex:none; width:44px; height:44px; border-radius:10px; border:1px solid var(--surface1); background:var(--surface0); color:var(--text); display:grid; place-items:center; cursor:pointer }
.cprev:hover, .cnext:hover { background:var(--accent); color:var(--crust); border-color:var(--accent) }
.ccount { position:absolute; bottom:12px; left:50%; transform:translateX(-50%); background:#000a; color:#fff; padding:.15rem .65rem; border-radius:8px; font-size:.75rem }
.lightbox { position:fixed; inset:0; z-index:100; background:#000e }
.lightbox[hidden] { display:none }
.lbstage { position:absolute; inset:0; overflow:auto; -webkit-overflow-scrolling:touch; display:flex; align-items:safe center; justify-content:safe center }
.lbimg { display:block; max-width:none; cursor:zoom-in; user-select:none; -webkit-user-drag:none }
.lbvideo { max-width:100%; max-height:100%; display:block }
.lbclose { position:fixed; top:14px; right:16px; width:42px; height:42px; border-radius:10px; border:1px solid var(--surface1); background:#000a; color:#fff; display:grid; place-items:center; cursor:pointer }
.lbclose:hover { background:var(--accent); color:var(--crust); border-color:var(--accent) }
.lbnav { position:fixed; top:50%; transform:translateY(-50%); width:48px; height:48px; border-radius:10px; border:1px solid var(--surface1); background:#000a; color:#fff; display:grid; place-items:center; cursor:pointer; z-index:2 }
.lbnav[hidden] { display:none }
.lbprev { left:14px }
.lbnext { right:14px }
.lbnav:hover { background:var(--accent); color:var(--crust); border-color:var(--accent) }
.lbcount { position:fixed; bottom:16px; left:50%; transform:translateX(-50%); background:#000a; color:#fff; padding:.2rem .7rem; border-radius:8px; font-size:.8rem; z-index:2 }
.lbcount[hidden] { display:none }
.body { margin:1rem auto 0; max-width:720px; color:var(--subtext1); white-space:pre-wrap; overflow-wrap:anywhere }
.reportbox { max-width:720px; margin:1.2rem auto 0 }
.reportbox summary { cursor:pointer; color:var(--subtext0); font-size:.85rem; list-style:none }
.reportbox form { display:flex; gap:.5rem; align-items:center; flex-wrap:wrap; margin-top:.6rem }
.reportbox select { padding:.35rem .6rem; border-radius:8px; background:var(--surface0); color:var(--text); border:1px solid var(--surface1) }
.reportbox button { padding:.35rem .7rem; border-radius:8px; background:var(--accent); color:var(--crust); border:none; cursor:pointer; font-weight:600 }
.helpbox { max-width:720px; margin:.8rem 0 1.2rem }
.helpbox summary { cursor:pointer; color:var(--accent); font-weight:600; list-style:none }
.helpbox summary::-webkit-details-marker { display:none }
.helpbox[open] summary { margin-bottom:.4rem }
.reported { max-width:720px; margin:1.2rem auto 0; color:#a6e3a1; font-size:.9rem }
.hp { position:absolute; left:-9999px; width:1px; height:1px; opacity:0 }
li.report.csam { border-left:3px solid var(--red); padding-left:.6rem }
.qblock { border:1px solid var(--surface1); border-radius:14px; padding:.9rem 1.1rem; margin:1.1rem 0; background:var(--mantle) }
.qhead { display:flex; align-items:center; justify-content:space-between; gap:1rem; flex-wrap:wrap; padding-bottom:.7rem; margin-bottom:.2rem; border-bottom:1px solid var(--surface0) }
.qauthorrow { display:flex; align-items:center; justify-content:space-between; gap:1rem; flex-wrap:wrap; padding:.7rem 0 }
.qauthorrow + .qauthorrow { border-top:1px solid var(--surface0) }
.who { display:flex; align-items:center; gap:.55rem; flex-wrap:wrap; min-width:0 }
.pager { display:flex; align-items:center; justify-content:center; gap:1.2rem; margin:1.5rem 0 }
.modbar { display:flex; align-items:center; gap:.5rem; flex-wrap:wrap; max-width:720px; margin:1rem auto 0; padding:.5rem .7rem; border:1px solid var(--surface1); border-radius:10px; background:var(--mantle) }
.viewlink { display:inline-block; padding:.45rem .9rem; border-radius:9px; border:1px solid var(--surface1); background:var(--surface0); color:var(--text); font-weight:600; font-size:.85rem }
.viewlink:hover { border-color:var(--accent); text-decoration:none }
.filelist { list-style:none; padding:0; margin:.5rem 0 }
.filelist li { padding:.4rem 0; border-bottom:1px solid var(--surface0) }
.filelink { font-weight:600 }
.postgroup { margin:1.1rem 0 }
.postgroup h3 { margin:.2rem 0; font-size:1.05rem }
.statusbar { display:flex; align-items:center; gap:.8rem; flex-wrap:wrap; padding:.6rem .9rem; border-radius:10px; margin:1rem 0; background:var(--surface0); border:1px solid var(--surface1); font-weight:600 }
.statusbar.ok { background:color-mix(in srgb, var(--green) 16%, var(--mantle)); border-color:transparent }
.statusbar.danger { background:color-mix(in srgb, var(--red) 16%, var(--mantle)); border-color:transparent }
.body img { max-width:100%; border-radius:10px }

.foot { border-top:1px solid var(--surface0); margin-top:2.5rem; padding:2rem 1.1rem }
.footinner { max-width:1240px; margin:0 auto; display:flex; flex-direction:column; align-items:center; gap:1.1rem; text-align:center }
.social { display:flex; flex-wrap:wrap; gap:.6rem; justify-content:center }
.soc { display:inline-flex; align-items:center; gap:.45rem; padding:.5rem .85rem; border-radius:8px; border:1px solid var(--surface1); background:var(--surface0); color:var(--subtext1); font-weight:600; font-size:.9rem }
.soc:hover { border-color:var(--accent); color:var(--text); text-decoration:none }
.soc svg { width:18px; height:18px; flex:none }
.footlinks { display:flex; gap:1.3rem; justify-content:center; flex-wrap:wrap }
.footlinks a { display:inline-flex; align-items:center; gap:.4rem; color:var(--subtext1); font-weight:600 }
.footlinks a:hover { color:var(--text); text-decoration:none }
.footlinks svg { width:16px; height:16px; flex:none }
.small { font-size:.8rem; color:var(--subtext0) }
.aboutblock { margin:1rem 0 1.5rem; color:var(--subtext1) }
.aboutblock a { color:var(--accent) }

.muted { color:var(--subtext0) }
.strong { color:var(--subtext1); font-weight:600 }
.error { color:var(--red) }
.list { list-style:none; padding:0 }
.list li { padding:.5rem 0; border-bottom:1px solid var(--surface0) }
.stats { display:grid; grid-template-columns:repeat(auto-fit,minmax(8rem,1fr)); gap:.75rem; margin:1rem 0 1.5rem }
.stat { border:1px solid var(--surface0); background:var(--mantle); border-radius:10px; padding:1rem }
.stat.statlink { color:var(--text); text-decoration:none; transition:border-color .1s }
.stat.statlink:hover { border-color:var(--accent); text-decoration:none }
.stat .num { font-size:1.7rem; font-weight:800 }
.stat .num.danger { color:var(--red) }
.stat .label { color:var(--subtext0); font-size:.85em }
.steps { list-style:none; counter-reset:step; padding:0 }
.step { counter-increment:step; border:1px solid var(--surface0); background:var(--mantle); border-radius:10px; padding:1.1rem 1.3rem; margin:1rem 0 }
.step h3 { margin:0 0 .5rem }
.step h3::before { content:counter(step) '. '; color:var(--accent); font-weight:800 }
.step img { max-width:100%; border-radius:8px; margin-top:.6rem }
.downloads { display:flex; flex-wrap:wrap; gap:.5rem; margin:.75rem 0 }
.modform { display:inline-block; margin:0 }
.modform button { padding:.45rem .9rem; border-radius:9px; border:1px solid var(--surface1); background:var(--surface0); color:var(--text); cursor:pointer; font-weight:600; font-size:.85rem }
.modform button:hover { filter:brightness(1.12) }
.modform button.ok { background:var(--green); color:var(--crust); border-color:transparent }
.modform button.danger { background:var(--red); color:var(--crust); border-color:transparent }
.rows li { display:flex; align-items:center; gap:.9rem; flex-wrap:wrap; padding:.7rem 0 }
.rowmain { flex:1 1 15rem; min-width:0 }
.rowtitle { font-weight:600; overflow-wrap:anywhere }
.rowmeta { font-size:.9rem; margin-top:.1rem }
.rowactions { display:flex; gap:.5rem; flex-wrap:wrap; align-items:center; margin-left:auto }
.takedown { display:flex; flex-wrap:wrap; gap:.4rem; margin:.6rem 0 1rem }
.takedown input, .takedown select { flex:1 1 12rem; padding:.5rem .7rem; border-radius:9px; border:1px solid var(--surface1); background:var(--surface0); color:var(--text) }
code { background:var(--surface0); padding:.1rem .35rem; border-radius:5px; word-break:break-all; font-size:.85em }
pre { white-space:pre-wrap; word-break:break-all; background:var(--mantle); border:1px solid var(--surface0); padding:.7rem .9rem; border-radius:10px }

@media (max-width:720px) {
  main { padding:1.1rem .8rem 2.5rem }
  .pagetitle { font-size:1.35rem }
  /* top bar: brand + nav on the first row, search full-width on the second */
  .topbar { flex-wrap:wrap; gap:.5rem .6rem; padding:.55rem .7rem }
  .brand { font-size:1rem }
  /* all actions on their own centered row, every one a boxed icon button like search + shuffle */
  .topactions { flex-basis:100%; justify-content:center; gap:.5rem; margin:.15rem 0 0 }
  .searchopen { display:grid }
  .topbar nav { gap:.5rem }
  .topbar nav a span { display:none }
  .topbar nav a { display:grid; place-items:center; width:40px; height:40px; border-radius:10px;
    background:var(--surface0); border:1px solid var(--surface1); gap:0 }
  .topbar nav a:hover { color:var(--crust); background:var(--accent); border-color:var(--accent) }
  .topsearch { display:none; order:3; flex-basis:100%; max-width:none; margin:.15rem 0 0 }
  .searchtoggle:checked ~ .topsearch { display:flex }
  /* grids: more, smaller cards */
  .grid { grid-template-columns:repeat(auto-fill,minmax(140px,1fr)); gap:10px }
  .grid.wide { grid-template-columns:repeat(auto-fill,minmax(150px,1fr)) }
  /* filter bar: search on its own row, the two selects share the next row */
  .filters { gap:.5rem }
  .searchfield { flex-basis:100%; min-width:0 }
  .filtersel { flex:1 }
  .filtersel select { width:100% }
  /* welcome + post header */
  .welcome { flex-direction:column; text-align:center }
  .mascot { width:120px }
  .pnav { max-width:none }
  .pnav span { display:none }
  .posthead { gap:.5rem }
  /* carousel: arrows overlay the image edges so it uses the full width */
  .carousel { gap:0; max-width:100% }
  .cstage { height:min(64vh,620px); border-radius:10px }
  .cprev, .cnext { position:absolute; top:50%; transform:translateY(-50%); z-index:2; width:38px; height:38px; background:#000b; color:#fff; border-color:transparent }
  .cprev { left:8px }
  .cnext { right:8px }
  .lbnav { width:42px; height:42px }
  .soc, .footlinks a { font-size:.85rem }
}

@media (max-width:400px) {
  .grid { grid-template-columns:repeat(2,1fr) }
}
.contribwrap { max-width:640px; margin:0 auto }
.keepwrap { max-width:760px; margin:0 auto }
.contribintro { text-align:center; margin:.6rem 0 1.6rem }
.contribintro h1 { margin:0 0 .5rem; font-size:1.7rem }
.contribintro p { color:var(--subtext0); margin:0 auto; max-width:58ch }
.panel { background:var(--mantle); border:1px solid var(--surface0); border-radius:14px; padding:1.3rem 1.5rem; margin:1rem 0 }
.panel h3 { margin:0 0 .5rem }
.panel pre { margin:.5rem 0 0 }
.panel .cidline { margin:.4rem 0 0; overflow-wrap:anywhere }
.warnbox { display:flex; gap:.75rem; align-items:flex-start; margin:1rem 0; padding:.95rem 1.15rem; border-radius:12px;
  background:color-mix(in srgb, var(--yellow) 12%, var(--mantle)); border:1px solid color-mix(in srgb, var(--yellow) 45%, var(--surface0)) }
.warnbox svg { flex:none; width:20px; height:20px; color:var(--yellow); margin-top:.15rem }
.warnbox p { margin:0; color:var(--subtext1) }
.confirm { text-align:center }
.confirm p { margin:.5rem 0 }
.confirm .btn { margin-top:.7rem }
.contribform { display:flex; flex-direction:column; gap:1rem; background:var(--mantle); border:1px solid var(--surface0); border-radius:14px; padding:1.4rem 1.5rem; margin:0 0 .5rem }
.contribform label { display:flex; flex-direction:column; gap:.35rem; font-weight:600 }
.contribform label.check { flex-direction:row; align-items:flex-start; gap:.55rem; font-weight:400; color:var(--subtext1) }
.contribform label.check .hint { color:var(--subtext0) }
.contribform input[type=text], .contribform textarea, .contribform select { padding:.6rem .65rem; border-radius:9px; border:1px solid var(--surface2); background:var(--surface0); color:var(--text); font:inherit }
.contribform textarea { resize:vertical; min-height:2.6rem; font-family:var(--mono, monospace); font-size:.85rem; line-height:1.4; word-break:break-all }
.contribform input[type=text]:focus, .contribform textarea:focus, .contribform select:focus { outline:none; border-color:var(--accent) }
.contribform button { align-self:stretch; padding:.7rem 1.2rem; border-radius:9px; border:0; background:var(--accent); color:var(--crust); font-weight:700; font-size:1rem; cursor:pointer }
.contribform button:hover { filter:brightness(1.08) }
.contribform .formnote { margin:0; font-size:.8rem; font-weight:400; color:var(--subtext0) }
.faq { margin:1.7rem 0 .5rem }
.faq h2 { font-size:1.1rem; margin:0 0 .8rem; text-align:center; color:var(--subtext1); font-weight:700 }
.faqitem { border:1px solid var(--surface0); border-radius:12px; background:var(--mantle); padding:1rem 1.2rem; margin:.7rem 0 }
.faqitem h3 { margin:0 0 .4rem; font-size:1rem; color:var(--accent) }
.faqitem p { margin:.35rem 0; color:var(--subtext1) }
.faqitem ol, .faqitem ul { margin:.5rem 0 .2rem; padding-left:1.2rem; color:var(--subtext1) }
.faqitem li { margin:.35rem 0 }
.faqitem code { color:var(--text) }
.cookienames { margin:.5rem 0 .8rem }
.spoiler { border:1px solid var(--surface0); border-radius:10px; background:var(--base); margin:.55rem 0; overflow:hidden }
.spoiler .spoiler { background:var(--mantle) }
.spoiler > summary { cursor:pointer; list-style:none; padding:.65rem .95rem; font-weight:600; color:var(--text); display:flex; align-items:center; justify-content:space-between; gap:.6rem }
.spoiler > summary::-webkit-details-marker { display:none }
.spoiler > summary::after { content:'+'; color:var(--accent); font-weight:800; font-size:1.2rem; line-height:1 }
.spoiler[open] > summary::after { content:'-' }
.spoiler > summary:hover { color:var(--accent) }
.spbody { padding:.1rem .95rem .9rem }
.spbody ol { margin:.2rem 0; padding-left:1.3rem; color:var(--subtext1) }
.spbody li { margin:.55rem 0 }
.spbody strong { color:var(--text); font-weight:600 }
.guideimg { display:block; max-width:100%; border-radius:8px; border:1px solid var(--surface1); margin:.55rem 0 .1rem }
p.err { color:var(--red); font-weight:600; text-align:center }
";

// on mobile the search field is collapsed behind an icon; focus it the moment it opens
const SHELL_JS: &str = "
const st = document.getElementById('searchtoggle')
if (st) st.addEventListener('change', () => { if (st.checked) { const i = document.querySelector('.topsearch input'); if (i) i.focus() } })
";

const CAROUSEL_JS: &str = "
// lightbox: click a carousel image to open it fullscreen. click toggles fit/100%, the wheel zooms around the
// cursor, drag pans, and prev/next (buttons or arrow keys) walk the whole post at full size. built once, shared
const lb = document.createElement('div'); lb.className = 'lightbox'; lb.hidden = true
const lbstage = document.createElement('div'); lbstage.className = 'lbstage'
const mkbtn = (cls, label, svg) => { const btn = document.createElement('button'); btn.className = cls; btn.setAttribute('aria-label', label); btn.innerHTML = svg; return btn }
const chevron = (d) => `<svg viewBox='0 0 24 24' width='26' height='26' fill='none' stroke='currentColor' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'><path d='${d}'/></svg>`
const lbprev = mkbtn('lbnav lbprev', 'Previous', chevron('M15 18l-6-6 6-6'))
const lbnext = mkbtn('lbnav lbnext', 'Next', chevron('M9 18l6-6-6-6'))
const lbcount = document.createElement('div'); lbcount.className = 'lbcount'
const lbclose = mkbtn('lbclose', 'Close', `<svg viewBox='0 0 24 24' width='20' height='20' fill='none' stroke='currentColor' stroke-width='2' stroke-linecap='round'><path d='M6 6l12 12M18 6L6 18'/></svg>`)
lb.append(lbstage, lbprev, lbnext, lbcount, lbclose); document.body.appendChild(lb)

let lbItems = [], lbIdx = 0, lbSync = null, lbimg = null, natW = 0, natH = 0, scale = 1
const lbCache = {}
const fitScale = () => Math.min(window.innerWidth / natW, window.innerHeight / natH, 1)
const centre = () => { lbstage.scrollLeft = (lbstage.scrollWidth - lbstage.clientWidth) / 2; lbstage.scrollTop = (lbstage.scrollHeight - lbstage.clientHeight) / 2 }
const apply = (s) => { scale = Math.max(0.08, Math.min(s, 8)); if (lbimg) lbimg.style.width = (natW * scale) + 'px' }
const showImage = (img) => { natW = img.naturalWidth; natH = img.naturalHeight; lbimg = img; lbstage.replaceChildren(img); apply(fitScale()); centre() }
const showLbItem = (idx) => {
  lbIdx = (idx + lbItems.length) % lbItems.length
  if (lbSync) lbSync(lbIdx)
  lbcount.textContent = (lbIdx + 1) + ' / ' + lbItems.length
  const it = lbItems[lbIdx]
  lbimg = null
  if (it.v) {
    const v = document.createElement('video'); v.className = 'lbvideo'; v.controls = true; v.src = it.u
    lbstage.replaceChildren(v)
    return
  }
  const cached = lbCache[it.u]
  if (cached && cached.complete && cached.naturalWidth > 0) { showImage(cached); return }
  const load = document.createElement('div'); load.className = 'cload'; load.textContent = 'Loading...'
  lbstage.replaceChildren(load)
  const img = cached || new Image(); img.className = 'lbimg'; img.alt = ''
  const at = lbIdx
  img.onload = () => { if (at === lbIdx) showImage(img) }
  img.onerror = () => { if (at === lbIdx) load.textContent = 'unavailable - no seeders online right now' }
  if (!cached) { lbCache[it.u] = img; img.src = it.u }
}
const openLightbox = (items, idx, sync) => {
  lbItems = items; lbSync = sync || null
  const multi = items.length > 1
  lbprev.hidden = !multi; lbnext.hidden = !multi; lbcount.hidden = !multi
  lb.hidden = false; document.body.style.overflow = 'hidden'
  showLbItem(idx)
}
const closeLightbox = () => { lb.hidden = true; lbstage.replaceChildren(); lbimg = null; document.body.style.overflow = '' }
lbclose.addEventListener('click', closeLightbox)
lbprev.addEventListener('click', (e) => { e.stopPropagation(); showLbItem(lbIdx - 1) })
lbnext.addEventListener('click', (e) => { e.stopPropagation(); showLbItem(lbIdx + 1) })
lb.addEventListener('click', (e) => { if (e.target === lb || e.target === lbstage) closeLightbox() })
lbstage.addEventListener('click', (e) => { if (e.target === lbimg) { apply(Math.abs(scale - 1) < 0.01 ? fitScale() : 1); centre() } })
lbstage.addEventListener('wheel', (e) => {
  if (lb.hidden || !lbimg) return
  e.preventDefault()
  const rect = lbstage.getBoundingClientRect()
  const ax = e.clientX - rect.left + lbstage.scrollLeft
  const ay = e.clientY - rect.top + lbstage.scrollTop
  const prev = scale
  apply(scale * (e.deltaY < 0 ? 1.15 : 0.87))
  const r = scale / prev
  lbstage.scrollLeft = ax * r - (e.clientX - rect.left)
  lbstage.scrollTop = ay * r - (e.clientY - rect.top)
}, { passive: false })
let drag = false, dx = 0, dy = 0, dl = 0, dt = 0
lbstage.addEventListener('mousedown', (e) => { if (e.target !== lbimg) return; drag = true; dx = e.clientX; dy = e.clientY; dl = lbstage.scrollLeft; dt = lbstage.scrollTop; e.preventDefault() })
window.addEventListener('mousemove', (e) => { if (!drag) return; lbstage.scrollLeft = dl - (e.clientX - dx); lbstage.scrollTop = dt - (e.clientY - dy) })
window.addEventListener('mouseup', () => { drag = false })
window.addEventListener('keydown', (e) => {
  if (lb.hidden) return
  if (e.key === 'Escape') closeLightbox()
  else if (e.key === 'ArrowLeft') showLbItem(lbIdx - 1)
  else if (e.key === 'ArrowRight') showLbItem(lbIdx + 1)
})
// touch: a horizontal swipe navigates, but only when the image is not zoomed wider than the screen, so
// panning a zoomed image (native scroll) is never hijacked
let lsx = 0, lsy = 0
lbstage.addEventListener('touchstart', (e) => { lsx = e.changedTouches[0].clientX; lsy = e.changedTouches[0].clientY }, { passive: true })
lbstage.addEventListener('touchend', (e) => {
  const dx = e.changedTouches[0].clientX - lsx, dy = e.changedTouches[0].clientY - lsy
  if (Math.abs(dx) > 50 && Math.abs(dx) > Math.abs(dy) * 1.5 && lbstage.scrollWidth <= lbstage.clientWidth + 2) {
    showLbItem(lbIdx + (dx < 0 ? 1 : -1))
  }
}, { passive: true })

// full media pulled from the gateway, built once per item and kept, so returning to an already-loaded item
// shows it instantly with no flicker; the fixed-size stage shows a loading state only while still fetching
for (const el of document.querySelectorAll('.carousel')) {
  let items = []
  try { items = JSON.parse(el.dataset.items || '[]') } catch (e) {}
  const stage = el.querySelector('.cstage')
  const count = el.querySelector('.ccount')
  const nodes = []
  let cur = 0
  const ready = (n) => n && (n.tagName === 'VIDEO' ? n.readyState >= 2 : n.complete && n.naturalWidth > 0)
  const render = (idx) => {
    const n = nodes[idx]
    if (n && n.dataset.failed) {
      const p = document.createElement('p'); p.className = 'muted'
      p.textContent = 'unavailable - no seeders online right now'
      stage.replaceChildren(p)
    } else if (ready(n)) {
      stage.replaceChildren(n)
    } else {
      const load = document.createElement('div'); load.className = 'cload'; load.textContent = 'Loading...'
      stage.replaceChildren(load)
    }
  }
  const build = (idx) => {
    const it = items[idx]
    let n
    if (it.v) { n = document.createElement('video'); n.controls = true; n.preload = 'metadata' }
    else { n = new Image(); n.alt = ''; n.style.cursor = 'zoom-in'; n.title = 'click to view full size'; n.addEventListener('click', () => openLightbox(items, idx, show)) }
    n.className = 'cmedia'
    const settle = () => { if (cur === idx) render(idx) }
    if (it.v) n.onloadeddata = settle; else n.onload = settle
    n.onerror = () => { n.dataset.failed = '1'; if (cur === idx) render(idx) }
    n.src = it.u
    nodes[idx] = n
  }
  const show = (n) => {
    cur = (n + items.length) % items.length
    if (count) count.textContent = (cur + 1) + ' / ' + items.length
    if (!nodes[cur]) build(cur)
    render(cur)
  }
  el.querySelector('.cprev')?.addEventListener('click', () => show(cur - 1))
  el.querySelector('.cnext')?.addEventListener('click', () => show(cur + 1))
  if (items.length > 1) {
    document.addEventListener('keydown', (e) => {
      if (!lb.hidden) return
      if (e.key === 'ArrowLeft') show(cur - 1)
      else if (e.key === 'ArrowRight') show(cur + 1)
    })
    let sx = 0
    el.addEventListener('touchstart', (e) => { sx = e.changedTouches[0].clientX }, { passive: true })
    el.addEventListener('touchend', (e) => {
      const dx = e.changedTouches[0].clientX - sx
      if (Math.abs(dx) > 40) show(cur + (dx < 0 ? 1 : -1))
    }, { passive: true })
  }
  if (items.length) show(0)
}
";

#[cfg(test)]
mod tests {
    use super::pretty_date;

    #[test]
    fn formats_iso_dates_and_passes_junk_through() {
        assert_eq!(pretty_date("2026-03-14T10:00:00.000+00:00"), "Mar 14, 2026");
        assert_eq!(pretty_date("2026-03-14"), "Mar 14, 2026");
        assert_eq!(pretty_date("not a date"), "not a date");
    }
}
