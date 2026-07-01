// kinds sit in the NIP-33 parameterized-replaceable range 30000-39999
pub const KIND_MANIFEST: u16 = 31063;
pub const KIND_TAKEDOWN: u16 = 31064;

pub const PROTOCOL_VERSION: u16 = 1;

// NIP-13 proof-of-work floor on kind 31063 manifests: the app mints it, the board rejects below it
pub const POW_DIFFICULTY: u8 = 18;
