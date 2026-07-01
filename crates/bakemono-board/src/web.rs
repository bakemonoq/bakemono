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

async fn home(State(pool): State<PgPool>, Query(query): Query<HomeQuery>) -> Html<String> {
    let q = query.q.unwrap_or_default().trim().to_string();
    let creators = if q.is_empty() {
        db::creators(&pool).await.unwrap_or_default()
    } else {
        db::search_creators(&pool, &q).await.unwrap_or_default()
    };
    let recent = if q.is_empty() {
        db::recent(&pool, 24).await.unwrap_or_default()
    } else {
        Vec::new()
    };
    render(
        "",
        html! {
            form.search method="get" action="/" {
                input type="search" name="q" value=(q) placeholder="search authors" autofocus;
                button { "search" }
            }
            h2 { "Authors" }
            @if creators.is_empty() {
                @if q.is_empty() {
                    p.muted { "Nothing indexed yet. Publish some manifests to a relay the board subscribes to" }
                } @else {
                    p.muted { "No authors match \"" (q) "\"" }
                }
            }
            ul.list {
                @for c in &creators {
                    li {
                        a href=(format!("/c/{}/{}", c.platform, c.creator_id)) { (c.creator) }
                        span.muted { " " (c.platform) " - " (c.posts) " posts, " (c.files) " files" }
                    }
                }
            }
            @if !recent.is_empty() {
                h2 { "Recent files" }
                ul.list {
                    @for m in &recent {
                        li {
                            a href=(format!("/p/{}/{}/{}", m.platform, m.creator_id, m.post_id)) {
                                (m.post_title.clone().unwrap_or_else(|| m.post_id.clone()))
                            }
                            span.muted { " " (m.creator) " - " (m.mime) }
                        }
                    }
                }
            }
        },
    )
}

#[derive(serde::Deserialize)]
struct HomeQuery {
    q: Option<String>,
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
    let posts = db::posts_by_creator(&pool, &platform, &creator_id)
        .await
        .unwrap_or_default();
    let name = posts
        .first()
        .map(|p| p.creator.clone())
        .unwrap_or_else(|| creator_id.clone());
    render(
        &name,
        html! {
            p { a href="/" { "< home" } }
            h2 { (name) " " span.muted { "(" (platform) ")" } }
            ul.list {
                @for p in &posts {
                    li {
                        a href=(format!("/p/{}/{}/{}", p.platform, p.creator_id, p.post_id)) {
                            (p.post_title.clone().unwrap_or_else(|| p.post_id.clone()))
                        }
                        span.muted { " " (p.files) " files" @if let Some(at) = &p.posted_at { " - " (pretty_date(at)) } }
                    }
                }
            }
        },
    )
}

async fn post_page(
    State(pool): State<PgPool>,
    Path((platform, creator_id, post_id)): Path<(String, String, String)>,
) -> Html<String> {
    let files = db::post_files(&pool, &platform, &creator_id, &post_id)
        .await
        .unwrap_or_default();
    let first = files.first();
    let title = first
        .and_then(|f| f.post_title.clone())
        .unwrap_or_else(|| post_id.clone());
    let body = first.map(|f| f.content.clone()).unwrap_or_default();

    render(
        &title,
        html! {
            p {
                @if let Some(f) = first {
                    a href=(format!("/c/{}/{}", f.platform, f.creator_id)) { "< " (f.creator) }
                }
            }
            h2 { (title) }
            @if !body.is_empty() { div.body { (PreEscaped(body)) } }
            @for f in &files {
                div.file {
                    p.muted { (f.filename.clone().unwrap_or_else(|| f.file_hash.clone())) " - " (f.size) " bytes" }
                    (media_block(f))
                }
            }
            script { (PreEscaped(THUMB_JS)) }
        },
    )
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
    env_opt("BAKEMONO_BOARD_NAME").unwrap_or_else(|| "化け物 bakemono".to_string())
}

fn dmca_contact() -> Option<String> {
    env_opt("BAKEMONO_DMCA_CONTACT")
}

fn contact() -> Option<String> {
    env_opt("BAKEMONO_CONTACT")
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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
    let board = board_name();
    let tab = if title.is_empty() {
        board.clone()
    } else {
        format!("{title} - {board}")
    };
    Html(
        html! {
            (DOCTYPE)
            html lang="en" {
                head {
                    meta charset="utf-8";
                    meta name="viewport" content="width=device-width, initial-scale=1";
                    title { (tab) }
                    link rel="alternate" type="application/rss+xml" title="seed feed" href="/feed.xml";
                    style { (PreEscaped(STYLE)) }
                }
                body {
                    header {
                        a.brand href="/" { (board) }
                        nav {
                            a href="/" { "Browse" }
                            a href="/contribute" { "Contribute" }
                            a href="/keepers" { "Keepers" }
                            a href="/info" { "Info" }
                        }
                    }
                    main { (body) }
                }
            }
        }
        .into_string(),
    )
}

const REPO: &str = env!("CARGO_PKG_REPOSITORY");

// per-OS desktop builds, served by GitHub's stable latest-release redirect; names track Tauri's bundles
const DOWNLOADS: &[(&str, &str)] = &[
    ("Windows", "Bakemono_x64-setup.exe"),
    ("macOS (Apple Silicon)", "Bakemono_aarch64.dmg"),
    ("Linux (.deb)", "Bakemono_amd64.deb"),
];

const STYLE: &str = "
:root { color-scheme: light dark }
body { font-family: system-ui, sans-serif; max-width: 820px; margin: 0 auto; padding: 1rem }
header { display: flex; align-items: baseline; flex-wrap: wrap; gap: .5rem 1rem; border-bottom: 1px solid #8884; margin-bottom: 1rem; padding-bottom: .5rem }
.brand { font-weight: 700; text-decoration: none; color: inherit }
nav { margin-left: auto; display: flex; gap: 1rem }
nav a { text-decoration: none }
.list { list-style: none; padding: 0 }
.list li { padding: .35rem 0; border-bottom: 1px solid #8882 }
.muted { color: #8888 }
.error { color: #e4564a }
.body { margin: 1rem 0 }
.file { margin: 1rem 0; padding: .5rem; border: 1px solid #8884; border-radius: 6px }
.file img, .file video { max-width: 100%; display: block; margin-top: .5rem }
.media { display: inline-block; position: relative; cursor: pointer; margin-top: .5rem }
.media img.thumb { max-width: 360px; border-radius: 6px }
.media.loading { opacity: .55 }
.media.loading::after { content: 'loading...'; position: absolute; inset: 0; display: grid; place-items: center; color: #fff; background: #0007; border-radius: 6px }
.modform { display: inline; margin: .4rem .4rem 0 0 }
.takedown { display: flex; flex-wrap: wrap; gap: .4rem; margin: .6rem 0 1rem }
.takedown input { flex: 1 1 12rem }
.search { display: flex; gap: .4rem; margin-bottom: 1.2rem }
.search input { flex: 1 }
.steps { list-style: none; counter-reset: step; padding: 0 }
.step { counter-increment: step; border: 1px solid #8884; border-radius: 8px; padding: 1rem 1.25rem; margin: 1rem 0 }
.step h3 { margin: 0 0 .5rem }
.step h3::before { content: counter(step) '. '; color: #4488ff; font-weight: 700 }
.step img { max-width: 100%; border-radius: 6px; margin-top: .75rem; display: block }
.downloads { display: flex; flex-wrap: wrap; gap: .5rem; margin: .75rem 0 }
.btn { display: inline-block; padding: .55rem .9rem; border-radius: 6px; background: #4488ff; color: #fff; text-decoration: none; font-weight: 600 }
.btn:hover { filter: brightness(1.08) }
.stats { display: grid; grid-template-columns: repeat(auto-fit, minmax(7.5rem, 1fr)); gap: .75rem; margin: 1rem 0 1.5rem }
.stat { border: 1px solid #8884; border-radius: 8px; padding: .9rem 1rem }
.stat .num { font-size: 1.6rem; font-weight: 700 }
.stat .label { color: #8888; font-size: .85em }
code { word-break: break-all; font-size: .85em }
pre { white-space: pre-wrap; word-break: break-all; background: #8881; padding: .6rem .8rem; border-radius: 6px }
a { color: #4488ff }
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
