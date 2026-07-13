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
    // absolute base for URLs handed to external clients (booru API); unset = derive from the request
    pub public_url: Option<String>,
    // board-wide content rating reported by the booru API: general/sensitive/questionable/explicit
    pub rating: Option<String>,
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
        cfg.public_url = cfg.public_url.or_else(|| env_opt("BAKEMONO_PUBLIC_URL"));
        cfg
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
