//! Video posters are ordinary JPEG representations; originals are never rewritten.
use anyhow::{Context, Result, ensure};
use image::DynamicImage;
use std::path::Path;
use std::process::{Command, Stdio};
use vamp_protocol::Asset;

pub fn is_video(path: &Path, asset: Option<&Asset>) -> bool {
    asset.is_some_and(|asset| asset.original.mime.starts_with("video/"))
        || path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
            matches!(e.to_ascii_lowercase().as_str(), "mp4" | "m4v" | "mov" | "webm" | "mkv" | "avi")
        })
}

/// First displayed frame, autorotated by ffmpeg and bounded before image decoding.
/// ffmpeg is optional: lack of it is a real generation error, not a placeholder success.
pub fn poster(path: &Path) -> Result<(DynamicImage, (u32, u32))> {
    ensure!(path.is_file(), "video input is not a regular file: {}", path.display());
    let directory = tempfile::tempdir().context("creating video poster temporary directory")?;
    let frame = directory.path().join("poster.png");
    let ffmpeg = std::env::var_os("VAMP_FFMPEG").unwrap_or_else(|| "ffmpeg".into());
    let result = Command::new(ffmpeg)
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-threads", "1", "-protocol_whitelist", "file,pipe", "-i"])
        .arg(path)
        .args(["-map", "0:v:0", "-frames:v", "1", "-vf", "scale=w='min(4096,iw)':h='min(4096,ih)':force_original_aspect_ratio=decrease", "-threads", "1", "-update", "1"])
        .arg(&frame)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .context("video poster generation requires ffmpeg; install it on PATH or set VAMP_FFMPEG to its executable")?;
    ensure!(result.status.success(), "ffmpeg video poster failed: {}", String::from_utf8_lossy(&result.stderr).trim());
    super::decode_sized(&frame).context("decoding ffmpeg video poster")
}
