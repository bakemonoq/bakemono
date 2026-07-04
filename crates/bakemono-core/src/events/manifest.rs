//! Kind 31063 manifest: the signed Nostr event that describes one archived file.
//! It never carries the bytes, only points at them by sha256 (`x` tag) and magnet, so a
//! file's identity is what it is, not where it lives. Parameterized-replaceable per NIP-33
//! keyed on `d` = `platform:creator_id:post_id:file_index`, so a contributor can republish to
//! update their own entry while a different contributor's copy of the same post stays distinct

use nostr::{Event, EventBuilder, Keys, Kind, Tag, Timestamp};

use crate::error::{Error, Result};
use crate::protocol::KIND_MANIFEST;
use crate::tags;
use crate::validation::expect_kind;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Manifest {
    pub platform: String,
    pub creator: String,
    pub creator_id: String,
    pub post_id: String,
    pub file_index: u32,
    pub file_hash: String,
    pub size: u64,
    pub mime: String,
    pub magnet: String,
    // this file's index inside the post-bundle torrent the magnet points at (0 for a single-file magnet)
    pub bundle_index: u32,
    pub filename: Option<String>,
    pub post_title: Option<String>,
    pub posted_at: Option<String>,
    pub tier: Option<String>,
    pub topics: Vec<String>,
    pub thumb: Option<String>,
    pub content: String,
}

impl Manifest {
    pub fn d_tag(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.platform, self.creator_id, self.post_id, self.file_index
        )
    }

    pub fn to_event(&self, keys: &Keys) -> Result<Event> {
        self.sign(keys, None, 0)
    }

    pub fn to_event_at(&self, keys: &Keys, created_at: u64) -> Result<Event> {
        self.sign(keys, Some(created_at), 0)
    }

    pub fn to_event_pow(&self, keys: &Keys, difficulty: u8) -> Result<Event> {
        self.sign(keys, None, difficulty)
    }

    pub fn from_event(event: &Event) -> Result<Self> {
        expect_kind(event, KIND_MANIFEST)?;
        let d = tags::require(event, tags::D)?;
        let manifest = Self {
            platform: tags::require(event, tags::PLATFORM)?,
            creator: tags::require(event, tags::CREATOR)?,
            creator_id: tags::require(event, tags::CREATOR_ID)?,
            post_id: tags::require(event, tags::POST_ID)?,
            file_index: file_index_from_d(&d)?,
            file_hash: tags::require(event, tags::X)?,
            size: parse_u64(tags::SIZE, &tags::require(event, tags::SIZE)?)?,
            mime: tags::require(event, tags::MIME)?,
            magnet: tags::require(event, tags::MAGNET)?,
            bundle_index: tags::first(event, tags::BUNDLE_INDEX)
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            filename: tags::first(event, tags::FILENAME),
            post_title: tags::first(event, tags::POST_TITLE),
            posted_at: tags::first(event, tags::POSTED_AT),
            tier: tags::first(event, tags::TIER),
            topics: tags::all(event, tags::TOPIC),
            thumb: tags::first(event, tags::THUMB),
            content: event.content.clone(),
        };
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<()> {
        use crate::validation as v;
        v::require_field(tags::PLATFORM, &self.platform, v::MAX_PLATFORM)?;
        v::require_field(tags::CREATOR, &self.creator, v::MAX_CREATOR)?;
        v::require_field(tags::CREATOR_ID, &self.creator_id, v::MAX_ID)?;
        v::require_field(tags::POST_ID, &self.post_id, v::MAX_ID)?;
        v::hex_hash(tags::X, &self.file_hash)?;
        v::require_field(tags::MIME, &self.mime, v::MAX_MIME)?;
        v::magnet(&self.magnet)?;
        if self.size > v::MAX_FILE_SIZE {
            return Err(Error::TooLarge { field: tags::SIZE });
        }
        v::optional_field(tags::FILENAME, &self.filename, v::MAX_FILENAME)?;
        v::optional_field(tags::POST_TITLE, &self.post_title, v::MAX_TITLE)?;
        v::optional_field(tags::POSTED_AT, &self.posted_at, v::MAX_TIMESTAMP)?;
        v::optional_field(tags::TIER, &self.tier, v::MAX_TIER)?;
        v::optional_field(tags::THUMB, &self.thumb, v::MAX_THUMB)?;
        if self.topics.len() > v::MAX_TOPICS {
            return Err(Error::TooLarge { field: tags::TOPIC });
        }
        for topic in &self.topics {
            v::require_field(tags::TOPIC, topic, v::MAX_TOPIC)?;
        }
        v::within("content", &self.content, v::MAX_CONTENT)?;
        Ok(())
    }

    fn sign(&self, keys: &Keys, created_at: Option<u64>, difficulty: u8) -> Result<Event> {
        let mut builder = EventBuilder::new(Kind::from(KIND_MANIFEST), self.content.as_str())
            .tags(self.build_tags()?);
        if let Some(ts) = created_at {
            builder = builder.custom_created_at(Timestamp::from(ts));
        }
        if difficulty > 0 {
            builder = builder.pow(difficulty);
        }
        builder
            .sign_with_keys(keys)
            .map_err(|e| Error::Build(e.to_string()))
    }

    fn build_tags(&self) -> Result<Vec<Tag>> {
        let mut rows = vec![
            vec![tags::D.to_string(), self.d_tag()],
            vec![tags::X.to_string(), self.file_hash.clone()],
            vec![tags::SIZE.to_string(), self.size.to_string()],
            vec![tags::MIME.to_string(), self.mime.clone()],
            vec![tags::MAGNET.to_string(), self.magnet.clone()],
            vec![tags::BUNDLE_INDEX.to_string(), self.bundle_index.to_string()],
            vec![tags::PLATFORM.to_string(), self.platform.clone()],
            vec![tags::CREATOR.to_string(), self.creator.clone()],
            vec![tags::CREATOR_ID.to_string(), self.creator_id.clone()],
            vec![tags::POST_ID.to_string(), self.post_id.clone()],
        ];
        push_opt(&mut rows, tags::FILENAME, &self.filename);
        push_opt(&mut rows, tags::POST_TITLE, &self.post_title);
        push_opt(&mut rows, tags::POSTED_AT, &self.posted_at);
        push_opt(&mut rows, tags::TIER, &self.tier);
        push_opt(&mut rows, tags::THUMB, &self.thumb);
        for topic in &self.topics {
            rows.push(vec![tags::TOPIC.to_string(), topic.clone()]);
        }
        rows.into_iter()
            .map(|row| Tag::parse(row).map_err(|e| Error::Build(e.to_string())))
            .collect()
    }
}

fn push_opt(rows: &mut Vec<Vec<String>>, key: &str, value: &Option<String>) {
    if let Some(v) = value {
        rows.push(vec![key.to_string(), v.clone()]);
    }
}

fn file_index_from_d(d: &str) -> Result<u32> {
    d.rsplit(':')
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Error::MalformedTag {
            tag: tags::D,
            value: d.to_string(),
        })
}

fn parse_u64(tag: &'static str, raw: &str) -> Result<u64> {
    raw.parse().map_err(|_| Error::MalformedTag {
        tag,
        value: raw.to_string(),
    })
}
