use std::path::PathBuf;

use anyhow::{Context, Result};
use nostr_sdk::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let file: PathBuf = args
        .next()
        .context("usage: publish <file> [relay-url]")?
        .into();
    let relay = args.next().unwrap_or_else(default_relay);

    let keys = keys_from_env_or_generate()?;
    let manifest = bakemono_smoke::manifest_from_file(&file)?;
    let id = bakemono_smoke::publish(&relay, &keys, &manifest).await?;

    println!("published {} to {relay}", id.to_hex());
    println!("creator={} hash={}", manifest.creator, manifest.file_hash);
    println!("npub {}", keys.public_key().to_bech32()?);
    Ok(())
}

fn keys_from_env_or_generate() -> Result<Keys> {
    match std::env::var("BAKEMONO_NSEC") {
        Ok(nsec) => Ok(Keys::parse(&nsec)?),
        Err(_) => {
            let keys = Keys::generate();
            println!("generated nsec {}", keys.secret_key().to_bech32()?);
            Ok(keys)
        }
    }
}

fn default_relay() -> String {
    "ws://127.0.0.1:8080".to_string()
}
