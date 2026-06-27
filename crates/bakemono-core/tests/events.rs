use std::collections::HashMap;

use bakemono_core::nostr::{Event, EventBuilder, Keys, Kind, Tag};
use bakemono_core::{replaceable_address, Error, Manifest, ReplaceableAddress};

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
        thumb: None,
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
