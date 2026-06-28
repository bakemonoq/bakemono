pub mod catalog;
pub mod config;
pub mod identity;
pub mod pipeline;
pub mod scrape;
pub mod seeder;

use std::path::PathBuf;

pub fn data_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("bakemono")
}
