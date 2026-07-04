use std::future::Future;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::seeder::SeederHandle;

// progress events are opaque to the daemon - it just forwards them to whoever is listening
pub type ProgressFn<'a> = &'a (dyn Fn(Value) + Send + Sync);

// the head-specific half: where seeded content comes from. app = scrape -> sign -> publish;
// farm = fetch-by-policy from a demand feed. the daemon owns everything else.
pub trait ContentSource: Send + Sync + 'static {
    // run one job (job + summary are opaque to the daemon); seed via `seeder` when present
    fn run(
        &self,
        job: Value,
        seeder: Option<&SeederHandle>,
        cancel: &CancellationToken,
        progress: ProgressFn<'_>,
    ) -> impl Future<Output = Result<Value>> + Send;

    // files to (re)seed on startup, i.e. the current content set on disk
    fn seedable(&self, content_dir: &Path) -> Vec<PathBuf>;

    // the same content grouped by post, so a restart rebuilds one bundle torrent per post instead of one
    // per file; the default keeps each file in its own bundle
    fn seedable_bundles(&self, content_dir: &Path) -> Vec<Vec<PathBuf>> {
        self.seedable(content_dir)
            .into_iter()
            .map(|f| vec![f])
            .collect()
    }

    // the magnet this file was published under, if the head knows it; reseed compares it against the
    // deterministic infohash to keep swarms behind older published magnets alive
    fn published_magnet(&self, media: &Path) -> Option<String> {
        let _ = media;
        None
    }

    // opaque stats for the `stats` ipc command (files/bytes/...); default = none
    fn stats(&self, content_dir: &Path) -> Value {
        let _ = content_dir;
        Value::Null
    }
}
