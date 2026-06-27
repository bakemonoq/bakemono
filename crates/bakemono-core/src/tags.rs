use nostr::Event;

use crate::error::{Error, Result};

pub const D: &str = "d";
pub const X: &str = "x";
pub const SIZE: &str = "size";
pub const MIME: &str = "m";
pub const FILENAME: &str = "filename";
pub const MAGNET: &str = "magnet";
pub const PLATFORM: &str = "platform";
pub const CREATOR: &str = "creator";
pub const CREATOR_ID: &str = "creator_id";
pub const POST_ID: &str = "post_id";
pub const POST_TITLE: &str = "post_title";
pub const POSTED_AT: &str = "posted_at";
pub const TIER: &str = "tier";
pub const TOPIC: &str = "t";
pub const THUMB: &str = "thumb";
pub const EVENT_REF: &str = "e";
pub const PUBKEY_REF: &str = "p";
pub const REASON: &str = "reason";
pub const APPLIED_AT: &str = "applied_at";

pub fn require(event: &Event, key: &'static str) -> Result<String> {
    first(event, key).ok_or(Error::MissingTag(key))
}

pub fn first(event: &Event, key: &str) -> Option<String> {
    event
        .tags
        .iter()
        .find_map(|tag| value_for(tag.as_slice(), key))
}

pub fn all(event: &Event, key: &str) -> Vec<String> {
    event
        .tags
        .iter()
        .filter_map(|tag| value_for(tag.as_slice(), key))
        .collect()
}

fn value_for(row: &[String], key: &str) -> Option<String> {
    (row.first().map(String::as_str) == Some(key))
        .then(|| row.get(1).cloned())
        .flatten()
}
