use std::net::{IpAddr, Ipv4Addr};

use anyhow::Result;
use nostr_relay_builder::{LocalRelay, RelayBuilder};

#[tokio::main]
async fn main() -> Result<()> {
    let builder = RelayBuilder::default()
        .addr(IpAddr::V4(Ipv4Addr::LOCALHOST))
        .port(8080);
    let relay = LocalRelay::new(builder);
    relay.run().await?;

    println!("relay listening at {}", relay.url().await);
    println!("ctrl-c to stop");
    std::future::pending::<()>().await;
    Ok(())
}
