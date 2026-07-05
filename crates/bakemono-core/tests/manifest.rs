use bakemono_core::manifest::{shard_key, to_canonical_json, BoardKey, Head, Root, RevokedEntry, Shard};
use bakemono_core::{verify_head_json, Error};

fn test_key() -> BoardKey {
    BoardKey::from_hex(&"11".repeat(32)).unwrap()
}

fn build_head(key: &BoardKey) -> Head {
    Head::build(
        1,
        "bafyroot".into(),
        None,
        "2026-07-05T00:00:00Z".into(),
        key,
    )
    .unwrap()
}

#[test]
fn sign_verify_roundtrip() {
    let key = test_key();
    let head = build_head(&key);
    let raw = String::from_utf8(head.to_json().unwrap()).unwrap();
    let parsed = verify_head_json(&raw, &key.public_hex(), None).unwrap();
    assert_eq!(parsed, head);
}

#[test]
fn build_is_deterministic() {
    let key = test_key();
    assert_eq!(build_head(&key), build_head(&key));
}

#[test]
fn canonical_form_is_exactly_specified() {
    // the signature is computed over this exact byte string: keys sorted bytewise,
    // no whitespace, sig omitted. If canonicalization drifts, this test breaks first.
    let key = test_key();
    let pk = key.public_hex();
    let unsigned = format!(
        r#"{{"prev":null,"pubkey":"{pk}","published_at":"2026-07-05T00:00:00Z","root":"bafyroot","schema":1,"version":1}}"#
    );
    let sig = key.sign_hex(unsigned.as_bytes());
    // scrambled key order and whitespace on the wire must not matter
    let raw = format!(
        r#"{{ "sig": "{sig}", "version": 1, "schema": 1, "root": "bafyroot", "published_at": "2026-07-05T00:00:00Z", "pubkey": "{pk}", "prev": null }}"#
    );
    let head = verify_head_json(&raw, &pk, None).unwrap();
    assert_eq!(head, build_head(&key));
}

#[test]
fn version_must_advance() {
    let key = test_key();
    let raw = String::from_utf8(build_head(&key).to_json().unwrap()).unwrap();
    assert!(verify_head_json(&raw, &key.public_hex(), Some(0)).is_ok());
    assert!(matches!(
        verify_head_json(&raw, &key.public_hex(), Some(1)),
        Err(Error::StaleVersion { got: 1, last: 1 })
    ));
    assert!(matches!(
        verify_head_json(&raw, &key.public_hex(), Some(7)),
        Err(Error::StaleVersion { got: 1, last: 7 })
    ));
}

#[test]
fn tampered_field_fails() {
    let key = test_key();
    let raw = String::from_utf8(build_head(&key).to_json().unwrap()).unwrap();
    let tampered = raw.replace("bafyroot", "bafyevil");
    assert!(matches!(
        verify_head_json(&tampered, &key.public_hex(), None),
        Err(Error::BadSignature)
    ));
}

#[test]
fn untrusted_key_fails() {
    let key = test_key();
    let other = BoardKey::from_hex(&"22".repeat(32)).unwrap();
    let raw = String::from_utf8(build_head(&key).to_json().unwrap()).unwrap();
    assert!(matches!(
        verify_head_json(&raw, &other.public_hex(), None),
        Err(Error::UntrustedKey)
    ));
}

#[test]
fn unknown_schema_fails() {
    let key = test_key();
    let raw = String::from_utf8(build_head(&key).to_json().unwrap()).unwrap();
    let raw = raw.replace(r#""schema":1"#, r#""schema":2"#);
    assert!(matches!(
        verify_head_json(&raw, &key.public_hex(), None),
        Err(Error::UnknownSchema(2))
    ));
}

#[test]
fn unknown_fields_are_signed_and_tolerated() {
    // a field this build does not know still counts toward the signature ("zzz" sorts last)
    let key = test_key();
    let pk = key.public_hex();
    let unsigned = format!(
        r#"{{"prev":null,"pubkey":"{pk}","published_at":"2026-07-05T00:00:00Z","root":"bafyroot","schema":1,"version":1,"zzz":"future"}}"#
    );
    let sig = key.sign_hex(unsigned.as_bytes());
    let raw = format!(
        r#"{{"schema":1,"version":1,"root":"bafyroot","prev":null,"published_at":"2026-07-05T00:00:00Z","pubkey":"{pk}","zzz":"future","sig":"{sig}"}}"#
    );
    let head = verify_head_json(&raw, &pk, None).unwrap();
    assert_eq!(head.root, "bafyroot");

    // the same unknown field injected without re-signing is tampering
    let signed = String::from_utf8(build_head(&key).to_json().unwrap()).unwrap();
    let injected = signed.replace(r#""schema":1"#, r#""schema":1,"zzz":"future""#);
    assert!(matches!(
        verify_head_json(&injected, &pk, None),
        Err(Error::BadSignature)
    ));
}

#[test]
fn shard_bytes_are_delta_stable() {
    let shard = sample_shard();
    assert_eq!(shard.to_json().unwrap(), sample_shard().to_json().unwrap());

    let mut changed = sample_shard();
    changed.posts[0].files[0].size = 999;
    assert_ne!(shard.to_json().unwrap(), changed.to_json().unwrap());
}

#[test]
fn shard_and_root_survive_unknown_fields() {
    let mut value = serde_json::to_value(sample_shard()).unwrap();
    value["zzz"] = "future".into();
    let shard: Shard = serde_json::from_value(value).unwrap();
    assert_eq!(shard, sample_shard());

    let root: Root = serde_json::from_str(r#"{"shards":{},"zzz":"future"}"#).unwrap();
    assert!(root.shards.is_empty() && root.revoked.is_empty() && root.peers.is_empty());
}

#[test]
fn root_shard_keys_serialize_sorted() {
    let mut root = Root::default();
    for k in ["patreon:9", "fanbox:1", "patreon:10"] {
        root.shards.insert(
            k.into(),
            bakemono_core::manifest::ShardRef { cid: "bafy".into(), posts: 0, bytes: 0 },
        );
    }
    let json = String::from_utf8(root.to_json().unwrap()).unwrap();
    let positions: Vec<usize> = ["fanbox:1", "patreon:10", "patreon:9"]
        .iter()
        .map(|k| json.find(&format!(r#""{k}""#)).unwrap())
        .collect();
    assert!(positions.windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn revoked_entry_needs_a_target() {
    let entry = RevokedEntry {
        reason: "dmca-us".into(),
        revoked_at: "2026-07-05T00:00:00Z".into(),
        ..Default::default()
    };
    assert!(matches!(entry.validate(), Err(Error::EmptyRevoked)));

    let entry = RevokedEntry { cid: Some("bafy".into()), ..entry };
    assert!(entry.validate().is_ok());
}

#[test]
fn canonical_json_sorts_nested_objects() {
    let value = serde_json::json!({"b": {"d": 1, "c": [ {"z": 1, "a": 2} ]}, "a": 0});
    let bytes = to_canonical_json(&value).unwrap();
    assert_eq!(
        String::from_utf8(bytes).unwrap(),
        r#"{"a":0,"b":{"c":[{"a":2,"z":1}],"d":1}}"#
    );
}

#[test]
fn shard_key_format() {
    assert_eq!(shard_key("patreon", "12345"), "patreon:12345");
    assert_eq!(sample_shard().key(), "patreon:12345");
}

fn sample_shard() -> Shard {
    Shard {
        platform: "patreon".into(),
        creator_id: "12345".into(),
        creator: "somehandle".into(),
        posts: vec![bakemono_core::manifest::Post {
            post_id: "98765".into(),
            title: "March art dump".into(),
            body: String::new(),
            posted_at: Some("2026-03-14T10:00:00Z".into()),
            tier: Some("subscriber".into()),
            files: vec![bakemono_core::manifest::FileEntry {
                cid: "bafyfile".into(),
                sha256: "ab".repeat(32),
                size: 245760,
                mime: "image/jpeg".into(),
                filename: Some("post123_image.jpg".into()),
                thumb: Some("bafythumb".into()),
            }],
        }],
    }
}
