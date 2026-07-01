use std::path::Path;

use anyhow::Result;
use bakemono_torrent::Seeder;

// seed a local file over classic BT and keep running, so a gateway can pull it:
//   cargo run -p bakemono-torrent --example seed -- <file> [listen_port]
#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let file = args.next().expect("usage: seed <file> [listen_port]");
    let port: u16 = match args.next() {
        Some(s) => s.parse()?,
        None => 4250,
    };
    let staging = std::env::temp_dir().join(format!("bakemono-seed-{}", std::process::id()));
    let trackers = vec!["udp://tracker.opentrackr.org:1337/announce".to_string()];
    let seeder = Seeder::start(staging, trackers, Some(port), None, None).await?;
    let info = seeder.seed(Path::new(&file)).await?;
    println!("seeding {file} on 127.0.0.1:{port}");
    println!("infohash: {}", info.info_hash);
    println!("magnet:   {}", info.magnet);
    println!(
        "pull it: BAKEMONO_GATEWAY_PEERS=127.0.0.1:{port} cargo run -p bakemono-torrent --example fetch -- {} 0 pulled.bin",
        info.info_hash
    );
    tokio::signal::ctrl_c().await?;
    Ok(())
}
