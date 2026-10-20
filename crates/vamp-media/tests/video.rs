use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use vamp_protocol::{ToolRequest, ToolResponse};

fn generate(input: &Path, output: &Path, ffmpeg: &std::ffi::OsStr, edge: u32) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_vamp-media"))
        .env("VAMP_FFMPEG", ffmpeg)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().unwrap();
    let request = ToolRequest {
        input_path: Some(input.to_str().unwrap().into()),
        output_path: Some(output.to_str().unwrap().into()),
        max_edge: Some(edge),
        ..ToolRequest::default()
    };
    child.stdin.take().unwrap().write_all(&serde_json::to_vec(&request).unwrap()).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn missing_ffmpeg_is_a_truthful_generation_error() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("video.mp4");
    let output = dir.path().join("poster.jpg");
    std::fs::write(&input, b"video").unwrap();
    let result = generate(&input, &output, dir.path().join("missing-ffmpeg").as_os_str(), 512);
    assert!(!result.status.success());
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(error.contains("video poster generation requires ffmpeg"), "{error}");
    assert!(!output.exists());
}

#[test]
fn video_thumbnail_preview_and_corrupt_input() {
    let ffmpeg = std::env::var_os("VAMP_FFMPEG").unwrap_or_else(|| "ffmpeg".into());
    if Command::new(&ffmpeg).arg("-version").output().is_err() {
        eprintln!("skipping optional ffmpeg integration: tool unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("video.mp4");
    let encoded = Command::new(&ffmpeg)
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "color=c=red:s=720x720:r=24", "-frames:v", "2", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-movflags", "+faststart"])
        .arg(&input).output().unwrap();
    assert!(encoded.status.success(), "{}", String::from_utf8_lossy(&encoded.stderr));
    let original = std::fs::read(&input).unwrap();
    for (edge, expected) in [(512, 512), (1600, 720)] {
        let output = dir.path().join(format!("poster-{edge}.jpg"));
        let generated = generate(&input, &output, &ffmpeg, edge);
        assert!(generated.status.success(), "{}", String::from_utf8_lossy(&generated.stderr));
        let response: ToolResponse = serde_json::from_slice(&generated.stdout).unwrap();
        let rep = response.representation.unwrap();
        assert_eq!((rep.width, rep.height), (expected, expected));
        assert_eq!((rep.source_width, rep.source_height), (Some(720), Some(720)));
        assert_eq!(rep.mime, "image/jpeg");
        let image = image::open(output).unwrap().into_rgb8();
        let pixel = image.get_pixel(10, 10).0;
        assert!(pixel[0] > 220 && pixel[1] < 30 && pixel[2] < 30, "{pixel:?}");
    }
    assert_eq!(std::fs::read(&input).unwrap(), original, "original must remain byte-identical");
    std::fs::write(&input, b"not a video").unwrap();
    let output = dir.path().join("corrupt.jpg");
    let corrupt = generate(&input, &output, &ffmpeg, 512);
    assert!(!corrupt.status.success());
    assert!(String::from_utf8_lossy(&corrupt.stderr).contains("ffmpeg video poster failed"));
    assert!(!output.exists());
}
