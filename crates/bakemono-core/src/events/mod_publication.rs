use nostr::{Event, EventBuilder, Keys, Kind, Tag, Timestamp};

use crate::error::{Error, Result};
use crate::protocol::KIND_MOD_PUBLICATION;
use crate::tags;
use crate::validation::expect_kind;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModPublication {
    pub period: String,
    pub takedown_ids: Vec<String>,
    pub summary: String,
}

impl ModPublication {
    pub fn to_event(&self, keys: &Keys) -> Result<Event> {
        self.sign(keys, None)
    }

    pub fn to_event_at(&self, keys: &Keys, created_at: u64) -> Result<Event> {
        self.sign(keys, Some(created_at))
    }

    pub fn from_event(event: &Event) -> Result<Self> {
        expect_kind(event, KIND_MOD_PUBLICATION)?;
        Ok(Self {
            period: tags::require(event, tags::D)?,
            takedown_ids: tags::all(event, tags::EVENT_REF),
            summary: event.content.clone(),
        })
    }

    fn sign(&self, keys: &Keys, created_at: Option<u64>) -> Result<Event> {
        let mut builder =
            EventBuilder::new(Kind::from(KIND_MOD_PUBLICATION), self.summary.as_str())
                .tags(self.build_tags()?);
        if let Some(ts) = created_at {
            builder = builder.custom_created_at(Timestamp::from(ts));
        }
        builder
            .sign_with_keys(keys)
            .map_err(|e| Error::Build(e.to_string()))
    }

    fn build_tags(&self) -> Result<Vec<Tag>> {
        let mut rows = vec![vec![tags::D.to_string(), self.period.clone()]];
        for id in &self.takedown_ids {
            rows.push(vec![tags::EVENT_REF.to_string(), id.clone()]);
        }
        rows.into_iter()
            .map(|row| Tag::parse(row).map_err(|e| Error::Build(e.to_string())))
            .collect()
    }
}
