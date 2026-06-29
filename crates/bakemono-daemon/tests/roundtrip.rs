#![cfg(feature = "harness")]

use std::fs;
use std::time::Duration;

use nostr_relay_builder::MockRelay;
use nostr_sdk::prelude::*;
use tokio_util::sync::CancellationToken;

use bakemono_engine::identity::Identity;
use bakemono_daemon::pipeline::{run_ingest, JobContext, Progress};
use bakemono_core::Manifest;

const SIDECAR: &str = r#"{
  "id": 161883250,
  "num": 2,
  "category": "patreon",
  "title": "Lana's Special Delivery ",
  "is_paid": false,
  "creator": {"id": 8360519, "full_name": "BONI", "vanity": "bonifasko"}
}"#;

#[tokio::test]
async fn ingested_manifest_round_trips_through_a_relay() {
    let dir = std::env::temp_dir().join(format!("bakemono-app-rt-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let media = dir.join("161883250_post_02.jpg");
    fs::write(&media, [0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4]).unwrap();
    fs::write(dir.join("161883250_post_02.jpg.json"), SIDECAR).unwrap();

    let relay = MockRelay::run().await.unwrap();
    let url = relay.url().await.to_string();
    let identity = Identity::generate();
    let cancel = CancellationToken::new();
    let relays = vec![url.clone()];
    let noop = |_p: Progress| {};
    let ctx = JobContext {
        relays: &relays,
        identity: &identity,
        seeder: None,
        cancel: &cancel,
        progress: &noop,
    };

    let summary = run_ingest(&dir, &ctx).await.unwrap();
    assert_eq!(summary.event_ids.len(), 1);

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
        .find(|e| e.id.to_hex() == summary.event_ids[0])
        .expect("event present on relay");
    assert!(got.verify().is_ok());
    assert!(Manifest::from_event(&got).is_ok());
}
