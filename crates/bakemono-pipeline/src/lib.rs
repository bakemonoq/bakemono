use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;
use serde_json::Value;
use sha2::{Digest, Sha256};

use bakemono_core::Manifest;

pub fn gather_pairs(dir: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut files = Vec::new();
    walk(dir, &mut files)?;
    let present: std::collections::HashSet<&PathBuf> = files.iter().collect();
    let mut pairs = Vec::new();
    for path in &files {
        if is_sidecar(path) {
            continue;
        }
        let sidecar = sidecar_path(path);
        if present.contains(&sidecar) {
            pairs.push((path.clone(), sidecar));
        }
    }
    pairs.sort();
    Ok(pairs)
}

pub fn manifest_from_files(media: &Path, sidecar: &Path) -> Result<Manifest> {
    let bytes = fs::read(media).with_context(|| format!("reading {}", media.display()))?;
    let raw = fs::read(sidecar).with_context(|| format!("reading {}", sidecar.display()))?;
    let meta: Value =
        serde_json::from_slice(&raw).with_context(|| format!("parsing {}", sidecar.display()))?;

    let file_hash = hex::encode(Sha256::digest(&bytes));
    Ok(Manifest {
        platform: string_at(&meta, &["category"]).unwrap_or_else(|| "patreon".to_string()),
        creator: string_at(&meta, &["creator", "full_name"])
            .or_else(|| string_at(&meta, &["creator", "vanity"]))
            .unwrap_or_else(|| "unknown".to_string()),
        creator_id: string_at(&meta, &["creator", "id"]).context("sidecar missing creator.id")?,
        post_id: string_at(&meta, &["id"]).context("sidecar missing id")?,
        file_index: meta
            .get("num")
            .and_then(Value::as_u64)
            .map(|n| n.saturating_sub(1) as u32)
            .unwrap_or(0),
        size: bytes.len() as u64,
        mime: sniff_mime(&bytes).to_string(),
        magnet: placeholder_magnet(&file_hash),
        file_hash,
        filename: media.file_name().map(|n| n.to_string_lossy().into_owned()),
        post_title: string_at(&meta, &["title"]).map(|t| t.trim().to_string()),
        posted_at: string_at(&meta, &["published_at"]).or_else(|| string_at(&meta, &["date"])),
        tier: Some(tier_of(&meta)),
        topics: topics_of(&meta),
        thumb: None,
        content: string_at(&meta, &["content"]).unwrap_or_default(),
    })
}

pub async fn publish_manifests(
    relays: &[String],
    keys: &Keys,
    manifests: &[Manifest],
) -> Result<Vec<EventId>> {
    let client = Client::new(keys.clone());
    for relay in relays {
        client.add_relay(relay).await?;
    }
    client.connect().await;

    let mut ids = Vec::with_capacity(manifests.len());
    for manifest in manifests {
        let event = manifest.to_event(keys)?;
        client.send_event(&event).await?;
        ids.push(event.id);
    }
    client.disconnect().await;
    Ok(ids)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_dir() {
            walk(&entry.path(), out)?;
        } else if meta.is_file() {
            out.push(entry.path());
        }
    }
    Ok(())
}

fn is_sidecar(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("json")
}

fn sidecar_path(media: &Path) -> PathBuf {
    let mut name = media.to_path_buf().into_os_string();
    name.push(".json");
    PathBuf::from(name)
}

fn string_at(value: &Value, path: &[&str]) -> Option<String> {
    let mut cursor = value;
    for key in path {
        cursor = cursor.get(key)?;
    }
    match cursor {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn tier_of(meta: &Value) -> String {
    match meta.get("is_paid").and_then(Value::as_bool) {
        Some(true) => "paid",
        Some(false) => "free",
        None => "unknown",
    }
    .to_string()
}

fn topics_of(meta: &Value) -> Vec<String> {
    meta.get("tags")
        .and_then(Value::as_array)
        .map(|tags| {
            tags.iter()
                .filter_map(|t| t.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn sniff_mime(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        "image/png"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        "video/mp4"
    } else {
        "application/octet-stream"
    }
}

// btmh placeholder over the file's own sha256, the real BT v2 infohash lands with the seeder
fn placeholder_magnet(file_hash: &str) -> String {
    format!("magnet:?xt=urn:btmh:1220{file_hash}")
}
