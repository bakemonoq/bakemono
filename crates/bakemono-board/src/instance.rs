use nostr_sdk::prelude::*;

// the keypair this board signs kind 31064 takedowns with; unset means publishing is off
// while local hides still apply, so an operator can moderate before pinning an identity
pub fn load() -> Option<Keys> {
    let nsec = std::env::var("BAKEMONO_INSTANCE_NSEC").ok()?;
    let nsec = nsec.trim();
    if nsec.is_empty() {
        return None;
    }
    match Keys::parse(nsec) {
        Ok(keys) => Some(keys),
        Err(e) => {
            eprintln!("ignoring BAKEMONO_INSTANCE_NSEC: {e}");
            None
        }
    }
}

// peer operators whose takedowns this board honors, parsed from BAKEMONO_TRUSTED_INSTANCES
// (comma-separated npub or hex); our own pubkey is folded in so re-ingest of what we publish is a no-op
pub fn trusted(own: Option<&Keys>) -> Vec<PublicKey> {
    let configured = std::env::var("BAKEMONO_TRUSTED_INSTANCES").unwrap_or_default();
    let parsed = configured
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| match PublicKey::parse(s) {
            Ok(pk) => Some(pk),
            Err(e) => {
                eprintln!("ignoring trusted instance `{s}`: {e}");
                None
            }
        });
    let mut seen = std::collections::HashSet::new();
    parsed
        .chain(own.map(Keys::public_key))
        .filter(|pk| seen.insert(pk.to_hex()))
        .collect()
}
