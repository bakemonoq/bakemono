use anyhow::Result;
use nostr_sdk::prelude::*;

use bakemono_core::protocol::KIND_MANIFEST;

#[tokio::main]
async fn main() -> Result<()> {
    let relay = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "ws://127.0.0.1:8080".to_string());

    let client = Client::new(Keys::generate());
    client.add_relay(&relay).await?;
    client.connect().await;

    let filter = Filter::new().kind(Kind::from(KIND_MANIFEST));
    client.subscribe(filter, None).await?;
    println!("listening for kind {KIND_MANIFEST} on {relay}");

    client
        .handle_notifications(|notification| async move {
            if let RelayPoolNotification::Event { event, .. } = notification {
                print_event(&event);
            }
            Ok(false)
        })
        .await?;
    Ok(())
}

fn print_event(event: &Event) {
    match bakemono_core::Manifest::from_event(event) {
        Ok(m) => println!(
            "manifest {} platform={} creator={} hash={} size={}",
            event.id.to_hex(),
            m.platform,
            m.creator,
            m.file_hash,
            m.size
        ),
        Err(e) => println!("event {} (not a manifest: {e})", event.id.to_hex()),
    }
}
