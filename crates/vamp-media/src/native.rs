//! Pure-Rust decoders for formats the image crate cannot read, used where there is no
//! system converter (everything but macOS, which keeps `sips`): HEIC/HEIF through
//! heif-oxide (HEVC via rust_h265: grid tiles, 10-bit, irot/imir/clap, Display P3 to sRGB),
//! AVIF through avif-rust (AV1: grid, alpha, 10/12-bit, irot/imir/clap) and the merged
//! composite of PSD/PSB files. Both decoders are MIT/Apache, no C, no build tools.

use anyhow::{Context, Result, anyhow, bail, ensure};
use image::{DynamicImage, ImageBuffer};

/// Declared-size cap for these sources (48 MP iPhone photos and 63 MP panoramas fit); the
/// decoded image is downscaled to MAX_EDGE right after decoding, like the sips path.
const MAX_PIXELS: u64 = 96 * 1024 * 1024;

pub fn decode(data: &[u8]) -> Result<DynamicImage> {
    if data.starts_with(b"8BPS") {
        return psd(data);
    }
    let brands = ftyp_brands(data).context("unsupported image format")?;
    check_extents(data)?;
    if brands.iter().any(|&b| matches!(b, b"avif" | b"avis")) {
        let image = avif_rust::image_from_bytes(data).map_err(|e| anyhow!("decoding AVIF: {e}"))?;
        let (width, height) = (u32::try_from(image.width)?, u32::try_from(image.height)?);
        return ImageBuffer::from_raw(width, height, image.rgba)
            .map(DynamicImage::ImageRgba8)
            .context("AVIF decoder returned a short buffer");
    }
    if brands.iter().any(|&b| {
        matches!(
            b,
            b"heic" | b"heix" | b"heim" | b"heis" | b"hevc" | b"hevx" | b"mif1" | b"msf1"
        )
    }) {
        use heif_oxide::Pixels;
        let image = heif_oxide::decode_bytes(data).map_err(|e| anyhow!("decoding HEIF: {e}"))?;
        let (w, h) = (image.width, image.height);
        let decoded = match image.pixels {
            Pixels::Rgb8(v) => ImageBuffer::from_raw(w, h, v).map(DynamicImage::ImageRgb8),
            Pixels::Rgba8(v) => ImageBuffer::from_raw(w, h, v).map(DynamicImage::ImageRgba8),
            Pixels::Rgb16(v) => ImageBuffer::from_raw(w, h, v).map(DynamicImage::ImageRgb16),
            Pixels::Rgba16(v) => ImageBuffer::from_raw(w, h, v).map(DynamicImage::ImageRgba16),
        };
        return decoded.context("HEIF decoder returned a short buffer");
    }
    bail!("unsupported image format")
}

fn ensure_size(width: u64, height: u64) -> Result<()> {
    ensure!(
        (1..=u64::from(crate::MAX_DIMENSION)).contains(&width)
            && (1..=u64::from(crate::MAX_DIMENSION)).contains(&height)
            && width * height <= MAX_PIXELS,
        "image dimensions exceed pixel limit"
    );
    Ok(())
}

/// ISOBMFF child boxes as (type, payload); a malformed size ends the list with an error.
fn boxes(mut data: &[u8]) -> Result<Vec<([u8; 4], &[u8])>> {
    let mut out = Vec::new();
    while data.len() >= 8 {
        let kind: [u8; 4] = data[4..8].try_into()?;
        let (header, size) = match u32::from_be_bytes(data[..4].try_into()?) {
            0 => (8, data.len() as u64),
            1 => (
                16,
                u64::from_be_bytes(data.get(8..16).context("truncated box")?.try_into()?),
            ),
            size => (8, u64::from(size)),
        };
        ensure!(
            size >= header && size <= data.len() as u64,
            "malformed ISOBMFF box"
        );
        out.push((kind, &data[header as usize..size as usize]));
        data = &data[size as usize..];
    }
    Ok(out)
}

/// Major and compatible brands when the file starts with an ISOBMFF `ftyp` box.
fn ftyp_brands(data: &[u8]) -> Option<Vec<&[u8; 4]>> {
    let size = u32::from_be_bytes(data.get(..4)?.try_into().ok()?) as usize;
    if data.get(4..8)? != b"ftyp" || size < 16 {
        return None;
    }
    let (words, _) = data.get(8..size)?.as_chunks::<4>();
    // Major brand, minor version (skipped), compatible brands.
    Some(
        words
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 1)
            .map(|(_, b)| b)
            .collect(),
    )
}

fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Result<Option<&'a [u8]>> {
    Ok(boxes(data)?
        .into_iter()
        .find(|(k, _)| k == kind)
        .map(|(_, body)| body))
}

/// Every image item's declared extent (`meta/iprp/ipco/ispe`) must be within limits before a
/// decoder allocates for it.
fn check_extents(data: &[u8]) -> Result<()> {
    let meta = child(data, b"meta")?.context("HEIF file has no meta box")?;
    let ipco = child(meta.get(4..).context("truncated meta box")?, b"iprp")?
        .map(|iprp| child(iprp, b"ipco"))
        .transpose()?
        .flatten()
        .context("HEIF file has no item properties")?;
    let mut seen = false;
    for (kind, body) in boxes(ipco)? {
        if &kind == b"ispe" {
            let field = |at: usize| -> Result<u64> {
                let bytes = body.get(at..at + 4).context("truncated ispe box")?;
                Ok(u64::from(u32::from_be_bytes(bytes.try_into()?)))
            };
            ensure_size(field(4)?, field(8)?)?;
            seen = true;
        }
    }
    ensure!(seen, "HEIF file declares no image size");
    Ok(())
}

/// The merged composite of a PSD/PSB (8/16-bit RGB, grayscale or CMYK; raw or RLE). Extra
/// channels are spot channels or saved selections unless the layer count is negative, which
/// marks the first one as the composite's transparency.
fn psd(data: &[u8]) -> Result<DynamicImage> {
    let mut r = Reader(data);
    r.take(4)?;
    let psb = match r.u16()? {
        1 => false,
        2 => true,
        version => bail!("unsupported PSD version {version}"),
    };
    r.take(6)?;
    let channels = usize::from(r.u16()?);
    let height = r.u32()?;
    let width = r.u32()?;
    let depth = r.u16()?;
    let mode = r.u16()?;
    ensure_size(u64::from(width), u64::from(height))?;
    ensure!(
        depth == 8 || depth == 16,
        "unsupported PSD bit depth {depth}"
    );
    let color = match mode {
        1 => 1,
        3 => 3,
        4 => 4,
        _ => bail!("unsupported PSD color mode {mode}"),
    };
    ensure!(channels >= color, "PSD has too few channels");
    let section = |r: &mut Reader| -> Result<()> {
        let len = r.u32()?;
        r.take(len as usize)?;
        Ok(())
    };
    section(&mut r)?; // color mode data
    section(&mut r)?; // image resources
    let layers_len = if psb { r.u64()? } else { u64::from(r.u32()?) };
    let layers = r.take(usize::try_from(layers_len)?)?;
    let merged_alpha = {
        let mut l = Reader(layers);
        let info_len = if psb {
            l.u64().ok()
        } else {
            l.u32().ok().map(u64::from)
        };
        info_len.is_some_and(|n| n >= 2) && l.u16().is_ok_and(|count| (count as i16) < 0)
    };
    let alpha = usize::from(merged_alpha && channels > color);
    let planes = color + alpha;
    let (w, h) = (width as usize, height as usize);
    let bytes = if depth == 16 { 2 } else { 1 };
    let row_len = w * bytes;
    let mut pixels = vec![0u8; w * h * planes];
    let mut store = |plane: usize, y: usize, row: &[u8]| {
        let out = &mut pixels[y * w * planes..(y + 1) * w * planes];
        for (x, sample) in row.chunks_exact(bytes).enumerate() {
            out[x * planes + plane] = sample[0]; // 16-bit: the high byte
        }
    };
    let mut row = vec![0u8; row_len];
    match r.u16()? {
        0 => {
            for plane in 0..planes {
                for y in 0..h {
                    store(plane, y, r.take(row_len)?);
                }
            }
        }
        1 => {
            let counts = (0..channels * h)
                .map(|_| if psb { r.u32() } else { r.u16().map(u32::from) })
                .collect::<Result<Vec<_>>>()?;
            for plane in 0..planes {
                for y in 0..h {
                    unpack_bits(r.take(counts[plane * h + y] as usize)?, &mut row)?;
                    store(plane, y, &row);
                }
            }
        }
        compression => bail!("unsupported PSD composite compression {compression}"),
    }
    let image = match (color, alpha) {
        (1, 0) => ImageBuffer::from_raw(width, height, pixels).map(DynamicImage::ImageLuma8),
        (1, _) => ImageBuffer::from_raw(width, height, pixels).map(DynamicImage::ImageLumaA8),
        (3, 0) => ImageBuffer::from_raw(width, height, pixels).map(DynamicImage::ImageRgb8),
        (3, _) => ImageBuffer::from_raw(width, height, pixels).map(DynamicImage::ImageRgba8),
        _ => {
            // Inverted CMYK (255 = no ink) to RGB, without a color profile.
            let out_planes = 3 + alpha;
            let mut rgb = Vec::with_capacity(w * h * out_planes);
            for px in pixels.chunks_exact(planes) {
                let k = u32::from(px[3]);
                rgb.extend(
                    px[..3]
                        .iter()
                        .map(|&c| ((u32::from(c) * k + 127) / 255) as u8),
                );
                rgb.extend_from_slice(&px[4..]);
            }
            if alpha == 0 {
                ImageBuffer::from_raw(width, height, rgb).map(DynamicImage::ImageRgb8)
            } else {
                ImageBuffer::from_raw(width, height, rgb).map(DynamicImage::ImageRgba8)
            }
        }
    };
    image.context("PSD buffer size mismatch")
}

/// PackBits run-length decoding of one row; the row must be filled exactly.
fn unpack_bits(mut input: &[u8], row: &mut [u8]) -> Result<()> {
    let mut at = 0;
    while let Some((&header, rest)) = input.split_first() {
        let header = header as i8;
        if header >= 0 {
            let n = header as usize + 1;
            ensure!(
                rest.len() >= n && at + n <= row.len(),
                "malformed PSD RLE row"
            );
            row[at..at + n].copy_from_slice(&rest[..n]);
            at += n;
            input = &rest[n..];
        } else if header != -128 {
            let n = (1 - isize::from(header)) as usize;
            let &value = rest.first().context("malformed PSD RLE row")?;
            ensure!(at + n <= row.len(), "malformed PSD RLE row");
            row[at..at + n].fill(value);
            at += n;
            input = &rest[1..];
        } else {
            input = rest;
        }
    }
    ensure!(at == row.len(), "short PSD RLE row");
    Ok(())
}

struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(n <= self.0.len(), "truncated PSD");
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into()?))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packbits_literals_and_runs() {
        let mut row = [0u8; 6];
        unpack_bits(&[1, 7, 8, 0xfe, 9, 0x80, 0, 5], &mut row).unwrap();
        assert_eq!(row, [7, 8, 9, 9, 9, 5]);
        assert!(unpack_bits(&[0xfe, 9], &mut [0u8; 2]).is_err());
    }

    #[test]
    fn oversized_heif_extent_is_rejected_before_decoding() {
        fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut out = (8 + body.len() as u32).to_be_bytes().to_vec();
            out.extend_from_slice(kind);
            out.extend_from_slice(body);
            out
        }
        let ispe = bx(
            b"ispe",
            &[[0; 4], 40000u32.to_be_bytes(), 10u32.to_be_bytes()].concat(),
        );
        let meta = bx(
            b"meta",
            &[&[0; 4][..], &bx(b"iprp", &bx(b"ipco", &ispe))].concat(),
        );
        let file = [bx(b"ftyp", b"heic\0\0\0\0mif1heic"), meta].concat();
        let error = decode(&file).unwrap_err().to_string();
        assert!(error.contains("pixel limit"), "{error}");
        assert!(decode(b"not an image").is_err());
    }
}
