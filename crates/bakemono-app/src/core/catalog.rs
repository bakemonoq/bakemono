use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use super::scrape::gather_pairs;

// the scrape directory is the source of truth; stats are derived from it, never cached on disk
#[derive(Debug, Clone, Serialize)]
pub struct CatalogStats {
    pub files: usize,
    pub posts: usize,
    pub creators: usize,
    pub total_bytes: u64,
}

pub fn stats(dir: &Path) -> CatalogStats {
    let pairs = gather_pairs(dir).unwrap_or_default();
    let mut posts = BTreeSet::new();
    let mut creators = BTreeSet::new();
    let mut total_bytes = 0;
    for (media, sidecar) in &pairs {
        total_bytes += fs::metadata(media).map(|m| m.len()).unwrap_or(0);
        if let Some((platform, creator_id, post_id)) = read_ids(sidecar) {
            creators.insert(creator_id.clone());
            posts.insert(format!("{platform}:{creator_id}:{post_id}"));
        }
    }
    CatalogStats {
        files: pairs.len(),
        posts: posts.len(),
        creators: creators.len(),
        total_bytes,
    }
}

// read only the small sidecar, never the media, so this stays cheap to call on demand
fn read_ids(sidecar: &Path) -> Option<(String, String, String)> {
    let raw = fs::read(sidecar).ok()?;
    let value: Value = serde_json::from_slice(&raw).ok()?;
    let platform = value
        .get("category")
        .and_then(Value::as_str)
        .unwrap_or("patreon")
        .to_string();
    let creator_id = value.get("creator").and_then(|c| c.get("id")).map(num_or_str)?;
    let post_id = value.get("id").map(num_or_str)?;
    Some((platform, creator_id, post_id))
}

fn num_or_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}
