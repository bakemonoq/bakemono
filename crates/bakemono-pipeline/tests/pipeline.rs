use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nostr_relay_builder::MockRelay;
use nostr_sdk::prelude::*;

use bakemono_core::Manifest;
use bakemono_pipeline::{gather_pairs, manifest_from_files, publish_manifests};

const SIDECAR: &str = r#"{
  "id": 161883250,
  "num": 2,
  "category": "patreon",
  "title": "Lana's Special Delivery ",
  "published_at": "2026-06-23T17:46:49.000+00:00",
  "date": "2026-06-23 17:46:49",
  "is_paid": false,
  "tags": ["Lana", "nsfw"],
  "content": "<p>body</p>",
  "creator": {"id": 8360519, "full_name": "BONI", "vanity": "bonifasko"}
}"#;

#[test]
fn maps_real_sidecar_fields_into_manifest() {
    let dir = unique_dir("map");
    let (media, sidecar) = write_sample(&dir);

    let manifest = manifest_from_files(&media, &sidecar).unwrap();
    fs::remove_dir_all(&dir).ok();

    assert_eq!(manifest.platform, "patreon");
    assert_eq!(manifest.creator, "BONI");
    assert_eq!(manifest.creator_id, "8360519");
    assert_eq!(manifest.post_id, "161883250");
    assert_eq!(manifest.file_index, 1);
    assert_eq!(manifest.mime, "image/jpeg");
    assert_eq!(manifest.tier.as_deref(), Some("free"));
    assert_eq!(
        manifest.topics,
        vec!["Lana".to_string(), "nsfw".to_string()]
    );
    assert_eq!(
        manifest.post_title.as_deref(),
        Some("Lana's Special Delivery")
    );
    assert_eq!(manifest.d_tag(), "patreon:8360519:161883250:1");
    assert_eq!(manifest.file_hash.len(), 64);
}

#[tokio::test]
async fn ingested_manifest_round_trips_through_a_relay() {
    let dir = unique_dir("ingest");
    write_sample(&dir);

    let pairs = gather_pairs(&dir).unwrap();
    assert_eq!(pairs.len(), 1);
    let manifest = manifest_from_files(&pairs[0].0, &pairs[0].1).unwrap();

    let relay = MockRelay::run().await.unwrap();
    let url = relay.url().await.to_string();
    let keys = Keys::generate();
    let ids = publish_manifests(
        std::slice::from_ref(&url),
        &keys,
        std::slice::from_ref(&manifest),
    )
    .await
    .unwrap();
    assert_eq!(ids.len(), 1);

    let client = Client::new(Keys::generate());
    client.add_relay(&url).await.unwrap();
    client.connect().await;
    let events = client
        .fetch_events(
            Filter::new().kind(Kind::from(31063u16)),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    fs::remove_dir_all(&dir).ok();

    let got = events
        .into_iter()
        .find(|e| e.id == ids[0])
        .expect("event present on relay");
    assert!(got.verify().is_ok());
    assert_eq!(Manifest::from_event(&got).unwrap(), manifest);
}

fn write_sample(dir: &Path) -> (PathBuf, PathBuf) {
    fs::create_dir_all(dir).unwrap();
    let media = dir.join("161883250_post_02.jpg");
    fs::write(&media, [0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4]).unwrap();
    let sidecar = dir.join("161883250_post_02.jpg.json");
    fs::write(&sidecar, SIDECAR).unwrap();
    (media, sidecar)
}

fn unique_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bakemono-pipe-{tag}-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}
