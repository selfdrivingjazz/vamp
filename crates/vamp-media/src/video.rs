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

/// Animation detection is bounded and avoids decoding static WebP pixels.
fn animated_image(path: &Path, asset: Option<&Asset>) -> Result<bool> {
    use image::{AnimationDecoder, ImageDecoder};
    let reader = std::io::BufReader::new(std::fs::File::open(path)?);
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let mime = asset.map(|a| a.original.mime.as_str()).unwrap_or("");
    if ext == "webp" || mime == "image/webp" {
        return Ok(image::codecs::webp::WebPDecoder::new(reader)?.has_animation());
    }
    if ext == "gif" || mime == "image/gif" {
        let mut decoder = image::codecs::gif::GifDecoder::new(reader)?;
        let mut limits = image::Limits::default();
        limits.max_alloc = Some(super::MAX_DECODE_BYTES);
        limits.max_image_width = Some(super::MAX_DIMENSION);
        limits.max_image_height = Some(super::MAX_DIMENSION);
        decoder.set_limits(limits)?;
        let mut frames = decoder.into_frames();
        if let Some(frame) = frames.next() { frame?; }
        return frames.next().transpose().map(|frame| frame.is_some()).map_err(Into::into);
    }
    Ok(false)
}

/// Small, independently streamable grid loop. A static image is not a loop source.
pub fn loop_preview(path: &Path, output: &Path, asset: Option<&Asset>) -> Result<vamp_protocol::RepresentationOutput> {
    ensure!(path.is_file(), "loop input is not a regular file: {}", path.display());
    ensure!(is_video(path, asset) || animated_image(path, asset)?, "static images do not have loop previews");
    let temp = tempfile::Builder::new().prefix(".vamp-loop-").suffix(".mp4")
        .tempfile_in(output.parent().context("output parent is required")?)?;
    let ffmpeg = std::env::var_os("VAMP_FFMPEG").unwrap_or_else(|| "ffmpeg".into());
    // ffmpeg's native WebP decoder does not support animation in many releases.
    // Decode those frames with the existing bounded Rust decoder and stream RGBA;
    // this preserves timing/colors without a lossy GIF intermediate or full-loop allocation.
    let webp = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("webp"))
        || asset.is_some_and(|a| a.original.mime == "image/webp");
    let mut decoder = if webp {
        use image::ImageDecoder;
        ensure!(std::fs::metadata(path)?.len() <= super::MAX_SOURCE_BYTES, "WebP source exceeds 128 MiB");
        let mut decoder = image::codecs::webp::WebPDecoder::new(std::io::BufReader::new(std::fs::File::open(path)?))?;
        let (w, h) = decoder.dimensions();
        ensure!(u64::from(w) * u64::from(h) <= super::MAX_PIXELS, "WebP dimensions exceed pixel limit");
        let mut limits = image::Limits::default();
        limits.max_alloc = Some(super::MAX_DECODE_BYTES);
        limits.max_image_width = Some(super::MAX_DIMENSION);
        limits.max_image_height = Some(super::MAX_DIMENSION);
        decoder.set_limits(limits)?;
        Some(decoder)
    } else { None };
    let mut command = Command::new(ffmpeg);
    command.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-threads", "1", "-protocol_whitelist", "file,pipe"]);
    if let Some(decoder) = &decoder {
        use image::ImageDecoder;
        let (w, h) = decoder.dimensions();
        command.args(["-f", "rawvideo", "-pixel_format", "rgba", "-video_size", &format!("{w}x{h}"), "-framerate", "24", "-i", "pipe:0"]);
        command.stdin(Stdio::piped());
    } else { command.arg("-i").arg(path).stdin(Stdio::null()); }
    command.args(["-map", "0:v:0", "-an", "-t", "6", "-vf",
            "scale=w='min(480,iw)':h='min(480,ih)':force_original_aspect_ratio=decrease:force_divisible_by=2,setsar=1",
            "-r", "24", "-c:v", "libx264", "-profile:v", "main", "-pix_fmt", "yuv420p",
            "-preset", "veryfast", "-crf", "28", "-maxrate", "1M", "-bufsize", "1M",
            "-g", "24", "-keyint_min", "24", "-sc_threshold", "0", "-threads", "2",
            "-movflags", "+faststart", "-map_metadata", "-1", "-f", "mp4"])
        .arg(temp.path()).stdout(Stdio::null()).stderr(Stdio::piped());
    let result = if let Some(decoder) = decoder.take() {
        use image::AnimationDecoder;
        use std::io::Write;
        let mut child = command.spawn().context("video loop generation requires ffmpeg; install it on PATH or set VAMP_FFMPEG to its executable")?;
        let fed = (|| -> Result<()> {
            let mut stdin = child.stdin.take().context("ffmpeg stdin unavailable")?;
            let mut elapsed_ms = 0.0f64;
            let mut written = 0u32;
            for frame in decoder.into_frames().take(600) {
                let frame = frame?;
                let (numer, denom) = frame.delay().numer_denom_ms();
                elapsed_ms = (elapsed_ms + (f64::from(numer) / f64::from(denom)).max(10.0)).min(6000.0);
                let desired = ((elapsed_ms * 24.0 / 1000.0).ceil() as u32).min(144);
                for _ in written..desired { stdin.write_all(frame.buffer().as_raw())?; }
                written = desired;
                if written >= 144 { break; }
            }
            Ok(())
        })();
        if fed.is_err() { let _ = child.kill(); }
        let output = child.wait_with_output()?;
        fed.context("feeding animated WebP frames to ffmpeg")?;
        output
    } else {
        command.output().context("video loop generation requires ffmpeg; install it on PATH or set VAMP_FFMPEG to its executable")?
    };
    ensure!(result.status.success(), "ffmpeg video loop failed: {}", String::from_utf8_lossy(&result.stderr).trim());
    let (width, height) = mp4_dimensions(temp.path())?;
    let byte_size = temp.as_file().metadata()?.len();
    temp.persist_noclobber(output).context("publishing video loop")?;
    Ok(vamp_protocol::RepresentationOutput {
        path: output.to_str().context("output path is not UTF-8")?.into(),
        width, height, mime: "video/mp4".into(), byte_size,
        source_width: None, source_height: None,
    })
}

/// Read the generated MP4's track dimensions without a second decoder/tool invocation.
fn mp4_dimensions(path: &Path) -> Result<(u32, u32)> {
    use std::io::{Read, Seek, SeekFrom};
    fn track(file: &mut std::fs::File, end: u64) -> Result<Option<(u32, u32)>> {
        while file.stream_position()? + 8 <= end {
            let start = file.stream_position()?;
            let mut header = [0; 8];
            file.read_exact(&mut header)?;
            let mut size = u64::from(u32::from_be_bytes(header[..4].try_into()?));
            let header_len = if size == 1 {
                let mut extended = [0; 8]; file.read_exact(&mut extended)?;
                size = u64::from_be_bytes(extended); 16
            } else { 8 };
            ensure!(size >= header_len && size <= end - start, "invalid generated MP4 atom");
            match &header[4..] {
                b"moov" | b"trak" => if let Some(size) = track(file, start + size)? { return Ok(Some(size)); },
                b"tkhd" => {
                    ensure!(size >= header_len + 8, "invalid generated MP4 track");
                    file.seek(SeekFrom::Start(start + size - 8))?;
                    let mut dims = [0; 8]; file.read_exact(&mut dims)?;
                    let w = u32::from_be_bytes(dims[..4].try_into()?) >> 16;
                    let h = u32::from_be_bytes(dims[4..].try_into()?) >> 16;
                    if w > 0 && h > 0 { return Ok(Some((w, h))); }
                }
                _ => {}
            }
            file.seek(SeekFrom::Start(start + size))?;
        }
        Ok(None)
    }
    let mut file = std::fs::File::open(path)?;
    let end = file.metadata()?.len();
    track(&mut file, end)?.context("generated MP4 has no video dimensions")
}
