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

// only Cloudflare-gated sources need the scrape proxy; everyone else scrapes direct (full bandwidth, no
// proxy traffic). Fanbox is behind Cloudflare, so it needs the firefox-impersonating proxy
pub fn needs_proxy(id: &str) -> bool {
    matches!(id, "fanbox")
}

// a platform can be code-ready (live) but need runtime config before it actually works: a proxy-gated
// source only works once a scrape proxy is configured
fn runtime_ready(id: &str) -> bool {
    if needs_proxy(id) {
        std::env::var("BAKEMONO_SCRAPE_PROXY").is_ok_and(|v| !v.trim().is_empty())
    } else {
        true
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

// a real cookie value tops out around 4 KiB; a full cookies.txt export a few times that. anything
// past this is a pasted web page or worse, not a cookie - reject before we bother parsing it
pub const MAX_COOKIE_PASTE: usize = 32 * 1024;

// contributors paste all sorts of things into the cookie box: the bare value, `name=value`, a copied
// Cookie header, or a whole cookies.txt export from an extension. pull out just this platform's cookie
// value; give up (None) rather than hand gallery-dl something that clearly is not the cookie
pub fn extract_token(id: &str, raw: &str) -> Option<String> {
    let cookie_name = find(id)?.cookie_name;
    token_from_paste(cookie_name, raw)
}

fn token_from_paste(cookie_name: &str, raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > MAX_COOKIE_PASTE {
        return None;
    }
    let mut bare: Vec<&str> = Vec::new();
    let mut saw_named = false;
    for line in raw.lines() {
        let line = line.trim();
        // keep `#HttpOnly_` cookie rows (they carry tabs); drop real comment/header lines
        if line.is_empty() || (line.starts_with('#') && !line.contains('\t')) {
            continue;
        }
        if line.contains('\t') {
            if let Some(v) = tab_line_value(line, cookie_name) {
                return Some(strip_quotes(v).to_string());
            }
            continue;
        }
        for seg in line.split(';') {
            let seg = seg.trim();
            if seg.is_empty() {
                continue;
            }
            match seg_as_pair(seg) {
                Some((name, value)) => {
                    saw_named = true;
                    if name.eq_ignore_ascii_case(cookie_name) {
                        let value = strip_quotes(value);
                        if !value.is_empty() {
                            return Some(value.to_string());
                        }
                    }
                }
                None => bare.push(seg),
            }
        }
    }
    // no named match: accept only a single bare value, never a guess among several or a stray sentence
    if !saw_named && bare.len() == 1 {
        let value = strip_quotes(bare[0]);
        if !value.is_empty() && !value.contains(char::is_whitespace) {
            return Some(value.to_string());
        }
    }
    None
}

// Netscape and devtools-row exports both put the value in the field right after the name field
fn tab_line_value<'a>(line: &'a str, cookie_name: &str) -> Option<&'a str> {
    let fields: Vec<&str> = line.split('\t').map(str::trim).collect();
    let i = fields.iter().position(|f| f.eq_ignore_ascii_case(cookie_name))?;
    fields.get(i + 1).copied().filter(|v| !v.is_empty())
}

fn seg_as_pair(seg: &str) -> Option<(&str, &str)> {
    let (name, value) = seg.split_once('=')?;
    let (name, value) = (name.trim(), value.trim());
    // a real name=value pair: a token-ish name and a value that is more than base64 '=' padding
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    if value.is_empty() || value.bytes().all(|b| b == b'=') {
        return None;
    }
    Some((name, value))
}

fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
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

    #[test]
    fn takes_a_bare_value_as_is() {
        assert_eq!(token_from_paste("session_id", "  abc123 ").as_deref(), Some("abc123"));
    }

    #[test]
    fn strips_a_name_prefix() {
        assert_eq!(token_from_paste("session_id", "session_id=abc123").as_deref(), Some("abc123"));
    }

    #[test]
    fn finds_the_cookie_in_a_copied_header() {
        let h = "__cf_bm=zzz; session_id=abc123; g_recent_activity=1";
        assert_eq!(token_from_paste("session_id", h).as_deref(), Some("abc123"));
    }

    #[test]
    fn reads_a_netscape_export_including_httponly_rows() {
        let f = "# Netscape HTTP Cookie File\n\
                 #HttpOnly_.patreon.com\tTRUE\t/\tTRUE\t9999999999\tsession_id\tabc123\n\
                 .patreon.com\tTRUE\t/\tFALSE\t0\tg_recent_activity\t1\n";
        assert_eq!(token_from_paste("session_id", f).as_deref(), Some("abc123"));
    }

    #[test]
    fn keeps_base64_padding_on_a_bare_value() {
        assert_eq!(token_from_paste("session_id", "YWJjZGVm==").as_deref(), Some("YWJjZGVm=="));
    }

    #[test]
    fn keeps_internal_equals_in_a_value() {
        assert_eq!(token_from_paste("session_id", "session_id=aa=bb").as_deref(), Some("aa=bb"));
    }

    #[test]
    fn strips_surrounding_quotes() {
        assert_eq!(token_from_paste("session_id", "\"abc123\"").as_deref(), Some("abc123"));
    }

    #[test]
    fn rejects_an_export_for_the_wrong_platform() {
        let f = "#HttpOnly_.pixiv.net\tTRUE\t/\tTRUE\t9999\tPHPSESSID\txyz\n\
                 .pixiv.net\tTRUE\t/\tFALSE\t0\tdevice_token\tqqq\n";
        assert_eq!(token_from_paste("session_id", f), None);
    }

    #[test]
    fn rejects_a_single_pair_for_a_different_cookie() {
        assert_eq!(token_from_paste("session_id", "FANBOXSESSID=zzz"), None);
    }

    #[test]
    fn rejects_a_pasted_sentence() {
        assert_eq!(token_from_paste("session_id", "please help me import my stuff"), None);
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(token_from_paste("session_id", "   \n  "), None);
    }

    #[test]
    fn extract_token_resolves_the_platform_cookie_name() {
        assert_eq!(extract_token("patreon", "session_id=abc").as_deref(), Some("abc"));
        assert_eq!(extract_token("fanbox", "FANBOXSESSID=xyz").as_deref(), Some("xyz"));
        assert_eq!(extract_token("nope", "whatever"), None);
    }
}
