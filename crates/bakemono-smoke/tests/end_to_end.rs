use std::time::Duration;

use nostr_relay_builder::MockRelay;
use nostr_sdk::prelude::*;

use bakemono_core::Manifest;

#[tokio::test]
async fn manifest_round_trips_through_a_real_relay() {
    let relay = MockRelay::run().await.expect("start mock relay");
    let url = relay.url().await.to_string();

    let keys = Keys::generate();
    let manifest = sample_manifest();
    let id = bakemono_smoke::publish(&url, &keys, &manifest)
        .await
        .expect("publish");

    let events = bakemono_smoke::fetch_manifests(&url, Duration::from_secs(5))
        .await
        .expect("fetch");

    let received = events
        .iter()
        .find(|e| e.id == id)
        .expect("published event present on relay");
    assert!(received.verify().is_ok());
    assert_eq!(Manifest::from_event(received).unwrap(), manifest);
}

#[tokio::test]
async fn manifest_from_file_hashes_contents() {
    let path = std::env::temp_dir().join(format!("bakemono-smoke-{}.txt", std::process::id()));
    std::fs::write(&path, b"hello bakemono").unwrap();

    let manifest = bakemono_smoke::manifest_from_file(&path).unwrap();
    std::fs::remove_file(&path).ok();

    assert_eq!(manifest.size, 14);
    assert_eq!(manifest.mime, "text/plain");
    assert_eq!(manifest.file_hash.len(), 64);
}

fn sample_manifest() -> Manifest {
    Manifest {
        platform: "patreon".into(),
        creator: "BoxOfMittens".into(),
        creator_id: "12345".into(),
        post_id: "67890".into(),
        file_index: 0,
        file_hash: "a3f8d2e1".repeat(8),
        size: 245_760,
        mime: "image/jpeg".into(),
        magnet: "magnet:?xt=urn:btmh:1220abcd&dn=x.jpg".into(),
        filename: Some("x.jpg".into()),
        post_title: Some("March art dump".into()),
        posted_at: Some("2026-03-14T10:00:00Z".into()),
        tier: Some("paid".into()),
        topics: vec!["art".into()],
        thumb: None,
        content: "body".into(),
    }
}
