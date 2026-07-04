use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub relays: Vec<String>,
    #[serde(default = "default_trackers")]
    pub trackers: Vec<String>,
    pub seed: bool,
    pub max_up_mbit: u32,
    // 0 = unlimited; the download cap bounds background fetching
    #[serde(default)]
    pub max_down_mbit: u32,
    // default false: closing the desktop app leaves the daemon seeding in the background
    #[serde(default)]
    pub stop_daemon_on_exit: bool,
    // optional sidecar binary overrides the app hands the daemon; empty = bundled, then PATH default
    #[serde(default)]
    pub ffmpeg_bin: Option<String>,
    #[serde(default)]
    pub gallery_dl_bin: Option<String>,
    // which board's web UI the gui opens for upload status; boards are federated, so the user picks
    #[serde(default)]
    pub board_url: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            relays: default_relays(),
            trackers: default_trackers(),
            seed: true,
            max_up_mbit: 20,
            max_down_mbit: 0,
            stop_daemon_on_exit: false,
            ffmpeg_bin: None,
            gallery_dl_bin: None,
            board_url: None,
        }
    }
}

// our local relay first for the dev/demo loop, then the shared public set
fn default_relays() -> Vec<String> {
    std::iter::once("ws://127.0.0.1:8080".to_string())
        .chain(bakemono_core::default_relays())
        .collect()
}

fn default_trackers() -> Vec<String> {
    bakemono_core::default_trackers()
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
