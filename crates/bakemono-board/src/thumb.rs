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
