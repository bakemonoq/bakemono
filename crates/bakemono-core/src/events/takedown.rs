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
        Ok(Self {
            target: Target::from_event(event)?,
            reason: tags::require(event, tags::REASON)?,
            applied_at: tags::first(event, tags::APPLIED_AT),
            explanation: event.content.clone(),
        })
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
    fn parts(&self) -> (&'static str, &str) {
        match self {
            Target::Event(v) => (tags::EVENT_REF, v),
            Target::FileHash(v) => (tags::X, v),
            Target::Pubkey(v) => (tags::PUBKEY_REF, v),
        }
    }

    fn from_event(event: &Event) -> Result<Self> {
        if let Some(v) = tags::first(event, tags::EVENT_REF) {
            Ok(Target::Event(v))
        } else if let Some(v) = tags::first(event, tags::X) {
            Ok(Target::FileHash(v))
        } else if let Some(v) = tags::first(event, tags::PUBKEY_REF) {
            Ok(Target::Pubkey(v))
        } else {
            Err(Error::MissingTag(tags::EVENT_REF))
        }
    }
}
