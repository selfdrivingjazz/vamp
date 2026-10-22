use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use vamp_protocol::{ToolRequest, ToolResponse};

fn generate(input: &Path, output: &Path, ffmpeg: &std::ffi::OsStr) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_vamp-media"))
        .env("VAMP_FFMPEG", ffmpeg).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let request = ToolRequest { operation: "loop".into(), input_path: Some(input.to_str().unwrap().into()), output_path: Some(output.to_str().unwrap().into()), ..ToolRequest::default() };
    child.stdin.take().unwrap().write_all(&serde_json::to_vec(&request).unwrap()).unwrap();
    child.wait_with_output().unwrap()
}
fn tool() -> Option<std::ffi::OsString> {
    let ffmpeg = std::env::var_os("VAMP_FFMPEG").unwrap_or_else(|| "ffmpeg".into());
    if Command::new(&ffmpeg).arg("-version").output().is_err() { eprintln!("optional ffmpeg unavailable"); None } else { Some(ffmpeg) }
}
fn check(input: &Path, output: &Path, ffmpeg: &std::ffi::OsStr) {
    let before = std::fs::read(input).unwrap();
    let result = generate(input, output, ffmpeg);
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let response: ToolResponse = serde_json::from_slice(&result.stdout).unwrap();
    let rep = response.representation.unwrap();
    assert_eq!(rep.mime, "video/mp4");
    assert!(rep.width <= 480 && rep.height <= 480 && rep.width % 2 == 0 && rep.height % 2 == 0);
    assert_eq!(rep.byte_size, std::fs::metadata(output).unwrap().len());
    assert_eq!(std::fs::read(input).unwrap(), before);
    let bytes = std::fs::read(output).unwrap();
    let mut offset = 0; let mut atoms = Vec::new();
    while offset + 8 <= bytes.len() {
        let size = u32::from_be_bytes(bytes[offset..offset+4].try_into().unwrap()) as usize;
        assert!(size >= 8); atoms.push(bytes[offset+4..offset+8].to_vec()); offset += size;
    }
    assert!(atoms.iter().position(|a| a == b"moov").unwrap() < atoms.iter().position(|a| a == b"mdat").unwrap());
}

#[test]
fn mp4_loop_is_small_faststart_muted_and_capped() {
    let Some(ffmpeg) = tool() else { return };
    let dir = tempfile::tempdir().unwrap(); let input = dir.path().join("input.mp4"); let output = dir.path().join("loop.mp4");
    let encoded = Command::new(&ffmpeg).args(["-nostdin", "-v", "error", "-f", "lavfi", "-i", "color=c=red:s=720x480:r=24", "-f", "lavfi", "-i", "sine=frequency=440", "-t", "8", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"]).arg(&input).output().unwrap();
    assert!(encoded.status.success(), "{}", String::from_utf8_lossy(&encoded.stderr));
    check(&input, &output, &ffmpeg);
    let probe = Path::new(&ffmpeg).with_file_name("ffprobe");
    if let Ok(result) = Command::new(probe).args(["-v", "error", "-show_streams", "-show_format", "-of", "json"]).arg(&output).output() {
        assert!(result.status.success());
        let info: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        let streams = info["streams"].as_array().unwrap(); assert_eq!(streams.len(), 1);
        assert_eq!(streams[0]["codec_name"], "h264"); assert_eq!(streams[0]["pix_fmt"], "yuv420p");
        assert_eq!(streams[0]["width"], 480); assert_eq!(streams[0]["height"], 320);
        assert!(info["format"]["duration"].as_str().unwrap().parse::<f64>().unwrap() <= 6.01);
    }
}

#[test]
fn animated_gif_and_webp_get_loops_static_images_do_not() {
    let Some(ffmpeg) = tool() else { return };
    let dir = tempfile::tempdir().unwrap();
    let gif = dir.path().join("animation.gif");
    let frames = [[255,0,0,255], [0,255,0,255]].into_iter().map(|color| image::Frame::from_parts(image::RgbaImage::from_pixel(120, 80, image::Rgba(color)), 0, 0, image::Delay::from_numer_denom_ms(100,1)));
    image::codecs::gif::GifEncoder::new(std::fs::File::create(&gif).unwrap()).encode_frames(frames).unwrap();
    check(&gif, &dir.path().join("gif-loop.mp4"), &ffmpeg);
    let webp = dir.path().join("animation.webp");
    let encoded = Command::new(&ffmpeg).args(["-nostdin", "-v", "error", "-i"]).arg(&gif).args(["-c:v", "libwebp_anim", "-loop", "0"]).arg(&webp).output().unwrap();
    assert!(encoded.status.success(), "{}", String::from_utf8_lossy(&encoded.stderr));
    check(&webp, &dir.path().join("webp-loop.mp4"), &ffmpeg);
    let static_webp = dir.path().join("still.webp");
    image::RgbImage::from_pixel(32,32,image::Rgb([10u8,20,30])).save(&static_webp).unwrap();
    let target = dir.path().join("static-loop.mp4");
    let rejected = generate(&static_webp,&target,&ffmpeg);
    assert!(!rejected.status.success()); assert!(!target.exists());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("static images do not have loop previews"));
}
