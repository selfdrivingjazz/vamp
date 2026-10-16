//! Note cards: PNG stand-ins for exploration findings that have no image of their own (a
//! person, a studio, a link with notes), so they show up in the grid like any other asset.
//! Text is drawn from embedded DejaVu Sans Mono atlases (`tools/card-font.py`) covering Latin,
//! Greek, Cyrillic and common punctuation/symbols; see [`fold`] for everything else.
use anyhow::{Context as _, Result};
use image::{GrayImage, Rgb, RgbImage};
use std::sync::LazyLock;
use unicode_normalization::{UnicodeNormalization as _, char::is_combining_mark};

#[path = "card_font.rs"]
mod font;

pub(crate) struct Face {
    atlas: &'static [u8],
    cell_width: u32,
    cell_height: u32,
}

const WIDTH: u32 = 1200;
const HEIGHT: u32 = 900;
const MARGIN: u32 = 72;
const BACKGROUND: [u8; 3] = [18, 18, 18];
const MUTED: [u8; 3] = [138, 138, 138];
const TITLE: [u8; 3] = [242, 242, 242];
const BODY: [u8; 3] = [208, 208, 208];

/// What a card shows. `kind` is free text (person, studio, link, note…); known kinds get
/// their own accent colour.
pub struct Card<'a> {
    pub kind: &'a str,
    pub title: &'a str,
    pub note: Option<&'a str>,
    pub url: Option<&'a str>,
    pub tags: &'a [String],
}

fn accent(kind: &str) -> [u8; 3] {
    match kind {
        "person" | "people" => [224, 179, 84],
        "studio" | "studios" => [111, 182, 255],
        "link" | "site" => [155, 224, 138],
        "note" | "theme" => [201, 160, 255],
        _ => [230, 110, 110],
    }
}

/// Host of an http(s) URL (`www.` dropped), for the card header.
pub(crate) fn host(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host = rest.split(['/', '?', '#']).next()?;
    Some(host.strip_prefix("www.").unwrap_or(host)).filter(|h| !h.is_empty())
}

struct Atlas {
    face: &'static Face,
    pixels: GrayImage,
}

/// Atlas characters in code-point order (the generator emits them sorted).
static CHARS: LazyLock<Vec<char>> = LazyLock::new(|| font::CHARS.chars().collect());

/// Atlas cell of `c`, if the font has it.
fn glyph(c: char) -> Option<u32> {
    CHARS.binary_search(&c).ok().map(|i| i as u32)
}

/// Zero-width and formatting characters (joiners, variation selectors, BOM, soft hyphen)
/// that have no cell of their own.
fn invisible(c: char) -> bool {
    matches!(c, '\u{AD}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{FE00}'..='\u{FE0F}' | '\u{FEFF}')
}

/// Maps `text` onto drawable characters, one cell each: NFC first (so decomposed accents
/// compose), then per character: kept if the atlas has it, other whitespace becomes a space
/// (newlines stay), leftover combining marks and invisible characters are dropped, anything
/// else is replaced by its compatibility form without accents when that is drawable
/// (ﬁ → fi, Ｗ → W, ễ → e), else '?'.
fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.nfc() {
        if glyph(c).is_some() || c == '\n' {
            out.push(c);
        } else if c.is_whitespace() {
            out.push(' ');
        } else if is_combining_mark(c) || invisible(c) || c.is_control() {
        } else {
            let start = out.len();
            out.extend(std::iter::once(c).nfkd().filter(|&k| !is_combining_mark(k)));
            let compat = &out[start..];
            if compat.is_empty() || compat.contains(c) || !compat.chars().all(|k| glyph(k).is_some()) {
                out.truncate(start);
                out.push('?');
            }
        }
    }
    out
}

impl Atlas {
    fn load(face: &'static Face) -> Result<Self> {
        let pixels = image::load_from_memory(face.atlas)
            .context("card font atlas")?
            .to_luma8();
        Ok(Self { face, pixels })
    }
    /// Columns that fit in `width` pixels.
    fn columns(&self, width: u32) -> usize {
        (width / self.face.cell_width) as usize
    }
    /// Coverage of cell `(dx, dy)` of glyph `index`.
    fn alpha(&self, index: u32, dx: u32, dy: u32) -> u8 {
        let (cw, ch) = (self.face.cell_width, self.face.cell_height);
        self.pixels
            .get_pixel((index % font::COLUMNS) * cw + dx, (index / font::COLUMNS) * ch + dy)[0]
    }
    /// Draws one [`fold`]ed line with its top at `y`; characters outside the atlas render as '?'.
    fn draw(&self, canvas: &mut RgbImage, x: u32, y: u32, text: &str, color: [u8; 3]) {
        let (cw, ch) = (self.face.cell_width, self.face.cell_height);
        let fallback = glyph('?').unwrap_or(0);
        for (i, c) in text.chars().enumerate() {
            let index = glyph(c).unwrap_or(fallback);
            let left = x + i as u32 * cw;
            for dy in 0..ch {
                for dx in 0..cw {
                    let (px, py) = (left + dx, y + dy);
                    if px >= canvas.width() || py >= canvas.height() {
                        continue;
                    }
                    let alpha = self.alpha(index, dx, dy) as u32;
                    if alpha == 0 {
                        continue;
                    }
                    let under = canvas.get_pixel(px, py).0;
                    let blend = |b: u8, f: u8| ((b as u32 * (255 - alpha) + f as u32 * alpha) / 255) as u8;
                    canvas.put_pixel(
                        px,
                        py,
                        Rgb([
                            blend(under[0], color[0]),
                            blend(under[1], color[1]),
                            blend(under[2], color[2]),
                        ]),
                    );
                }
            }
        }
    }
}

/// The regular and bold atlases, decoded once.
fn atlases() -> Result<(&'static Atlas, &'static Atlas)> {
    static REGULAR: LazyLock<Result<Atlas, String>> =
        LazyLock::new(|| Atlas::load(&font::REGULAR).map_err(|e| format!("{e:#}")));
    static BOLD: LazyLock<Result<Atlas, String>> =
        LazyLock::new(|| Atlas::load(&font::BOLD).map_err(|e| format!("{e:#}")));
    let regular = REGULAR.as_ref().map_err(|e| anyhow::anyhow!("{e}"))?;
    let bold = BOLD.as_ref().map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((regular, bold))
}

/// Greedy word wrap to `columns`, hard-breaking longer words; paragraphs split on newlines.
/// At most `max_lines` lines; a cut ends in '…'.
fn wrap(text: &str, columns: usize, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut truncated = false;
    'paragraphs: for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let mut word: Vec<char> = word.chars().collect();
            loop {
                let used = line.chars().count();
                let room = if used == 0 { columns } else { columns.saturating_sub(used + 1) };
                if word.len() <= room {
                    if used > 0 {
                        line.push(' ');
                    }
                    line.extend(word.iter());
                    break;
                }
                if used == 0 {
                    line.extend(word.drain(..columns));
                }
                if lines.len() + 1 >= max_lines {
                    lines.push(line);
                    truncated = true;
                    break 'paragraphs;
                }
                lines.push(std::mem::take(&mut line));
            }
        }
        if lines.len() >= max_lines {
            truncated = true;
            break;
        }
        lines.push(line);
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    if truncated && let Some(last) = lines.last_mut() {
        let mut chars: Vec<char> = last.chars().collect();
        chars.truncate(columns.saturating_sub(1));
        *last = chars.into_iter().collect::<String>() + "…";
    }
    lines
}

/// One line, cut with '…' to `columns`.
fn clip(text: &str, columns: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= columns {
        return text.to_owned();
    }
    chars[..columns.saturating_sub(1)].iter().collect::<String>() + "…"
}

/// Renders `card` as a 1200×900 PNG.
pub fn render(card: &Card) -> Result<Vec<u8>> {
    let (regular, bold) = atlases()?;
    let accent = accent(card.kind);
    let mut canvas = RgbImage::from_pixel(WIDTH, HEIGHT, Rgb(BACKGROUND));
    for y in 0..HEIGHT {
        for x in 0..12 {
            canvas.put_pixel(x, y, Rgb(accent));
        }
    }
    let inner = WIDTH - 2 * MARGIN;
    let mut y = MARGIN - 8;
    let mut header = card.kind.to_uppercase();
    if let Some(host) = card.url.and_then(host) {
        header.push_str(" · ");
        header.push_str(host);
    }
    regular.draw(&mut canvas, MARGIN, y, &clip(&fold(&header), regular.columns(inner)), accent);
    y += regular.face.cell_height + 20;
    for line in wrap(&fold(card.title), bold.columns(inner), 3) {
        bold.draw(&mut canvas, MARGIN, y, &line, TITLE);
        y += bold.face.cell_height;
    }
    y += 24;
    // Footer: the URL, then tags, bottom-aligned.
    let mut footer = Vec::new();
    if let Some(url) = card.url {
        footer.push((clip(&fold(url), regular.columns(inner)), accent));
    }
    if !card.tags.is_empty() {
        let tags = card
            .tags
            .iter()
            .map(|t| format!("#{t}"))
            .collect::<Vec<_>>()
            .join("  ");
        footer.push((clip(&fold(&tags), regular.columns(inner)), MUTED));
    }
    let footer_top = HEIGHT - MARGIN - footer.len() as u32 * regular.face.cell_height;
    if let Some(note) = card.note.map(str::trim).filter(|n| !n.is_empty()) {
        let room = footer_top.saturating_sub(y + 24) / regular.face.cell_height;
        for line in wrap(&fold(note), regular.columns(inner), room as usize) {
            regular.draw(&mut canvas, MARGIN, y, &line, BODY);
            y += regular.face.cell_height;
        }
    }
    for (i, (line, color)) in footer.iter().enumerate() {
        let top = footer_top + i as u32 * regular.face.cell_height;
        regular.draw(&mut canvas, MARGIN, top, line, *color);
    }
    let mut out = std::io::Cursor::new(Vec::new());
    canvas.write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_and_truncates() {
        assert_eq!(wrap("a bb ccc", 4, 5), ["a bb", "ccc"]);
        assert_eq!(wrap("abcdefghij", 4, 5), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("one\ntwo", 10, 5), ["one", "two"]);
        let cut = wrap("aa bb cc dd ee", 2, 2);
        assert_eq!(cut, ["aa", "b…"]);
        assert_eq!(host("https://www.bakers.studio/work?x=1"), Some("bakers.studio"));
        assert_eq!(host("ftp://x"), None);
    }

    #[test]
    fn renders_png() {
        let tags = vec!["typography".to_owned(), "studio".to_owned()];
        let png = render(&Card {
            kind: "studio",
            title: "Bakers Studio — independent design practice",
            note: Some("Why it matters: confident editorial grid, warm palette.\nPortfolio is the lander."),
            url: Some("https://bakers.studio/"),
            tags: &tags,
        })
        .unwrap();
        let image = image::load_from_memory(&png).unwrap();
        assert_eq!((image.width(), image.height()), (WIDTH, HEIGHT));
    }

    /// Names and typography agents paste (Turkish dotless ı/İ/ş/ğ, accents, ®, ™, dashes,
    /// curly quotes, Greek/Cyrillic) all have a real glyph in both faces: none folds to '?'
    /// and every cell is inked and differs from the '?' cell.
    #[test]
    fn covers_names_and_typography() {
        let sample = "Kaan Eryılmaz — OpenPurpose® “test” ‘quoted’ – … İstanbul Şişli Çağrı Ünlü \
                      Øresund Łódź Dvořák Hű Đặng ß æ œ € £ ¥ ™ © № ° ± × → • · «» „“ ′ \
                      Αθήνα Москва";
        assert_eq!(fold(sample), sample);
        let (regular, bold) = atlases().unwrap();
        let question = glyph('?').unwrap();
        for c in sample.chars().filter(|c| !c.is_whitespace()) {
            let index = glyph(c).unwrap_or_else(|| panic!("no glyph for {c:?}"));
            for atlas in [regular, bold] {
                let (cw, ch) = (atlas.face.cell_width, atlas.face.cell_height);
                let cell = |i| (0..ch).flat_map(|dy| (0..cw).map(move |dx| (dx, dy))).map(move |(dx, dy)| atlas.alpha(i, dx, dy)).collect::<Vec<_>>();
                let pixels = cell(index);
                assert!(pixels.iter().any(|&a| a > 0), "blank glyph for {c:?}");
                if c != '?' {
                    assert_ne!(pixels, cell(question), "{c:?} draws as '?'");
                }
            }
        }
        // A rendered card differs from the same card with '?' in place of each character.
        let card = |title| render(&Card { kind: "person", title, note: None, url: None, tags: &[] }).unwrap();
        assert_ne!(card("Eryılmaz®"), card("Ery?lmaz?"));
    }

    #[test]
    fn folds_unknown_characters() {
        assert_eq!(fold("Cafe\u{301}"), "Café"); // decomposed accents compose
        assert_eq!(fold("ﬁne Ｗork Nguyễn"), "fine Work Nguyen"); // compatibility forms, bare letters
        assert_eq!(fold("a\u{200D}b\u{FE0F}\u{AD}c"), "abc"); // invisible characters
        assert_eq!(fold("a\tb\u{A0}c\nd"), "a b c\nd"); // whitespace
        assert_eq!(fold("東京 🎨"), "?? ?"); // no glyph at all
    }
}
