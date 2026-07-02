use std::collections::HashSet;
use std::sync::OnceLock;

use ammonia::Builder;

// contributor post bodies are untrusted HTML, so render only an allowlist: formatting and links, never
// scripts, styles, event handlers, or media. no <img>/embeds on purpose - the files come through the
// gateway carousel, and a hotlinked image in the body would leak the viewer's IP to a third-party host
pub fn body(html: &str) -> String {
    cleaner().clean(html).to_string()
}

fn cleaner() -> &'static Builder<'static> {
    static CLEANER: OnceLock<Builder<'static>> = OnceLock::new();
    CLEANER.get_or_init(|| {
        const TAGS: &[&str] = &[
            "a", "p", "br", "hr", "ul", "ol", "li", "b", "strong", "i", "em", "u", "s", "blockquote",
            "code", "pre", "h1", "h2", "h3", "h4", "h5", "h6", "span", "div",
        ];
        let mut b = Builder::default();
        b.tags(TAGS.iter().copied().collect::<HashSet<_>>())
            .link_rel(Some("noopener noreferrer nofollow"));
        b
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_scripts_and_event_handlers() {
        let out = body(r#"<p onclick="steal()">hi</p><script>alert(1)</script>"#);
        assert_eq!(out, "<p>hi</p>");
    }

    #[test]
    fn drops_javascript_urls_but_keeps_safe_links() {
        assert!(!body(r#"<a href="javascript:alert(1)">x</a>"#).contains("javascript"));
        let safe = body(r#"<a href="https://example.com">x</a>"#);
        assert!(safe.contains("href=\"https://example.com\""));
        assert!(safe.contains("rel=\"noopener noreferrer nofollow\""));
    }

    #[test]
    fn removes_images_and_iframes() {
        let out = body(r#"<img src="https://tracker.example/pixel.gif"><iframe src="x"></iframe><p>text</p>"#);
        assert!(!out.contains("img"));
        assert!(!out.contains("iframe"));
        assert!(out.contains("<p>text</p>"));
    }

    #[test]
    fn keeps_basic_formatting() {
        let out = body("<p>a <strong>bold</strong> and <em>italic</em></p>");
        assert_eq!(out, "<p>a <strong>bold</strong> and <em>italic</em></p>");
    }
}
