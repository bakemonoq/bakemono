use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;
use maud::{html, Markup, PreEscaped, DOCTYPE};
use sqlx::postgres::PgPool;

use crate::db;

pub fn router(pool: PgPool) -> Router {
    Router::new()
        .route("/", get(home))
        .route("/c/{platform}/{creator_id}", get(creator_page))
        .route("/p/{platform}/{creator_id}/{post_id}", get(post_page))
        .route("/webtorrent.min.js", get(webtorrent_js))
        .with_state(pool)
}

async fn home(State(pool): State<PgPool>) -> Html<String> {
    let creators = db::creators(&pool).await.unwrap_or_default();
    let recent = db::recent(&pool, 24).await.unwrap_or_default();
    render(
        "Bakemono",
        html! {
            h2 { "Creators" }
            @if creators.is_empty() {
                p.muted { "Nothing indexed yet. Publish some manifests to a relay the board subscribes to" }
            }
            ul.list {
                @for c in &creators {
                    li {
                        a href=(format!("/c/{}/{}", c.platform, c.creator_id)) { (c.creator) }
                        span.muted { " " (c.platform) " - " (c.posts) " posts, " (c.files) " files" }
                    }
                }
            }
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
        },
    )
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
                        span.muted { " " (p.files) " files" @if let Some(at) = &p.posted_at { " - " (at) } }
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
                div.file data-magnet=(f.magnet) data-mime=(f.mime) {
                    p.muted { (f.filename.clone().unwrap_or_else(|| f.file_hash.clone())) " - " (f.size) " bytes" }
                }
            }
            script type="module" { (PreEscaped(PLAYER_JS)) }
        },
    )
}

async fn webtorrent_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/javascript")],
        WEBTORRENT_JS,
    )
}

fn render(title: &str, body: Markup) -> Html<String> {
    Html(
        html! {
            (DOCTYPE)
            html lang="en" {
                head {
                    meta charset="utf-8";
                    meta name="viewport" content="width=device-width, initial-scale=1";
                    title { (title) }
                    style { (PreEscaped(STYLE)) }
                }
                body {
                    header { a.brand href="/" { "化け物 bakemono" } }
                    main { (body) }
                }
            }
        }
        .into_string(),
    )
}

const WEBTORRENT_JS: &str = include_str!("../assets/webtorrent.min.js");

const STYLE: &str = "
:root { color-scheme: light dark }
body { font-family: system-ui, sans-serif; max-width: 820px; margin: 0 auto; padding: 1rem }
header { border-bottom: 1px solid #8884; margin-bottom: 1rem; padding-bottom: .5rem }
.brand { font-weight: 700; text-decoration: none; color: inherit }
.list { list-style: none; padding: 0 }
.list li { padding: .35rem 0; border-bottom: 1px solid #8882 }
.muted { color: #8888 }
.body { margin: 1rem 0 }
.file { margin: 1rem 0; padding: .5rem; border: 1px solid #8884; border-radius: 6px }
.file img, .file video { max-width: 100%; display: block; margin-top: .5rem }
a { color: #4488ff }
";

const PLAYER_JS: &str = "
import WebTorrent from '/webtorrent.min.js'
const client = new WebTorrent()
for (const el of document.querySelectorAll('.file')) {
  const status = document.createElement('p')
  status.className = 'muted'
  status.textContent = 'connecting to swarm...'
  el.appendChild(status)
  const torrent = client.add(el.dataset.magnet)
  const tick = setInterval(() => {
    status.textContent = `peers: ${torrent.numPeers} | ${Math.round(torrent.progress * 100)}%`
  }, 1000)
  torrent.on('ready', () => {
    for (const file of torrent.files) {
      file.getBlobURL((err, url) => {
        if (err) { status.textContent = 'error: ' + err.message; return }
        const isVideo = /\\.(mp4|webm|mov)$/i.test(file.name)
        const node = document.createElement(isVideo ? 'video' : 'img')
        if (isVideo) node.controls = true
        node.src = url
        el.appendChild(node)
      })
    }
  })
  torrent.on('done', () => { clearInterval(tick); status.textContent = 'loaded (' + torrent.numPeers + ' peers)' })
}
";
