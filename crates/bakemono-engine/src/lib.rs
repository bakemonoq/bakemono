pub mod config;
pub mod identity;
pub mod content;
pub mod daemon;
pub mod ipc;
pub mod logging;
pub mod seeder;

use std::path::PathBuf;

// BAKEMONO_DATA_DIR lets a seednode or test point everything (config, staging, content) elsewhere
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("BAKEMONO_DATA_DIR") {
        return PathBuf::from(dir);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("bakemono")
}
