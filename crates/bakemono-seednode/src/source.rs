use std::future::Future;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use bakemono_engine::content::{ContentSource, ProgressFn};
use bakemono_engine::seeder::SeederHandle;

// the farm's half: fetch popular/endangered content by policy from a board demand feed.
// the fetch itself is not implemented yet (it needs the board feeds), so for now this
// only re-seeds whatever is already in the cache and reports its size
pub struct FarmContentSource;

impl ContentSource for FarmContentSource {
    fn run(
        &self,
        _job: Value,
        _seeder: Option<&SeederHandle>,
        _cancel: &CancellationToken,
        _progress: ProgressFn<'_>,
    ) -> impl Future<Output = Result<Value>> + Send {
        // the seednode fetches autonomously; it does not take run jobs over IPC
        async { anyhow::bail!("seednode fetches autonomously and does not accept run jobs") }
    }

    // farm content is raw fetched files (no gallery-dl sidecars), so seed every file
    fn seedable(&self, content_dir: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        walk(content_dir, &mut files);
        files
    }

    fn stats(&self, content_dir: &Path) -> Value {
        let mut files = Vec::new();
        walk(content_dir, &mut files);
        let total_bytes: u64 = files
            .iter()
            .map(|f| std::fs::metadata(f).map(|m| m.len()).unwrap_or(0))
            .sum();
        json!({"files": files.len(), "total_bytes": total_bytes})
    }
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}
