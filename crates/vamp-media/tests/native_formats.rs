//! Thumbnails/previews of HEIC, AVIF and PSD sources through the built-in decoders (every
//! platform but macOS, where sips converts them). Fixtures: tests/fixtures/generate.sh.
#![cfg(not(target_os = "macos"))]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use vamp_protocol::{ToolRequest, ToolResponse};

const RED: [u8; 3] = [220, 30, 30];
const GREEN: [u8; 3] = [30, 180, 60];
const BLUE: [u8; 3] = [40, 60, 220];
const YELLOW: [u8; 3] = [240, 220, 40];
const WHITE: [u8; 3] = [255, 255, 255];

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn run(request: &ToolRequest) -> ToolResponse {
    let mut child = Command::new(env!("CARGO_BIN_EXE_vamp-media"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(request).unwrap())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "vamp-media failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// Renders `name` at `max_edge` and returns the decoded JPEG plus the response.
fn generate(name: &str, max_edge: u32) -> (image::RgbImage, ToolResponse) {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.jpg");
    let response = run(&ToolRequest {
        input_path: Some(fixture(name).to_str().unwrap().into()),
        output_path: Some(output.to_str().unwrap().into()),
        max_edge: Some(max_edge),
        ..ToolRequest::default()
    });
    let rep = response.representation.as_ref().unwrap();
    assert_eq!(rep.mime, "image/jpeg");
    let jpeg = image::open(&output).unwrap().into_rgb8();
    assert_eq!((jpeg.width(), jpeg.height()), (rep.width, rep.height));
    (jpeg, response)
}

/// Pixel at a fraction of the image, within JPEG/chroma tolerance of `expected` (the
/// fixtures add a 0..24 brightness ramp from left to right).
fn assert_color(image: &image::RgbImage, fx: f32, fy: f32, expected: [u8; 3], what: &str) {
    let x = ((image.width() as f32 * fx) as u32).min(image.width() - 1);
    let y = ((image.height() as f32 * fy) as u32).min(image.height() - 1);
    let pixel = image.get_pixel(x, y).0;
    let close = pixel
        .iter()
        .zip(expected)
        .all(|(&got, want)| (i32::from(got) - i32::from(want)).abs() <= 40);
    assert!(
        close,
        "{what}: pixel at ({x},{y}) is {pixel:?}, expected about {expected:?}"
    );
}

fn assert_quadrants(image: &image::RgbImage, [tl, tr, bl, br]: [[u8; 3]; 4], what: &str) {
    assert_color(image, 0.25, 0.3, tl, what);
    assert_color(image, 0.75, 0.3, tr, what);
    assert_color(image, 0.25, 0.75, bl, what);
    assert_color(image, 0.75, 0.75, br, what);
}

const UPRIGHT: [[u8; 3]; 4] = [RED, GREEN, BLUE, YELLOW];

#[test]
fn heic_grid_thumbnail_and_preview() {
    // Thumbnail (512) and preview (1600) edges never upscale the 256x192 source.
    for edge in [512, 1600] {
        let (jpeg, response) = generate("grid.heic", edge);
        let rep = response.representation.unwrap();
        assert_eq!((rep.width, rep.height), (256, 192));
        assert_eq!(
            (rep.source_width, rep.source_height),
            (Some(256), Some(192))
        );
        assert_quadrants(&jpeg, UPRIGHT, "grid.heic");
    }
}

#[test]
fn heic_grid_downscales() {
    let (jpeg, response) = generate("grid.heic", 128);
    let rep = response.representation.unwrap();
    assert_eq!((rep.width, rep.height), (128, 96));
    assert_eq!(
        (rep.source_width, rep.source_height),
        (Some(256), Some(192))
    );
    assert_quadrants(&jpeg, UPRIGHT, "grid.heic at 128");
}

#[test]
fn heic_irot_rotates_display() {
    let (jpeg, response) = generate("rotated-grid.heic", 512);
    let rep = response.representation.unwrap();
    assert_eq!((rep.width, rep.height), (192, 256));
    assert_eq!(
        (rep.source_width, rep.source_height),
        (Some(192), Some(256))
    );
    // 90 degrees clockwise: the top-left red quadrant ends up top-right.
    assert_quadrants(&jpeg, [BLUE, RED, YELLOW, GREEN], "rotated-grid.heic");
}

#[test]
fn heic_10bit() {
    let (jpeg, response) = generate("10bit.heic", 128);
    let rep = response.representation.unwrap();
    assert_eq!((rep.width, rep.height), (128, 96));
    assert_eq!(
        (rep.source_width, rep.source_height),
        (Some(256), Some(192))
    );
    assert_quadrants(&jpeg, UPRIGHT, "10bit.heic");
}

#[test]
fn avif_grid() {
    for edge in [512, 1600] {
        let (jpeg, response) = generate("grid.avif", edge);
        let rep = response.representation.unwrap();
        assert_eq!((rep.width, rep.height), (256, 192));
        assert_quadrants(&jpeg, UPRIGHT, "grid.avif");
    }
}

#[test]
fn avif_10bit_alpha_composites_on_white() {
    let (jpeg, response) = generate("alpha-10bit.avif", 512);
    let rep = response.representation.unwrap();
    assert_eq!((rep.width, rep.height), (256, 192));
    assert_quadrants(&jpeg, UPRIGHT, "alpha-10bit.avif");
    assert_color(&jpeg, 0.25, 0.04, WHITE, "transparent band");
}

#[test]
fn psd_composites() {
    let (jpeg, response) = generate("rgba.psd", 512);
    let rep = response.representation.unwrap();
    assert_eq!((rep.width, rep.height), (64, 48));
    assert_quadrants(&jpeg, UPRIGHT, "rgba.psd");
    assert_color(&jpeg, 0.25, 0.04, WHITE, "transparent PSD band");

    let (jpeg, response) = generate("rgb16.psd", 512);
    let rep = response.representation.unwrap();
    assert_eq!((rep.width, rep.height), (64, 48));
    assert_quadrants(&jpeg, UPRIGHT, "rgb16.psd");
}

#[test]
fn heic_palette() {
    let response = run(&ToolRequest {
        operation: "palette".into(),
        input_path: Some(fixture("grid.heic").to_str().unwrap().into()),
        ..ToolRequest::default()
    });
    let palette = response.palette.unwrap();
    assert!(palette.len() >= 4, "{palette:?}");
}
