//! Kind 31064 takedown: an instance operator's signed statement that a target should be hidden.
//! The target is one of an event id (`e`), a file hash (`x`), or a contributor pubkey (`p`), with a
//! free-form `reason` such as `dmca-us` or `csam`. It hides nothing on its own: each board decides
//! whether to honor a peer operator's takedown and relays keep the original event regardless, so the
//! set of takedowns doubles as a public, signed transparency log of every moderation decision

use nostr::{Event, EventBuilder, Keys, Kind, Tag, Timestamp};

use crate::error::{Error, Result};
use crate::protocol::KIND_TAKEDOWN;
use crate::tags;
use crate::validation::expect_kind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Event(String),
    FileHash(String),
    Pubkey(String),
    Post(String),
    Creator(String),
    Infohash(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Takedown {
    pub target: Target,
    pub reason: String,
    pub applied_at: Option<String>,
    pub explanation: String,
}

impl Takedown {
    pub fn d_tag(&self) -> String {
        let (kind, value) = self.target.parts();
        format!("takedown:{kind}:{value}")
    }

    pub fn to_event(&self, keys: &Keys) -> Result<Event> {
        self.sign(keys, None)
    }

    pub fn to_event_at(&self, keys: &Keys, created_at: u64) -> Result<Event> {
        self.sign(keys, Some(created_at))
    }

    pub fn from_event(event: &Event) -> Result<Self> {
        expect_kind(event, KIND_TAKEDOWN)?;
        let takedown = Self {
            target: Target::from_event(event)?,
            reason: tags::require(event, tags::REASON)?,
            applied_at: tags::first(event, tags::APPLIED_AT),
            explanation: event.content.clone(),
        };
        takedown.validate()?;
        Ok(takedown)
    }

    fn validate(&self) -> Result<()> {
        use crate::validation as v;
        let (key, value) = self.target.parts();
        match self.target {
            Target::Post(_) | Target::Creator(_) => v::require_field(key, value, v::MAX_TARGET)?,
            Target::Infohash(_) => v::infohash_hex(key, value)?,
            _ => v::hex_hash(key, value)?,
        }
        v::require_field(tags::REASON, &self.reason, v::MAX_REASON)?;
        v::optional_field(tags::APPLIED_AT, &self.applied_at, v::MAX_TIMESTAMP)?;
        v::within("explanation", &self.explanation, v::MAX_EXPLANATION)?;
        Ok(())
    }

    fn sign(&self, keys: &Keys, created_at: Option<u64>) -> Result<Event> {
        let mut builder = EventBuilder::new(Kind::from(KIND_TAKEDOWN), self.explanation.as_str())
            .tags(self.build_tags()?);
        if let Some(ts) = created_at {
            builder = builder.custom_created_at(Timestamp::from(ts));
        }
        builder
            .sign_with_keys(keys)
            .map_err(|e| Error::Build(e.to_string()))
    }

    fn build_tags(&self) -> Result<Vec<Tag>> {
        let (target_key, target_value) = self.target.parts();
        let mut rows = vec![
            vec![tags::D.to_string(), self.d_tag()],
            vec![target_key.to_string(), target_value.to_string()],
            vec![tags::REASON.to_string(), self.reason.clone()],
        ];
        if let Some(at) = &self.applied_at {
            rows.push(vec![tags::APPLIED_AT.to_string(), at.clone()]);
        }
        rows.into_iter()
            .map(|row| Tag::parse(row).map_err(|e| Error::Build(e.to_string())))
            .collect()
    }
}

impl Target {
    pub fn post(platform: &str, creator_id: &str, post_id: &str) -> Self {
        Target::Post(format!("{platform}:{creator_id}:{post_id}"))
    }

    pub fn creator(platform: &str, creator_id: &str) -> Self {
        Target::Creator(format!("{platform}:{creator_id}"))
    }

    // tag key and value, the same pair used in the d_tag and the target tag. e/x/p carry a 64-hex
    // value; post/creator carry a `platform:creator_id[:post_id]` key the board matches by column
    pub fn parts(&self) -> (&'static str, &str) {
        match self {
            Target::Event(v) => (tags::EVENT_REF, v),
            Target::FileHash(v) => (tags::X, v),
            Target::Pubkey(v) => (tags::PUBKEY_REF, v),
            Target::Post(v) => (tags::POST_REF, v),
            Target::Creator(v) => (tags::CREATOR_REF, v),
            Target::Infohash(v) => (tags::INFOHASH_REF, v),
        }
    }

    pub fn from_parts(kind: &str, value: String) -> Option<Self> {
        match kind {
            tags::EVENT_REF => Some(Target::Event(value)),
            tags::X => Some(Target::FileHash(value)),
            tags::PUBKEY_REF => Some(Target::Pubkey(value)),
            tags::POST_REF => Some(Target::Post(value)),
            tags::CREATOR_REF => Some(Target::Creator(value)),
            tags::INFOHASH_REF => Some(Target::Infohash(value.to_ascii_lowercase())),
            _ => None,
        }
    }

    fn from_event(event: &Event) -> Result<Self> {
        if let Some(v) = tags::first(event, tags::EVENT_REF) {
            Ok(Target::Event(v))
        } else if let Some(v) = tags::first(event, tags::X) {
            Ok(Target::FileHash(v))
        } else if let Some(v) = tags::first(event, tags::PUBKEY_REF) {
            Ok(Target::Pubkey(v))
        } else if let Some(v) = tags::first(event, tags::POST_REF) {
            Ok(Target::Post(v))
        } else if let Some(v) = tags::first(event, tags::CREATOR_REF) {
            Ok(Target::Creator(v))
        } else if let Some(v) = tags::first(event, tags::INFOHASH_REF) {
            Ok(Target::Infohash(v.to_ascii_lowercase()))
        } else {
            Err(Error::MissingTag(tags::EVENT_REF))
        }
    }
}
