use std::path::{Path, PathBuf};

use bakemono_seeder::Seeder;

#[tokio::test]
async fn seeds_a_file_and_returns_a_btih_magnet() {
    let script = sidecar_script();
    if !installed(&script) {
        eprintln!(
            "skipping: webtorrent sidecar not installed (run npm install in sidecars/webtorrent)"
        );
        return;
    }

    let file = std::env::temp_dir().join(format!("bakemono-seed-{}.bin", std::process::id()));
    std::fs::write(&file, b"bakemono seed test payload").unwrap();

    let mut seeder = Seeder::start(Path::new("node"), &script).await.unwrap();
    let info = seeder.seed(&file).await.unwrap();
    seeder.shutdown().await.unwrap();
    std::fs::remove_file(&file).ok();

    assert!(
        info.magnet.starts_with("magnet:?xt=urn:btih:"),
        "magnet: {}",
        info.magnet
    );
    assert_eq!(info.info_hash.len(), 40);
    assert!(info.info_hash.chars().all(|c| c.is_ascii_hexdigit()));
}

fn sidecar_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sidecars/webtorrent/seed.mjs")
}

fn installed(script: &Path) -> bool {
    script.exists()
        && script
            .parent()
            .map(|dir| dir.join("node_modules").exists())
            .unwrap_or(false)
}
