use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use tokio::process::Command;

use bakemono_core::validation::MAX_THUMB;

// small enough to embed in the signed event and stay under a relay's size limit; a 320px webp
// at this quality lands well under the 32KB thumb cap for typical images
const SCALE: &str = "scale=320:320:force_original_aspect_ratio=decrease:force_divisible_by=2";
const QUALITY: &str = "50";

// best-effort downscaled preview for any image, gif, or video, encoded inline as a webp data URI.
// callers treat None (too big to embed) and Err (no ffmpeg / decode failed) as "no thumbnail",
// never a fatal scrape failure
pub async fn generate_inline(media: &Path, mime: &str) -> Result<Option<String>> {
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
        bail!("ffmpeg produced no thumbnail for {}", media.display());
    }
    let bytes = std::fs::read(&out).with_context(|| format!("reading {}", out.display()));
    let _ = std::fs::remove_file(&out);
    let uri = format!("data:image/webp;base64,{}", STANDARD.encode(bytes?));
    Ok((uri.len() <= MAX_THUMB).then_some(uri))
}

fn tmp_path(media: &Path) -> PathBuf {
    let mut name = media.to_path_buf().into_os_string();
    name.push(".thumbtmp.webp");
    PathBuf::from(name)
}

async fn run(media: &Path, out: &Path, seek: Option<&str>) -> Result<()> {
    let bin = ffmpeg_bin();
    let mut cmd = Command::new(&bin);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y");
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
        .arg("libwebp")
        .arg("-quality")
        .arg(QUALITY)
        .arg(out)
        .status()
        .await
        .with_context(|| format!("spawning ffmpeg '{}'", bin.to_string_lossy()))?;
    if !status.success() {
        bail!("ffmpeg '{}' exited with {status}", bin.to_string_lossy());
    }
    Ok(())
}

fn nonempty(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false)
}

fn ffmpeg_bin() -> OsString {
    std::env::var_os("BAKEMONO_FFMPEG").unwrap_or_else(|| OsString::from("ffmpeg"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // makes a real source image with ffmpeg, then thumbnails it inline; skips if ffmpeg is absent
    #[tokio::test]
    async fn makes_an_inline_webp_thumbnail_from_an_image() {
        let dir = std::env::temp_dir().join(format!("bakemono-thumb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.png");

        let made = Command::new(ffmpeg_bin())
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i"])
            .arg("color=c=red:s=600x300")
            .args(["-frames:v", "1"])
            .arg(&src)
            .status()
            .await;
        match made {
            Ok(s) if s.success() => {}
            _ => {
                eprintln!("skipping: ffmpeg not available");
                std::fs::remove_dir_all(&dir).ok();
                return;
            }
        }

        let uri = generate_inline(&src, "image/png").await.unwrap();
        std::fs::remove_dir_all(&dir).ok();

        let uri = uri.expect("thumbnail small enough to embed");
        assert!(uri.starts_with("data:image/webp;base64,"));
        assert!(uri.len() <= MAX_THUMB, "stays under the relay-safe cap");
        assert!(!tmp_path(&src).exists(), "temp file cleaned up");
    }
}
