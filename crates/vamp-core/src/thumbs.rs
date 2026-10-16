//! Derivatives of each asset made on the authority:
//! - micro: a ~40 px JPEG (~0.5–1 KB) kept in the catalog, so every tile can show a blurred
//!   image on its first frame, on any machine (replicas pull them in bulk);
//! - grid: an 800 px lossy WebP file (`representations/grid/<id>.webp`), what grids draw and
//!   what replicas download. Rendered from the largest locally cached source — the hires
//!   tier once it exists, the preview when its long edge reaches 800 px, else the thumbnail —
//!   never the original itself. A grid rendered from a smaller thumbnail is rendered again
//!   when the hires tier lands (see [`store_hires`]);
//! - hires ("preview" tier): a 2048 px lossy WebP (`representations/hires/<id>.webp`) from
//!   the original, read on the background I/O pool with a deadline (a stalled Google Drive
//!   defers the asset and it is retried later), else from the preview representation.
//!
//! Rendering decodes images, so it is split from the catalog reads/writes
//! ([`pending`], [`render`], [`store`]) and callers holding a lock can drop it while rendering.
//! Both are keyed by the thumbnail representation's `source_hash`: a re-thumbnailed asset
//! is rendered again. Grid rows carry `tier_version`; rows below [`GRID_VERSION`] (the old
//! 400 px JPEGs, grids that predate hires sources) stay served until their replacement is
//! rendered in the background.
use super::*;
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS micro_thumbs(asset_id TEXT PRIMARY KEY, source TEXT NOT NULL, data BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS grid_thumbs(asset_id TEXT PRIMARY KEY, source TEXT NOT NULL, path TEXT NOT NULL, byte_size INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS hires_thumbs(asset_id TEXT PRIMARY KEY, source TEXT NOT NULL, path TEXT NOT NULL, byte_size INTEGER NOT NULL, width INTEGER NOT NULL, height INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS hires_retry(asset_id TEXT PRIMARY KEY, source TEXT NOT NULL, stalls INTEGER NOT NULL, retry_at INTEGER NOT NULL);";

/// Adds columns to catalogs made before they existed: `grid_thumbs.tier_version` (old
/// 400 px JPEG rows: 1) and `hires_retry.dataless_ok`.
pub(crate) fn migrate(conn: &rusqlite::Connection) -> Result<()> {
    for (table, column, ddl) in [
        ("grid_thumbs", "tier_version", "ALTER TABLE grid_thumbs ADD COLUMN tier_version INTEGER NOT NULL DEFAULT 1"),
        ("hires_retry", "dataless_ok", "ALTER TABLE hires_retry ADD COLUMN dataless_ok INTEGER NOT NULL DEFAULT 0"),
    ] {
        let exists = conn
            .prepare(&format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name='{column}'"))?
            .exists([])?;
        if !exists {
            conn.execute_batch(ddl)?;
        }
    }
    Ok(())
}

pub const MICRO_EDGE: u32 = 40;
pub const GRID_EDGE: u32 = 800;
pub const HIRES_EDGE: u32 = 2048;
/// `grid_thumbs.tier_version` of 800 px WebP grids rendered from the best source (1: the
/// old 400 px JPEGs; 2: WebP grids that never considered the hires tier). Bumping it
/// re-renders every grid once; `PENDING_WHERE` and [`store_hires`] use the same number.
pub const GRID_VERSION: i64 = 3;
const MICRO_QUALITY: u8 = 45;
const GRID_QUALITY: f32 = 80.0;
const HIRES_QUALITY: f32 = 82.0;
/// Stalled original reads before the hires tier settles for the preview representation.
const HIRES_MAX_STALLS: i64 = 3;
/// Extensions the decoder reads; other originals (video, RAW, PSD…) use the preview rep.
const DECODABLE: &[&str] = &[".jpg", ".jpeg", ".png", ".webp", ".gif", ".tif", ".tiff", ".bmp"];
/// Ids per pack request/response.
pub const MAX_PACK: usize = 512;

/// A thumbnail waiting for its micro/grid derivatives. `paths` are tried in order (the
/// hires tier, the preview representation when large enough, then the thumbnail).
pub struct Source {
    pub asset_id: String,
    pub source: String,
    pub paths: Vec<PathBuf>,
}

/// Rendered derivatives; `None` when the thumbnail could not be decoded (left alone until
/// the thumbnail changes: the failure is recorded as an empty micro).
pub struct Rendered {
    pub asset_id: String,
    pub source: String,
    pub micro: Vec<u8>,
    pub grid: Option<Vec<u8>>,
}

/// Rows needing a micro, or a current-version grid (undecodable ones, recorded as an empty
/// micro, are not retried until the thumbnail changes). `3` is [`GRID_VERSION`].
const PENDING_WHERE: &str = "r.kind='thumbnail' AND (NOT EXISTS (SELECT 1 FROM micro_thumbs m WHERE m.asset_id=r.asset_id AND m.source=r.source_hash) OR (EXISTS (SELECT 1 FROM micro_thumbs m WHERE m.asset_id=r.asset_id AND m.source=r.source_hash AND length(m.data)>0) AND NOT EXISTS (SELECT 1 FROM grid_thumbs g WHERE g.asset_id=r.asset_id AND g.source=r.source_hash AND g.tier_version>=3)))";

/// Up to `limit` live assets whose current thumbnail has no micro/current grid derivative.
/// Authority catalogs only: replicas receive derivatives from their authority.
pub fn pending(library: &Library, limit: usize) -> Result<Vec<Source>> {
    if library.context.remote.is_some() {
        return Ok(Vec::new());
    }
    let mut stmt = library.conn.prepare_cached(&format!(
        "SELECT r.asset_id, r.source_hash, r.document, p.document, h.path FROM representations r JOIN assets a ON a.id=r.asset_id AND a.deleted=0 LEFT JOIN representations p ON p.asset_id=r.asset_id AND p.kind='preview' LEFT JOIN hires_thumbs h ON h.asset_id=r.asset_id AND h.source=r.source_hash WHERE {PENDING_WHERE} ORDER BY a.imported DESC LIMIT ?1"
    ))?;
    let rows = stmt
        .query_map([limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut sources = Vec::with_capacity(rows.len());
    for (asset_id, source, document, preview, hires) in rows {
        let rep: Representation = parse(document)?;
        let mut paths = Vec::with_capacity(3);
        if let Some(hires) = hires {
            paths.push(local_representation(&library.state_dir, &hires)?);
        }
        if let Some(preview) = preview {
            let preview: Representation = parse(preview)?;
            if preview.width.max(preview.height) >= GRID_EDGE {
                paths.push(local_representation(&library.state_dir, &preview.path)?);
            }
        }
        paths.push(local_representation(&library.state_dir, &rep.path)?);
        sources.push(Source { asset_id, source, paths });
    }
    Ok(sources)
}

fn jpeg(image: &image::DynamicImage, quality: u8) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality)
        .encode_image(&image.to_rgb8())?;
    Ok(out)
}

/// Lossy WebP (libwebp; the `image` crate only encodes lossless). Alpha kept when present.
pub fn webp(image: &image::DynamicImage, quality: f32) -> Result<Vec<u8>> {
    let (w, h) = (image.width(), image.height());
    let encoded = if image.color().has_alpha() {
        webp::Encoder::from_rgba(image.to_rgba8().as_raw(), w, h).encode_simple(false, quality)
    } else {
        webp::Encoder::from_rgb(image.to_rgb8().as_raw(), w, h).encode_simple(false, quality)
    };
    encoded
        .map(|m| m.to_vec())
        .map_err(|e| anyhow::anyhow!("webp encode failed: {e:?}"))
}

/// Downscales so the long edge is at most `edge` (never upscales).
pub fn fit(image: image::DynamicImage, edge: u32) -> image::DynamicImage {
    if image.width().max(image.height()) > edge {
        image.resize(edge, edge, image::imageops::FilterType::CatmullRom)
    } else {
        image
    }
}

/// Renders micro (JPEG) and grid (WebP) from one decoded image.
pub fn render_image(image: image::DynamicImage) -> Result<(Vec<u8>, Vec<u8>)> {
    let grid = fit(image, GRID_EDGE);
    let micro = grid.thumbnail(MICRO_EDGE, MICRO_EDGE);
    Ok((jpeg(&micro, MICRO_QUALITY)?, webp(&grid, GRID_QUALITY)?))
}

/// Decodes the first readable source and renders its derivatives (CPU only; no catalog
/// access).
pub fn render(source: &Source) -> Rendered {
    let rendered = source.paths.iter().find_map(|path| {
        let image = image::load_from_memory(&fs::read(path).ok()?).ok()?;
        render_image(image).ok()
    });
    let (micro, grid) = match rendered {
        Some((micro, grid)) => (micro, Some(grid)),
        None => (Vec::new(), None),
    };
    Rendered {
        asset_id: source.asset_id.clone(),
        source: source.source.clone(),
        micro,
        grid,
    }
}

/// Renders on every core; order preserved.
pub fn render_all(sources: &[Source]) -> Vec<Rendered> {
    parallel(sources, std::thread::available_parallelism().map_or(4, |n| n.get()), render)
}

fn parallel<S: Sync, R: Send>(items: &[S], threads: usize, f: impl Fn(&S) -> R + Sync) -> Vec<R> {
    let chunk = items.len().div_ceil(threads.max(1)).max(1);
    let f = &f;
    std::thread::scope(|scope| {
        items
            .chunks(chunk)
            .map(|part| scope.spawn(move || part.iter().map(f).collect::<Vec<_>>()))
            .collect::<Vec<_>>()
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    })
}

/// Writes `bytes` to `dir/name` atomically; returns the state-relative path.
fn write_file(library: &Library, tier: &str, name: &str, bytes: &[u8]) -> Result<String> {
    let dir = library.state_dir.join("representations").join(tier);
    fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".{name}.tmp"));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, dir.join(name))?;
    Ok(format!("representations/{tier}/{name}"))
}

/// Writes rendered derivatives (grid files first, then one transaction). Replaced 400 px
/// JPEG files are removed after the commit.
pub fn store(library: &mut Library, rendered: &[Rendered]) -> Result<()> {
    let mut grids = Vec::with_capacity(rendered.len());
    for r in rendered {
        grids.push(match &r.grid {
            Some(bytes) => {
                let name = format!("{}.webp", safe_name(&r.asset_id)?);
                Some((write_file(library, "grid", &name, bytes)?, bytes.len()))
            }
            None => None,
        });
    }
    let mut stale = Vec::new();
    let tx = library.conn.transaction()?;
    for (r, grid) in rendered.iter().zip(grids) {
        tx.prepare_cached("INSERT OR REPLACE INTO micro_thumbs(asset_id,source,data) VALUES(?1,?2,?3)")?
            .execute(params![r.asset_id, r.source, r.micro])?;
        if let Some((path, size)) = grid {
            if let Some(old) = tx
                .prepare_cached("SELECT path FROM grid_thumbs WHERE asset_id=?1")?
                .query_row([&r.asset_id], |row| row.get::<_, String>(0))
                .optional()?
                && old != path
            {
                stale.push(old);
            }
            tx.prepare_cached("INSERT OR REPLACE INTO grid_thumbs(asset_id,source,path,byte_size,tier_version) VALUES(?1,?2,?3,?4,?5)")?
                .execute(params![r.asset_id, r.source, path, size as i64, GRID_VERSION])?;
        }
    }
    tx.commit()?;
    for old in stale {
        if let Ok(path) = local_representation(&library.state_dir, &old) {
            let _ = fs::remove_file(path);
        }
    }
    Ok(())
}

fn safe_name(id: &str) -> Result<&str> {
    ensure!(
        !id.is_empty()
            && id.len() <= 128
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid asset id"
    );
    Ok(id)
}

/// Renders every pending micro/grid derivative in batches; `progress(done, total)` after
/// each batch. Returns `{rendered}`.
pub fn generate_all_with(library: &mut Library, progress: &mut dyn FnMut(u64, u64)) -> Result<Value> {
    let total = pending_count(library)?;
    let mut done = 0u64;
    loop {
        let batch = pending(library, 256)?;
        if batch.is_empty() {
            break;
        }
        let rendered = render_all(&batch);
        store(library, &rendered)?;
        done += rendered.len() as u64;
        progress(done, total.max(done));
    }
    Ok(json!({ "rendered": done }))
}

/// [`generate_all_with`] without progress.
pub fn generate_all(library: &mut Library) -> Result<Value> {
    generate_all_with(library, &mut |_, _| {})
}

/// Count of thumbnails still waiting for micro/grid derivatives.
pub fn pending_count(library: &Library) -> Result<u64> {
    if library.context.remote.is_some() {
        return Ok(0);
    }
    Ok(library.conn.query_row(
        &format!("SELECT count(*) FROM representations r JOIN assets a ON a.id=r.asset_id AND a.deleted=0 WHERE {PENDING_WHERE}"),
        [],
        |r| r.get::<_, i64>(0),
    )? as u64)
}

/// An asset waiting for its hires tier.
pub struct HiresSource {
    pub asset_id: String,
    pub source: String,
    /// The original, when the decoder can read its format.
    pub original: Option<crate::storage::OriginalLookup>,
    pub preview: Option<PathBuf>,
    /// Earlier stalled original reads.
    pub stalls: i64,
    /// Already passed over once for being a cloud placeholder: read it now.
    pub dataless_ok: bool,
}

pub enum HiresOutcome {
    Rendered { bytes: Vec<u8>, width: u32, height: u32, from: &'static str },
    /// The original's storage did not respond: retry later.
    Stalled,
    /// The original is a cloud placeholder (not downloaded): queued behind local ones.
    Dataless,
    /// No original-read slot free: nothing recorded, picked up again next pass.
    Busy,
    /// Nothing decodable (retried after a day, or when the thumbnail changes).
    Failed,
}

/// Concurrent hires original reads. Drive reads abandoned at the deadline keep their pool
/// thread until the provider answers, so they count until they actually finish, and no new
/// read starts while one is abandoned: the shared background pool keeps its headroom.
const HIRES_SLOTS: usize = 2;
static HIRES_INFLIGHT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static HIRES_ABANDONED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// One hires original read; released when the pool operation ends (or is dropped unstarted).
struct HiresSlot(Arc<std::sync::atomic::AtomicU8>);
const SLOT_RUNNING: u8 = 0;
const SLOT_ABANDONED: u8 = 1;
const SLOT_DONE: u8 = 2;

impl HiresSlot {
    fn acquire() -> Option<Self> {
        use std::sync::atomic::Ordering::SeqCst;
        if HIRES_ABANDONED.load(SeqCst) > 0 {
            return None;
        }
        HIRES_INFLIGHT
            .fetch_update(SeqCst, SeqCst, |n| (n < HIRES_SLOTS).then_some(n + 1))
            .ok()?;
        Some(Self(Arc::new(std::sync::atomic::AtomicU8::new(SLOT_RUNNING))))
    }
    /// A handle the caller uses to mark the read abandoned after its deadline.
    fn state(&self) -> Arc<std::sync::atomic::AtomicU8> {
        self.0.clone()
    }
    fn abandon(state: &std::sync::atomic::AtomicU8) {
        use std::sync::atomic::Ordering::SeqCst;
        if state.compare_exchange(SLOT_RUNNING, SLOT_ABANDONED, SeqCst, SeqCst).is_ok() {
            HIRES_ABANDONED.fetch_add(1, SeqCst);
        }
    }
}

impl Drop for HiresSlot {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::SeqCst;
        if self.0.swap(SLOT_DONE, SeqCst) == SLOT_ABANDONED {
            HIRES_ABANDONED.fetch_sub(1, SeqCst);
        }
        HIRES_INFLIGHT.fetch_sub(1, SeqCst);
    }
}

/// macOS File Provider placeholder whose bytes are not on disk (`SF_DATALESS`).
fn is_dataless(meta: &fs::Metadata) -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt;
        meta.st_flags() & 0x4000_0000 != 0
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = meta;
        false
    }
}

/// Reads a whole file, reporting progress per chunk so a slow but moving download is not
/// cut off by the inactivity deadline.
fn read_ticking(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let mut file = fs::File::open(path)?;
    let mut out = Vec::with_capacity(file.metadata().map_or(0, |m| m.len() as usize));
    let mut chunk = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut chunk)?;
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&chunk[..n]);
        crate::io::tick();
    }
}

const HIRES_PENDING_WHERE: &str = "r.kind='thumbnail' AND NOT EXISTS (SELECT 1 FROM hires_thumbs h WHERE h.asset_id=r.asset_id AND h.source=r.source_hash)";

fn now_secs() -> i64 {
    Utc::now().timestamp()
}

/// Up to `limit` live assets without a hires tier for their current thumbnail, skipping
/// those deferred until later (stalled or failed). Authority only.
pub fn pending_hires(library: &Library, limit: usize) -> Result<Vec<HiresSource>> {
    if library.context.remote.is_some() {
        return Ok(Vec::new());
    }
    let mut stmt = library.conn.prepare_cached(&format!(
        "SELECT r.asset_id, r.source_hash, p.document, x.stalls, coalesce(x.dataless_ok,0) FROM representations r JOIN assets a ON a.id=r.asset_id AND a.deleted=0 LEFT JOIN representations p ON p.asset_id=r.asset_id AND p.kind='preview' LEFT JOIN hires_retry x ON x.asset_id=r.asset_id AND x.source=r.source_hash WHERE {HIRES_PENDING_WHERE} AND (x.retry_at IS NULL OR x.retry_at<=?2) ORDER BY coalesce(x.dataless_ok,0), a.imported DESC LIMIT ?1"
    ))?;
    let rows = stmt
        .query_map(params![limit as i64, now_secs()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, bool>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for (asset_id, source, preview, stalls, dataless_ok) in rows {
        let stalls = stalls.unwrap_or(0);
        let asset = get_asset(&library.conn, &asset_id)?;
        let ext = crate::storage::original_extension(&asset.original).to_ascii_lowercase();
        let original = if stalls < HIRES_MAX_STALLS && DECODABLE.contains(&ext.as_str()) {
            library.lookup_of(&asset.original).ok()
        } else {
            None
        };
        let preview = preview
            .map(parse::<Representation>)
            .transpose()?
            .map(|p| local_representation(&library.state_dir, &p.path))
            .transpose()?;
        out.push(HiresSource { asset_id, source, original, preview, stalls, dataless_ok });
    }
    Ok(out)
}

fn render_hires_image(image: image::DynamicImage) -> Result<(Vec<u8>, u32, u32)> {
    let image = fit(image, HIRES_EDGE);
    Ok((webp(&image, HIRES_QUALITY)?, image.width(), image.height()))
}

/// Reads the original on the background I/O pool (at most [`HIRES_SLOTS`] at once; Drive
/// stalls end at the deadline) and renders it; else renders the preview representation.
/// Never blocks past the deadline.
pub fn render_hires(source: &HiresSource) -> HiresOutcome {
    if let Some(lookup) = source.original.clone() {
        let Some(slot) = HiresSlot::acquire() else {
            return HiresOutcome::Busy;
        };
        let state = slot.state();
        let dataless_ok = source.dataless_ok;
        let read = crate::io::background().try_run(
            format!("hires {}", source.asset_id),
            crate::io::DRIVE_DEADLINE,
            move || -> Result<Option<Vec<u8>>> {
                let _slot = slot;
                let path = lookup.fetch()?;
                if !dataless_ok && is_dataless(&fs::metadata(&path)?) {
                    return Ok(None);
                }
                read_ticking(&path).map(Some)
            },
        );
        match read {
            Ok(None) => return HiresOutcome::Dataless,
            Ok(Some(bytes)) => {
                if let Ok((bytes, width, height)) =
                    image::load_from_memory(&bytes).map_err(anyhow::Error::from).and_then(render_hires_image)
                {
                    return HiresOutcome::Rendered { bytes, width, height, from: "original" };
                }
            }
            Err(e) if crate::io::is_stalled(&e) => {
                HiresSlot::abandon(&state);
                return HiresOutcome::Stalled;
            }
            Err(_) => {}
        }
    }
    let from_preview = source.preview.as_ref().and_then(|path| {
        render_hires_image(image::load_from_memory(&fs::read(path).ok()?).ok()?).ok()
    });
    match from_preview {
        Some((bytes, width, height)) => HiresOutcome::Rendered { bytes, width, height, from: "preview" },
        None => HiresOutcome::Failed,
    }
}

/// Renders a batch with a few threads (originals can be large; memory stays bounded).
pub fn render_hires_all(sources: &[HiresSource]) -> Vec<HiresOutcome> {
    parallel(sources, 4, render_hires)
}

/// Records hires outcomes: files + rows for rendered ones, retry times for the rest
/// (stalls back off 2^n minutes up to 6 h; failures wait a day; placeholders are queued
/// after every local original). `Busy` ones are left untouched. A grid rendered from a
/// source smaller than the new hires tier (and than [`GRID_EDGE`]) is queued to render
/// again from it (still served meanwhile). Returns how many settled (anything but `Busy`).
pub fn store_hires(library: &mut Library, sources: &[HiresSource], outcomes: &[HiresOutcome]) -> Result<usize> {
    let mut files = Vec::with_capacity(outcomes.len());
    for (s, o) in sources.iter().zip(outcomes) {
        files.push(match o {
            HiresOutcome::Rendered { bytes, .. } => {
                Some(write_file(library, "hires", &format!("{}.webp", safe_name(&s.asset_id)?), bytes)?)
            }
            _ => None,
        });
    }
    let now = now_secs();
    let mut settled = 0;
    let tx = library.conn.transaction()?;
    let retry = |s: &HiresSource, stalls: i64, at: i64, dataless_ok: bool| -> Result<()> {
        tx.prepare_cached("INSERT OR REPLACE INTO hires_retry(asset_id,source,stalls,retry_at,dataless_ok) VALUES(?1,?2,?3,?4,?5)")?
            .execute(params![s.asset_id, s.source, stalls, at, dataless_ok])?;
        Ok(())
    };
    for ((s, o), file) in sources.iter().zip(outcomes).zip(files) {
        match (o, file) {
            (HiresOutcome::Busy, _) => continue,
            (HiresOutcome::Rendered { bytes, width, height, .. }, Some(path)) => {
                tx.prepare_cached("INSERT OR REPLACE INTO hires_thumbs(asset_id,source,path,byte_size,width,height) VALUES(?1,?2,?3,?4,?5,?6)")?
                    .execute(params![s.asset_id, s.source, path, bytes.len() as i64, width, height])?;
                tx.prepare_cached("DELETE FROM hires_retry WHERE asset_id=?1")?.execute([&s.asset_id])?;
                // The grid's own sources: the thumbnail and a preview representation.
                let mut source_edge = 0u32;
                for document in tx
                    .prepare_cached("SELECT document FROM representations WHERE asset_id=?1 AND kind IN ('thumbnail','preview')")?
                    .query_map([&s.asset_id], |r| r.get::<_, String>(0))?
                {
                    let rep: Representation = parse(document?)?;
                    source_edge = source_edge.max(rep.width.max(rep.height));
                }
                if source_edge < GRID_EDGE.min(*width.max(height)) {
                    tx.prepare_cached("UPDATE grid_thumbs SET tier_version=2 WHERE asset_id=?1 AND tier_version>=?2")?
                        .execute(params![s.asset_id, GRID_VERSION])?;
                }
            }
            (HiresOutcome::Stalled, _) => {
                let delay = (60i64 << s.stalls.min(9)).min(6 * 3600);
                retry(s, s.stalls + 1, now + delay, s.dataless_ok)?;
            }
            (HiresOutcome::Dataless, _) => retry(s, s.stalls, now, true)?,
            _ => retry(s, s.stalls, now + 86_400, s.dataless_ok)?,
        }
        settled += 1;
    }
    tx.commit()?;
    Ok(settled)
}

/// Renders every due hires tier in batches; `progress(done, total)` after each batch.
/// Returns `{rendered, from_original, from_preview, stalled, failed}`.
pub fn generate_hires_with(library: &mut Library, progress: &mut dyn FnMut(u64, u64)) -> Result<Value> {
    let total = hires_pending_count(library)?;
    let (mut original, mut preview, mut stalled, mut failed) = (0u64, 0u64, 0u64, 0u64);
    let mut idle = 0u32;
    loop {
        let batch = pending_hires(library, 8)?;
        if batch.is_empty() {
            break;
        }
        let outcomes = render_hires_all(&batch);
        for o in &outcomes {
            match o {
                HiresOutcome::Rendered { from: "original", .. } => original += 1,
                HiresOutcome::Rendered { .. } => preview += 1,
                HiresOutcome::Stalled => stalled += 1,
                HiresOutcome::Failed => failed += 1,
                HiresOutcome::Dataless | HiresOutcome::Busy => {}
            }
        }
        if store_hires(library, &batch, &outcomes)? == 0 {
            // Every read slot is held by a stalled read: wait for the provider, but give
            // the remainder to the server's jobs thread if it stays wedged.
            idle += 1;
            if idle > 300 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        }
        idle = 0;
        let done = original + preview + stalled + failed;
        progress(done, total.max(done));
    }
    Ok(json!({
        "rendered": original + preview,
        "from_original": original,
        "from_preview": preview,
        "stalled": stalled,
        "failed": failed,
    }))
}

/// Live assets without a hires tier for their current thumbnail (deferred ones included).
pub fn hires_pending_count(library: &Library) -> Result<u64> {
    if library.context.remote.is_some() {
        return Ok(0);
    }
    Ok(library.conn.query_row(
        &format!("SELECT count(*) FROM representations r JOIN assets a ON a.id=r.asset_id AND a.deleted=0 WHERE {HIRES_PENDING_WHERE}"),
        [],
        |r| r.get::<_, i64>(0),
    )? as u64)
}

/// `GET /v1/tiers/stats` body.
pub fn tier_stats(library: &Library) -> Result<Value> {
    let sum = |table: &str, filter: &str| -> Result<(u64, u64)> {
        Ok(library.conn.query_row(
            &format!("SELECT count(*), coalesce(sum(byte_size),0) FROM {table} {filter}"),
            [],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
        )?)
    };
    let (grid_count, grid_bytes) = sum("grid_thumbs", "WHERE tier_version>=2")?;
    let (hires_count, hires_bytes) = sum("hires_thumbs", "")?;
    let hires_pending = hires_pending_count(library)?;
    // Until some exist, assume ~300 KB per 2048 px q82 WebP.
    let average = hires_bytes.checked_div(hires_count).unwrap_or(300_000);
    Ok(json!({
        "grid": {"count": grid_count, "bytes": grid_bytes, "pending": pending_count(library)?},
        "preview": {
            "count": hires_count,
            "bytes": hires_bytes,
            "pending": hires_pending,
            "estimated_total_bytes": hires_bytes + hires_pending * average,
        },
    }))
}

/// The generated WebP for `tier` (`grid` | `preview`), if any; never touches originals.
pub fn tier_path(library: &Library, id: &str, tier: &str) -> Result<Option<PathBuf>> {
    let sql = match tier {
        "grid" => "SELECT path FROM grid_thumbs WHERE asset_id=?1 AND tier_version>=2",
        "preview" => "SELECT path FROM hires_thumbs WHERE asset_id=?1",
        other => anyhow::bail!("unknown tier: {other}"),
    };
    Ok(library
        .conn
        .prepare_cached(sql)?
        .query_row([id], |r| r.get::<_, String>(0))
        .optional()?
        .map(|p| local_representation(&library.state_dir, &p))
        .transpose()?
        .filter(|p| p.is_file()))
}

/// Local micro-thumbnails for `ids` (absent or undecodable ones omitted). On a replica,
/// ids missing locally are first pulled from the authority (best effort: offline is fine).
pub fn micro_for(library: &mut Library, ids: &[String]) -> Result<HashMap<String, Vec<u8>>> {
    #[cfg_attr(not(feature = "remote"), allow(unused_mut))]
    let mut found = local_micro(library, ids)?;
    #[cfg(feature = "remote")]
    if library.context.remote.is_some() {
        let missing: Vec<String> = ids
            .iter()
            .filter(|id| !found.contains_key(*id) && !library_has_empty_micro(library, id))
            .cloned()
            .collect();
        if !missing.is_empty() {
            if let Err(e) = pull_micro(library, &missing) {
                eprintln!("micro thumbnails: {e:#}");
            }
            found.extend(local_micro(library, &missing)?);
        }
    }
    Ok(found)
}

fn local_micro(library: &Library, ids: &[String]) -> Result<HashMap<String, Vec<u8>>> {
    let mut found = HashMap::with_capacity(ids.len());
    let mut stmt = library
        .conn
        .prepare_cached("SELECT data FROM micro_thumbs WHERE asset_id=?1 AND length(data)>0")?;
    for id in ids {
        if let Some(data) = stmt.query_row([id], |r| r.get::<_, Vec<u8>>(0)).optional()? {
            found.insert(id.clone(), data);
        }
    }
    Ok(found)
}

#[cfg(feature = "remote")]
fn library_has_empty_micro(library: &Library, id: &str) -> bool {
    library
        .conn
        .query_row("SELECT 1 FROM micro_thumbs WHERE asset_id=?1", [id], |_| Ok(()))
        .optional()
        .ok()
        .flatten()
        .is_some()
}

/// Pack response body: per entry `u16` id length, id bytes, `u32` data length, data
/// (all little endian); length 0 means the authority has nothing for that id.
pub fn encode_pack(entries: &[(String, Option<Vec<u8>>)]) -> Vec<u8> {
    let size: usize = entries
        .iter()
        .map(|(id, d)| 6 + id.len() + d.as_ref().map_or(0, Vec::len))
        .sum();
    let mut out = Vec::with_capacity(size);
    for (id, data) in entries {
        out.extend_from_slice(&(id.len() as u16).to_le_bytes());
        out.extend_from_slice(id.as_bytes());
        let data = data.as_deref().unwrap_or(&[]);
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
    }
    out
}

pub fn decode_pack(mut bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    let mut entries = Vec::new();
    let take = |bytes: &mut &[u8], n: usize| -> Result<Vec<u8>> {
        ensure!(bytes.len() >= n, "truncated thumbnail pack");
        let (head, rest) = bytes.split_at(n);
        *bytes = rest;
        Ok(head.to_vec())
    };
    while !bytes.is_empty() {
        let len = u16::from_le_bytes(take(&mut bytes, 2)?.try_into().unwrap()) as usize;
        let id = String::from_utf8(take(&mut bytes, len)?)?;
        let len = u32::from_le_bytes(take(&mut bytes, 4)?.try_into().unwrap()) as usize;
        entries.push((id, take(&mut bytes, len)?));
    }
    Ok(entries)
}

/// Authority side of `POST /v1/thumbnails/pack`: `kind` `micro` (catalog blobs) or `grid`
/// (the grid file — 800 px WebP, or a not yet regenerated 400 px JPEG — else the asset's
/// current thumbnail). Returns the file paths to read
/// for `grid` so callers can read them without holding a lock.
pub enum PackItem {
    Bytes(Vec<u8>),
    File(PathBuf),
    Missing,
}

pub fn pack_items(library: &Library, ids: &[String], kind: &str) -> Result<Vec<(String, PackItem)>> {
    ensure!(ids.len() <= MAX_PACK, "at most {MAX_PACK} ids per pack");
    let mut out = Vec::with_capacity(ids.len());
    match kind {
        "micro" => {
            let found = local_micro(library, ids)?;
            for id in ids {
                let item = found.get(id).cloned().map_or(PackItem::Missing, PackItem::Bytes);
                out.push((id.clone(), item));
            }
        }
        "grid" => {
            let mut stmt = library.conn.prepare_cached(
                "SELECT g.path FROM grid_thumbs g JOIN representations r ON r.asset_id=g.asset_id AND r.kind='thumbnail' AND r.source_hash=g.source WHERE g.asset_id=?1",
            )?;
            for id in ids {
                let grid = stmt
                    .query_row([id], |r| r.get::<_, String>(0))
                    .optional()?
                    .map(|p| local_representation(&library.state_dir, &p))
                    .transpose()?
                    .filter(|p| p.is_file());
                let path = match grid {
                    Some(path) => Some(path),
                    None => library.fetch(id, "thumbnail").ok(),
                };
                out.push((id.clone(), path.map_or(PackItem::Missing, PackItem::File)));
            }
        }
        other => anyhow::bail!("unknown pack kind: {other}"),
    }
    Ok(out)
}

/// Reads `pack_items` into a response body (files read here, outside any lock).
pub fn pack_body(items: Vec<(String, PackItem)>) -> Vec<u8> {
    let entries: Vec<(String, Option<Vec<u8>>)> = items
        .into_iter()
        .map(|(id, item)| {
            let data = match item {
                PackItem::Bytes(b) => Some(b),
                PackItem::File(p) => fs::read(p).ok(),
                PackItem::Missing => None,
            };
            (id, data)
        })
        .collect();
    encode_pack(&entries)
}

/// Fetches `ids` (≤ [`MAX_PACK`]) of `kind` from the authority in one request.
#[cfg(feature = "remote")]
pub fn fetch_pack(
    client: &crate::remote::Client,
    ids: &[String],
    kind: &str,
) -> Result<Vec<(String, Vec<u8>)>> {
    let body = client.post_bytes("/thumbnails/pack", &json!({"ids":ids,"kind":kind}))?;
    decode_pack(&body)
}

/// Pulls micro-thumbnails for `ids` from the authority and stores them (an empty blob
/// records "authority has none", so it is not asked again until the next `sync_micros`).
#[cfg(feature = "remote")]
pub fn pull_micro(library: &mut Library, ids: &[String]) -> Result<u64> {
    let client = library.remote_client()?;
    let mut stored = 0u64;
    for chunk in ids.chunks(MAX_PACK) {
        let entries = fetch_pack(&client, chunk, "micro")?;
        let tx = library.conn.transaction()?;
        for (id, data) in entries {
            if !chunk.contains(&id) {
                continue;
            }
            tx.prepare_cached("INSERT OR REPLACE INTO micro_thumbs(asset_id,source,data) VALUES(?1,'remote',?2)")?
                .execute(params![id, data])?;
            stored += u64::from(!data.is_empty());
        }
        tx.commit()?;
    }
    Ok(stored)
}

/// Replica: fetches every live asset's micro-thumbnail it lacks (empty placeholders retried).
#[cfg(feature = "remote")]
pub fn sync_micros(library: &mut Library) -> Result<Value> {
    let ids: Vec<String> = {
        let mut stmt = library.conn.prepare(
            "SELECT a.id FROM assets a WHERE a.deleted=0 AND NOT EXISTS (SELECT 1 FROM micro_thumbs m WHERE m.asset_id=a.id AND length(m.data)>0) ORDER BY a.imported DESC",
        )?;
        stmt.query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    let stored = pull_micro(library, &ids)?;
    Ok(json!({"requested":ids.len(),"stored":stored}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_round_trips() {
        let entries = vec![
            ("a".to_owned(), Some(vec![1, 2, 3])),
            ("bb".to_owned(), None),
            ("c".to_owned(), Some(vec![])),
        ];
        let decoded = decode_pack(&encode_pack(&entries)).unwrap();
        assert_eq!(
            decoded,
            vec![
                ("a".to_owned(), vec![1, 2, 3]),
                ("bb".to_owned(), vec![]),
                ("c".to_owned(), vec![])
            ]
        );
        assert!(decode_pack(&encode_pack(&entries)[..5]).is_err());
    }

    fn image(width: u32, height: u32) -> image::DynamicImage {
        image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        }))
    }

    fn is_webp(bytes: &[u8]) -> bool {
        bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP"
    }

    #[test]
    fn grid_and_micro_sizes_and_formats() {
        let (micro, grid) = render_image(image(3000, 1500)).unwrap();
        assert!(micro.starts_with(&[0xff, 0xd8]), "micro stays JPEG");
        assert!(is_webp(&grid));
        let decoded = image::load_from_memory(&grid).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (GRID_EDGE, 400));
        let micro = image::load_from_memory(&micro).unwrap();
        assert_eq!(micro.width().max(micro.height()), MICRO_EDGE);

        // Never upscaled.
        let (_, small) = render_image(image(300, 500)).unwrap();
        let small = image::load_from_memory(&small).unwrap();
        assert_eq!((small.width(), small.height()), (300, 500));
    }

    #[test]
    fn hires_sizes_and_format() {
        let (bytes, w, h) = render_hires_image(image(1000, 5000)).unwrap();
        assert!(is_webp(&bytes));
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height(), w, h), (410, 2048, 410, 2048));
        let (_, w, h) = render_hires_image(image(1200, 900)).unwrap();
        assert_eq!((w, h), (1200, 900));
    }

    #[test]
    fn alpha_survives_webp() {
        let rgba = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(64, 64, image::Rgba([10, 20, 30, 0])));
        let decoded = image::load_from_memory(&webp(&rgba, GRID_QUALITY).unwrap()).unwrap();
        assert!(decoded.color().has_alpha());
        assert_eq!(decoded.to_rgba8().get_pixel(5, 5)[3], 0);
    }

    #[test]
    fn hires_reads_are_capped_and_abandoned_reads_block_new_ones() {
        let a = HiresSlot::acquire().unwrap();
        let b = HiresSlot::acquire().unwrap();
        assert!(HiresSlot::acquire().is_none(), "at most {HIRES_SLOTS} reads");
        drop(b);
        let c = HiresSlot::acquire().unwrap();
        // `a` missed its deadline but its pool thread still runs: nothing new starts.
        HiresSlot::abandon(&a.state());
        drop(c);
        assert!(HiresSlot::acquire().is_none(), "a stalled read is still running");
        drop(a);
        let d = HiresSlot::acquire().expect("the stalled read finished");
        // Abandoning after it finished changes nothing.
        let state = d.state();
        drop(d);
        HiresSlot::abandon(&state);
        assert!(HiresSlot::acquire().is_some());
    }
}
