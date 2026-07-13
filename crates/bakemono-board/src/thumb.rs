use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tokio::process::Command;

// ~400px longest side per docs/PROTOCOL.md; mjpeg ships in every ffmpeg build, unlike libwebp
const SCALE: &str = "scale=400:400:force_original_aspect_ratio=decrease:force_divisible_by=2";

// best-effort: None means "no preview", never a fatal ingest failure
pub async fn generate(media: &Path, mime: &str) -> Option<Vec<u8>> {
    if !mime.starts_with("image/") && !mime.starts_with("video/") {
        return None;
    }
    match try_generate(media, mime).await {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            tracing::debug!("no thumbnail for {}: {e:#}", media.display());
            None
        }
    }
}

async fn try_generate(media: &Path, mime: &str) -> Result<Vec<u8>> {
    let out = tmp_path(media);
    let made = if mime.starts_with("video/") {
        // a 1s seek skips black intro frames; fall back to the first frame for sub-second clips
        (run(media, &out, Some("1")).await.is_ok() && nonempty(&out))
            || run(media, &out, Some("0")).await.is_ok()
    } else {
        run(media, &out, None).await.is_ok()
    };
    if !made || !nonempty(&out) {
        let _ = std::fs::remove_file(&out);
        bail!("ffmpeg produced no thumbnail");
    }
    let bytes = std::fs::read(&out).with_context(|| format!("reading {}", out.display()));
    let _ = std::fs::remove_file(&out);
    bytes
}

async fn run(media: &Path, out: &Path, seek: Option<&str>) -> Result<()> {
    let bin = ffmpeg_bin();
    let mut cmd = Command::new(&bin);
    cmd.arg("-hide_banner").arg("-loglevel").arg("error").arg("-y");
    if let Some(ss) = seek {
        cmd.arg("-ss").arg(ss);
    }
    let status = cmd
        .arg("-i")
        .arg(media)
        .arg("-frames:v")
        .arg("1")
        .arg("-vf")
        .arg(SCALE)
        .arg("-c:v")
        .arg("mjpeg")
        .arg("-q:v")
        .arg("6")
        .arg(out)
        .status()
        .await
        .with_context(|| format!("spawning ffmpeg '{}'", bin.to_string_lossy()))?;
    if !status.success() {
        bail!("ffmpeg '{}' exited with {status}", bin.to_string_lossy());
    }
    Ok(())
}

// one-time catch-up for files ingested before dimensions were recorded (or restored from a
// manifest): probe them through the local gateway, which serves pinned bytes with Range support
// so ffprobe reads headers instead of whole videos. 0x0 marks a failed probe as done
pub async fn backfill_dims(pool: sqlx::PgPool) {
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    let gateway = gateway_url();
    let mut done = 0u64;
    loop {
        let batch = match crate::db::files_missing_dims(&pool, 64).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("dims backfill query failed: {e:#}");
                return;
            }
        };
        if batch.is_empty() {
            break;
        }
        for cid in batch {
            let (w, h) = dimensions(format!("{gateway}/ipfs/{cid}")).await.unwrap_or((0, 0));
            if let Err(e) = crate::db::set_dims(&pool, &cid, w, h).await {
                tracing::warn!("dims backfill update failed for {cid}: {e:#}");
                return;
            }
            done += 1;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        tracing::info!("dims backfill: {done} files probed");
    }
    if done > 0 {
        tracing::info!("dims backfill complete: {done} files");
    }
}

pub async fn dimensions(input: impl AsRef<std::ffi::OsStr>) -> Option<(i32, i32)> {
    let mut cmd = Command::new(ffprobe_bin());
    cmd.arg("-v")
        .arg("error")
        .arg("-select_streams")
        .arg("v:0")
        .arg("-show_entries")
        .arg("stream=width,height")
        .arg("-of")
        .arg("csv=s=x:p=0")
        .arg(input.as_ref())
        .kill_on_drop(true);
    let out = tokio::time::timeout(std::time::Duration::from_secs(60), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text.lines().next()?.trim().split('x');
    let w: i32 = parts.next()?.parse().ok()?;
    let h: i32 = parts.next()?.parse().ok()?;
    (w > 0 && h > 0).then_some((w, h))
}

fn tmp_path(media: &Path) -> PathBuf {
    let mut name = media.to_path_buf().into_os_string();
    name.push(".thumbtmp.jpg");
    PathBuf::from(name)
}

fn nonempty(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false)
}

fn ffmpeg_bin() -> OsString {
    std::env::var_os("BAKEMONO_FFMPEG").unwrap_or_else(|| OsString::from("ffmpeg"))
}

fn ffprobe_bin() -> OsString {
    std::env::var_os("BAKEMONO_FFPROBE").unwrap_or_else(|| OsString::from("ffprobe"))
}

fn gateway_url() -> String {
    if let Ok(g) = std::env::var("BAKEMONO_KUBO_GATEWAY") {
        if !g.trim().is_empty() {
            return g.trim().trim_end_matches('/').to_string();
        }
    }
    // kubo's gateway rides next to its RPC port, so the API address points at the right host
    let api = std::env::var("BAKEMONO_KUBO_API").unwrap_or_default();
    match api.trim().trim_end_matches('/').strip_suffix(":5001") {
        Some(host) if !host.is_empty() => format!("{host}:8080"),
        _ => "http://127.0.0.1:8080".to_string(),
    }
}
