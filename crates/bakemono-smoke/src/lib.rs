use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use sha2::{Digest, Sha256};

use bakemono_core::protocol::KIND_MANIFEST;
use bakemono_core::Manifest;

pub fn manifest_from_file(path: &Path) -> Result<Manifest> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let file_hash = hex::encode(Sha256::digest(&bytes));
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let magnet = placeholder_magnet(&file_hash, &filename);
    Ok(Manifest {
        platform: "local".into(),
        creator: "smoke".into(),
        creator_id: "0".into(),
        post_id: file_hash[..16].to_string(),
        file_index: 0,
        file_hash,
        size: bytes.len() as u64,
        mime: guess_mime(path).to_string(),
        magnet,
        filename: Some(filename),
        post_title: None,
        posted_at: None,
        tier: Some("unknown".into()),
        topics: Vec::new(),
        thumb: None,
        content: String::new(),
    })
}

pub async fn publish(relay_url: &str, keys: &Keys, manifest: &Manifest) -> Result<EventId> {
    let event = manifest.to_event(keys)?;
    let client = Client::new(keys.clone());
    client.add_relay(relay_url).await?;
    client.connect().await;
    client.send_event(&event).await?;
    client.disconnect().await;
    Ok(event.id)
}

pub async fn fetch_manifests(relay_url: &str, timeout: Duration) -> Result<Vec<Event>> {
    let client = Client::new(Keys::generate());
    client.add_relay(relay_url).await?;
    client.connect().await;
    let filter = Filter::new().kind(Kind::from(KIND_MANIFEST));
    let events = client.fetch_events(filter, timeout).await?;
    client.disconnect().await;
    Ok(events.into_iter().collect())
}

// placeholder btih derived from the content hash, replaced by the real v1 infohash once seeded
fn placeholder_magnet(file_hash: &str, filename: &str) -> String {
    format!("magnet:?xt=urn:btih:{}&dn={}", &file_hash[..40], filename)
}

fn guess_mime(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("txt") => "text/plain",
        _ => "application/octet-stream",
    }
}
