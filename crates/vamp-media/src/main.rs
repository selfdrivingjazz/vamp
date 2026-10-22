use anyhow::{Context, Result, bail, ensure};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, Rgb, RgbImage};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use vamp_protocol::{
    ColorSwatch, ErrorDetail, ErrorResponse, PROTOCOL_VERSION, RepresentationOutput, SheetTile,
    ToolRequest, ToolResponse,
};

#[cfg(not(target_os = "macos"))]
mod native;
mod video;

const MAX_REQUEST: u64 = 1024 * 1024;
const MAX_SOURCE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_PIXELS: u64 = 32 * 1024 * 1024;
const MAX_DIMENSION: u32 = 32768;
const MAX_EDGE: u32 = 4096;
const MAX_ITEMS: usize = 256;
const MAX_SHEET_EDGE: u32 = 8192;
const LABEL_HEIGHT: u32 = 24;

fn main() {
    if let Err(error) = run() {
        let response = ErrorResponse {
            protocol_version: PROTOCOL_VERSION.into(),
            error: ErrorDetail {
                code: "media_error".into(),
                message: format!("{error:#}"),
            },
        };
        let _ = serde_json::to_writer(std::io::stderr().lock(), &response);
        eprintln!();
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() == 1 && (args[0] == "--help" || args[0] == "-h") {
        println!(
            "vamp-media {}\nOptional, standalone VAMP image processor.\n\nUSAGE: vamp-media < request.json\n       vamp-media --help | --version\n\nReads one ToolRequest JSON object on stdin and emits one ToolResponse JSON object.\nProtocol version: {}\nOperations:\n  generate: input_path, output_path, max_edge (default 512, 1..4096)\n  sheet: items [{{id,path}}], output_path, columns (default 4), cell_size (default 256, 32..1024)\n  palette: input_path; returns up to 8 dominant colors {{hex,ratio}}, largest first (no output_path)\n\nJPEG, PNG, WebP, GIF, TIFF, BMP; first frame/page only. Applies decoder-supported EXIF orientation.\nHEIC/HEIF, AVIF, PSD: macOS converts them (and other ImageIO formats) with sips; elsewhere\nbuilt-in decoders read HEIC/HEIF, AVIF and PSD/PSB composites up to 96 megapixels.\nConverted sources are downscaled to 4096 pixels before rendering.\nOutputs JPEG with white transparency, no upscaling or cropping. Numbered sheet tiles preserve input order.\nOutput parent must exist; output must not exist. Originals are never modified.\nBounds: 1 MiB request, 128 MiB source, 32 megapixels, 256 MiB decoder allocation budget,\n32768-pixel source edges; 256 sheet items, 8192-pixel sheet edges and 32 megapixels.\nDecoder allocation budgets are best-effort; dimension and pixel limits are enforced.\nErrors are JSON on stderr with nonzero exit status.",
            env!("CARGO_PKG_VERSION"),
            PROTOCOL_VERSION
        );
        println!("Video posters: first displayed frame via optional ffmpeg on PATH (or VAMP_FFMPEG).\nVideo sources are streamed, not subject to the image source-byte limit; extracted frames\nare bounded to 4096px before normal image limits. Missing ffmpeg is a generation error.");
        println!("  loop: input_path, output_path; H.264 MP4, muted, <=480px, <=6s, faststart,\n        24fps, CRF28, max1Mbps; videos and animated GIF/WebP only.");
        return Ok(());
    }
    if args.len() == 1 && (args[0] == "--version" || args[0] == "-V") {
        println!("vamp-media {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    ensure!(args.is_empty(), "unexpected arguments; use --help");
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .take(MAX_REQUEST + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= MAX_REQUEST, "request exceeds 1 MiB");
    let request: ToolRequest =
        serde_json::from_slice(&bytes).context("invalid ToolRequest JSON")?;
    ensure!(
        request.protocol_version == PROTOCOL_VERSION,
        "unsupported protocol version {:?}",
        request.protocol_version
    );
    if request.operation == "palette" {
        let input = request
            .input_path
            .as_deref()
            .context("input_path is required")?;
        let response = ToolResponse {
            representation: None,
            observation: None,
            tiles: None,
            palette: Some(palette(&decode(Path::new(input))?)),
        };
        let mut stdout = std::io::stdout().lock();
        serde_json::to_writer(&mut stdout, &response)?;
        writeln!(stdout)?;
        return Ok(());
    }
    let output = Path::new(
        request
            .output_path
            .as_deref()
            .context("output_path is required")?,
    );
    ensure!(!output.as_os_str().is_empty(), "output_path is empty");
    ensure!(
        fs::symlink_metadata(output).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "output already exists or cannot be inspected"
    );
    if request.operation == "loop" {
        let input = Path::new(request.input_path.as_deref().context("input_path is required")?);
        let representation = video::loop_preview(input, output, request.asset.as_ref())?;
        serde_json::to_writer(std::io::stdout().lock(), &ToolResponse {
            representation: Some(representation), observation: None, tiles: None, palette: None,
        })?;
        return Ok(());
    }
    let (image, tiles, source) = match request.operation.as_str() {
        "generate" => {
            let edge = request.max_edge.unwrap_or(512);
            ensure!(
                (1..=MAX_EDGE).contains(&edge),
                "max_edge must be 1..={MAX_EDGE}"
            );
            let input = request
                .input_path
                .as_deref()
                .context("input_path is required")?;
            let input = Path::new(input);
            let (decoded, source) = if video::is_video(input, request.asset.as_ref()) {
                video::poster(input)?
            } else {
                decode_sized(input)?
            };
            let source = Some(source);
            (fit_white(decoded, edge, edge), None, source)
        }
        "sheet" => {
            let (image, tiles) = sheet(&request)?;
            (image, Some(tiles), None)
        }
        other => bail!("unsupported operation {other:?}; expected generate, sheet or palette"),
    };
    let byte_size = publish_jpeg(output, &image)?;
    let response = ToolResponse {
        representation: Some(RepresentationOutput {
            path: output.to_str().context("output path is not UTF-8")?.into(),
            width: image.width(),
            height: image.height(),
            mime: "image/jpeg".into(),
            byte_size,
            source_width: source.map(|s| s.0),
            source_height: source.map(|s| s.1),
        }),
        observation: None,
        tiles,
        palette: None,
    };
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, &response)?;
    writeln!(stdout)?;
    Ok(())
}

const PALETTE_SIZE: usize = 8;
/// Squared RGB distance under which two dominant colors are merged.
const PALETTE_MERGE_DISTANCE: u32 = 40 * 40;

/// Deterministic dominant colors: 4-bit-per-channel buckets, near-duplicates merged,
/// largest first, ratios over the kept swatches summing to 1. Transparent pixels are ignored.
fn palette(image: &DynamicImage) -> Vec<ColorSwatch> {
    // Per bucket: count and channel sums, over pixels at most 64px on the long edge.
    let small = if image.width().max(image.height()) > 64 {
        image.thumbnail(64, 64).to_rgba8()
    } else {
        image.to_rgba8()
    };
    let mut buckets = vec![[0u64; 4]; 4096];
    for pixel in small.pixels() {
        let [r, g, b, a] = pixel.0;
        if a < 128 {
            continue;
        }
        let index = (usize::from(r >> 4) << 8) | (usize::from(g >> 4) << 4) | usize::from(b >> 4);
        let bucket = &mut buckets[index];
        bucket[0] += 1;
        bucket[1] += u64::from(r);
        bucket[2] += u64::from(g);
        bucket[3] += u64::from(b);
    }
    let mut ordered: Vec<(usize, [u64; 4])> = buckets
        .into_iter()
        .enumerate()
        .filter(|(_, b)| b[0] > 0)
        .collect();
    ordered.sort_by(|a, b| b.1[0].cmp(&a.1[0]).then(a.0.cmp(&b.0)));
    let mean = |b: &[u64; 4]| [1, 2, 3].map(|c| (b[c] / b[0]) as u32);
    let mut merged: Vec<[u64; 4]> = Vec::new();
    for (_, bucket) in ordered {
        let color = mean(&bucket);
        let near = merged.iter_mut().find(|m| {
            let other = mean(m);
            (0..3)
                .map(|c| color[c].abs_diff(other[c]).pow(2))
                .sum::<u32>()
                < PALETTE_MERGE_DISTANCE
        });
        match near {
            Some(m) => (0..4).for_each(|c| m[c] += bucket[c]),
            None => merged.push(bucket),
        }
    }
    merged.sort_by(|a, b| b[0].cmp(&a[0]));
    merged.truncate(PALETTE_SIZE);
    let total: u64 = merged.iter().map(|b| b[0]).sum();
    merged
        .iter()
        .map(|b| {
            let [r, g, bl] = mean(b);
            ColorSwatch {
                hex: format!("#{r:02x}{g:02x}{bl:02x}"),
                ratio: b[0] as f64 / total as f64,
            }
        })
        .collect()
}

fn decode(path: &Path) -> Result<DynamicImage> {
    decode_sized(path).map(|(image, _)| image)
}

/// The decoded image and the source size to record. Converted formats arrive downscaled to
/// MAX_EDGE; the pure-Rust converters still report the full (oriented) source size.
fn decode_sized(path: &Path) -> Result<(DynamicImage, (u32, u32))> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file(),
        "input is not a regular file: {}",
        path.display()
    );
    ensure!(
        metadata.len() <= MAX_SOURCE_BYTES,
        "source exceeds 128 MiB: {}",
        path.display()
    );
    let mut reader = ImageReader::new(BufReader::new(file))
        .with_guessed_format()
        .context("detecting image format")?;
    let supported = matches!(
        reader.format(),
        Some(
            ImageFormat::Jpeg
                | ImageFormat::Png
                | ImageFormat::WebP
                | ImageFormat::Gif
                | ImageFormat::Tiff
                | ImageFormat::Bmp
        )
    );
    #[cfg(target_os = "macos")]
    if !supported {
        let image = decode_with_sips(path)?;
        let size = (image.width(), image.height());
        return Ok((image, size));
    }
    #[cfg(not(target_os = "macos"))]
    if !supported {
        let mut data = Vec::new();
        reader.into_inner().read_to_end(&mut data)?;
        let image =
            native::decode(&data).with_context(|| format!("decoding {}", path.display()))?;
        let size = (image.width(), image.height());
        let image = if size.0.max(size.1) > MAX_EDGE {
            image.thumbnail(MAX_EDGE, MAX_EDGE)
        } else {
            image
        };
        return Ok((image, size));
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    let mut decoder = reader
        .into_decoder()
        .with_context(|| format!("reading image header {}", path.display()))?;
    let (width, height) = decoder.dimensions();
    ensure!(
        width > 0 && height > 0 && u64::from(width) * u64::from(height) <= MAX_PIXELS,
        "image dimensions exceed pixel limit"
    );
    ensure!(
        decoder.total_bytes() <= MAX_DECODE_BYTES,
        "decoded image exceeds allocation budget"
    );
    let orientation = decoder.orientation().context("reading image orientation")?;
    let mut image = DynamicImage::from_decoder(decoder)
        .with_context(|| format!("decoding {}", path.display()))?;
    image.apply_orientation(orientation);
    let size = (image.width(), image.height());
    Ok((image, size))
}

/// HEIC/AVIF/PSD and other ImageIO formats via the system `sips`; oversized sources
/// are downscaled to MAX_EDGE during conversion, smaller ones are never upscaled.
#[cfg(target_os = "macos")]
fn decode_with_sips(path: &Path) -> Result<DynamicImage> {
    use std::process::{Command, Stdio};
    let probe = Command::new("/usr/bin/sips")
        .args(["-g", "pixelWidth", "-g", "pixelHeight"])
        .arg(path)
        .stderr(Stdio::null())
        .output()
        .context("running sips")?;
    let edge = String::from_utf8_lossy(&probe.stdout)
        .lines()
        .filter_map(|line| line.trim().strip_prefix("pixel"))
        .filter_map(|rest| rest.split(':').nth(1)?.trim().parse::<u32>().ok())
        .max()
        .with_context(|| format!("unsupported image format: {}", path.display()))?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let temp = std::env::temp_dir().join(format!("vamp-media-{}-{nonce}.png", std::process::id()));
    let mut command = Command::new("/usr/bin/sips");
    command.args(["-s", "format", "png"]);
    if edge > MAX_EDGE {
        command
            .arg("--resampleHeightWidthMax")
            .arg(MAX_EDGE.to_string());
    }
    let status = command
        .arg(path)
        .arg("--out")
        .arg(&temp)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("running sips")?;
    let image = if status.success() && temp.is_file() {
        decode(&temp)
    } else {
        Err(anyhow::anyhow!(
            "unsupported image format: {}",
            path.display()
        ))
    };
    let _ = fs::remove_file(&temp);
    image
}

fn fit_white(image: DynamicImage, width: u32, height: u32) -> RgbImage {
    // Composite before resizing so hidden RGB in transparent pixels cannot create dark fringes.
    let rgb = if image.color().has_alpha() {
        let mut rgb = RgbImage::new(image.width(), image.height());
        use image::GenericImageView;
        for (x, y, pixel) in image.pixels() {
            let alpha = u32::from(pixel[3]);
            let mut channels = [0; 3];
            for channel in 0..3 {
                channels[channel] =
                    ((u32::from(pixel[channel]) * alpha + 255 * (255 - alpha) + 127) / 255) as u8;
            }
            rgb.put_pixel(x, y, Rgb(channels));
        }
        drop(image);
        rgb
    } else {
        image.into_rgb8()
    };
    if rgb.width() <= width && rgb.height() <= height {
        rgb
    } else {
        DynamicImage::ImageRgb8(rgb)
            .thumbnail(width, height)
            .into_rgb8()
    }
}

fn sheet(request: &ToolRequest) -> Result<(RgbImage, Vec<SheetTile>)> {
    ensure!(
        !request.items.is_empty() && request.items.len() <= MAX_ITEMS,
        "sheet requires 1..={MAX_ITEMS} items"
    );
    let columns = request.columns.unwrap_or(4);
    let cell = request.cell_size.unwrap_or(256);
    ensure!(
        (1..=MAX_ITEMS as u32).contains(&columns),
        "columns must be 1..={MAX_ITEMS}"
    );
    ensure!((32..=1024).contains(&cell), "cell_size must be 32..=1024");
    let rows = (request.items.len() as u32).div_ceil(columns);
    let width = columns * cell;
    let height = rows * cell;
    ensure!(
        width <= MAX_SHEET_EDGE
            && height <= MAX_SHEET_EDGE
            && u64::from(width) * u64::from(height) <= MAX_PIXELS,
        "sheet dimensions exceed limits"
    );
    let mut canvas = RgbImage::from_pixel(width, height, Rgb([255; 3]));
    let mut tiles = Vec::with_capacity(request.items.len());
    for (index, item) in request.items.iter().enumerate() {
        ensure!(
            !item.id.is_empty(),
            "sheet item {} has an empty id",
            index + 1
        );
        let number = index as u32 + 1;
        let x = index as u32 % columns * cell;
        let y = index as u32 / columns * cell;
        let image = fit_white(
            decode(Path::new(&item.path))
                .with_context(|| format!("sheet item {number} ({})", item.id))?,
            cell - 8,
            cell - LABEL_HEIGHT - 4,
        );
        let image_x = x + (cell - image.width()) / 2;
        let image_y = y + (cell - LABEL_HEIGHT - image.height()) / 2;
        image::imageops::replace(&mut canvas, &image, i64::from(image_x), i64::from(image_y));
        label(&mut canvas, number, x + 4, y + cell - LABEL_HEIGHT + 4);
        tiles.push(SheetTile {
            number,
            asset_id: item.id.clone(),
            x,
            y,
            width: cell,
            height: cell,
        });
    }
    Ok((canvas, tiles))
}

fn label(canvas: &mut RgbImage, number: u32, x: u32, y: u32) {
    const DIGITS: [[u8; 5]; 10] = [
        [7, 5, 5, 5, 7],
        [2, 6, 2, 2, 7],
        [7, 1, 7, 4, 7],
        [7, 1, 7, 1, 7],
        [5, 5, 7, 1, 1],
        [7, 4, 7, 1, 7],
        [7, 4, 7, 5, 7],
        [7, 1, 1, 1, 1],
        [7, 5, 7, 5, 7],
        [7, 5, 7, 1, 7],
    ];
    for (offset, digit) in number.to_string().bytes().enumerate() {
        for (row, bits) in DIGITS[usize::from(digit - b'0')].iter().enumerate() {
            for column in 0..3 {
                if bits & (1 << (2 - column)) != 0 {
                    for dy in 0..2 {
                        for dx in 0..2 {
                            canvas.put_pixel(
                                x + offset as u32 * 8 + column * 2 + dx,
                                y + row as u32 * 2 + dy,
                                Rgb([24; 3]),
                            );
                        }
                    }
                }
            }
        }
    }
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn publish_jpeg(output: &Path, image: &RgbImage) -> Result<u64> {
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let mut temporary = None;
    for attempt in 0..32 {
        let path = parent.join(format!(
            ".vamp-media-{}-{nonce}-{attempt}.tmp",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                temporary = Some((Temporary(path), file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("creating temporary JPEG in output directory"),
        }
    }
    let (temporary, file) = temporary.context("unable to allocate temporary output filename")?;
    let mut writer = BufWriter::new(file);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, 88)
        .encode(
            image.as_raw(),
            image.width(),
            image.height(),
            image::ExtendedColorType::Rgb8,
        )
        .context("encoding JPEG")?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    let bytes = writer.get_ref().metadata()?.len();
    drop(writer);
    // Unlike rename, hard_link never replaces an existing destination, including a source alias.
    fs::hard_link(&temporary.0, output).with_context(|| format!("publishing new JPEG {}; destination must not exist and filesystem must support hard links", output.display()))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solid_red_is_one_dominant_swatch() {
        let image = DynamicImage::ImageRgb8(RgbImage::from_pixel(32, 32, Rgb([255, 0, 0])));
        let swatches = palette(&image);
        assert_eq!(swatches[0].hex, "#ff0000");
        assert!(swatches[0].ratio > 0.9);
    }

    #[test]
    fn half_red_half_blue_splits_evenly() {
        let image = DynamicImage::ImageRgb8(RgbImage::from_fn(32, 32, |x, _| {
            if x < 16 {
                Rgb([255, 0, 0])
            } else {
                Rgb([0, 0, 255])
            }
        }));
        let swatches = palette(&image);
        assert_eq!(swatches.len(), 2);
        let mut hexes: Vec<_> = swatches.iter().map(|s| s.hex.as_str()).collect();
        hexes.sort();
        assert_eq!(hexes, ["#0000ff", "#ff0000"]);
        for swatch in &swatches {
            assert!((swatch.ratio - 0.5).abs() < 0.05, "{}", swatch.ratio);
        }
    }

    #[test]
    fn near_duplicate_shades_merge() {
        let image = DynamicImage::ImageRgb8(RgbImage::from_fn(32, 32, |x, _| {
            if x % 2 == 0 {
                Rgb([200, 10, 10])
            } else {
                Rgb([210, 18, 12])
            }
        }));
        assert_eq!(palette(&image).len(), 1);
    }
}
