use nostr::Event;

use crate::error::{Error, Result};
use crate::tags;

pub fn verify(event: &Event) -> Result<()> {
    event.verify().map_err(|_| Error::BadSignature)
}

pub fn expect_kind(event: &Event, expected: u16) -> Result<()> {
    let got = event.kind.as_u16();
    if got == expected {
        Ok(())
    } else {
        Err(Error::WrongKind { expected, got })
    }
}

pub fn replaceable_address(event: &Event) -> Option<ReplaceableAddress> {
    tags::first(event, tags::D).map(|d| ReplaceableAddress {
        pubkey: event.pubkey.to_hex(),
        kind: event.kind.as_u16(),
        d,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReplaceableAddress {
    pub pubkey: String,
    pub kind: u16,
    pub d: String,
}

// per-field caps enforced on ingest so one event cannot bloat the index or the relay
pub const MAX_PLATFORM: usize = 64;
pub const MAX_CREATOR: usize = 256;
pub const MAX_ID: usize = 128;
pub const MAX_TARGET: usize = 512;
pub const MAX_MIME: usize = 128;
pub const MAX_MAGNET: usize = 2048;
pub const MAX_FILENAME: usize = 512;
pub const MAX_TITLE: usize = 512;
pub const MAX_TIER: usize = 32;
pub const MAX_TIMESTAMP: usize = 64;
pub const MAX_TOPIC: usize = 64;
pub const MAX_TOPICS: usize = 32;
pub const MAX_THUMB: usize = 32 * 1024;
pub const MAX_CONTENT: usize = 64 * 1024;
pub const MAX_REASON: usize = 64;
pub const MAX_EXPLANATION: usize = 8 * 1024;
pub const MAX_FILE_SIZE: u64 = 1 << 40;
const HASH_LEN: usize = 64;
const INFOHASH_LEN: usize = 40;

pub(crate) fn require_field(tag: &'static str, value: &str, max: usize) -> Result<()> {
    if value.is_empty() {
        return Err(Error::MalformedTag {
            tag,
            value: String::new(),
        });
    }
    within(tag, value, max)
}

pub(crate) fn optional_field(tag: &'static str, value: &Option<String>, max: usize) -> Result<()> {
    match value {
        Some(v) => within(tag, v, max),
        None => Ok(()),
    }
}

pub(crate) fn within(field: &'static str, value: &str, max: usize) -> Result<()> {
    if value.len() > max {
        Err(Error::TooLarge { field })
    } else {
        Ok(())
    }
}

// every takedown target and the file `x` are 32-byte hashes, lowercase hex
pub(crate) fn hex_hash(tag: &'static str, value: &str) -> Result<()> {
    let ok = value.len() == HASH_LEN
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if ok {
        Ok(())
    } else {
        Err(Error::MalformedTag {
            tag,
            value: clip(value),
        })
    }
}

// a v1 btih infohash is 40 lowercase hex chars, distinct from the 64-hex file/event/pubkey hashes
pub(crate) fn infohash_hex(tag: &'static str, value: &str) -> Result<()> {
    let ok = value.len() == INFOHASH_LEN
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if ok {
        Ok(())
    } else {
        Err(Error::MalformedTag {
            tag,
            value: clip(value),
        })
    }
}

pub(crate) fn magnet(value: &str) -> Result<()> {
    if value.starts_with("magnet:?") && value.len() <= MAX_MAGNET {
        Ok(())
    } else {
        Err(Error::MalformedTag {
            tag: tags::MAGNET,
            value: clip(value),
        })
    }
}

fn clip(value: &str) -> String {
    value.chars().take(48).collect()
}
