use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tokio::process::Command;

// fit inside a 400x400 box, keep aspect, even dims so mjpeg accepts the frame
const SCALE: &str = "scale=400:400:force_original_aspect_ratio=decrease:force_divisible_by=2";

pub fn thumb_path(media: &Path) -> PathBuf {
    let mut name = media.to_path_buf().into_os_string();
    name.push(".thumb.jpg");
    PathBuf::from(name)
}

// best-effort downscaled poster frame for any image, gif, or video; the jpeg path on success.
// callers treat an error as "no thumbnail", never a fatal scrape failure
pub async fn generate(media: &Path, mime: &str) -> Result<PathBuf> {
    let out = thumb_path(media);
    if mime.starts_with("video/") {
        // a 1s seek skips black intro frames; fall back to the first frame for sub-second clips
        if run(media, &out, Some("1")).await.is_ok() && nonempty(&out) {
            return Ok(out);
        }
        run(media, &out, Some("0")).await?;
    } else {
        run(media, &out, None).await?;
    }
    if nonempty(&out) {
        Ok(out)
    } else {
        bail!("ffmpeg produced no thumbnail for {}", media.display())
    }
}

async fn run(media: &Path, out: &Path, seek: Option<&str>) -> Result<()> {
    let bin = ffmpeg_bin();
    let mut cmd = Command::new(&bin);
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
        .arg("-q:v")
        .arg("5")
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

    // makes a real source image with ffmpeg, then thumbnails it; skips if ffmpeg is absent
    #[tokio::test]
    async fn makes_a_jpeg_thumbnail_from_an_image() {
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

        let thumb = generate(&src, "image/png").await.unwrap();
        let bytes = std::fs::read(&thumb).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert!(!bytes.is_empty());
        assert_eq!(&bytes[0..3], &[0xFF, 0xD8, 0xFF], "output is a jpeg");
    }
}
