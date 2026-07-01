use std::collections::HashMap;

use bakemono_core::nostr::{Event, EventBuilder, Keys, Kind, Tag};
use bakemono_core::{replaceable_address, Error, Manifest, ReplaceableAddress, Takedown, Target};

#[test]
fn manifest_round_trips_through_a_signed_event() {
    let keys = Keys::generate();
    let manifest = sample_manifest();
    let event = manifest.to_event(&keys).unwrap();

    assert!(event.verify().is_ok());
    assert_eq!(event.kind.as_u16(), 31063);
    assert_eq!(Manifest::from_event(&event).unwrap(), manifest);
}

#[test]
fn pow_manifest_meets_difficulty_and_round_trips() {
    let keys = Keys::generate();
    let manifest = sample_manifest();
    let event = manifest.to_event_pow(&keys, 8).unwrap();

    assert!(event.verify().is_ok());
    assert!(event.id.check_pow(8));
    assert_eq!(Manifest::from_event(&event).unwrap(), manifest);
}

#[test]
fn from_event_rejects_a_missing_required_tag() {
    let keys = Keys::generate();
    let event = EventBuilder::new(Kind::from(31063u16), "")
        .tags(manifest_tags_without_magnet())
        .sign_with_keys(&keys)
        .unwrap();

    let err = Manifest::from_event(&event).unwrap_err();
    assert!(matches!(err, Error::MissingTag("magnet")));
}

#[test]
fn from_event_rejects_an_oversized_content_body() {
    let keys = Keys::generate();
    let mut manifest = sample_manifest();
    manifest.content = "x".repeat(70_000);
    let event = manifest.to_event(&keys).unwrap();

    assert!(matches!(
        Manifest::from_event(&event).unwrap_err(),
        Error::TooLarge { field: "content" }
    ));
}

#[test]
fn from_event_rejects_a_non_hex_file_hash() {
    let keys = Keys::generate();
    let mut manifest = sample_manifest();
    manifest.file_hash = "not-a-real-hash".into();
    let event = manifest.to_event(&keys).unwrap();

    assert!(matches!(
        Manifest::from_event(&event).unwrap_err(),
        Error::MalformedTag { tag: "x", .. }
    ));
}

#[test]
fn from_event_rejects_a_non_magnet_uri() {
    let keys = Keys::generate();
    let mut manifest = sample_manifest();
    manifest.magnet = "https://example.com/evil".into();
    let event = manifest.to_event(&keys).unwrap();

    assert!(matches!(
        Manifest::from_event(&event).unwrap_err(),
        Error::MalformedTag { tag: "magnet", .. }
    ));
}

#[test]
fn from_event_rejects_a_topic_flood() {
    let keys = Keys::generate();
    let mut manifest = sample_manifest();
    manifest.topics = vec!["spam".into(); 64];
    let event = manifest.to_event(&keys).unwrap();

    assert!(matches!(
        Manifest::from_event(&event).unwrap_err(),
        Error::TooLarge { .. }
    ));
}

#[test]
fn from_event_rejects_a_takedown_with_a_non_hex_target() {
    let keys = Keys::generate();
    let takedown = Takedown {
        target: Target::FileHash("abc".into()),
        reason: "csam".into(),
        applied_at: None,
        explanation: String::new(),
    };
    let event = takedown.to_event(&keys).unwrap();

    assert!(matches!(
        Takedown::from_event(&event).unwrap_err(),
        Error::MalformedTag { tag: "x", .. }
    ));
}

#[test]
fn newer_event_replaces_older_for_same_pubkey_kind_d() {
    let keys = Keys::generate();
    let manifest = sample_manifest();
    let older = manifest.to_event_at(&keys, 1_000).unwrap();
    let newer = manifest.to_event_at(&keys, 2_000).unwrap();

    let mut store: HashMap<ReplaceableAddress, Event> = HashMap::new();
    upsert(&mut store, older);
    upsert(&mut store, newer.clone());

    assert_eq!(store.len(), 1);
    assert_eq!(store.values().next().unwrap().id, newer.id);

    let other = Keys::generate();
    upsert(&mut store, manifest.to_event_at(&other, 1_500).unwrap());
    assert_eq!(store.len(), 2);
}

#[test]
fn tampered_event_fails_verification() {
    let keys = Keys::generate();
    let event = sample_manifest().to_event(&keys).unwrap();

    let mut json = serde_json::to_value(&event).unwrap();
    json["content"] = serde_json::Value::String("tampered".into());
    let forged: Event = serde_json::from_value(json).unwrap();

    assert!(forged.verify().is_err());
    assert!(bakemono_core::verify(&forged).is_err());
}

#[test]
fn takedown_round_trips_through_a_signed_event() {
    let keys = Keys::generate();
    let takedown = Takedown {
        target: Target::FileHash("a3f8d2e1".repeat(8)),
        reason: "dmca-us".into(),
        applied_at: Some("2026-06-27T20:00:00Z".into()),
        explanation: "rights holder request".into(),
    };
    let event = takedown.to_event(&keys).unwrap();

    assert!(event.verify().is_ok());
    assert_eq!(event.kind.as_u16(), 31064);
    assert_eq!(Takedown::from_event(&event).unwrap(), takedown);
}

#[test]
fn takedown_d_tag_is_replaceable_per_target() {
    let by_hash = Takedown {
        target: Target::FileHash("abc".into()),
        reason: "csam".into(),
        applied_at: None,
        explanation: String::new(),
    };
    let by_pubkey = Takedown {
        target: Target::Pubkey("def".into()),
        ..by_hash.clone()
    };
    assert_eq!(by_hash.d_tag(), "takedown:x:abc");
    assert_eq!(by_pubkey.d_tag(), "takedown:p:def");
    assert_ne!(by_hash.d_tag(), by_pubkey.d_tag());
}

#[test]
fn post_and_creator_takedowns_round_trip() {
    let keys = Keys::generate();
    for target in [Target::post("patreon", "c1", "p1"), Target::creator("patreon", "c1")] {
        let takedown = Takedown {
            target: target.clone(),
            reason: "csam".into(),
            applied_at: None,
            explanation: String::new(),
        };
        let event = takedown.to_event(&keys).unwrap();
        assert!(event.verify().is_ok());
        assert_eq!(Takedown::from_event(&event).unwrap().target, target);
    }
}

#[test]
fn post_and_creator_targets_build_composite_d_tags() {
    assert_eq!(Target::post("patreon", "c1", "p1"), Target::Post("patreon:c1:p1".into()));
    assert_eq!(Target::creator("patreon", "c1"), Target::Creator("patreon:c1".into()));
    let t = Takedown {
        target: Target::post("patreon", "c1", "p1"),
        reason: "spam".into(),
        applied_at: None,
        explanation: String::new(),
    };
    assert_eq!(t.d_tag(), "takedown:post:patreon:c1:p1");
    assert_eq!(Target::from_parts("creator", "patreon:c1".into()), Some(Target::Creator("patreon:c1".into())));
}

#[test]
fn target_from_parts_rejects_unknown_kind() {
    assert_eq!(
        Target::from_parts("x", "hash".into()),
        Some(Target::FileHash("hash".into()))
    );
    assert_eq!(Target::from_parts("zzz", "v".into()), None);
}

fn upsert(store: &mut HashMap<ReplaceableAddress, Event>, event: Event) {
    let addr = replaceable_address(&event).unwrap();
    let keep = store
        .get(&addr)
        .is_none_or(|existing| event.created_at >= existing.created_at);
    if keep {
        store.insert(addr, event);
    }
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
        magnet: "magnet:?xt=urn:btmh:1220abcd&dn=post123_image.jpg".into(),
        filename: Some("post123_image.jpg".into()),
        post_title: Some("March art dump".into()),
        posted_at: Some("2026-03-14T10:00:00Z".into()),
        tier: Some("paid".into()),
        topics: vec!["furry".into(), "art".into()],
        thumb: Some("data:image/webp;base64,UklGRhIAAABXRUJQVlA4TAYAAAAvAAAAAA==".into()),
        content: "post body text".into(),
    }
}

fn manifest_tags_without_magnet() -> Vec<Tag> {
    [
        ["d", "patreon:12345:67890:0"],
        ["x", "abc"],
        ["size", "10"],
        ["m", "image/png"],
        ["platform", "patreon"],
        ["creator", "BoxOfMittens"],
        ["creator_id", "12345"],
        ["post_id", "67890"],
    ]
    .into_iter()
    .map(|row| Tag::parse(row).unwrap())
    .collect()
}
