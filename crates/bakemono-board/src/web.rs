use axum::extract::{Form, FromRef, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use base64::Engine;
use chrono::Utc;
use maud::{html, Markup, PreEscaped, DOCTYPE};
use nostr_sdk::prelude::{Client, Event, Keys, PublicKey, ToBech32};
use sqlx::postgres::PgPool;

use bakemono_core::{Takedown, Target};

use crate::db;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub relays: Vec<String>,
    pub signer: Option<Keys>,
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
        .route("/contribute", get(contribute))
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
        .route("/webtorrent.min.js", get(webtorrent_js))
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
                div.file data-magnet=(f.magnet) data-mime=(f.mime) data-hash=(f.file_hash) {
                    p.muted {
                        (f.filename.clone().unwrap_or_else(|| f.file_hash.clone())) " - " (f.size) " bytes"
                        a.magnet href=(f.magnet) title="magnet link" { "🧲" }
                    }
                }
            }
            script { (PreEscaped(format!("window.__bakemonoIce = {};", ice_servers_json()))) }
            script type="module" { (PreEscaped(PLAYER_JS)) }
        },
    )
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
            eprintln!("takedown sign failed: {e}");
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
        eprintln!("takedown {id} publish failed (kept local): {e:#}");
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

// BAKEMONO_ICE_SERVERS is a JSON array of RTCIceServer objects, default none (host-only)
fn ice_servers_json() -> String {
    std::env::var("BAKEMONO_ICE_SERVERS").unwrap_or_else(|_| "[]".to_string())
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

async fn webtorrent_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/javascript")],
        WEBTORRENT_JS,
    )
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
                    style { (PreEscaped(STYLE)) }
                }
                body {
                    header {
                        a.brand href="/" { (board) }
                        nav {
                            a href="/" { "Browse" }
                            a href="/contribute" { "Contribute" }
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

#[cfg(test)]
mod tests {
    use super::pretty_date;

    #[test]
    fn formats_iso_dates_and_passes_junk_through() {
        assert_eq!(pretty_date("2026-06-23T17:46:49.000+00:00"), "Jun 23, 2026");
        assert_eq!(pretty_date("2026-01-03 10:00:00"), "Jan 3, 2026");
        assert_eq!(pretty_date("whenever"), "whenever");
    }
}

const WEBTORRENT_JS: &str = include_str!("../assets/webtorrent.min.js");

const REPO: &str = env!("CARGO_PKG_REPOSITORY");

// per-OS desktop builds, served by GitHub's stable latest-release redirect; names track Tauri's bundles
const DOWNLOADS: &[(&str, &str)] = &[
    ("Windows", "Bakemono_x64-setup.exe"),
    ("macOS (Apple Silicon)", "Bakemono_aarch64.dmg"),
    ("Linux", "Bakemono_amd64.AppImage"),
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
.magnet { margin-left: .4rem; text-decoration: none; opacity: .55; font-size: .9em }
.magnet:hover { opacity: 1 }
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
a { color: #4488ff }
";

const PLAYER_JS: &str = "
import WebTorrent from '/webtorrent.min.js'
// WebTorrent needs Web Crypto, which only exists in a secure context (https or http://localhost)
const secure = window.isSecureContext
// iceServers from the board config (empty = host-only, fast on a LAN; set STUN/TURN for the internet)
const iceServers = window.__bakemonoIce || []
const client = secure ? new WebTorrent({ tracker: { rtcConfig: { iceServers } } }) : null
async function sha256Hex(buf) {
  const digest = await crypto.subtle.digest('SHA-256', buf)
  return Array.from(new Uint8Array(digest)).map((b) => b.toString(16).padStart(2, '0')).join('')
}
for (const el of document.querySelectorAll('.file')) {
  const status = document.createElement('p')
  status.className = 'muted'
  el.appendChild(status)
  if (!secure) {
    status.textContent = 'open this board over https or via http://localhost (a LAN IP over http has no Web Crypto)'
    continue
  }
  status.textContent = 'connecting...'
  const want = (el.dataset.hash || '').toLowerCase()
  const torrent = client.add(el.dataset.magnet)
  // tracker complete/incomplete counts include us and 20-min ghost peers, so they lie; numPeers is
  // the only honest 'a seeder is actually reachable' signal, and metadata never arrives without one
  let deadline = Date.now() + 30000
  const tick = setInterval(() => {
    if (torrent.numPeers > 0) {
      deadline = Date.now() + 30000
      status.className = 'muted'
      status.textContent = 'downloading ' + Math.round(torrent.progress * 100) + '%'
    } else if (Date.now() > deadline) {
      status.className = 'error'
      status.textContent = 'no seeders online - nobody is sharing this file right now'
    } else {
      status.className = 'muted'
      status.textContent = 'connecting...'
    }
  }, 500)
  // the magnet is attacker-controlled; only render bytes that match the signed sha256
  torrent.on('ready', async () => {
    let shown = false
    for (const file of torrent.files) {
      let buf
      try {
        buf = await file.arrayBuffer()
      } catch (err) {
        clearInterval(tick)
        status.className = 'error'
        status.textContent = 'error: ' + err.message
        return
      }
      if (want && (await sha256Hex(buf)) !== want) continue
      const isVideo = /\\.(mp4|webm|mov)$/i.test(file.name)
      const node = document.createElement(isVideo ? 'video' : 'img')
      if (isVideo) node.controls = true
      node.src = URL.createObjectURL(new Blob([buf], { type: el.dataset.mime || '' }))
      el.appendChild(node)
      shown = true
    }
    clearInterval(tick)
    if (shown) {
      status.remove()
    } else {
      status.className = 'error'
      status.textContent = 'integrity check failed - file does not match its signed hash'
    }
  })
}
";
