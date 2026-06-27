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
