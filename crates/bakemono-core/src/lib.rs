pub mod defaults;
pub mod error;
pub mod events;
pub mod manifest;
pub mod protocol;
pub mod tags;
pub mod validation;

pub use defaults::{default_relays, default_trackers};
pub use error::{Error, Result};
pub use events::manifest::Manifest;
pub use events::takedown::{Takedown, Target};
pub use manifest::{verify_head_json, BoardKey, Head, Root, Shard};
pub use validation::{expect_kind, replaceable_address, verify, ReplaceableAddress};

pub use nostr;
