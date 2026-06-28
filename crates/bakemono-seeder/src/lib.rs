use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

#[derive(Debug, Clone)]
pub struct SeedInfo {
    pub magnet: String,
    pub info_hash: String,
}

pub struct Seeder {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    staging: PathBuf,
}

impl Seeder {
    pub async fn start(
        node: &Path,
        script: &Path,
        staging_root: &Path,
        extra_env: &[(String, String)],
    ) -> Result<Self> {
        let mut command = Command::new(node);
        command
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("spawning {} {}", node.display(), script.display()))?;
        let stdin = child.stdin.take().context("sidecar stdin missing")?;
        let stdout = child.stdout.take().context("sidecar stdout missing")?;
        let lines = BufReader::new(stdout).lines();

        // staging persists and is reused across runs: a file already linked here is skipped,
        // so launches cost O(new files), not O(all files), and there is nothing to sweep
        let staging = staging_root.to_path_buf();
        std::fs::create_dir_all(&staging)
            .with_context(|| format!("creating staging dir {}", staging.display()))?;

        let mut seeder = Self {
            child,
            stdin,
            lines,
            staging,
        };
        seeder.wait_for("ready").await?;
        Ok(seeder)
    }

    pub async fn from_env() -> Result<Self> {
        Self::from_env_with(&[], None).await
    }

    // extra_env is set on the sidecar (e.g. BAKEMONO_TRACKERS/BAKEMONO_STUN from app config);
    // staging_root should sit on the same volume as the source files so hardlinks never fall back to copy
    pub async fn from_env_with(
        extra_env: &[(String, String)],
        staging_root: Option<&Path>,
    ) -> Result<Self> {
        let node = std::env::var("BAKEMONO_NODE").unwrap_or_else(|_| "node".to_string());
        let script = std::env::var("BAKEMONO_WEBTORRENT")
            .unwrap_or_else(|_| "sidecars/webtorrent/seed.mjs".to_string());
        let default_root = std::env::temp_dir();
        let staging_root = staging_root.unwrap_or(&default_root);
        Self::start(Path::new(&node), Path::new(&script), staging_root, extra_env).await
    }

    // webtorrent mis-hashes pieces when the source path has odd chars, so seed a sanitized hardlink
    pub async fn seed(&mut self, file: &Path) -> Result<SeedInfo> {
        let path = self.stage(file)?.to_string_lossy().into_owned();
        self.send(&serde_json::json!({"cmd": "seed", "path": path}))
            .await?;
        loop {
            let event = self.next_event().await?;
            match event.get("event").and_then(Value::as_str) {
                Some("seeded") if str_field(&event, "path") == Some(path.as_str()) => {
                    return Ok(SeedInfo {
                        magnet: str_field(&event, "magnet").unwrap_or_default().to_string(),
                        info_hash: str_field(&event, "infoHash")
                            .unwrap_or_default()
                            .to_string(),
                    });
                }
                Some("error") => bail!(
                    "seeder: {}",
                    str_field(&event, "message").unwrap_or("unknown")
                ),
                _ => continue,
            }
        }
    }

    pub async fn shutdown(mut self) -> Result<()> {
        let _ = self.send(&serde_json::json!({"cmd": "shutdown"})).await;
        self.child
            .wait()
            .await
            .context("waiting for sidecar exit")?;
        // staging is intentionally left in place; it is reused by the next run
        Ok(())
    }

    // drop staged links whose source is no longer present, so deleted files stop pinning disk;
    // one directory listing, no content reads, removes only dead entries
    pub fn retain_staging(&self, live_sources: &[PathBuf]) {
        let keep: std::collections::HashSet<String> = live_sources
            .iter()
            .filter_map(|p| p.canonicalize().ok())
            .map(|p| staging_key(&p))
            .collect();
        let Ok(entries) = std::fs::read_dir(&self.staging) else {
            return;
        };
        for entry in entries.flatten() {
            if !keep.contains(entry.file_name().to_string_lossy().as_ref()) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    fn stage(&self, file: &Path) -> Result<PathBuf> {
        let src = file
            .canonicalize()
            .with_context(|| format!("resolving {}", file.display()))?;
        let dir = self.staging.join(staging_key(&src));
        std::fs::create_dir_all(&dir)?;
        let staged = dir.join(safe_filename(&src));
        if !staged.exists() && std::fs::hard_link(&src, &staged).is_err() {
            std::fs::copy(&src, &staged).with_context(|| format!("staging {}", src.display()))?;
        }
        Ok(staged)
    }

    async fn wait_for(&mut self, event_name: &str) -> Result<()> {
        loop {
            let event = self.next_event().await?;
            match event.get("event").and_then(Value::as_str) {
                Some(name) if name == event_name => return Ok(()),
                Some("error") => bail!(
                    "seeder: {}",
                    str_field(&event, "message").unwrap_or("unknown")
                ),
                _ => continue,
            }
        }
    }

    async fn next_event(&mut self) -> Result<Value> {
        let line = self
            .lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("sidecar closed its output"))?;
        serde_json::from_str(&line).with_context(|| format!("parsing sidecar line: {line}"))
    }

    async fn send(&mut self, message: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(message)?;
        bytes.push(b'\n');
        self.stdin.write_all(&bytes).await?;
        self.stdin.flush().await?;
        Ok(())
    }
}

fn str_field<'a>(event: &'a Value, key: &str) -> Option<&'a str> {
    event.get(key).and_then(Value::as_str)
}

fn staging_key(canonical_src: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    canonical_src.to_string_lossy().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn safe_filename(src: &Path) -> String {
    let raw = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "file".to_string()
    } else {
        cleaned
    }
}
