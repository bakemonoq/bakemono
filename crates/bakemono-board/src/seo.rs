use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use sqlx::postgres::PgPool;

use crate::config;
use crate::db;

// well under the 50k-URL / 50MB sitemap ceiling, so a chunk never needs splitting
const SITEMAP_CHUNK: i64 = 40_000;

// most URLs an IndexNow submission carries in one request; the engines cap a batch at 10k
const INDEXNOW_MAX: i64 = 9000;

pub fn routes() -> axum::Router<crate::web::AppState> {
    axum::Router::new()
        .route("/robots.txt", get(robots))
        .route("/sitemap.xml", get(sitemap_index))
        .route("/sitemap-creators.xml", get(sitemap_creators))
        .route("/sitemap-posts/{n}", get(sitemap_posts))
        .route("/indexnow.txt", get(indexnow_keyfile))
}

async fn robots() -> Response {
    let cfg = config::get();
    let mut out = String::from("User-agent: *\n");
    // thin or infinite-space routes kept out of the index; browse/creator/post pages stay crawlable
    for path in ["/mod", "/search", "/random", "/api", "/index.php", "/autocomplete.php", "/contribute"] {
        out.push_str("Disallow: ");
        out.push_str(path);
        out.push('\n');
    }
    // Yandex-only: collapse the sort/filter query permutations onto one canonical target
    out.push_str("Clean-param: sort&dir&tier&source&tab /posts\n");
    out.push_str("Clean-param: sort&dir&tier&source&tab /creators\n");
    if let Some(base) = cfg.base_url() {
        out.push('\n');
        out.push_str(&format!("Sitemap: {base}/sitemap.xml\n"));
    }
    text(out, "text/plain; charset=utf-8")
}

// the index points at the creators sitemap plus one posts chunk per SITEMAP_CHUNK URLs
async fn sitemap_index(State(pool): State<PgPool>) -> Response {
    let Some(base) = config::get().base_url() else {
        return no_base();
    };
    let total = db::sitemap_post_count(&pool).await.unwrap_or(0);
    let chunks = ((total + SITEMAP_CHUNK - 1) / SITEMAP_CHUNK).max(1);
    let mut xml = String::from(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    xml.push_str("\n<sitemapindex xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n");
    xml.push_str(&format!("<sitemap><loc>{}/sitemap-creators.xml</loc></sitemap>\n", xml_escape(base)));
    for i in 0..chunks {
        xml.push_str(&format!("<sitemap><loc>{}/sitemap-posts/{i}</loc></sitemap>\n", xml_escape(base)));
    }
    xml.push_str("</sitemapindex>\n");
    text(xml, "application/xml; charset=utf-8")
}

async fn sitemap_creators(State(pool): State<PgPool>) -> Response {
    let Some(base) = config::get().base_url() else {
        return no_base();
    };
    let rows = db::sitemap_creators(&pool).await.unwrap_or_default();
    let urls = rows.iter().map(|r| {
        let loc = format!("{base}/c/{}/{}", r.platform, r.creator_id);
        url_entry(&loc, r.lastmod)
    });
    text(urlset(urls), "application/xml; charset=utf-8")
}

async fn sitemap_posts(State(pool): State<PgPool>, Path(n): Path<i64>) -> Response {
    let Some(base) = config::get().base_url() else {
        return no_base();
    };
    let offset = n.max(0) * SITEMAP_CHUNK;
    let rows = db::sitemap_posts(&pool, SITEMAP_CHUNK, offset).await.unwrap_or_default();
    let urls = rows.iter().map(|r| {
        let post = r.post_id.as_deref().unwrap_or_default();
        let loc = format!("{base}/p/{}/{}/{}", r.platform, r.creator_id, post);
        url_entry(&loc, r.lastmod)
    });
    text(urlset(urls), "application/xml; charset=utf-8")
}

fn urlset(entries: impl Iterator<Item = String>) -> String {
    let mut xml = String::from(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    xml.push_str("\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n");
    for e in entries {
        xml.push_str(&e);
    }
    xml.push_str("</urlset>\n");
    xml
}

fn url_entry(loc: &str, lastmod_epoch: i64) -> String {
    match chrono::DateTime::from_timestamp(lastmod_epoch, 0) {
        Some(dt) => format!("<url><loc>{}</loc><lastmod>{}</lastmod></url>\n", xml_escape(loc), dt.format("%Y-%m-%d")),
        None => format!("<url><loc>{}</loc></url>\n", xml_escape(loc)),
    }
}

async fn indexnow_keyfile() -> Response {
    match config::get().indexnow_key.as_deref() {
        Some(key) if !key.is_empty() => text(key.to_string(), "text/plain; charset=utf-8"),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

// fire-and-forget: tell IndexNow (Bing/Yandex, and via Bing, DuckDuckGo) about posts added since the last
// publish. Google ignores IndexNow, so it stays on the sitemap + Search Console path
pub async fn ping_indexnow(pool: PgPool, since: i64) {
    let cfg = config::get();
    let (Some(base), Some(key)) = (cfg.base_url(), cfg.indexnow_key.as_deref().filter(|k| !k.is_empty()))
    else {
        return;
    };
    let posts = match db::posts_since(&pool, since, INDEXNOW_MAX).await {
        Ok(p) if !p.is_empty() => p,
        Ok(_) => return,
        Err(e) => {
            tracing::warn!("indexnow: querying new posts: {e:#}");
            return;
        }
    };
    let host = base.split("://").nth(1).unwrap_or(base);
    let urls: Vec<String> =
        posts.iter().map(|(p, c, i)| format!("{base}/p/{p}/{c}/{i}")).collect();
    let body = serde_json::json!({
        "host": host,
        "key": key,
        "keyLocation": format!("{base}/indexnow.txt"),
        "urlList": urls,
    });
    let count = urls.len();
    let client = reqwest::Client::new();
    match client
        .post("https://api.indexnow.org/indexnow")
        .json(&body)
        .send()
        .await
    {
        Ok(resp) => tracing::info!("indexnow: submitted {count} urls, status {}", resp.status()),
        Err(e) => tracing::warn!("indexnow: submit failed: {e:#}"),
    }
}

fn text(body: String, content_type: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

// a public board sets public_url; without it there is no absolute origin to emit, so the sitemap is empty
fn no_base() -> Response {
    (StatusCode::NOT_FOUND, "set public_url to enable the sitemap").into_response()
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    // building the router parses every path template; a bad one (e.g. the sitemap chunk param) panics here
    // instead of at server start
    #[test]
    fn routes_build() {
        let _ = super::routes();
    }

    #[test]
    fn xml_escapes_ampersands() {
        assert_eq!(super::xml_escape("a&b<c>"), "a&amp;b&lt;c&gt;");
    }
}
