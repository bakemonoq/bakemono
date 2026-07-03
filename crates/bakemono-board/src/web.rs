use std::io::SeekFrom;
use std::net::{IpAddr, SocketAddr};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Body;
use axum::extract::{ConnectInfo, Form, FromRef, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Json, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use chrono::Utc;
use maud::{html, Markup, PreEscaped, DOCTYPE};
use nostr_sdk::prelude::{Client, Event, Keys, PublicKey, ToBech32};
use sqlx::postgres::PgPool;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use bakemono_core::{Takedown, Target};

use crate::config;
use crate::db;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub relays: Vec<String>,
    pub signer: Option<Keys>,
    pub gateway: Arc<bakemono_torrent::Gateway>,
    pub cold_limiter: Arc<crate::ratelimit::ColdLimiter>,
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
        .route("/posts", get(posts_index))
        .route("/creators", get(creators_index))
        .route("/search", get(search_index))
        .route("/random", get(random_redirect))
        .route("/feed.xml", get(seed_feed))
        .route("/contribute", get(contribute))
        .route("/keepers", get(keepers))
        .route("/info", get(info_page))
        .route("/c/{platform}/{creator_id}", get(creator_page))
        .route("/p/{platform}/{creator_id}/{post_id}", get(post_page))
        .route("/mod", get(mod_queue))
        .route("/mod/queue-approve", post(mod_queue_approve))
        .route("/mod/queue-reject", post(mod_queue_reject))
        .route("/mod/ban-contributor", post(mod_ban_contributor))
        .route("/mod/queue/{pubkey}/{platform}/{creator_id}", get(mod_queue_group))
        .route("/mod/takedown", post(mod_takedown))
        .route("/mod/untakedown", post(mod_untakedown))
        .route("/report", post(submit_report))
        .route("/mod/report-dismiss", post(mod_report_dismiss))
        .route("/mod/ban-post", post(mod_ban_post))
        .route("/mod/ban-creator", post(mod_ban_creator))
        .route("/mod/pubkey/{pubkey}", get(mod_pubkey_view))
        .route("/mod/post/{platform}/{creator_id}/{post_id}", get(mod_post_view))
        .route("/mod/author/{platform}/{creator_id}", get(mod_author_view))
        .route("/t/{infohash}/meta", get(gateway_meta))
        .route("/t/{infohash}/f/{file_index}", get(gateway_file))
        .with_state(state)
}

// how many cards a browse page shows; one extra is fetched to detect a next page without a count query
const PAGE: i64 = 60;

async fn home(State(pool): State<PgPool>) -> Html<String> {
    // 12 keeps Recent to two rows on a wide screen
    let posts = db::list_posts(&pool, "", db::SortField::Created, true, "", 12, 0)
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
    let (q, source, sort, desc, page) = query.parts();
    let platforms = db::platforms(&pool).await.unwrap_or_default();
    let mut posts = db::list_posts(&pool, &source, sort, desc, &q, PAGE + 1, page * PAGE)
        .await
        .unwrap_or_default();
    let has_next = posts.len() as i64 > PAGE;
    posts.truncate(PAGE as usize);
    render(
        "posts",
        html! {
            h1.pagetitle { "Posts" }
            (filter_bar("/posts", &q, &source, sort, desc, &platforms, None))
            (pager("/posts", &q, &source, sort, desc, None, page, has_next, true))
            @if posts.is_empty() { p.muted { "No posts match" } }
            (posts_grid(&posts))
            (pager("/posts", &q, &source, sort, desc, None, page, has_next, false))
        },
    )
}

async fn creators_index(
    State(pool): State<PgPool>,
    Query(query): Query<BrowseQuery>,
) -> Html<String> {
    let (q, source, sort, desc, page) = query.parts();
    let platforms = db::platforms(&pool).await.unwrap_or_default();
    let mut creators = db::list_creators(&pool, &source, sort, desc, &q, PAGE + 1, page * PAGE)
        .await
        .unwrap_or_default();
    let has_next = creators.len() as i64 > PAGE;
    creators.truncate(PAGE as usize);
    render(
        "creators",
        html! {
            h1.pagetitle { "Creators" }
            (filter_bar("/creators", &q, &source, sort, desc, &platforms, None))
            (pager("/creators", &q, &source, sort, desc, None, page, has_next, true))
            @if creators.is_empty() { p.muted { "No creators match" } }
            (creators_grid(&creators))
            (pager("/creators", &q, &source, sort, desc, None, page, has_next, false))
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
        posts = db::list_posts(&pool, &source, sort, desc, &q, PAGE + 1, page * PAGE)
            .await
            .unwrap_or_default();
        has_next = posts.len() as i64 > PAGE;
        posts.truncate(PAGE as usize);
    }

    render(
        "search",
        html! {
            h1.pagetitle { "Search" }
            div.tabs {
                a.tab.active[!creators_tab] href=(browse_url("/search", &q, &source, sort, desc, Some("posts"), 0)) { "Posts" }
                a.tab.active[creators_tab] href=(browse_url("/search", &q, &source, sort, desc, Some("creators"), 0)) { "Creators" }
            }
            (filter_bar("/search", &q, &source, sort, desc, &platforms, Some(tab)))
            @if !q.is_empty() { (pager("/search", &q, &source, sort, desc, Some(tab), page, has_next, true)) }
            @if q.is_empty() {
                p.muted { "Type something to search posts and creators" }
            } @else if creators_tab {
                @if creators.is_empty() { p.muted { "No creators match \"" (q) "\"" } }
                (creators_grid(&creators))
            } @else {
                @if posts.is_empty() { p.muted { "No posts match \"" (q) "\"" } }
                (posts_grid(&posts))
            }
            @if !q.is_empty() { (pager("/search", &q, &source, sort, desc, Some(tab), page, has_next, false)) }
        },
    )
}

#[derive(serde::Deserialize)]
struct SearchQuery {
    q: Option<String>,
    source: Option<String>,
    sort: Option<String>,
    dir: Option<String>,
    tab: Option<String>,
    page: Option<i64>,
}

#[derive(serde::Deserialize)]
struct BrowseQuery {
    q: Option<String>,
    source: Option<String>,
    sort: Option<String>,
    dir: Option<String>,
    page: Option<i64>,
}

impl BrowseQuery {
    // (query, source, sort field, desc, page); desc unless dir=asc
    fn parts(self) -> (String, String, db::SortField, bool, i64) {
        (
            self.q.unwrap_or_default().trim().to_string(),
            self.source.unwrap_or_default().trim().to_string(),
            db::SortField::parse(self.sort.as_deref()),
            self.dir.as_deref() != Some("asc"),
            self.page.unwrap_or(0).max(0),
        )
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
    platforms: &[String],
    tab: Option<&str>,
) -> Markup {
    html! {
        form.filters method="get" action=(base) {
            @if let Some(t) = tab { input type="hidden" name="tab" value=(t); }
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
            a.dirtoggle href=(browse_url(base, q, source, sort, !desc, tab, 0))
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
    tab: Option<&str>,
    page: i64,
    has_next: bool,
    top: bool,
) -> Markup {
    html! {
        @if page > 0 || has_next {
            div.pager.top[top] {
                @if page > 0 {
                    a.btn.ghost href=(browse_url(base, q, source, sort, desc, tab, page - 1)) { "prev" }
                } @else {
                    span.btn.ghost.off { "prev" }
                }
                span.muted { "page " (page + 1) }
                @if has_next {
                    a.btn.ghost href=(browse_url(base, q, source, sort, desc, tab, page + 1)) { "next" }
                } @else {
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

// standard torrent RSS: point a client's auto-download rule (qBittorrent, Deluge, ruTorrent) at this and it
// adds + seeds every new magnet, so any commodity client can help keep content alive with no bakemono software
const DEFAULT_FEED: i64 = 200;
const MAX_FEED: i64 = 1000;

async fn seed_feed(
    State(pool): State<PgPool>,
    Query(q): Query<FeedQuery>,
    headers: HeaderMap,
) -> Response {
    let base = base_url(&headers);
    let limit = q.limit.unwrap_or(DEFAULT_FEED).clamp(1, MAX_FEED);
    let (self_href, next_href, items_xml) = if q.sort.as_deref() == Some("endangered") {
        endangered_feed(&pool, &base, limit).await
    } else {
        catalog_feed(&pool, &base, limit, &q).await
    };
    let xml = build_feed(&base, &self_href, next_href.as_deref(), &items_xml);
    (
        [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
        xml,
    )
        .into_response()
}

#[derive(serde::Deserialize)]
struct FeedQuery {
    limit: Option<i64>,
    before: Option<i64>,
    before_id: Option<String>,
    sort: Option<String>,
    platform: Option<String>,
    creator: Option<String>,
    post: Option<String>,
    npub: Option<String>,
}

impl FeedQuery {
    fn scope(&self) -> db::FeedScope {
        db::FeedScope {
            platform: self.platform.clone(),
            creator_id: self.creator.clone(),
            post_id: self.post.clone(),
            // accept hex or npub; a malformed key filters to nothing rather than 500ing
            pubkey: self
                .npub
                .as_deref()
                .map(|s| PublicKey::parse(s).map(|p| p.to_hex()).unwrap_or_default()),
        }
    }
}

// the default feed: newest torrents in this scope, with a cursor so a mirror can page back through it all
async fn catalog_feed(
    pool: &PgPool,
    base: &str,
    limit: i64,
    q: &FeedQuery,
) -> (String, Option<String>, String) {
    let scope = q.scope();
    let scope_qs = scope_query(&scope);
    let rows = db::feed(pool, limit, q.before, q.before_id.as_deref(), &scope)
        .await
        .unwrap_or_default();
    let items = rows.iter().map(|m| feed_item(base, m)).collect();
    let self_href = format!("{base}/feed.xml?limit={limit}{scope_qs}");
    // a full page means older torrents remain: hand out the cursor to the next page of this same slice
    let next = (rows.len() as i64 == limit)
        .then(|| rows.last())
        .flatten()
        .map(|last| {
            format!(
                "{base}/feed.xml?before={}&before_id={}&limit={limit}{scope_qs}",
                last.created_at,
                qs_encode(&last.event_id)
            )
        });
    (self_href, next, items)
}

// the keeper work list: fewest-seeded torrents first. no cursor - it is a priority list, not a full mirror
async fn endangered_feed(pool: &PgPool, base: &str, limit: i64) -> (String, Option<String>, String) {
    let rows = db::endangered(pool, limit).await.unwrap_or_default();
    let items = rows.iter().map(|r| endangered_item(base, r)).collect();
    (format!("{base}/feed.xml?sort=endangered&limit={limit}"), None, items)
}

// rebuild the active scope as a query suffix so the self and next links stay inside the same slice
fn scope_query(scope: &db::FeedScope) -> String {
    let mut qs = String::new();
    let mut push = |key: &str, val: &Option<String>| {
        if let Some(v) = val {
            qs.push_str(&format!("&{key}={}", qs_encode(v)));
        }
    };
    push("platform", &scope.platform);
    push("creator", &scope.creator_id);
    push("post", &scope.post_id);
    push("npub", &scope.pubkey);
    qs
}

fn build_feed(base: &str, self_href: &str, next_href: Option<&str>, items_xml: &str) -> String {
    let board = board_name();
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<rss version=\"2.0\" xmlns:atom=\"http://www.w3.org/2005/Atom\">\n<channel>\n");
    out.push_str(&format!("<title>{} - seed feed</title>\n", xml_escape(&board)));
    out.push_str(&format!("<link>{base}/</link>\n"));
    out.push_str(&format!(
        "<atom:link href=\"{}\" rel=\"self\" type=\"application/rss+xml\"/>\n",
        xml_escape(self_href)
    ));
    out.push_str("<description>Torrents to seed. Point your BitTorrent client's RSS auto-download at this feed to help keep content online; follow the next link to page through the whole catalog</description>\n");
    if let Some(next) = next_href {
        out.push_str(&format!(
            "<atom:link rel=\"next\" href=\"{}\"/>\n",
            xml_escape(next)
        ));
    }
    out.push_str(items_xml);
    out.push_str("</channel>\n</rss>\n");
    out
}

fn feed_item(base: &str, m: &db::ManifestRow) -> String {
    let guid = m.infohash.clone().unwrap_or_else(|| m.event_id.clone());
    feed_item_xml(
        base, &m.platform, &m.creator_id, &m.post_id, &m.creator,
        &item_label(m.post_title.as_deref(), m.filename.as_deref(), &m.post_id),
        &guid, m.created_at, &m.magnet, m.size, None,
    )
}

fn endangered_item(base: &str, r: &db::EndangeredRow) -> String {
    let guid = r.infohash.clone().unwrap_or_else(|| r.event_id.clone());
    let note = r.seeders.map(|s| format!("{s} seeder(s)"));
    feed_item_xml(
        base, &r.platform, &r.creator_id, &r.post_id, &r.creator,
        &item_label(r.post_title.as_deref(), r.filename.as_deref(), &r.post_id),
        &guid, r.created_at, &r.magnet, r.size, note.as_deref(),
    )
}

fn item_label(post_title: Option<&str>, filename: Option<&str>, post_id: &str) -> String {
    post_title
        .or(filename)
        .unwrap_or(post_id)
        .to_string()
}

#[allow(clippy::too_many_arguments)]
fn feed_item_xml(
    base: &str,
    platform: &str,
    creator_id: &str,
    post_id: &str,
    creator: &str,
    label: &str,
    guid: &str,
    created_at: i64,
    magnet: &str,
    size: i64,
    note: Option<&str>,
) -> String {
    let title = xml_escape(&format!("{creator} - {label}"));
    let link = format!("{base}/p/{platform}/{creator_id}/{post_id}");
    let desc = note
        .map(|n| format!("<description>{}</description>\n", xml_escape(n)))
        .unwrap_or_default();
    format!(
        "<item>\n<title>{title}</title>\n<link>{link}</link>\n{desc}\
         <guid isPermaLink=\"false\">{guid}</guid>\n<pubDate>{}</pubDate>\n\
         <enclosure url=\"{}\" length=\"{}\" type=\"application/x-bittorrent\"/>\n</item>\n",
        rfc822(created_at),
        xml_escape(magnet),
        size,
    )
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

fn rfc822(unix_secs: i64) -> String {
    match chrono::DateTime::from_timestamp(unix_secs, 0) {
        Some(dt) => dt.to_rfc2822(),
        None => String::new(),
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
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

async fn contribute() -> Html<String> {
    render(
        "contribute",
        html! {
            h2 { "Help grow the archive" }
            p { "Bakemono has no central server full of files. Every preview you see is streamed from someone running the desktop app and sharing what they archived. The more people who run it, the more stays online. Here is how to join in" }
            p.muted { "Your Patreon and other logins never leave your computer. The app opens the real site in a window, you sign in yourself, and only the files you choose are shared - never your account or cookies" }
            ol.steps {
                li.step {
                    h3 { "Get the app" }
                    p { "Pick your system:" }
                    (download_buttons())
                    p.muted {
                        "All builds come from the "
                        a href=(format!("{REPO}/releases/latest")) { "latest release" }
                        ". If a button does not start a download, the newest build may still be uploading - open that page and grab the file by hand"
                    }
                }
                li.step {
                    h3 { "Install and open it" }
                    p { "Run the file you downloaded and follow the installer. On macOS drag Bakemono to Applications, then open it. On Windows confirm the SmartScreen prompt the first time" }
                }
                li.step {
                    h3 { "Sign in to a creator you support" }
                    p { "Inside the app, open the creator site in the built-in window and log in the way you normally would. The session stays on your machine" }
                }
                li.step {
                    h3 { "Pick what to archive" }
                    p { "Choose a creator you subscribe to and start. The app downloads your paid posts and begins sharing them with everyone browsing the board" }
                }
                li.step {
                    h3 { "Leave it running" }
                    p { "Bakemono keeps sharing in the background - close the window and the daemon keeps seeding from the tray. The longer it runs, the more reliably others can preview the files you shared. That is the whole contribution - bytes from your machine to the swarm" }
                }
            }
        },
    )
}

fn download_buttons() -> Markup {
    html! {
        div.downloads {
            @for &(os, asset) in DOWNLOADS {
                a.btn href=(format!("{REPO}/releases/latest/download/{asset}")) { "Download for " (os) }
            }
        }
    }
}

async fn keepers(State(state): State<AppState>, headers: HeaderMap) -> Html<String> {
    let base = base_url(&headers);
    let feed = format!("{base}/feed.xml");
    let backfill = format!(
        "curl -s \"{feed}?limit=1000\" \\\n  | grep -oE 'magnet:[^\"]+' | sed 's/&amp;/\\&/g'"
    );
    let endangered = db::endangered(&state.pool, 30).await.unwrap_or_default();
    render(
        "keepers",
        html! {
            h2 { "Become a keeper" }
            p { "Every file here lives in a BitTorrent swarm, not on the board. When the last person seeding a file goes offline, that file is gone. Keepers are volunteers who adopt part of the archive and keep seeding it, so nothing rests on one machine. The model is borrowed from RuTracker's keepers" }
            p.muted { "No Bakemono software needed. Any BitTorrent client - qBittorrent, Deluge, Transmission - can seed. The board publishes a feed of what to seed and your client does the rest" }

            ol.steps {
                li.step {
                    h3 { "Pick what to keep" }
                    p { "Seed the whole board, or just the creators you care about. The feed is:" }
                    p { code { (feed) } }
                    p { "Narrow it to adopt one slice:" }
                    ul {
                        li { code { (format!("{feed}?platform=patreon&creator=<id>")) } " - one creator" }
                        li { code { (format!("{feed}?npub=<npub>")) } " - one contributor" }
                        li { code { (format!("{feed}?sort=endangered")) } " - whatever is closest to dying" }
                    }
                }
                li.step {
                    h3 { "Point your client at it" }
                    p { "In qBittorrent: View -> RSS, add the feed URL, then add an auto-download rule that matches everything. Deluge uses the YaRSS2 plugin, ruTorrent has an RSS plugin. Your client then adds and seeds every new torrent on its own" }
                }
                li.step {
                    h3 { "Mirror everything (optional)" }
                    p { "Auto-download only catches torrents added from now on. To back-fill the whole archive, walk the feed and hand the magnets to your client:" }
                    pre { code { (backfill) } }
                    p.muted {
                        "Robust walker and per-client setup: "
                        a href=(format!("{REPO}/blob/main/docs/SEEDING.md")) { "docs/SEEDING.md" }
                    }
                }
                li.step {
                    h3 { "Leave it seeding" }
                    p { "That is the whole job. The longer your client stays online, the more resilient the archive. Seeder counts are shown below so you can see where help is needed" }
                }
            }

            h2 { "Endangered now" }
            @if endangered.is_empty() {
                p.muted { "Seeder counts are still being gathered, or everything is healthy. Check back soon" }
            } @else {
                p.muted { "Fewest seeders first - adopt these before they vanish" }
                ul.list {
                    @for e in &endangered {
                        li {
                            a href=(format!("/p/{}/{}/{}", e.platform, e.creator_id, e.post_id)) {
                                (item_label(e.post_title.as_deref(), e.filename.as_deref(), &e.post_id))
                            }
                            span.muted { " " (e.creator) " - " (e.seeders.unwrap_or(0)) " seeder(s) " }
                            a href=(e.magnet) { "magnet" }
                            " "
                            a href=(format!("/feed.xml?platform={}&creator={}", qs_encode(&e.platform), qs_encode(&e.creator_id))) { "adopt creator" }
                        }
                    }
                }
            }
        },
    )
}

async fn info_page(State(state): State<AppState>) -> Html<String> {
    let stats = db::stats(&state.pool).await.unwrap_or_default();
    let board_pubkey = state
        .signer
        .as_ref()
        .map(|k| npub(&k.public_key().to_hex()));
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
                (stat_card(stats.authors, "authors"))
                (stat_card(stats.files, "files"))
                (stat_card(stats.contributors, "contributors"))
            }

            h3 { "Board identity" }
            @match &board_pubkey {
                Some(key) => {
                    p { "Public key to integrate with. Add it to a peer board's trusted instances to honor this board's takedowns:" }
                    p { code { (key) } }
                }
                None => p.muted { "This board has not published an instance key yet" }
            }

            h3 { "Relays" }
            p.muted { "Manifests are indexed from and takedowns published to:" }
            ul.list {
                @for relay in &state.relays {
                    li { code { (relay) } }
                }
            }

            h3 { "Source" }
            p { a href=(REPO) { (REPO) } }

            h3 { "Desktop app" }
            p {
                "Installers for macOS, Windows and Linux are on the "
                a href=(format!("{REPO}/releases/latest")) { "latest release" }
                " page"
            }
            details.helpbox {
                summary { "macOS says the app is \"damaged\"?" }
                p { "The builds are not signed with an Apple Developer ID yet, so macOS quarantines the download and reports it as \"damaged\" rather than offering an Open button. It is not actually damaged. Drag Bakemono to Applications, then clear the quarantine flag once from Terminal:" }
                pre { code { "xattr -dr com.apple.quarantine /Applications/Bakemono.app" } }
                p.muted { "After that it opens normally. If macOS still refuses, run " code { "sudo xattr -cr /Applications/Bakemono.app" } }
            }

            h3 { "DMCA and contact" }
            p { "Takedowns on this board are published as signed kind 31064 Nostr events, a public transparency log. Each board sets its own posture by jurisdiction" }
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

async fn creator_page(
    State(pool): State<PgPool>,
    Path((platform, creator_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Html<String> {
    let posts = db::creator_posts(&pool, &platform, &creator_id, 300, 0)
        .await
        .unwrap_or_default();
    let name = posts
        .first()
        .map(|p| p.creator.clone())
        .unwrap_or_else(|| creator_id.clone());
    render(
        &name,
        html! {
            div.crumbs { a href="/creators" { "Creators" } " / " span { (name) } }
            div.creatorhead {
                h1 { (name) }
                span.chip.platform { (pretty_platform(&platform)) }
                span.muted { (posts.len()) @if posts.len() == 1 { " post" } @else { " posts" } }
            }
            @if is_mod(&headers) { (mod_bar_creator(&platform, &creator_id)) }
            @if posts.is_empty() { p.muted { "Nothing here yet" } }
            (posts_grid(&posts))
        },
    )
}

async fn post_page(
    State(pool): State<PgPool>,
    Path((platform, creator_id, post_id)): Path<(String, String, String)>,
    Query(query): Query<ReportedQuery>,
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
            (report_box(&platform, &creator_id, &post_id, query.reported.is_some()))
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

// full media (not the tiny preview) streamed straight from the gateway, one at a time and centered - content
// is the point of the page. each item loads only when shown, so a many-image post does not fetch it all at once
fn carousel(files: &[db::ManifestRow]) -> Markup {
    let items: Vec<String> = files
        .iter()
        .filter_map(|f| {
            let ih = f.infohash.as_deref()?;
            let video = f.mime.starts_with("video/");
            Some(format!("{{\"u\":\"/t/{ih}/f/0\",\"v\":{video}}}"))
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

// the gateway is the only thing here that speaks BitTorrent: it joins a swarm cold for an infohash the
// board carries and hands the bytes back as plain HTTP, so browsers do no P2P

async fn gateway_meta(
    State(state): State<AppState>,
    Path(infohash): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let Some((magnet, private)) = resolve_gateway(&state, &infohash, &headers).await else {
        return (StatusCode::NOT_FOUND, "unknown infohash").into_response();
    };
    if !private {
        if let Some(resp) = cold_miss_guard(&state, &magnet, &headers, peer) {
            return resp;
        }
    }
    match state.gateway.meta(&magnet).await {
        Ok(meta) => private_if(Json(meta).into_response(), private),
        Err(e) => (StatusCode::BAD_GATEWAY, format!("swarm error: {e:#}")).into_response(),
    }
}

async fn gateway_file(
    State(state): State<AppState>,
    Path((infohash, file_index)): Path<(String, usize)>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let Some((magnet, private)) = resolve_gateway(&state, &infohash, &headers).await else {
        return (StatusCode::NOT_FOUND, "unknown infohash").into_response();
    };
    if !private {
        if let Some(resp) = cold_miss_guard(&state, &magnet, &headers, peer) {
            return resp;
        }
    }
    match state.gateway.open(&magnet, file_index).await {
        Ok(file) => private_if(stream_file(file, &headers).await, private),
        Err(e) => (StatusCode::BAD_GATEWAY, format!("swarm error: {e:#}")).into_response(),
    }
}

// warm content serves from disk and is never limited; a cold miss joins a swarm, so cap how fast one
// client can trigger those. returns Some(429) when the client is over budget, None to proceed
fn cold_miss_guard(
    state: &AppState,
    magnet: &str,
    headers: &HeaderMap,
    peer: SocketAddr,
) -> Option<Response> {
    let infohash = bakemono_torrent::infohash_from_magnet(magnet)?;
    if state.gateway.is_cached(&infohash) {
        return None;
    }
    let client = client_ip(headers, peer);
    if state.cold_limiter.allow(client) {
        return None;
    }
    let mut resp = (StatusCode::TOO_MANY_REQUESTS, "cold-fetch rate limit, retry shortly").into_response();
    resp.headers_mut()
        .insert(header::RETRY_AFTER, header::HeaderValue::from_static("2"));
    Some(resp)
}

// the real client behind our proxy: only when the socket peer is a trusted proxy (Cloudflare by default)
// do we believe CF-Connecting-IP / X-Forwarded-For; a direct hit uses the peer, so an untrusted client
// cannot forge the rate-limit key by setting the header itself
fn client_ip(headers: &HeaderMap, peer: SocketAddr) -> IpAddr {
    if !crate::trusted_proxy::is_trusted_proxy(peer.ip()) {
        return peer.ip();
    }
    if let Some(ip) = headers
        .get("cf-connecting-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse().ok())
    {
        return ip;
    }
    if let Some(ip) = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.trim().parse().ok())
    {
        return ip;
    }
    peer.ip()
}

// the gateway builds its own magnet from the catalog infohash plus operator-configured trackers and never
// reuses a contributor's trackers, so a manifest cannot make the board announce to a host of its choosing
// (blind SSRF). only infohashes the board carries and that pass moderation resolve, so it is never an open
// proxy; BAKEMONO_GATEWAY_OPEN lifts the catalog check for local testing of a cold load
async fn resolve_gateway(state: &AppState, infohash: &str, headers: &HeaderMap) -> Option<(String, bool)> {
    let infohash = infohash.trim().to_ascii_lowercase();
    if infohash.len() != 40 || !infohash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    if public_infohash(state, &infohash).await {
        return Some((clean_magnet(&live_infohash(state, &infohash).await), false));
    }
    // a moderator additionally resolves pending or taken-down content (flagged private so it is never
    // cached) so the mod views can preview what the public cannot see
    if is_mod(headers) && matches!(db::magnet_by_infohash_any(&state.pool, &infohash).await, Ok(Some(_))) {
        return Some((clean_magnet(&infohash), true));
    }
    None
}

async fn public_infohash(state: &AppState, infohash: &str) -> bool {
    if matches!(db::magnet_by_infohash(&state.pool, infohash).await, Ok(Some(_))) {
        return true;
    }
    env_opt("BAKEMONO_GATEWAY_OPEN").is_some()
}

// a swarm the prober saw empty falls back to a sibling torrent carrying the same file bytes (same
// file_hash), so the response stays byte-identical to what the requested infohash names
async fn live_infohash(state: &AppState, requested: &str) -> String {
    if state.gateway.is_cached(requested) {
        return requested.to_string();
    }
    let alternates = match db::swarm_alternates(&state.pool, requested).await {
        Ok(alternates) => alternates,
        Err(_) => return requested.to_string(),
    };
    choose_alternate(requested, &alternates, |ih| state.gateway.is_cached(ih))
        .unwrap_or_else(|| requested.to_string())
}

// the requested torrent wins unless its last probe found zero seeders; then prefer a sibling already
// on disk, then the best-seeded live one. an unprobed sibling is never picked over the requested hash
fn choose_alternate(
    requested: &str,
    alternates: &[(String, Option<i32>)],
    is_cached: impl Fn(&str) -> bool,
) -> Option<String> {
    let dead = alternates
        .iter()
        .any(|(ih, seeders)| ih == requested && *seeders == Some(0));
    if !dead {
        return None;
    }
    let siblings = || alternates.iter().filter(|(ih, _)| ih != requested);
    if let Some((ih, _)) = siblings().find(|(ih, _)| is_cached(ih)) {
        return Some(ih.clone());
    }
    siblings()
        .filter(|(_, seeders)| seeders.is_some_and(|n| n > 0))
        .max_by_key(|(_, seeders)| *seeders)
        .map(|(ih, _)| ih.clone())
}

fn clean_magnet(infohash: &str) -> String {
    let trackers: Vec<String> = bakemono_core::default_trackers()
        .into_iter()
        .filter(|t| !t.starts_with("wss://"))
        .collect();
    bakemono_torrent::synth_magnet(infohash, &trackers)
}

fn private_if(mut resp: Response, private: bool) -> Response {
    if private {
        resp.headers_mut().insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("private, no-store"),
        );
    }
    resp
}

async fn stream_file(mut file: bakemono_torrent::OpenFile, headers: &HeaderMap) -> Response {
    let total = file.size;
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|r| parse_range(r, total));

    let (status, body, content_len, content_range) = match range {
        Some((start, end)) => {
            if file.stream.seek(SeekFrom::Start(start)).await.is_err() {
                return (StatusCode::INTERNAL_SERVER_ERROR, "seek failed").into_response();
            }
            let len = end - start + 1;
            let body = Body::from_stream(ReaderStream::new(file.stream.take(len)));
            let cr = format!("bytes {start}-{end}/{total}");
            (StatusCode::PARTIAL_CONTENT, body, len, Some(cr))
        }
        None => {
            let body = Body::from_stream(ReaderStream::new(file.stream));
            (StatusCode::OK, body, total, None)
        }
    };

    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    let h = resp.headers_mut();
    if let Ok(v) = file.mime.parse() {
        h.insert(header::CONTENT_TYPE, v);
    }
    h.insert(header::ACCEPT_RANGES, header::HeaderValue::from_static("bytes"));
    // immutable: the URL is content-addressed by infohash, so the bytes can never change
    h.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    h.insert(header::CONTENT_LENGTH, header::HeaderValue::from(content_len));
    if let Some(cr) = content_range.and_then(|cr| cr.parse().ok()) {
        h.insert(header::CONTENT_RANGE, cr);
    }
    resp
}

// one "bytes=start-end" range; suffix ("-N") and open-ended ("N-") forms supported, multi-range is not
fn parse_range(raw: &str, total: u64) -> Option<(u64, u64)> {
    let spec = raw.strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (s, e) = spec.split_once('-')?;
    let (start, end) = if s.is_empty() {
        let n: u64 = e.parse().ok()?;
        if n == 0 {
            return None;
        }
        (total.saturating_sub(n), total.saturating_sub(1))
    } else {
        let start: u64 = s.parse().ok()?;
        let end = if e.is_empty() {
            total.saturating_sub(1)
        } else {
            e.parse::<u64>().ok()?.min(total.saturating_sub(1))
        };
        (start, end)
    };
    (total > 0 && start <= end && start < total).then_some((start, end))
}

async fn mod_queue(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(denied) = require_mod(&headers) {
        return denied;
    }
    let groups = db::pending_groups(&state.pool, 1_000).await.unwrap_or_default();
    let pending_posts = db::pending_post_count(&state.pool).await.unwrap_or(0);
    let takedowns = db::takedowns(&state.pool).await.unwrap_or_default();
    let reports = db::open_reports(&state.pool, 100).await.unwrap_or_default();
    let report_count = db::open_report_count(&state.pool).await.unwrap_or(0);
    let blocks = group_contributors(&groups);
    let mut td_links: Vec<Option<String>> = Vec::with_capacity(takedowns.len());
    for t in &takedowns {
        let link = if t.target_type == "p" {
            Some(format!("/mod/pubkey/{}", t.target))
        } else {
            db::locate_takedown(&state.pool, &t.target_type, &t.target)
                .await
                .ok()
                .flatten()
                .map(|(p, c, post)| format!("/mod/post/{p}/{c}/{post}"))
        };
        td_links.push(link);
    }
    let page = render(
        "mod queue",
        html! {
            div.modhead {
                h1.pagetitle { "Moderation" }
                a.muted href="/" { "< back to board" }
            }
            div.stats {
                a.stat.statlink href="#reports" {
                    div.num.danger[report_count > 0] { (report_count) }
                    div.label { "open reports" }
                }
                a.stat.statlink href="#queue" {
                    div.num.danger[pending_posts > 0] { (pending_posts) }
                    div.label { "pending posts" }
                }
                a.stat.statlink href="#queue" {
                    div.num { (blocks.len()) }
                    div.label { "contributors in queue" }
                }
                a.stat.statlink href="#takedowns" {
                    div.num { (takedowns.len()) }
                    div.label { "takedowns" }
                }
            }
            (reports_section(&reports, report_count))
            section.block id="queue" {
                div.blockhead {
                    h2 { "Pending review" }
                    @if pending_posts > 0 { span.chip { (pending_posts) " posts" } }
                }
                p.muted { "grouped by contributor and author; open a group to page through its posts, or approve / reject whole groups here" }
                @if blocks.is_empty() { p.muted { "nothing awaiting review" } }
                @for c in &blocks {
                    div.qblock {
                        div.qhead {
                            div.who { code { (npub(c.pubkey)) } span.muted { (block_post_total(c)) " posts" } }
                            div.rowactions {
                                (queue_form("/mod/queue-approve", c.pubkey, "", "", "", "/mod", "approve all", false))
                                (queue_form("/mod/queue-reject", c.pubkey, "", "", "", "/mod", "reject all", true))
                                (queue_form("/mod/ban-contributor", c.pubkey, "", "", "", "/mod", "ban", true))
                            }
                        }
                        @for a in &c.authors {
                            div.qauthorrow {
                                div.who {
                                    span.strong { (a.creator.as_deref().unwrap_or(a.creator_id.as_str())) }
                                    span.chip.platform { (pretty_platform(&a.platform)) }
                                    span.muted { (a.posts) " posts - " (a.files) " files" }
                                }
                                div.rowactions {
                                    a.viewlink href=(format!("/mod/queue/{}/{}/{}", c.pubkey, a.platform, a.creator_id)) { "view posts" }
                                    (queue_form("/mod/queue-approve", c.pubkey, &a.platform, &a.creator_id, "", "/mod", "approve all", false))
                                    (queue_form("/mod/queue-reject", c.pubkey, &a.platform, &a.creator_id, "", "/mod", "reject all", true))
                                }
                            }
                        }
                    }
                }
            }
            (takedown_section(&state, &takedowns, &td_links))
        },
    );
    let mut resp = page.into_response();
    if let Ok(v) = header::HeaderValue::from_str(&mod_session_cookie()) {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    resp
}

fn takedown_section(state: &AppState, takedowns: &[db::TakedownRow], links: &[Option<String>]) -> Markup {
    html! {
      section.block id="takedowns" {
        div.blockhead { h2 { "Takedowns" } }
        @match &state.signer {
            Some(keys) => p.muted { "publishing kind 31064 as " code { (npub(&keys.public_key().to_hex())) } }
            None => p.muted { "set BAKEMONO_INSTANCE_NSEC to publish takedowns to peers; hides apply locally either way" }
        }
        form method="post" action="/mod/takedown" class="takedown" {
            select name="target_type" {
                option value="e" { "event id" }
                option value="x" { "file hash" }
                option value="i" { "infohash" }
                option value="p" { "pubkey" }
                option value="post" { "post" }
                option value="creator" { "creator" }
            }
            input type="text" name="target" placeholder="target value (hash, infohash, npub, or platform:creator_id[:post_id])" required;
            input type="text" name="reason" placeholder="reason (dmca-us, csam, spam...)" required;
            input type="text" name="explanation" placeholder="note (optional)";
            button { "hide + publish" }
        }
        @if takedowns.is_empty() { p.muted { "no takedowns recorded" } }
        ul.list.rows {
            @for (t, link) in takedowns.iter().zip(links) {
                li {
                    div.rowmain {
                        div.rowtitle {
                            @match link {
                                Some(href) => a href=(href) { code { (t.target_type) ":" (t.target) } }
                                None => code { (t.target_type) ":" (t.target) }
                            }
                        }
                        div.rowmeta { span.muted {
                            (t.reason)
                            @if !t.explanation.is_empty() { " - " (t.explanation) }
                            " - via " (takedown_source(&t.source))
                            @if !t.applied_at.is_empty() { " - " (pretty_date(&t.applied_at)) }
                        } }
                    }
                    div.rowactions {
                        form method="post" action="/mod/untakedown" class="modform" {
                            input type="hidden" name="d_tag" value=(t.d_tag);
                            button { "undo" }
                        }
                    }
                }
            }
        }
      }
    }
}

#[derive(serde::Deserialize)]
struct QueueScope {
    #[serde(default)]
    pubkey: String,
    #[serde(default)]
    platform: String,
    #[serde(default)]
    creator_id: String,
    #[serde(default)]
    post_id: String,
    #[serde(default)]
    back: String,
}

async fn mod_queue_approve(
    State(pool): State<PgPool>,
    headers: HeaderMap,
    Form(form): Form<QueueScope>,
) -> Response {
    if let Err(denied) = require_mod(&headers) {
        return denied;
    }
    let _ = db::approve_pending(&pool, &form.pubkey, &form.platform, &form.creator_id, &form.post_id).await;
    Redirect::to(&safe_back(&form.back)).into_response()
}

async fn mod_queue_reject(
    State(pool): State<PgPool>,
    headers: HeaderMap,
    Form(form): Form<QueueScope>,
) -> Response {
    if let Err(denied) = require_mod(&headers) {
        return denied;
    }
    let _ = db::reject_pending(&pool, &form.pubkey, &form.platform, &form.creator_id, &form.post_id).await;
    Redirect::to(&safe_back(&form.back)).into_response()
}

// paginated per-group review: the individual pending posts for one (contributor, author), so the main
// queue never has to list thousands of posts inline
async fn mod_queue_group(
    State(state): State<AppState>,
    Path((pubkey, platform, creator_id)): Path<(String, String, String)>,
    Query(q): Query<PageQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(denied) = require_mod(&headers) {
        return denied;
    }
    const PER_PAGE: i64 = 50;
    let total = db::pending_group_post_count(&state.pool, &pubkey, &platform, &creator_id)
        .await
        .unwrap_or(0);
    let last = if total == 0 { 0 } else { (total - 1) / PER_PAGE };
    let page = q.page.clamp(0, last);
    let posts = db::pending_posts_for(&state.pool, &pubkey, &platform, &creator_id, PER_PAGE, page * PER_PAGE)
        .await
        .unwrap_or_default();
    let name = posts
        .first()
        .and_then(|p| p.creator.clone())
        .unwrap_or_else(|| creator_id.clone());
    let base = format!("/mod/queue/{pubkey}/{platform}/{creator_id}");
    let back = format!("{base}?page={page}");
    let rendered = render(
        &name,
        html! {
            p { a href="/mod" { "< mod queue" } }
            div.modhead { h2 { (name) " " span.chip.platform { (pretty_platform(&platform)) } } }
            p.muted { "by contributor " code { (npub(&pubkey)) } " - " (total) " pending posts" }
            div.modbar {
                (queue_form("/mod/queue-approve", &pubkey, &platform, &creator_id, "", "/mod", "approve all pending", false))
                (queue_form("/mod/queue-reject", &pubkey, &platform, &creator_id, "", "/mod", "reject all pending", true))
            }
            @if posts.is_empty() { p.muted { "no pending posts" } }
            ul.list.rows {
                @for post in &posts {
                    li {
                        div.rowmain {
                            div.rowtitle {
                                a href=(format!("/mod/post/{}/{}/{}", platform, creator_id, post.post_id)) {
                                    (post.post_title.clone().unwrap_or_else(|| post.post_id.clone()))
                                }
                            }
                            div.rowmeta { span.muted { (post.files) " file(s)" } }
                        }
                        div.rowactions {
                            (queue_form("/mod/queue-approve", &pubkey, &platform, &creator_id, &post.post_id, &back, "approve", false))
                            (queue_form("/mod/queue-reject", &pubkey, &platform, &creator_id, &post.post_id, &back, "reject", true))
                        }
                    }
                }
            }
            @if last > 0 {
                div.pager {
                    @if page > 0 { a.btn.ghost href=(format!("{base}?page={}", page - 1)) { "< prev" } }
                    span.muted { "page " (page + 1) " of " (last + 1) }
                    @if page < last { a.btn.ghost href=(format!("{base}?page={}", page + 1)) { "next >" } }
                }
            }
        },
    );
    private_page(rendered)
}

#[derive(serde::Deserialize)]
struct PageQuery {
    #[serde(default)]
    page: i64,
}

// ban a contributor: publish a kind 31064 pubkey takedown (so their future uploads auto-drop at ingest)
// and clear their current queue
async fn mod_ban_contributor(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ModForm>,
) -> Response {
    if let Err(denied) = require_mod(&headers) {
        return denied;
    }
    apply_ban(&state, Target::Pubkey(form.pubkey.clone())).await;
    let _ = db::reject_pending(&state.pool, &form.pubkey, "", "", "").await;
    Redirect::to("/mod").into_response()
}

// group the per-(contributor, author) summary rows into contributor blocks, each holding its authors
struct CBlock<'a> {
    pubkey: &'a str,
    authors: Vec<&'a db::QueueGroup>,
}
fn group_contributors(groups: &[db::QueueGroup]) -> Vec<CBlock<'_>> {
    let mut blocks: Vec<CBlock> = Vec::new();
    for g in groups {
        if blocks.last().map(|c| c.pubkey != g.pubkey).unwrap_or(true) {
            blocks.push(CBlock { pubkey: &g.pubkey, authors: Vec::new() });
        }
        blocks.last_mut().unwrap().authors.push(g);
    }
    blocks
}
fn block_post_total(c: &CBlock) -> i64 {
    c.authors.iter().map(|a| a.posts).sum()
}
fn queue_form(
    action: &str,
    pubkey: &str,
    platform: &str,
    creator_id: &str,
    post_id: &str,
    back: &str,
    label: &str,
    danger: bool,
) -> Markup {
    html! {
        form method="post" action=(action) class="modform" {
            input type="hidden" name="pubkey" value=(pubkey);
            input type="hidden" name="platform" value=(platform);
            input type="hidden" name="creator_id" value=(creator_id);
            input type="hidden" name="post_id" value=(post_id);
            input type="hidden" name="back" value=(back);
            button class=(if danger { "danger" } else { "ok" }) { (label) }
        }
    }
}

// record the hide locally first so it takes effect even if relays are unreachable, then sign and fan
// the kind 31064 out to the relay set; a missing instance key keeps the hide local-only
async fn mod_takedown(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<TakedownForm>,
) -> Response {
    if let Err(denied) = require_mod(&headers) {
        return denied;
    }
    let value = form.target.trim().to_string();
    let Some(target) = Target::from_parts(&form.target_type, value) else {
        return (StatusCode::BAD_REQUEST, "unknown target type").into_response();
    };
    if target.parts().1.is_empty() {
        return Redirect::to("/mod").into_response();
    }
    let takedown = Takedown {
        target,
        reason: non_empty(form.reason).unwrap_or_else(|| "unspecified".into()),
        applied_at: Some(Utc::now().to_rfc3339()),
        explanation: form.explanation.unwrap_or_default().trim().to_string(),
    };
    match &state.signer {
        Some(keys) => publish_takedown(&state, keys, &takedown).await,
        None => {
            let _ = db::record_takedown(&state.pool, &takedown, "local", None).await;
        }
    }
    Redirect::to("/mod").into_response()
}

async fn publish_takedown(state: &AppState, keys: &Keys, takedown: &Takedown) {
    let event = match takedown.to_event(keys) {
        Ok(event) => event,
        Err(e) => {
            tracing::error!("takedown sign failed: {e}");
            let _ = db::record_takedown(&state.pool, takedown, "local", None).await;
            return;
        }
    };
    let id = event.id.to_hex();
    let _ = db::record_takedown(
        &state.pool,
        takedown,
        &keys.public_key().to_hex(),
        Some(&id),
    )
    .await;
    if let Err(e) = send_to_relays(&state.relays, keys, &event).await {
        tracing::warn!("takedown {id} publish failed (kept local): {e:#}");
    }
}

async fn send_to_relays(relays: &[String], keys: &Keys, event: &Event) -> anyhow::Result<()> {
    let client = Client::new(keys.clone());
    for relay in relays {
        client.add_relay(relay).await?;
    }
    client.connect().await;
    client.send_event(event).await?;
    client.disconnect().await;
    Ok(())
}

async fn mod_untakedown(
    State(pool): State<PgPool>,
    headers: HeaderMap,
    Form(form): Form<UntakedownForm>,
) -> Response {
    if let Err(denied) = require_mod(&headers) {
        return denied;
    }
    let _ = db::remove_takedown(&pool, &form.d_tag).await;
    Redirect::to("/mod").into_response()
}

const REPORT_REASONS: &[&str] = &[
    "csam", "dmca", "spam", "malware", "mislabeled", "broken", "other",
];

// unauthenticated: the one write path a random visitor reaches, so it is gated by a honeypot, an
// issue-time token that forces a real page load first, and a per-ip + per-post rate limit
async fn submit_report(
    State(pool): State<PgPool>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<ReportForm>,
) -> Response {
    let back = format!("/p/{}/{}/{}", form.platform, form.creator_id, form.post_id);
    if !form.website.trim().is_empty() {
        return Redirect::to(&back).into_response();
    }
    if !REPORT_REASONS.contains(&form.reason.as_str()) {
        return Redirect::to(&back).into_response();
    }
    let now = now_secs();
    if !verify_report_token(&form.platform, &form.creator_id, &form.post_id, &form.token, now) {
        return Redirect::to(&back).into_response();
    }
    let ip = client_ip(&headers, peer);
    let post_key = format!("{}:{}:{}", form.platform, form.creator_id, form.post_id);
    if !report_limiter().allow(&ip_hash(&ip.to_string()), &post_key, now) {
        return Redirect::to(&back).into_response();
    }
    if !matches!(
        db::post_is_visible(&pool, &form.platform, &form.creator_id, &form.post_id).await,
        Ok(true)
    ) {
        return Redirect::to(&back).into_response();
    }
    let _ =
        db::record_report(&pool, &form.platform, &form.creator_id, &form.post_id, &form.reason).await;
    Redirect::to(&format!("{back}?reported=1")).into_response()
}

async fn mod_report_dismiss(
    State(pool): State<PgPool>,
    headers: HeaderMap,
    Form(form): Form<PostForm>,
) -> Response {
    if let Err(denied) = require_mod(&headers) {
        return denied;
    }
    let _ = db::resolve_report(&pool, &form.platform, &form.creator_id, &form.post_id).await;
    Redirect::to("/mod").into_response()
}

// hide a whole post with one kind 31064 post-target event (federates), then clear any report on it
async fn mod_ban_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<BanPostForm>,
) -> Response {
    if let Err(denied) = require_mod(&headers) {
        return denied;
    }
    apply_ban(&state, Target::post(&form.platform, &form.creator_id, &form.post_id)).await;
    let _ = db::resolve_report(&state.pool, &form.platform, &form.creator_id, &form.post_id).await;
    Redirect::to(&safe_back(&form.back)).into_response()
}

// hide every post from a creator with one kind 31064 creator-target event, so a fresh approved
// contributor re-uploading the same creator stays hidden too
async fn mod_ban_creator(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<BanCreatorForm>,
) -> Response {
    if let Err(denied) = require_mod(&headers) {
        return denied;
    }
    apply_ban(&state, Target::creator(&form.platform, &form.creator_id)).await;
    Redirect::to(&safe_back(&form.back)).into_response()
}

// sign + fan the takedown to peers when the board has an instance key, else record it locally only
async fn apply_ban(state: &AppState, target: Target) {
    let takedown = Takedown {
        target,
        reason: "moderator".into(),
        applied_at: Some(Utc::now().to_rfc3339()),
        explanation: String::new(),
    };
    match &state.signer {
        Some(keys) => publish_takedown(state, keys, &takedown).await,
        None => {
            let _ = db::record_takedown(&state.pool, &takedown, "local", None).await;
        }
    }
}

// only ever bounce to a local path so a crafted back field cannot turn this into an open redirect
fn safe_back(back: &str) -> String {
    if back.starts_with('/') && !back.starts_with("//") {
        back.to_string()
    } else {
        "/mod".to_string()
    }
}

fn mod_bar_post(platform: &str, creator_id: &str, post_id: &str) -> Markup {
    let back = format!("/c/{platform}/{creator_id}");
    html! {
        div.modbar {
            span.muted { "mod" }
            form method="post" action="/mod/ban-post" class="modform" {
                input type="hidden" name="platform" value=(platform);
                input type="hidden" name="creator_id" value=(creator_id);
                input type="hidden" name="post_id" value=(post_id);
                input type="hidden" name="back" value=(back);
                button.danger { "hide post" }
            }
            form method="post" action="/mod/ban-creator" class="modform" {
                input type="hidden" name="platform" value=(platform);
                input type="hidden" name="creator_id" value=(creator_id);
                input type="hidden" name="back" value=(back);
                button.danger { "ban author" }
            }
            a.btn.ghost href="/mod" { "mod queue" }
        }
    }
}

fn mod_bar_creator(platform: &str, creator_id: &str) -> Markup {
    html! {
        div.modbar {
            span.muted { "mod" }
            form method="post" action="/mod/ban-creator" class="modform" {
                input type="hidden" name="platform" value=(platform);
                input type="hidden" name="creator_id" value=(creator_id);
                input type="hidden" name="back" value="/creators";
                button.danger { "ban author" }
            }
            a.btn.ghost href="/mod" { "mod queue" }
        }
    }
}

// mod-only: everything a contributor uploaded, grouped by post, so a mod can open the files and review
// before approving; nothing here is public until the pubkey is approved
async fn mod_pubkey_view(
    State(state): State<AppState>,
    Path(pubkey): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(denied) = require_mod(&headers) {
        return denied;
    }
    let files = db::pubkey_files(&state.pool, &pubkey).await.unwrap_or_default();
    let page = render(
        "contributor",
        html! {
            p { a href="/mod" { "< mod queue" } }
            h2 { "Contributor" }
            p { code { (npub(&pubkey)) } }
            div.modbar {
                span.muted { "review the posts, then:" }
                (queue_form("/mod/queue-approve", &pubkey, "", "", "", "/mod", "approve all pending", false))
                (queue_form("/mod/queue-reject", &pubkey, "", "", "", "/mod", "reject all pending", true))
                (queue_form("/mod/ban-contributor", &pubkey, "", "", "", "/mod", "ban contributor", true))
            }
            @if files.is_empty() { p.muted { "no files" } }
            (post_groups(&files))
        },
    );
    private_page(page)
}

// mod-only: one post's files at any status, with a banner and one-click ban or unban
async fn mod_post_view(
    State(state): State<AppState>,
    Path((platform, creator_id, post_id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(denied) = require_mod(&headers) {
        return denied;
    }
    let files = db::post_files_any(&state.pool, &platform, &creator_id, &post_id)
        .await
        .unwrap_or_default();
    let visible = db::post_is_visible(&state.pool, &platform, &creator_id, &post_id)
        .await
        .unwrap_or(false);
    let takedown = db::post_takedown(&state.pool, &platform, &creator_id, &post_id)
        .await
        .ok()
        .flatten();
    let title = files
        .first()
        .and_then(|f| f.post_title.clone())
        .unwrap_or_else(|| post_id.clone());
    let creator = files.first().map(|f| f.creator.clone());
    let page = render(
        &title,
        html! {
            p { a href="/mod" { "< mod queue" } }
            h2 { (title) }
            p.muted {
                @if let Some(c) = &creator { "by " a href=(format!("/c/{platform}/{creator_id}")) { (c) } " - " }
                span.chip.platform { (pretty_platform(&platform)) }
            }
            @if visible {
                div.statusbar.ok { "public - this post is live" }
                (mod_bar_post(&platform, &creator_id, &post_id))
            } @else if let Some((d_tag, reason)) = &takedown {
                div.statusbar.danger {
                    span { "hidden by takedown: " (reason) }
                    form method="post" action="/mod/untakedown" class="modform" {
                        input type="hidden" name="d_tag" value=(d_tag);
                        button.ok { "unban" }
                    }
                }
            } @else {
                div.statusbar { "pending review - not yet public" }
                (mod_bar_post(&platform, &creator_id, &post_id))
            }
            @if files.is_empty() { p.muted { "no files" } }
            (carousel(&files))
            script { (PreEscaped(CAROUSEL_JS)) }
        },
    );
    private_page(page)
}

// mod-only: an author's files grouped by post, so a mod can review a first-seen creator before its
// content is allowed to publish
async fn mod_author_view(
    State(state): State<AppState>,
    Path((platform, creator_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(denied) = require_mod(&headers) {
        return denied;
    }
    let files = db::author_files(&state.pool, &platform, &creator_id)
        .await
        .unwrap_or_default();
    let name = files
        .first()
        .map(|f| f.creator.clone())
        .unwrap_or_else(|| creator_id.clone());
    let page = render(
        &name,
        html! {
            p { a href="/mod" { "< mod queue" } }
            h2 { (name) " " span.chip.platform { (pretty_platform(&platform)) } }
            div.modbar {
                span.muted { "approve or drop this author's queued posts from every contributor:" }
                (queue_form("/mod/queue-approve", "", &platform, &creator_id, "", "/mod", "approve all pending", false))
                (queue_form("/mod/queue-reject", "", &platform, &creator_id, "", "/mod", "reject all pending", true))
            }
            @if files.is_empty() { p.muted { "no files" } }
            (post_groups(&files))
        },
    );
    private_page(page)
}

// a contributor's files split into their posts, each header linking to that post's mod view
fn post_groups(files: &[db::ManifestRow]) -> Markup {
    let mut groups: Vec<(&str, &str, &str, &str, Vec<&db::ManifestRow>)> = Vec::new();
    for f in files {
        match groups.last_mut() {
            Some(g) if g.0 == f.platform.as_str() && g.1 == f.creator_id.as_str() && g.2 == f.post_id.as_str() => {
                g.4.push(f)
            }
            _ => groups.push((
                f.platform.as_str(),
                f.creator_id.as_str(),
                f.post_id.as_str(),
                f.post_title.as_deref().unwrap_or(f.post_id.as_str()),
                vec![f],
            )),
        }
    }
    html! {
        @for (platform, creator_id, post_id, title, rows) in &groups {
            div.postgroup {
                h3 { a href=(format!("/mod/post/{platform}/{creator_id}/{post_id}")) { (title) } }
                (file_list(rows))
            }
        }
    }
}

// files as openable links, never inline images, so a moderator opens each item deliberately
fn file_list(files: &[&db::ManifestRow]) -> Markup {
    html! {
        ul.filelist {
            @for f in files {
                li {
                    @match &f.infohash {
                        Some(ih) => a.filelink href=(format!("/t/{ih}/f/0")) target="_blank" rel="noopener" {
                            (f.filename.clone().unwrap_or_else(|| format!("file {}", f.file_index)))
                        }
                        None => span { (f.filename.clone().unwrap_or_else(|| format!("file {}", f.file_index))) }
                    }
                    span.muted { " - " (f.mime) " - " (human_size(f.size)) }
                }
            }
        }
    }
}

fn human_size(bytes: i64) -> String {
    let b = bytes as f64;
    if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.0} KB", b / 1e3)
    } else {
        format!("{bytes} B")
    }
}

fn private_page(page: Html<String>) -> Response {
    let mut resp = page.into_response();
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("private, no-store"),
    );
    resp
}

#[derive(serde::Deserialize)]
struct BanPostForm {
    platform: String,
    creator_id: String,
    post_id: String,
    #[serde(default)]
    back: String,
}

#[derive(serde::Deserialize)]
struct BanCreatorForm {
    platform: String,
    creator_id: String,
    #[serde(default)]
    back: String,
}

fn reports_section(reports: &[db::ReportGroup], open_count: i64) -> Markup {
    html! {
      section.block id="reports" {
        div.blockhead {
            h2 { "Reports" }
            @if open_count > 0 { span.chip.danger { (open_count) " open" } }
        }
        @if reports.is_empty() {
            p.muted { "no open reports" }
        } @else {
            p.muted { "user-flagged posts, most severe first; hide publishes a takedown, dismiss clears the flag" }
            ul.list.rows {
                @for r in reports {
                    li.report.csam[r.has_csam] {
                        div.rowmain {
                            div.rowtitle {
                                a href=(format!("/mod/post/{}/{}/{}", r.platform, r.creator_id, r.post_id)) {
                                    (r.post_title.clone().unwrap_or_else(|| r.post_id.clone()))
                                }
                                " " span.chip.platform { (pretty_platform(&r.platform)) }
                                @if r.has_csam { " " span.chip.danger { "CSAM" } }
                            }
                            div.rowmeta { span.muted {
                                @if !r.creator.is_empty() { (r.creator) " - " }
                                @if let Some(rs) = &r.reasons { (rs) " - " }
                                (r.total) " report(s)"
                            } }
                        }
                        div.rowactions {
                            form method="post" action="/mod/ban-post" class="modform" {
                                input type="hidden" name="platform" value=(r.platform);
                                input type="hidden" name="creator_id" value=(r.creator_id);
                                input type="hidden" name="post_id" value=(r.post_id);
                                input type="hidden" name="back" value="/mod";
                                button.danger { "hide post" }
                            }
                            form method="post" action="/mod/ban-creator" class="modform" {
                                input type="hidden" name="platform" value=(r.platform);
                                input type="hidden" name="creator_id" value=(r.creator_id);
                                input type="hidden" name="back" value="/mod";
                                button.danger { "ban author" }
                            }
                            form method="post" action="/mod/report-dismiss" class="modform" {
                                input type="hidden" name="platform" value=(r.platform);
                                input type="hidden" name="creator_id" value=(r.creator_id);
                                input type="hidden" name="post_id" value=(r.post_id);
                                button { "dismiss" }
                            }
                        }
                    }
                }
            }
        }
      }
    }
}

fn report_box(platform: &str, creator_id: &str, post_id: &str, reported: bool) -> Markup {
    html! {
        @if reported {
            p.reported { "Thanks - a moderator will review this post" }
        } @else {
            details.reportbox {
                summary { "Report this post" }
                form method="post" action="/report" {
                    input type="hidden" name="platform" value=(platform);
                    input type="hidden" name="creator_id" value=(creator_id);
                    input type="hidden" name="post_id" value=(post_id);
                    input type="hidden" name="token" value=(report_token(platform, creator_id, post_id, now_secs()));
                    input.hp type="text" name="website" tabindex="-1" autocomplete="off" aria-hidden="true";
                    select name="reason" {
                        option value="csam" { "illegal / CSAM" }
                        option value="dmca" { "copyright / DMCA" }
                        option value="spam" { "spam" }
                        option value="malware" { "malware" }
                        option value="mislabeled" { "mislabeled" }
                        option value="broken" { "broken / unavailable" }
                        option value="other" { "other" }
                    }
                    button { "Submit report" }
                }
            }
        }
    }
}

fn now_secs() -> i64 {
    Utc::now().timestamp()
}

// derived from the mod token so it is stable across restarts with no rng dep; it salts the ip hash
// and keys the anti-replay token
fn report_secret() -> &'static [u8; 32] {
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

fn report_token(platform: &str, creator_id: &str, post_id: &str, issued_at: i64) -> String {
    let mut mac = HmacSha256::new_from_slice(report_secret()).expect("hmac key");
    mac.update(format!("{platform}:{creator_id}:{post_id}:{issued_at}").as_bytes());
    let tag = mac.finalize().into_bytes();
    let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&tag[..16]);
    format!("{issued_at}.{sig}")
}

const REPORT_TOKEN_MIN_AGE: i64 = 2;
const REPORT_TOKEN_MAX_AGE: i64 = 3600;

fn verify_report_token(
    platform: &str,
    creator_id: &str,
    post_id: &str,
    token: &str,
    now: i64,
) -> bool {
    let Some((ts, sig_b64)) = token.split_once('.') else {
        return false;
    };
    let Ok(issued_at) = ts.parse::<i64>() else {
        return false;
    };
    let age = now - issued_at;
    if !(REPORT_TOKEN_MIN_AGE..=REPORT_TOKEN_MAX_AGE).contains(&age) {
        return false;
    }
    let Ok(sig) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(sig_b64) else {
        return false;
    };
    let mut mac = HmacSha256::new_from_slice(report_secret()).expect("hmac key");
    mac.update(format!("{platform}:{creator_id}:{post_id}:{issued_at}").as_bytes());
    mac.verify_truncated_left(&sig).is_ok()
}

// hashed with the secret so a raw ip is never stored or logged; keying only, privacy per project stance
fn ip_hash(ip: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"bakemono-report-ip-v1");
    h.update(report_secret());
    h.update(ip.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(h.finalize())
}

const REPORT_WINDOW_SECS: i64 = 600;
const REPORT_PER_POST_MAX: u32 = 3;
const REPORT_PER_IP_MAX: u32 = 20;
const REPORT_GLOBAL_MAX: u32 = 500;
const REPORT_MAX_TRACKED: usize = 50_000;

fn report_limiter() -> &'static ReportLimiter {
    static L: OnceLock<ReportLimiter> = OnceLock::new();
    L.get_or_init(ReportLimiter::default)
}

// fixed-window limiter over hashed ip, hashed ip+post, and a global ceiling; a submit bumps all three
// so hammering a blocked key keeps it blocked
#[derive(Default)]
struct ReportLimiter {
    state: Mutex<ReportLimiterState>,
}

#[derive(Default)]
struct ReportLimiterState {
    windows: HashMap<String, ReportWindow>,
    global: ReportWindow,
}

#[derive(Default, Clone, Copy)]
struct ReportWindow {
    start: i64,
    count: u32,
}

impl ReportLimiter {
    fn allow(&self, ip_hash: &str, post_key: &str, now: i64) -> bool {
        let mut st = self.state.lock().unwrap();
        if st.windows.len() > REPORT_MAX_TRACKED {
            st.windows.retain(|_, w| now - w.start < REPORT_WINDOW_SECS);
        }
        let per_ip = bump_window(
            st.windows.entry(format!("ip:{ip_hash}")).or_default(),
            now,
            REPORT_PER_IP_MAX,
        );
        let per_post = bump_window(
            st.windows.entry(format!("pp:{ip_hash}:{post_key}")).or_default(),
            now,
            REPORT_PER_POST_MAX,
        );
        let mut global = st.global;
        let global_ok = bump_window(&mut global, now, REPORT_GLOBAL_MAX);
        st.global = global;
        per_ip && per_post && global_ok
    }
}

fn bump_window(w: &mut ReportWindow, now: i64, max: u32) -> bool {
    if now - w.start >= REPORT_WINDOW_SECS {
        w.start = now;
        w.count = 0;
    }
    w.count += 1;
    w.count <= max
}

#[derive(serde::Deserialize)]
struct ReportForm {
    platform: String,
    creator_id: String,
    post_id: String,
    reason: String,
    token: String,
    #[serde(default)]
    website: String,
}

#[derive(serde::Deserialize)]
struct PostForm {
    platform: String,
    creator_id: String,
    post_id: String,
}

#[derive(serde::Deserialize)]
struct ReportedQuery {
    reported: Option<String>,
}

#[derive(serde::Deserialize)]
struct ModForm {
    pubkey: String,
}

#[derive(serde::Deserialize)]
struct TakedownForm {
    target_type: String,
    target: String,
    reason: String,
    explanation: Option<String>,
}

#[derive(serde::Deserialize)]
struct UntakedownForm {
    d_tag: String,
}

fn non_empty(value: String) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

// peer takedowns store the signer pubkey; render it as an npub, leave "local" as-is
fn takedown_source(source: &str) -> String {
    if source.len() == 64 && source.bytes().all(|b| b.is_ascii_hexdigit()) {
        npub(source)
    } else {
        source.to_string()
    }
}

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
// public pages without re-prompting; keyed on report_secret (mod-token-derived) so rotating the token
// invalidates every outstanding session
fn mod_session_cookie() -> String {
    let expiry = now_secs() + MOD_SESSION_TTL;
    let sig = mod_session_sig(expiry);
    format!("modsession={expiry}.{sig}; Path=/; Max-Age={MOD_SESSION_TTL}; HttpOnly; SameSite=Strict")
}

fn mod_session_sig(expiry: i64) -> String {
    let mut mac = HmacSha256::new_from_slice(report_secret()).expect("hmac key");
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
    let mut mac = HmacSha256::new_from_slice(report_secret()).expect("hmac key");
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

fn npub(pubkey_hex: &str) -> String {
    PublicKey::from_hex(pubkey_hex)
        .ok()
        .and_then(|pk| pk.to_bech32().ok())
        .unwrap_or_else(|| pubkey_hex.to_string())
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

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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

fn render(title: &str, body: Markup) -> Html<String> {
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
                    link rel="alternate" type="application/rss+xml" title="seed feed" href="/feed.xml";
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

// per-OS desktop builds, served by GitHub's stable latest-release redirect; names track Tauri's bundles
const DOWNLOADS: &[(&str, &str)] = &[
    ("Windows", "Bakemono_x64-setup.exe"),
    ("macOS (Apple Silicon)", "Bakemono_aarch64.dmg"),
    ("Linux (.deb)", "Bakemono_amd64.deb"),
];

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
  --mauve:#cba6f7; --red:#f38ba8; --green:#a6e3a1;
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
    use super::{build_feed, choose_alternate, endangered_item, feed_item, pretty_date};
    use super::{report_token, verify_report_token, ReportLimiter, REPORT_WINDOW_SECS};
    use crate::db::{EndangeredRow, ManifestRow};

    #[test]
    fn alternate_kicks_in_only_for_a_probed_dead_swarm() {
        let alts = |req: Option<i32>| {
            vec![
                ("req".to_string(), req),
                ("weak".to_string(), Some(1)),
                ("strong".to_string(), Some(4)),
                ("unprobed".to_string(), None),
            ]
        };
        let uncached = |_: &str| false;
        // live or never-probed requested hash is served as-is
        assert_eq!(choose_alternate("req", &alts(Some(2)), uncached), None);
        assert_eq!(choose_alternate("req", &alts(None), uncached), None);
        // dead requested hash falls back to the best-seeded sibling, never an unprobed one
        assert_eq!(
            choose_alternate("req", &alts(Some(0)), uncached),
            Some("strong".to_string())
        );
        // a sibling already on disk beats swarm counts
        assert_eq!(
            choose_alternate("req", &alts(Some(0)), |ih| ih == "weak"),
            Some("weak".to_string())
        );
        // no usable sibling -> stay on the requested hash
        let lonely = vec![("req".to_string(), Some(0)), ("other".to_string(), None)];
        assert_eq!(choose_alternate("req", &lonely, uncached), None);
    }

    #[test]
    fn report_token_round_trips_within_window() {
        let t = report_token("patreon", "c1", "p1", 1_000);
        assert!(verify_report_token("patreon", "c1", "p1", &t, 1_010));
    }

    #[test]
    fn report_token_rejects_too_fast_and_too_old() {
        let t = report_token("patreon", "c1", "p1", 1_000);
        assert!(!verify_report_token("patreon", "c1", "p1", &t, 1_001));
        assert!(!verify_report_token("patreon", "c1", "p1", &t, 5_000));
    }

    #[test]
    fn report_token_is_bound_to_the_post() {
        let t = report_token("patreon", "c1", "p1", 1_000);
        assert!(!verify_report_token("patreon", "c1", "p2", &t, 1_010));
        assert!(!verify_report_token("patreon", "c2", "p1", &t, 1_010));
    }

    #[test]
    fn report_token_rejects_garbage() {
        assert!(!verify_report_token("patreon", "c1", "p1", "not-a-token", 1_010));
        assert!(!verify_report_token("patreon", "c1", "p1", "1000.zzzz", 1_010));
    }

    #[test]
    fn report_limiter_caps_per_post_then_resets() {
        let lim = ReportLimiter::default();
        let first: Vec<bool> = (0..5).map(|_| lim.allow("iphash", "post", 1_000)).collect();
        assert_eq!(first, vec![true, true, true, false, false]);
        assert!(lim.allow("iphash", "post", 1_000 + REPORT_WINDOW_SECS));
    }

    #[test]
    fn formats_iso_dates_and_passes_junk_through() {
        assert_eq!(pretty_date("2026-06-23T17:46:49.000+00:00"), "Jun 23, 2026");
        assert_eq!(pretty_date("2026-01-03 10:00:00"), "Jan 3, 2026");
        assert_eq!(pretty_date("whenever"), "whenever");
    }

    #[test]
    fn item_emits_magnet_enclosure_and_escapes() {
        let xml = feed_item("https://board.example", &row("evt", 1_700_000_000));
        assert!(xml.contains("<link>https://board.example/p/patreon/c1/p1</link>"));
        assert!(xml.contains("url=\"magnet:?xt=urn:btih:abc&amp;tr=udp://t\""));
        assert!(xml.contains("type=\"application/x-bittorrent\""));
        // no infohash -> guid falls back to the event id
        assert!(xml.contains("<guid isPermaLink=\"false\">evt</guid>"));
        assert!(xml.contains("Foo &amp; Bar"));
    }

    #[test]
    fn endangered_item_carries_seeder_note() {
        let xml = endangered_item(
            "https://board.example",
            &EndangeredRow {
                platform: "patreon".into(),
                creator_id: "c1".into(),
                post_id: "p1".into(),
                creator: "C".into(),
                post_title: Some("Hi".into()),
                filename: None,
                magnet: "magnet:?xt=urn:btih:abc".into(),
                infohash: Some("abc".into()),
                event_id: "evt".into(),
                created_at: 10,
                size: 1,
                seeders: Some(2),
            },
        );
        assert!(xml.contains("<description>2 seeder(s)</description>"));
        // the infohash is the stable guid when present
        assert!(xml.contains("<guid isPermaLink=\"false\">abc</guid>"));
    }

    #[test]
    fn build_feed_wraps_self_and_next_links() {
        let items = feed_item("https://board.example", &row("evt", 10));
        let next = "https://board.example/feed.xml?before=10&limit=3&creator=xyz";
        let xml = build_feed(
            "https://board.example",
            "https://board.example/feed.xml?limit=3&creator=xyz",
            Some(next),
            &items,
        );
        assert!(xml.contains("rel=\"self\""));
        // the cursor href is xml-escaped so its & separators do not break the feed
        assert!(xml.contains(
            "rel=\"next\" href=\"https://board.example/feed.xml?before=10&amp;limit=3&amp;creator=xyz\""
        ));
        assert!(xml.contains("<guid isPermaLink=\"false\">evt</guid>"));
    }

    fn row(event_id: &str, created_at: i64) -> ManifestRow {
        ManifestRow {
            event_id: event_id.into(),
            pubkey: "pk".into(),
            created_at,
            d_tag: "d".into(),
            file_hash: "h".into(),
            size: 4096,
            mime: "image/jpeg".into(),
            magnet: "magnet:?xt=urn:btih:abc&tr=udp://t".into(),
            platform: "patreon".into(),
            creator: "Foo & Bar".into(),
            creator_id: "c1".into(),
            post_id: "p1".into(),
            file_index: 0,
            filename: None,
            post_title: Some("Hi".into()),
            posted_at: None,
            tier: None,
            content: "body".into(),
            thumb: None,
            infohash: None,
        }
    }
}
