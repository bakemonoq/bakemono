use std::sync::OnceLock;

use serde::Deserialize;

static CONFIG: OnceLock<BoardConfig> = OnceLock::new();

pub fn get() -> &'static BoardConfig {
    CONFIG.get_or_init(BoardConfig::load)
}

// operator-facing board.toml; every field is optional so a board with no file still boots with defaults.
// welcome_html is rendered raw on the home page - it is operator-authored, same trust level as the binary
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct BoardConfig {
    pub name: String,
    pub tagline: Option<String>,
    pub mascot: Option<String>,
    pub welcome_html: Option<String>,
    pub about_html: Option<String>,
    pub accent: Option<String>,
    pub dmca_contact: Option<String>,
    pub contact: Option<String>,
    pub static_dir: Option<String>,
    // absolute base for URLs handed to external clients (booru API, canonical/OG tags, sitemap); unset =
    // derive from the request. crawlers need an absolute canonical, so set this on a public board
    pub public_url: Option<String>,
    // board-wide content rating reported by the booru API: general/sensitive/questionable/explicit
    pub rating: Option<String>,
    // raw markup injected verbatim into every <head>, for search-console verification meta tags and the
    // like; operator-authored, same trust level as the binary
    pub head_html: Option<String>,
    // shared secret for IndexNow submissions (Bing/Yandex/DuckDuckGo). any 8-128 hex-ish string; the board
    // serves it at /indexnow.txt so the engines can verify ownership. unset = no IndexNow pings
    pub indexnow_key: Option<String>,
    pub community: Vec<CommunityLink>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommunityLink {
    pub label: String,
    pub url: String,
}

impl BoardConfig {
    fn load() -> Self {
        let mut cfg = read_file().unwrap_or_default();
        // board.toml wins; the env vars stay as a fallback so a deploy with no board.toml still works
        if cfg.name.is_empty() {
            cfg.name = env_opt("BAKEMONO_BOARD_NAME").unwrap_or_else(|| "化け物 bakemono".to_string());
        }
        cfg.dmca_contact = cfg.dmca_contact.or_else(|| env_opt("BAKEMONO_DMCA_CONTACT"));
        cfg.contact = cfg.contact.or_else(|| env_opt("BAKEMONO_CONTACT"));
        cfg.public_url = cfg
            .public_url
            .or_else(|| env_opt("BAKEMONO_PUBLIC_URL"))
            .map(|u| u.trim_end_matches('/').to_string());
        cfg.indexnow_key = cfg.indexnow_key.or_else(|| env_opt("BAKEMONO_INDEXNOW_KEY"));
        cfg
    }

    // the public origin with no trailing slash, or None when unset. absolute URLs for canonical/OG/sitemap
    // are only emitted when this is known - a guessed host would poison the canonical
    pub fn base_url(&self) -> Option<&str> {
        self.public_url.as_deref().filter(|u| !u.is_empty())
    }
}

fn read_file() -> Option<BoardConfig> {
    let path = env_opt("BAKEMONO_CONFIG").unwrap_or_else(|| "board.toml".to_string());
    let raw = std::fs::read_to_string(&path).ok()?;
    match toml::from_str(&raw) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            tracing::warn!("ignoring {path}: {e}");
            None
        }
    }
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
