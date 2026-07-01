use anyhow::{anyhow, Result};
use bakemono_torrent::{synth_magnet, Gateway};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// cold-fetch a torrent straight from the swarm with no board, db, or cache in the way:
//   cargo run -p bakemono-torrent --example fetch -- <magnet|infohash> [file_index] [out_path]
#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let target = args
        .next()
        .ok_or_else(|| anyhow!("usage: fetch <magnet|infohash> [file_index] [out_path]"))?;
    let file_index: usize = match args.next() {
        Some(s) => s.parse()?,
        None => 0,
    };
    let out = args.next().unwrap_or_else(|| "out.bin".to_string());

    let magnet = if target.starts_with("magnet:") {
        target
    } else {
        synth_magnet(&target, &PUBLIC_TRACKERS.map(String::from))
    };

    let dir = std::env::temp_dir().join(format!("bakemono-fetch-{}", std::process::id()));
    let peers = std::env::var("BAKEMONO_GATEWAY_PEERS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let gw = Gateway::new(dir, None, peers).await?;

    let meta = gw.meta(&magnet).await?;
    eprintln!("name:     {}", meta.name);
    eprintln!("infohash: {}", meta.info_hash);
    for f in &meta.files {
        eprintln!("  [{}] {} - {} bytes - {}", f.index, f.path, f.size, f.mime);
    }

    let mut file = gw.open(&magnet, file_index).await?;
    eprintln!("\ncold-filling file {file_index} ({} bytes) -> {out}", file.size);
    let mut sink = tokio::fs::File::create(&out).await?;
    let mut buf = vec![0u8; 1 << 16];
    let mut total = 0u64;
    loop {
        let n = file.stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        sink.write_all(&buf[..n]).await?;
        total += n as u64;
    }
    sink.flush().await?;
    eprintln!("done: {total} bytes");
    Ok(())
}

const PUBLIC_TRACKERS: [&str; 2] = [
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://tracker.openbittorrent.com:6969/announce",
];
