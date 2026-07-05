mod canonical;
mod key;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};

pub use canonical::to_canonical_json;
pub use key::{verify_sig, BoardKey};

pub const SCHEMA: u32 = 1;

// frozen `ipfs add` parameters: any deviation forks the CID for identical bytes (docs/PROTOCOL.md)
pub const ADD_CID_VERSION: u32 = 1;
pub const ADD_CHUNKER: &str = "size-1048576";
pub const ADD_HASH: &str = "sha2-256";
pub const ADD_RAW_LEAVES: bool = true;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Head {
    pub schema: u32,
    pub version: u64,
    pub root: String,
    pub prev: Option<String>,
    pub published_at: String,
    pub pubkey: String,
    pub sig: String,
}

impl Head {
    pub fn build(
        version: u64,
        root: String,
        prev: Option<String>,
        published_at: String,
        key: &BoardKey,
    ) -> Result<Head> {
        let mut head = Head {
            schema: SCHEMA,
            version,
            root,
            prev,
            published_at,
            pubkey: key.public_hex(),
            sig: String::new(),
        };
        head.sig = key.sign_hex(&head.signing_bytes()?);
        Ok(head)
    }

    pub fn to_json(&self) -> Result<Vec<u8>> {
        to_canonical_json(self)
    }

    fn signing_bytes(&self) -> Result<Vec<u8>> {
        let mut value = serde_json::to_value(self).map_err(|e| Error::Build(e.to_string()))?;
        value
            .as_object_mut()
            .expect("head serializes to an object")
            .remove("sig");
        to_canonical_json(&value)
    }
}

// verification runs on the raw JSON, not the typed struct: the signature covers every field
// the publisher wrote, including ones this build does not know about yet
pub fn verify_head_json(
    raw: &str,
    trusted_pubkey: &str,
    last_version: Option<u64>,
) -> Result<Head> {
    let value: Value =
        serde_json::from_str(raw).map_err(|e| Error::MalformedHead(e.to_string()))?;
    let mut unsigned = value
        .as_object()
        .cloned()
        .ok_or_else(|| Error::MalformedHead("not a JSON object".into()))?;
    let sig = unsigned
        .remove("sig")
        .and_then(|v| v.as_str().map(str::to_owned))
        .ok_or_else(|| Error::MalformedHead("missing sig".into()))?;
    let head: Head =
        serde_json::from_value(value).map_err(|e| Error::MalformedHead(e.to_string()))?;

    if head.schema != SCHEMA {
        return Err(Error::UnknownSchema(head.schema));
    }
    if !head.pubkey.eq_ignore_ascii_case(trusted_pubkey) {
        return Err(Error::UntrustedKey);
    }
    verify_sig(&head.pubkey, &to_canonical_json(&unsigned)?, &sig)?;
    if let Some(last) = last_version {
        if head.version <= last {
            return Err(Error::StaleVersion { got: head.version, last });
        }
    }
    Ok(head)
}

pub fn shard_key(platform: &str, creator_id: &str) -> String {
    format!("{platform}:{creator_id}")
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Root {
    pub shards: BTreeMap<String, ShardRef>,
    #[serde(default)]
    pub revoked: Vec<RevokedEntry>,
    #[serde(default)]
    pub peers: Vec<Peer>,
}

impl Root {
    pub fn to_json(&self) -> Result<Vec<u8>> {
        to_canonical_json(self)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShardRef {
    pub cid: String,
    pub posts: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RevokedEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator: Option<String>,
    pub reason: String,
    pub revoked_at: String,
}

impl RevokedEntry {
    pub fn validate(&self) -> Result<()> {
        let has_target = self.cid.is_some()
            || self.sha256.is_some()
            || self.post.is_some()
            || self.creator.is_some();
        if !has_target {
            return Err(Error::EmptyRevoked);
        }
        if self.reason.is_empty() {
            return Err(Error::MalformedHead("revoked entry without reason".into()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Peer {
    pub name: String,
    pub pubkey: String,
    pub pointer: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Shard {
    pub platform: String,
    pub creator_id: String,
    pub creator: String,
    pub posts: Vec<Post>,
}

impl Shard {
    pub fn to_json(&self) -> Result<Vec<u8>> {
        to_canonical_json(self)
    }

    pub fn key(&self) -> String {
        shard_key(&self.platform, &self.creator_id)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Post {
    pub post_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub posted_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    pub files: Vec<FileEntry>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    pub cid: String,
    pub sha256: String,
    pub size: u64,
    pub mime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumb: Option<String>,
}
