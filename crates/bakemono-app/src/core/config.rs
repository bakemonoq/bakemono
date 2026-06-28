use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// our local relay first for the dev/demo loop, then the public set baked in per MVP
pub const DEFAULT_RELAYS: &[&str] = &[
    "ws://127.0.0.1:8080",
    "wss://relay.damus.io",
    "wss://nos.lol",
    "wss://relay.snort.social",
    "wss://nostr.wine",
];

pub const DEFAULT_TRACKERS: &[&str] = &[
    "wss://tracker.openwebtorrent.com",
    "wss://tracker.webtorrent.dev",
    "udp://tracker.opentrackr.org:1337/announce",
];

pub const DEFAULT_STUN: &[&str] = &["stun:stun.l.google.com:19302"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub relays: Vec<String>,
    #[serde(default = "default_trackers")]
    pub trackers: Vec<String>,
    #[serde(default = "default_stun")]
    pub stun: Vec<String>,
    pub seed: bool,
    pub max_up_mbit: u32,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            relays: DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect(),
            trackers: default_trackers(),
            stun: default_stun(),
            seed: true,
            max_up_mbit: 20,
        }
    }
}

fn default_trackers() -> Vec<String> {
    DEFAULT_TRACKERS.iter().map(|s| s.to_string()).collect()
}

fn default_stun() -> Vec<String> {
    DEFAULT_STUN.iter().map(|s| s.to_string()).collect()
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let path = config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

pub fn config_path() -> PathBuf {
    super::data_dir().join("config.json")
}
