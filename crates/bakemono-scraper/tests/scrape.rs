#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use bakemono_scraper::{ScrapeRequest, Scraper};

#[test]
fn scrape_runs_the_binary_and_collects_written_files() {
    let workdir = unique_dir("scrape-run");
    let stub = write_stub(&workdir);
    let dest = workdir.join("out");

    let outcome = Scraper::with_binary(&stub)
        .scrape(&ScrapeRequest::new("boxofmittens", &dest))
        .expect("scrape");

    assert_eq!(outcome.creator, "boxofmittens");
    let names: Vec<String> = outcome
        .files
        .iter()
        .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"post1.jpg".to_string()), "{names:?}");
    assert!(names.contains(&"post1.jpg.json".to_string()), "{names:?}");

    fs::remove_dir_all(&workdir).ok();
}

#[test]
fn version_errors_when_the_binary_is_missing() {
    let scraper = Scraper::with_binary("definitely-not-a-real-gallery-dl-xyz");
    assert!(scraper.version().is_err());
}

fn write_stub(dir: &Path) -> PathBuf {
    let path = dir.join("fake-gallery-dl");
    let script = r#"#!/bin/sh
dest=""
while [ $# -gt 0 ]; do
  case "$1" in
    --destination) dest="$2"; shift 2 ;;
    *) shift ;;
  esac
done
[ -n "$dest" ] || exit 1
mkdir -p "$dest/patreon/boxofmittens"
printf 'imagedata' > "$dest/patreon/boxofmittens/post1.jpg"
printf '{"creator":"boxofmittens"}' > "$dest/patreon/boxofmittens/post1.jpg.json"
"#;
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn unique_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bakemono-{tag}-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}
