use std::io::SeekFrom;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Form, FromRef, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Json, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use base64::Engine;
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
        .route("/random", get(random_redirect))
        .route("/feed.xml", get(seed_feed))
        .route("/contribute", get(contribute))
        .route("/keepers", get(keepers))
        .route("/info", get(info_page))
        .route("/c/{platform}/{creator_id}", get(creator_page))
        .route("/p/{platform}/{creator_id}/{post_id}", get(post_page))
        .route("/mod", get(mod_queue))
        .route("/mod/approve", post(mod_approve))
        .route("/mod/reject", post(mod_reject))
        .route("/mod/approve-creator", post(mod_approve_creator))
        .route("/mod/reject-creator", post(mod_reject_creator))
        .route("/mod/takedown", post(mod_takedown))
        .route("/mod/untakedown", post(mod_untakedown))
        .route("/t/{infohash}/meta", get(gateway_meta))
        .route("/t/{infohash}/f/{file_index}", get(gateway_file))
        .with_state(state)
}

// how many cards a browse page shows; one extra is fetched to detect a next page without a count query
const PAGE: i64 = 60;

async fn home(State(pool): State<PgPool>) -> Html<String> {
    let posts = db::list_posts(&pool, db::Sort::Recent, "", 18, 0)
        .await
        .unwrap_or_default();
    let creators = db::list_creators(&pool, db::Sort::Popular, "", 12, 0)
        .await
        .unwrap_or_default();
    let cfg = config::get();
    render(
        "",
        html! {
            (welcome(cfg, posts.first()))
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
    let (q, sort, page) = query.parts();
    let mut posts = db::list_posts(&pool, sort, &q, PAGE + 1, page * PAGE)
        .await
        .unwrap_or_default();
    let has_next = sort != db::Sort::Random && posts.len() as i64 > PAGE;
    posts.truncate(PAGE as usize);
    render(
        "posts",
        html! {
            h1.pagetitle { "Posts" }
            (toolbar("/posts", &q, sort, "search posts"))
            @if posts.is_empty() { p.muted { "No posts match" } }
            (posts_grid(&posts))
            (pager("/posts", sort, &q, page, has_next))
        },
    )
}

async fn creators_index(
    State(pool): State<PgPool>,
    Query(query): Query<BrowseQuery>,
) -> Html<String> {
    let (q, sort, page) = query.parts();
    let mut creators = db::list_creators(&pool, sort, &q, PAGE + 1, page * PAGE)
        .await
        .unwrap_or_default();
    let has_next = sort != db::Sort::Random && creators.len() as i64 > PAGE;
    creators.truncate(PAGE as usize);
    render(
        "creators",
        html! {
            h1.pagetitle { "Creators" }
            (toolbar("/creators", &q, sort, "search creators"))
            @if creators.is_empty() { p.muted { "No creators match" } }
            (creators_grid(&creators))
            (pager("/creators", sort, &q, page, has_next))
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

#[derive(serde::Deserialize)]
struct BrowseQuery {
    q: Option<String>,
    sort: Option<String>,
    page: Option<i64>,
}

impl BrowseQuery {
    fn parts(self) -> (String, db::Sort, i64) {
        (
            self.q.unwrap_or_default().trim().to_string(),
            db::Sort::parse(self.sort.as_deref()),
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
            div.cardthumb { (card_thumb(p.thumb.as_deref(), p.infohash.as_deref(), &p.mime)) }
            div.cardmeta {
                div.cardtitle { (title) }
                div.cardsub {
                    span.strong { (p.creator) }
                    " - " (p.files) @if p.files == 1 { " file" } @else { " files" }
                    @if p.views > 0 { " - " (p.views) " views" }
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
            div.cardthumb.banner { (card_thumb(c.thumb.as_deref(), c.infohash.as_deref(), c.mime.as_deref().unwrap_or(""))) }
            div.cardmeta {
                div.cardtitle { (c.creator) }
                div.cardsub {
                    span.chip.platform { (c.platform) }
                    " " (c.posts) " posts - " (c.files) " files"
                }
            }
        }
    }
}

// the thumbnail area: inline preview paints instantly with zero fetch. with none, a still image can fall back
// to the gateway (lazy), but a video never loads its full bytes into a grid cell - it shows a placeholder
fn card_thumb(thumb: Option<&str>, infohash: Option<&str>, mime: &str) -> Markup {
    let is_video = mime.starts_with("video/");
    html! {
        @match (thumb, is_video, infohash) {
            (Some(t), _, _) => { img.cover src=(t) loading="lazy" alt=""; }
            (None, false, Some(ih)) => { img.cover src=(format!("/t/{ih}/f/0")) loading="lazy" alt=""; }
            _ => { (placeholder(mime)) }
        }
        @if is_video { span.playbadge {} }
    }
}

fn placeholder(mime: &str) -> Markup {
    let label = mime.rsplit('/').next().unwrap_or("file").to_uppercase();
    html! { div.placeholder { span { (label) } } }
}

fn toolbar(base: &str, q: &str, sort: db::Sort, placeholder: &str) -> Markup {
    html! {
        div.toolbar {
            form.search method="get" action=(base) {
                input type="search" name="q" value=(q) placeholder=(placeholder);
                @if sort != db::Sort::Recent { input type="hidden" name="sort" value=(sort.as_str()); }
                button { "go" }
            }
            div.tabs {
                @for (label, s) in [("Recent", db::Sort::Recent), ("Popular", db::Sort::Popular), ("Random", db::Sort::Random)] {
                    a.tab.active[sort == s] href=(browse_href(base, s, q, 0)) { (label) }
                }
            }
        }
    }
}

fn pager(base: &str, sort: db::Sort, q: &str, page: i64, has_next: bool) -> Markup {
    html! {
        @if page > 0 || has_next {
            div.pager {
                @if page > 0 {
                    a.btn.ghost href=(browse_href(base, sort, q, page - 1)) { "prev" }
                } @else {
                    span.btn.ghost.off { "prev" }
                }
                span.muted { "page " (page + 1) }
                @if has_next {
                    a.btn.ghost href=(browse_href(base, sort, q, page + 1)) { "next" }
                } @else {
                    span.btn.ghost.off { "next" }
                }
            }
        }
    }
}

fn browse_href(base: &str, sort: db::Sort, q: &str, page: i64) -> String {
    let mut out = format!("{base}?sort={}", sort.as_str());
    if !q.is_empty() {
        out.push_str(&format!("&q={}", qs_encode(q)));
    }
    if page > 0 {
        out.push_str(&format!("&page={page}"));
    }
    out
}

fn welcome(cfg: &config::BoardConfig, featured: Option<&db::PostCard>) -> Markup {
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
        } @else if let Some(f) = featured {
            a.hero href=(format!("/p/{}/{}/{}", f.platform, f.creator_id, f.post_id)) {
                div.cardthumb { (card_thumb(f.thumb.as_deref(), f.infohash.as_deref(), &f.mime)) }
                div.herobody {
                    span.eyebrow { "Latest" }
                    h1 { (f.post_title.clone().unwrap_or_else(|| f.post_id.clone())) }
                    p.muted { (f.creator) @if let Some(at) = &f.posted_at { " - " (pretty_date(at)) } }
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
    let rows = db::feed(pool, limit, q.before, &scope).await.unwrap_or_default();
    let items = rows.iter().map(|m| feed_item(base, m)).collect();
    let self_href = format!("{base}/feed.xml?limit={limit}{scope_qs}");
    // a full page means older torrents remain: hand out the cursor to the next page of this same slice
    let next = (rows.len() as i64 == limit)
        .then(|| rows.last())
        .flatten()
        .map(|last| format!("{base}/feed.xml?before={}&limit={limit}{scope_qs}", last.created_at));
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
                span.chip.platform { (platform) }
                span.muted { (posts.len()) @if posts.len() == 1 { " post" } @else { " posts" } }
                a.btn.ghost href=(format!("/feed.xml?platform={}&creator={}", qs_encode(&platform), qs_encode(&creator_id))) { "seed feed" }
            }
            @if posts.is_empty() { p.muted { "Nothing here yet" } }
            (posts_grid(&posts))
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
    let body = first.map(|f| f.content.clone()).unwrap_or_default();
    let creator = first.map(|f| f.creator.clone());

    // count one view per browser: a rolling cookie holds the last post opened, so a refresh does not re-count
    let token = view_token(&platform, &creator_id, &post_id);
    let repeat = cookie_value(&headers, "lastview").as_deref() == Some(token.as_str());
    if !repeat && !files.is_empty() {
        let _ = db::bump_views(&pool, &platform, &creator_id, &post_id).await;
    }

    let page = render(
        &title,
        html! {
            div.crumbs {
                a href="/posts" { "Posts" }
                @if let Some(f) = first {
                    " / " a href=(format!("/c/{}/{}", f.platform, f.creator_id)) { (f.creator) }
                }
            }
            h1.pagetitle { (title) }
            @if let Some(c) = &creator {
                p.muted { "by " a.strong href=(format!("/c/{platform}/{creator_id}")) { (c) } }
            }
            @if !body.is_empty() { div.body { (PreEscaped(body)) } }
            @if files.is_empty() { p.muted { "This post has no files, or they are hidden" } }
            div.gallery {
                @for f in &files {
                    figure.file {
                        (media_block(f))
                        figcaption.muted { (f.filename.clone().unwrap_or_else(|| f.file_hash.clone())) " - " (human_size(f.size)) }
                    }
                }
            }
            script { (PreEscaped(THUMB_JS)) }
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

// the preview is embedded in the event as a webp data URI, so it renders with zero fetch and no seeder;
// the full file is content-addressed and pulled from the gateway over HTTP only when the thumb is clicked
fn media_block(f: &db::ManifestRow) -> Markup {
    let Some(infohash) = f.infohash.as_deref() else {
        return html! { p.muted { "unavailable: manifest carries no infohash" } };
    };
    let full = format!("/t/{infohash}/f/0");
    let is_video = f.mime.starts_with("video/");
    html! {
        @match f.thumb.as_deref() {
            Some(thumb) => {
                div.media data-full=(full) data-video=[is_video.then_some("1")] {
                    img.thumb src=(thumb) loading="lazy" alt="";
                }
            }
            None => {
                @if is_video {
                    video controls preload="metadata" src=(full) {}
                } @else {
                    img src=(full) loading="lazy" alt="" onerror="bakemonoErr(this)";
                }
            }
        }
    }
}

// the gateway is the only thing here that speaks BitTorrent: it joins a swarm cold for an infohash the
// board carries and hands the bytes back as plain HTTP, so browsers do no P2P

async fn gateway_meta(State(state): State<AppState>, Path(infohash): Path<String>) -> Response {
    let Some(magnet) = resolve_magnet(&state, &infohash).await else {
        return (StatusCode::NOT_FOUND, "unknown infohash").into_response();
    };
    match state.gateway.meta(&magnet).await {
        Ok(meta) => Json(meta).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, format!("swarm error: {e:#}")).into_response(),
    }
}

async fn gateway_file(
    State(state): State<AppState>,
    Path((infohash, file_index)): Path<(String, usize)>,
    headers: HeaderMap,
) -> Response {
    let Some(magnet) = resolve_magnet(&state, &infohash).await else {
        return (StatusCode::NOT_FOUND, "unknown infohash").into_response();
    };
    match state.gateway.open(&magnet, file_index).await {
        Ok(file) => stream_file(file, &headers).await,
        Err(e) => (StatusCode::BAD_GATEWAY, format!("swarm error: {e:#}")).into_response(),
    }
}

// only infohashes the board carries (and that pass moderation) are served, so the gateway is never an open
// proxy. BAKEMONO_GATEWAY_OPEN lifts the catalog check for local testing of a cold load
async fn resolve_magnet(state: &AppState, infohash: &str) -> Option<String> {
    let infohash = infohash.trim().to_ascii_lowercase();
    if infohash.len() != 40 || !infohash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    if let Ok(Some(magnet)) = db::magnet_by_infohash(&state.pool, &infohash).await {
        return Some(magnet);
    }
    if env_opt("BAKEMONO_GATEWAY_OPEN").is_some() {
        let trackers: Vec<String> = bakemono_core::default_trackers()
            .into_iter()
            .filter(|t| !t.starts_with("wss://"))
            .collect();
        return Some(bakemono_torrent::synth_magnet(&infohash, &trackers));
    }
    None
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
    let groups = db::pending_groups(&state.pool, 50).await.unwrap_or_default();
    let pending = db::pending_pubkeys(&state.pool, 100).await.unwrap_or_default();
    let takedowns = db::takedowns(&state.pool).await.unwrap_or_default();
    render(
        "mod queue",
        html! {
            p { a href="/" { "< home" } }
            h2 { "Mod queue" }
            p.muted { "first-seen pubkeys wait here; approve to publish their files, reject to drop them" }
            @if groups.is_empty() && pending.is_empty() { p.muted { "nothing awaiting review" } }
            @if !groups.is_empty() {
                h3 { "By creator" }
                p.muted { "bulk-act on a flood: approve or reject every pending key that posted to a creator" }
                ul.list {
                    @for g in &groups {
                        li {
                            div { (g.creator.clone().unwrap_or_else(|| g.creator_id.clone())) " " span.muted { "(" (g.platform) ")" } }
                            span.muted { (g.pubkeys) " pubkey(s) - " (g.files) " file(s)" }
                            div {
                                form method="post" action="/mod/approve-creator" class="modform" {
                                    input type="hidden" name="platform" value=(g.platform);
                                    input type="hidden" name="creator_id" value=(g.creator_id);
                                    button { "approve all" }
                                }
                                form method="post" action="/mod/reject-creator" class="modform" {
                                    input type="hidden" name="platform" value=(g.platform);
                                    input type="hidden" name="creator_id" value=(g.creator_id);
                                    button { "reject all" }
                                }
                            }
                        }
                    }
                }
            }
            @if !pending.is_empty() {
                h3 { "By pubkey" }
                p.muted { "newest first, capped at 100; clear the backlog or use the per-creator actions above" }
                ul.list {
                    @for p in &pending {
                        li {
                            div { code { (npub(&p.pubkey)) } }
                            span.muted {
                                (p.files) " file(s)"
                                @if let Some(c) = &p.creator { " - " (c) }
                                @if let Some(s) = &p.sample { " - " (s) }
                            }
                            div {
                                form method="post" action="/mod/approve" class="modform" {
                                    input type="hidden" name="pubkey" value=(p.pubkey);
                                    button { "approve" }
                                }
                                form method="post" action="/mod/reject" class="modform" {
                                    input type="hidden" name="pubkey" value=(p.pubkey);
                                    button { "reject" }
                                }
                            }
                        }
                    }
                }
            }
            (takedown_section(&state, &takedowns))
        },
    )
    .into_response()
}

fn takedown_section(state: &AppState, takedowns: &[db::TakedownRow]) -> Markup {
    html! {
        h2 { "Takedowns" }
        @match &state.signer {
            Some(keys) => p.muted { "publishing kind 31064 as " code { (npub(&keys.public_key().to_hex())) } }
            None => p.muted { "set BAKEMONO_INSTANCE_NSEC to publish takedowns to peers; hides apply locally either way" }
        }
        form method="post" action="/mod/takedown" class="takedown" {
            select name="target_type" {
                option value="e" { "event id" }
                option value="x" { "file hash" }
                option value="p" { "pubkey" }
            }
            input type="text" name="target" placeholder="target value (id / hash / npub or hex)" required;
            input type="text" name="reason" placeholder="reason (dmca-us, csam, spam...)" required;
            input type="text" name="explanation" placeholder="note (optional)";
            button { "hide + publish" }
        }
        @if takedowns.is_empty() { p.muted { "no takedowns recorded" } }
        ul.list {
            @for t in takedowns {
                li {
                    div { code { (t.target_type) ":" (t.target) } }
                    span.muted {
                        (t.reason)
                        @if !t.explanation.is_empty() { " - " (t.explanation) }
                        " - via " (takedown_source(&t.source))
                        @if !t.applied_at.is_empty() { " - " (pretty_date(&t.applied_at)) }
                    }
                    form method="post" action="/mod/untakedown" class="modform" {
                        input type="hidden" name="d_tag" value=(t.d_tag);
                        button { "undo" }
                    }
                }
            }
        }
    }
}

async fn mod_approve(
    State(pool): State<PgPool>,
    headers: HeaderMap,
    Form(form): Form<ModForm>,
) -> Response {
    if let Err(denied) = require_mod(&headers) {
        return denied;
    }
    let _ = db::approve_pubkey(&pool, &form.pubkey).await;
    Redirect::to("/mod").into_response()
}

async fn mod_reject(
    State(pool): State<PgPool>,
    headers: HeaderMap,
    Form(form): Form<ModForm>,
) -> Response {
    if let Err(denied) = require_mod(&headers) {
        return denied;
    }
    let _ = db::reject_pubkey(&pool, &form.pubkey).await;
    Redirect::to("/mod").into_response()
}

async fn mod_approve_creator(
    State(pool): State<PgPool>,
    headers: HeaderMap,
    Form(form): Form<CreatorForm>,
) -> Response {
    if let Err(denied) = require_mod(&headers) {
        return denied;
    }
    let _ = db::approve_creator(&pool, &form.platform, &form.creator_id).await;
    Redirect::to("/mod").into_response()
}

async fn mod_reject_creator(
    State(pool): State<PgPool>,
    headers: HeaderMap,
    Form(form): Form<CreatorForm>,
) -> Response {
    if let Err(denied) = require_mod(&headers) {
        return denied;
    }
    let _ = db::reject_creator(&pool, &form.platform, &form.creator_id).await;
    Redirect::to("/mod").into_response()
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

#[derive(serde::Deserialize)]
struct ModForm {
    pubkey: String,
}

#[derive(serde::Deserialize)]
struct CreatorForm {
    platform: String,
    creator_id: String,
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
    let token = std::env::var("BAKEMONO_MOD_TOKEN").unwrap_or_default();
    if token.is_empty() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "mod queue disabled; set BAKEMONO_MOD_TOKEN on the board",
        )
            .into_response());
    }
    if basic_auth_password(headers).as_deref() == Some(token.as_str()) {
        return Ok(());
    }
    Err((
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"bakemono mod\"")],
        "authentication required",
    )
        .into_response())
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

// bytes as a short human string for file captions
fn human_size(bytes: i64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
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
                        form.topsearch method="get" action="/posts" {
                            input type="search" name="q" placeholder="Search" aria-label="Search";
                        }
                        div.navwrap {
                            a.shuffle href="/random" title="Random post" aria-label="Random post" { (PreEscaped(ICON_SHUFFLE)) }
                            nav {
                                a href="/creators" { "Creators" }
                                a href="/posts" { "Posts" }
                                a href="/contribute" { "Contribute" }
                            }
                        }
                    }
                    main { (body) }
                    (footer(cfg))
                }
            }
        }
        .into_string(),
    )
}

fn footer(cfg: &config::BoardConfig) -> Markup {
    html! {
        footer.foot {
            @if !cfg.community.is_empty() {
                div.community {
                    @for l in &cfg.community {
                        a.chip href=(l.url) rel="noopener noreferrer" target="_blank" { (l.label) }
                    }
                }
            }
            div.footlinks {
                a href="/info" { "Info" }
                a href="/keepers" { "Keepers" }
                a href="/contribute" { "Contribute" }
                @if let Some(email) = &cfg.contact { a href=(format!("mailto:{email}")) { "Contact" } }
            }
            p.small.muted {
                "Files are served from a peer swarm, not stored here. "
                a href="/info" { "How this works" }
            }
        }
    }
}

const REPO: &str = env!("CARGO_PKG_REPOSITORY");

// per-OS desktop builds, served by GitHub's stable latest-release redirect; names track Tauri's bundles
const DOWNLOADS: &[(&str, &str)] = &[
    ("Windows", "Bakemono_x64-setup.exe"),
    ("macOS (Apple Silicon)", "Bakemono_aarch64.dmg"),
    ("Linux (.deb)", "Bakemono_amd64.deb"),
];

// two crossing arrows for the Random button; inline so the page pulls no icon font or external asset
const ICON_SHUFFLE: &str = "<svg viewBox='0 0 24 24' width='18' height='18' fill='none' stroke='currentColor' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'><path d='M16 3h5v5'/><path d='M4 20 21 3'/><path d='M21 16v5h-5'/><path d='M15 15l6 6'/><path d='M4 4l5 5'/></svg>";

// Catppuccin Mocha, self-hosted and static: no external font, no CDN, no third-party request from any page
const STYLE: &str = "
:root {
  --base:#1e1e2e; --mantle:#181825; --crust:#11111b;
  --surface0:#313244; --surface1:#45475a; --surface2:#585b70;
  --overlay0:#6c7086; --overlay1:#7f849c;
  --text:#cdd6f4; --subtext1:#bac2de; --subtext0:#a6adc8;
  --mauve:#cba6f7; --red:#f38ba8;
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
.brand { display:flex; align-items:center; gap:.5rem; font-weight:800; font-size:1.15rem; color:var(--text) }
.brand:hover { text-decoration:none }
.brandmascot { width:28px; height:28px; border-radius:7px; object-fit:cover }
.topsearch { flex:1; max-width:520px; margin:0 auto }
.topsearch input { width:100%; padding:.55rem .95rem; border-radius:999px; border:1px solid var(--surface1);
  background:var(--surface0); color:var(--text) }
.topsearch input:focus { outline:none; border-color:var(--accent) }
.navwrap { display:flex; align-items:center; gap:1rem; margin-left:auto }
.topbar nav { display:flex; gap:1.1rem }
.topbar nav a { color:var(--subtext1); font-weight:600 }
.topbar nav a:hover { color:var(--text); text-decoration:none }
.shuffle { display:grid; place-items:center; width:38px; height:38px; border-radius:10px;
  background:var(--surface0); color:var(--subtext1); border:1px solid var(--surface1) }
.shuffle:hover { color:var(--crust); background:var(--accent); border-color:var(--accent) }

main { max-width:1240px; margin:0 auto; padding:1.4rem 1.1rem 3rem }
.pagetitle { font-size:1.6rem; margin:.2rem 0 1rem }
.block { margin:1.8rem 0 }
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
  border-radius:14px; overflow:hidden; color:var(--text);
  transition:transform .12s ease, border-color .12s ease, box-shadow .12s ease }
.card:hover { transform:translateY(-3px); border-color:var(--accent); box-shadow:0 8px 24px #0006; text-decoration:none }
.cardthumb { position:relative; aspect-ratio:3/4; background:var(--crust); overflow:hidden }
.cardthumb.banner { aspect-ratio:16/10 }
.cover { width:100%; height:100%; object-fit:cover }
.placeholder { position:absolute; inset:0; display:grid; place-items:center; color:var(--overlay1);
  font-size:.8rem; letter-spacing:.08em; background:linear-gradient(135deg,var(--surface0),var(--crust)) }
.playbadge { position:absolute; top:8px; left:8px; padding:.15rem .45rem; border-radius:6px;
  background:#000a; color:#fff; font-size:.65rem; font-weight:700; letter-spacing:.06em }
.playbadge::before { content:'VIDEO' }
.cardmeta { padding:.55rem .6rem .65rem }
.cardtitle { font-weight:600; font-size:.9rem; display:-webkit-box; -webkit-line-clamp:2;
  -webkit-box-orient:vertical; overflow:hidden }
.cardsub { margin-top:.3rem; color:var(--subtext0); font-size:.76rem }
.cardsub .strong { color:var(--subtext1); font-weight:600 }

.chip { display:inline-block; padding:.12rem .5rem; border-radius:999px; background:var(--surface1);
  color:var(--subtext1); font-size:.72rem; font-weight:600 }
.chip.platform { background:color-mix(in srgb, var(--accent) 22%, var(--surface1)); color:var(--text) }

.hero { display:block; position:relative; border-radius:18px; overflow:hidden; margin-bottom:1.6rem; color:#fff }
.hero .cardthumb { aspect-ratio:21/9 }
.hero:hover { text-decoration:none }
.herobody { position:absolute; left:0; right:0; bottom:0; padding:1.4rem 1.6rem;
  background:linear-gradient(to top,#000e,#0000) }
.herobody h1 { margin:.2rem 0 .1rem; font-size:1.9rem }
.eyebrow { text-transform:uppercase; letter-spacing:.12em; font-size:.72rem; color:var(--accent); font-weight:700 }
.hero .muted { color:#cdd6f4bb }

.toolbar { display:flex; flex-wrap:wrap; gap:.8rem; align-items:center; justify-content:space-between; margin-bottom:1.1rem }
.toolbar .search { display:flex; gap:.4rem; flex:1; min-width:220px; max-width:420px; margin:0 }
.toolbar .search input { flex:1; padding:.5rem .8rem; border-radius:10px; border:1px solid var(--surface1);
  background:var(--surface0); color:var(--text) }
.tabs { display:flex; gap:.4rem; background:var(--surface0); padding:.25rem; border-radius:12px }
.tab { padding:.4rem .85rem; border-radius:9px; color:var(--subtext1); font-weight:600; font-size:.9rem }
.tab:hover { color:var(--text); text-decoration:none }
.tab.active { background:var(--accent); color:var(--crust) }

.btn { display:inline-block; padding:.5rem .9rem; border-radius:10px; background:var(--accent);
  color:var(--crust); font-weight:700; border:none; cursor:pointer }
.btn:hover { filter:brightness(1.08); text-decoration:none }
.btn.ghost { background:var(--surface0); color:var(--text); border:1px solid var(--surface1) }
.btn.ghost.off { opacity:.4; pointer-events:none }
.search button, .takedown button { padding:.5rem .8rem; border-radius:10px; background:var(--accent);
  color:var(--crust); font-weight:700; border:none; cursor:pointer }
.pager { display:flex; gap:1rem; align-items:center; justify-content:center; margin:1.6rem 0 }

.welcome { display:flex; gap:1.4rem; align-items:center; background:var(--mantle); border:1px solid var(--surface0);
  border-radius:16px; padding:1.4rem 1.6rem; margin-bottom:1.6rem }
.mascot { width:150px; height:auto; border-radius:12px; flex:none }
.welcometext h1 { margin:0 0 .3rem }
.tagline { color:var(--subtext0); margin:.2rem 0 .6rem }
.welcomebody { color:var(--subtext1) }

.gallery { display:flex; flex-direction:column; gap:1.2rem; margin-top:1rem }
.file { margin:0; padding:0; border:none }
figcaption { margin-top:.4rem; font-size:.8rem }
.media { display:inline-block; position:relative; cursor:pointer; border-radius:12px; overflow:hidden; background:var(--crust) }
.media img.thumb { max-width:min(100%,520px); height:auto; display:block; border-radius:12px }
.media.loading { opacity:.6 }
.media.loading::after { content:'loading...'; position:absolute; inset:0; display:grid; place-items:center; color:#fff; background:#0007 }
.file img, .file video { max-width:min(100%,760px); border-radius:12px }
.body { margin:1rem 0; color:var(--subtext1) }
.body img { max-width:100%; border-radius:10px }

.foot { border-top:1px solid var(--surface0); margin-top:2.5rem; padding:1.6rem 1.1rem; text-align:center; color:var(--subtext0) }
.community { display:flex; flex-wrap:wrap; gap:.5rem; justify-content:center; margin-bottom:1rem }
.community .chip { padding:.35rem .8rem; font-size:.85rem; background:var(--surface0); border:1px solid var(--surface1) }
.community .chip:hover { border-color:var(--accent); color:var(--text); text-decoration:none }
.footlinks { display:flex; gap:1.2rem; justify-content:center; flex-wrap:wrap; margin-bottom:.6rem }
.footlinks a { color:var(--subtext1); font-weight:600 }
.small { font-size:.8rem }

.muted { color:var(--subtext0) }
.strong { color:var(--subtext1); font-weight:600 }
.error { color:var(--red) }
.list { list-style:none; padding:0 }
.list li { padding:.5rem 0; border-bottom:1px solid var(--surface0) }
.stats { display:grid; grid-template-columns:repeat(auto-fit,minmax(8rem,1fr)); gap:.75rem; margin:1rem 0 1.5rem }
.stat { border:1px solid var(--surface0); background:var(--mantle); border-radius:12px; padding:1rem }
.stat .num { font-size:1.7rem; font-weight:800 }
.stat .label { color:var(--subtext0); font-size:.85em }
.steps { list-style:none; counter-reset:step; padding:0 }
.step { counter-increment:step; border:1px solid var(--surface0); background:var(--mantle); border-radius:14px; padding:1.1rem 1.3rem; margin:1rem 0 }
.step h3 { margin:0 0 .5rem }
.step h3::before { content:counter(step) '. '; color:var(--accent); font-weight:800 }
.step img { max-width:100%; border-radius:8px; margin-top:.6rem }
.downloads { display:flex; flex-wrap:wrap; gap:.5rem; margin:.75rem 0 }
.modform { display:inline; margin:.4rem .4rem 0 0 }
.takedown { display:flex; flex-wrap:wrap; gap:.4rem; margin:.6rem 0 1rem }
.takedown input, .takedown select { flex:1 1 12rem; padding:.5rem .7rem; border-radius:9px; border:1px solid var(--surface1); background:var(--surface0); color:var(--text) }
code { background:var(--surface0); padding:.1rem .35rem; border-radius:5px; word-break:break-all; font-size:.85em }
pre { white-space:pre-wrap; word-break:break-all; background:var(--mantle); border:1px solid var(--surface0); padding:.7rem .9rem; border-radius:10px }

@media (max-width:640px) {
  .topsearch { display:none }
  .grid { grid-template-columns:repeat(auto-fill,minmax(140px,1fr)); gap:10px }
  .welcome { flex-direction:column; text-align:center }
  .mascot { width:120px }
}
";

const THUMB_JS: &str = "
// the gateway 502s when it can't reach a seeder; show that instead of a broken-image icon
function bakemonoErr(node) {
  const box = node.closest('.media') || node
  const msg = document.createElement('p'); msg.className = 'muted'
  msg.textContent = 'unavailable - no seeders online right now'
  box.classList.remove('loading'); box.replaceChildren(msg)
}
// the full file is fetched only on click, never prefetched, so a page of thumbnails stays cheap.
// the inline thumbnail stays visible under a loading overlay until the full lands, then swaps in
for (const el of document.querySelectorAll('.media')) {
  const full = el.dataset.full
  const isVideo = el.dataset.video === '1'
  el.title = 'click to load the full file'
  el.addEventListener('click', () => {
    if (el.dataset.open) return
    el.dataset.open = '1'
    if (isVideo) {
      const v = document.createElement('video'); v.controls = true; v.autoplay = true; v.src = full
      v.onerror = () => bakemonoErr(el)
      el.replaceChildren(v)
      return
    }
    el.classList.add('loading')
    const img = new Image(); img.alt = ''
    img.onload = () => { el.classList.remove('loading'); el.replaceChildren(img) }
    img.onerror = () => { el.dataset.open = ''; bakemonoErr(el) }
    img.src = full
  })
}
";

#[cfg(test)]
mod tests {
    use super::{build_feed, endangered_item, feed_item, pretty_date};
    use crate::db::{EndangeredRow, ManifestRow};

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
