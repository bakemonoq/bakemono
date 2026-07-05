// every platform is now just a gallery-dl feed entry point plus the session cookie it needs.
// gallery-dl enumerates the whole subscription feed from the cookie and downloads it; we no longer
// hand-write per-platform discovery. only platforms with a "your subscriptions" feed extractor in
// gallery-dl can work this way (Patreon, Fanbox, Boosty). others expose only per-creator scraping.
pub struct Platform {
    pub id: &'static str,
    pub label: &'static str,
    pub cookie_name: &'static str,
    pub cookie_domain: &'static str,
    pub feed_url: &'static str,
    pub live: bool,
}

pub const PLATFORMS: &[Platform] = &[
    Platform {
        id: "patreon",
        label: "Patreon",
        cookie_name: "session_id",
        cookie_domain: "patreon.com",
        feed_url: "https://www.patreon.com/home",
        live: true,
    },
    // api.fanbox.cc is behind Cloudflare; the curl_cffi fork (firefox impersonation) clears it from a
    // residential IP, so Fanbox needs BAKEMONO_FANBOX_PROXY set (runtime_ready gates it)
    Platform {
        id: "fanbox",
        label: "Fanbox",
        cookie_name: "FANBOXSESSID",
        cookie_domain: "fanbox.cc",
        feed_url: "https://fanbox.cc/home/supporting",
        live: true,
    },
    // untested against real credentials and likely Cloudflare-gated like Fanbox; enable once verified
    Platform {
        id: "boosty",
        label: "Boosty",
        cookie_name: "auth",
        cookie_domain: "boosty.to",
        feed_url: "https://boosty.to/",
        live: false,
    },
];

fn find(id: &str) -> Option<&'static Platform> {
    PLATFORMS.iter().find(|p| p.id == id)
}

pub fn is_live(id: &str) -> bool {
    find(id).is_some_and(|p| p.live && runtime_ready(p.id))
}

// a platform can be code-ready (live) but need runtime config before it actually works. Fanbox is
// behind Cloudflare, so it only works once a scrape proxy is configured
fn runtime_ready(id: &str) -> bool {
    match id {
        "fanbox" => std::env::var("BAKEMONO_SCRAPE_PROXY").is_ok_and(|v| !v.trim().is_empty()),
        _ => true,
    }
}

pub fn live_platforms() -> impl Iterator<Item = &'static Platform> {
    PLATFORMS.iter().filter(|p| p.live && runtime_ready(p.id))
}

pub fn label(id: &str) -> &str {
    find(id).map(|p| p.label).unwrap_or(id)
}

pub fn feed_url(id: &str) -> Option<&'static str> {
    find(id).map(|p| p.feed_url)
}

// a Netscape cookies.txt gallery-dl can read, holding just the session cookie
pub fn netscape_cookie(id: &str, token: &str) -> Option<String> {
    let p = find(id)?;
    Some(format!(
        "# Netscape HTTP Cookie File\n.{}\tTRUE\t/\tTRUE\t9999999999\t{}\t{}\n",
        p.cookie_domain, p.cookie_name, token
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netscape_has_the_right_cookie_name() {
        let c = netscape_cookie("patreon", "TOK").unwrap();
        assert!(c.contains("session_id\tTOK"));
        assert!(c.contains(".patreon.com"));
        assert!(netscape_cookie("nope", "x").is_none());
    }

    #[test]
    fn every_live_platform_has_a_feed() {
        for p in live_platforms() {
            assert!(p.feed_url.starts_with("https://"), "{}", p.id);
        }
    }
}
