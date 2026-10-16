#!/bin/sh
# Regenerates the HEIC/AVIF/PSD fixtures for tests/native_formats.rs inside a throwaway
# Alpine container (libheif heif-enc with x265/aom, ImageMagick, Pillow); nothing is
# installed on the host. The source pictures are drawn here, so the fixtures carry no
# third-party content:   crates/vamp-media/tests/fixtures/generate.sh
# Every picture is four quadrants (top-left red, top-right green, bottom-left blue,
# bottom-right yellow) that the tests sample to check tile assembly and orientation.
set -eu
cd "$(dirname "$0")"
exec docker run --rm -e OWNER="$(id -u):$(id -g)" -v "$PWD:/out" -w /work alpine:3.24 sh -euc '
apk add -q libheif-tools libheif-x265 libheif-aom imagemagick py3-pillow >/dev/null
python3 - <<EOF
from PIL import Image
quadrants = [(220, 30, 30), (30, 180, 60), (40, 60, 220), (240, 220, 40)]
def picture(width, height, alpha=False):
    image = Image.new("RGBA", (width, height))
    for y in range(height):
        for x in range(width):
            r, g, b = quadrants[(2 if y >= height // 2 else 0) + (1 if x >= width // 2 else 0)]
            shade = 24 * x // width  # a gentle ramp so tiles are not flat
            a = 0 if alpha and y < height // 8 else 255  # transparent top band
            image.putpixel((x, y), (min(r + shade, 255), min(g + shade, 255), min(b + shade, 255), a))
    return image
picture(256, 192).convert("RGB").save("rgb.png")
picture(256, 192, alpha=True).save("rgba.png")
picture(64, 48, alpha=True).save("small-rgba.png")
EOF
magick rgb.png -depth 16 PNG48:rgb16.png
magick rgba.png -depth 16 PNG64:rgba16.png
# iPhone-style HEIC: 8-bit 4:2:0 grid of 64x64 HEVC tiles (4x3).
heif-enc -q 60 --cut-tiles 64 -o /out/grid.heic rgb.png
# Same grid shown rotated 90 degrees clockwise through an irot property.
heif-enc -q 60 --cut-tiles 64 --rotate-cw 90 -o /out/rotated-grid.heic rgb.png
# 10-bit (Main 10) single-image HEIC.
heif-enc -q 60 -b 10 -o /out/10bit.heic rgb16.png
# AVIF: 8-bit 4:2:0 grid of 128x128 AV1 tiles (2x2), and 10-bit with an alpha plane.
heif-enc -A -q 60 --cut-tiles 128 -o /out/grid.avif rgb.png
heif-enc -A -q 60 -b 10 -o /out/alpha-10bit.avif rgba16.png
# PSD composites (Photoshop-style RLE image data): 8-bit RGBA and 16-bit RGB.
magick small-rgba.png -compress RLE /out/rgba.psd
magick small-rgba.png -alpha off -depth 16 -compress RLE /out/rgb16.psd
chown "$OWNER" /out/*.heic /out/*.avif /out/*.psd
for f in /out/*.heic /out/*.avif; do echo "$f"; heif-info "$f" | grep -E "^image|tiles|bit depth|alpha channel|rotate"; done
ls -l /out
'
