use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

use bakemono_core::Manifest;

pub fn gather_pairs(dir: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut files = Vec::new();
    walk(dir, &mut files)?;
    let present: std::collections::HashSet<&PathBuf> = files.iter().collect();
    let mut pairs = Vec::new();
    for path in &files {
        if is_sidecar(path) || is_thumb(path) {
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

// signed events we saved next to each media file at publish time, so a new relay can be backfilled
// without re-hashing or re-mining proof-of-work
pub fn gather_event_sidecars(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    walk(dir, &mut files)?;
    let mut events: Vec<PathBuf> = files.into_iter().filter(|p| is_event_sidecar(p)).collect();
    events.sort();
    Ok(events)
}

pub fn manifest_from_files(media: &Path, sidecar: &Path) -> Result<Manifest> {
    let raw = fs::read(sidecar).with_context(|| format!("reading {}", sidecar.display()))?;
    let meta: Value =
        serde_json::from_slice(&raw).with_context(|| format!("parsing {}", sidecar.display()))?;

    let (file_hash, size, mime) = hash_media(media)?;
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
        size,
        mime,
        magnet: placeholder_magnet(&file_hash),
        bundle_index: 0,
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

// one streaming pass over the media: sha256 + byte count + a 12-byte header sniff, never buffering the whole file
fn hash_media(media: &Path) -> Result<(String, u64, String)> {
    let mut file = fs::File::open(media).with_context(|| format!("reading {}", media.display()))?;
    let mut hasher = Sha256::new();
    let mut header = Vec::with_capacity(12);
    let mut size: u64 = 0;
    let mut buf = vec![0u8; 128 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("reading {}", media.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        if header.len() < 12 {
            let take = (12 - header.len()).min(n);
            header.extend_from_slice(&buf[..take]);
        }
        size += n as u64;
    }
    Ok((hex::encode(hasher.finalize()), size, sniff_mime(&header).to_string()))
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

fn is_event_sidecar(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(".event.json"))
}

// skip legacy seeded previews and any leftover inline-thumb temp file so neither is ingested as media
fn is_thumb(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(".thumb.jpg") || n.ends_with(".thumbtmp.webp"))
}

fn sidecar_path(media: &Path) -> PathBuf {
    let mut name = media.to_path_buf().into_os_string();
    name.push(".json");
    PathBuf::from(name)
}

pub fn event_sidecar_path(media: &Path) -> PathBuf {
    let mut name = media.to_path_buf().into_os_string();
    name.push(".event.json");
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

// placeholder btih derived from the content hash, replaced by the real v1 infohash once seeded
fn placeholder_magnet(file_hash: &str) -> String {
    format!("magnet:?xt=urn:btih:{}", &file_hash[..40])
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIDECAR: &str = r#"{
      "id": 161883250,
      "num": 2,
      "category": "patreon",
      "title": "Lana's Special Delivery ",
      "published_at": "2026-06-23T17:46:49.000+00:00",
      "is_paid": false,
      "tags": ["Lana", "nsfw"],
      "content": "<p>body</p>",
      "creator": {"id": 8360519, "full_name": "BONI", "vanity": "bonifasko"}
    }"#;

    #[test]
    fn maps_real_sidecar_fields_into_manifest() {
        let dir = std::env::temp_dir().join(format!("bakemono-scrape-map-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let media = dir.join("161883250_post_02.jpg");
        fs::write(&media, [0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4]).unwrap();
        let sidecar = dir.join("161883250_post_02.jpg.json");
        fs::write(&sidecar, SIDECAR).unwrap();

        let manifest = manifest_from_files(&media, &sidecar).unwrap();
        fs::remove_dir_all(&dir).ok();

        assert_eq!(manifest.platform, "patreon");
        assert_eq!(manifest.creator, "BONI");
        assert_eq!(manifest.creator_id, "8360519");
        assert_eq!(manifest.post_id, "161883250");
        assert_eq!(manifest.file_index, 1);
        assert_eq!(manifest.mime, "image/jpeg");
        assert_eq!(manifest.tier.as_deref(), Some("free"));
        assert_eq!(manifest.d_tag(), "patreon:8360519:161883250:1");
        assert_eq!(manifest.file_hash.len(), 64);
    }
}
