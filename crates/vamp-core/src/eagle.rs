//! Eagle library import and incremental sync. Never opens an original: originals are only
//! `stat`ed (lazy fingerprint). Two phases: the blocking import writes catalog rows, folders,
//! tags and smart folders and queues Eagle's own `_thumbnail.png` files in the durable
//! `eagle_thumbnails` table; [`Library::copy_eagle_thumbnails`] copies them into state later
//! (visible items first, then newest first). Drive reads run on a bounded worker pool; SQLite
//! writes stay on the calling thread in batched transactions.
use super::catalog::{
    insert_collection, millis_rfc3339, renumber, save_smart_folder, set_added_by, set_setting,
    swatch,
};
use super::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Instant;

const BATCH: usize = 500;
const MAX_METADATA: u64 = 4 * 1024 * 1024;
/// Concurrent File Provider reads: each one is a network round trip on Google Drive.
const READERS: usize = 24;
/// Thumbnails copied per transaction; small so `prioritize_thumbnails` takes effect quickly.
const THUMB_BATCH: usize = 48;
/// Copy attempts before a queued thumbnail is left alone.
pub(crate) const THUMB_ATTEMPTS: i64 = 3;

/// `kind`: `copy` (Eagle's `_thumbnail.png`, `path`) or `generate` (no Eagle thumbnail: Eagle
/// shows small originals directly, so phase B renders one from the original at `path`).
pub(crate) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS eagle_thumbnails(asset_id TEXT PRIMARY KEY REFERENCES assets(id), kind TEXT NOT NULL DEFAULT 'copy', path TEXT NOT NULL, revision INTEGER NOT NULL, order_key INTEGER NOT NULL, priority INTEGER NOT NULL DEFAULT 0, attempts INTEGER NOT NULL DEFAULT 0, error TEXT);
CREATE INDEX IF NOT EXISTS eagle_thumbnails_order ON eagle_thumbnails(priority DESC, kind, order_key DESC);";

struct Known {
    id: String,
    stamp: String,
    /// Eagle's `mtime.json` value when last imported.
    mt: Option<i64>,
    eagle_deleted: bool,
}

#[derive(Default)]
struct Counts {
    items: u64,
    created: u64,
    updated: u64,
    unchanged: u64,
    trashed: u64,
    restored: u64,
    queued_thumbnails: u64,
    queued_generate: u64,
    missing_thumbnails: u64,
    skipped: Vec<Value>,
}

/// Library-level sync state: the Eagle folder and smart-folder ids imported last time.
#[derive(Default, Serialize, serde::Deserialize)]
struct LibraryState {
    folders: Vec<String>,
    smart_folders: Vec<String>,
}

/// One `<ID>.info` directory after the (parallel, read-only) Drive work.
enum Scan {
    /// `mtime.json` matches the imported value: nothing was read.
    Skipped(String),
    /// Metadata read but its stamp is unchanged; refreshes the remembered `mtime.json` value.
    Unchanged(String, Option<i64>),
    Changed(Box<Prepared>),
    Failed(PathBuf, anyhow::Error),
}
struct Prepared {
    dir: PathBuf,
    raw: Value,
    eid: String,
    stamp: String,
    deleted: bool,
    mt: Option<i64>,
    name: String,
    original: Original,
    thumb: Option<PathBuf>,
}

fn scan(dir: &Path, mtimes: &HashMap<String, i64>, known: &HashMap<String, Known>) -> Scan {
    let stem = dir
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mt = mtimes.get(&stem).copied();
    if mt.is_some() && known.get(&stem).is_some_and(|k| k.mt == mt) {
        return Scan::Skipped(stem);
    }
    let result = (|| -> Result<Scan> {
        let raw = read_json(&dir.join("metadata.json"))?;
        let eid = text(&raw["id"])
            .or_else(|| (!stem.is_empty()).then(|| stem.clone()))
            .context("Eagle item has no id")?;
        let deleted = raw["isDeleted"] == true;
        let stamp = format!(
            "{}:{}:{}:{}",
            int(&raw["modificationTime"]).unwrap_or(0),
            int(&raw["lastModified"]).unwrap_or(0),
            int(&raw["mtime"]).unwrap_or(0),
            deleted
        );
        let mt = mtimes.get(&eid).copied().or(mt);
        if known.get(&eid).is_some_and(|k| k.stamp == stamp) {
            return Ok(Scan::Unchanged(eid, mt));
        }
        let ext = text(&raw["ext"])
            .context("Eagle item has no extension")?
            .to_ascii_lowercase();
        ensure!(
            ext.bytes().all(|b| b.is_ascii_alphanumeric()),
            "invalid Eagle extension"
        );
        let name = text(&raw["name"]).unwrap_or_else(|| eid.clone());
        let (original, thumb) = locate(dir, &name, &ext)?;
        let original = stat_original(&original.canonicalize()?)?;
        Ok(Scan::Changed(Box::new(Prepared {
            dir: dir.to_owned(),
            raw,
            eid,
            stamp,
            deleted,
            mt,
            name,
            original,
            thumb,
        })))
    })();
    result.unwrap_or_else(|e| Scan::Failed(dir.to_owned(), e))
}

/// Runs `work` over `inputs` on up to [`READERS`] threads, handing each result to `sink`
/// on the calling thread as it arrives (unordered). Stops early when `sink` fails.
fn parallel<I: Sync, O: Send>(
    inputs: &[I],
    work: impl Fn(&I) -> O + Sync,
    mut sink: impl FnMut(O) -> Result<()>,
) -> Result<()> {
    let next = AtomicUsize::new(0);
    let (sender, receiver) = mpsc::sync_channel::<O>(READERS * 4);
    std::thread::scope(|scope| {
        for _ in 0..READERS.min(inputs.len()) {
            let sender = sender.clone();
            let (next, work) = (&next, &work);
            scope.spawn(move || {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(input) = inputs.get(index) else {
                        break;
                    };
                    if sender.send(work(input)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(sender);
        let result = receiver.iter().try_for_each(&mut sink);
        if result.is_err() {
            // Unblock and stop the readers.
            next.store(usize::MAX / 2, Ordering::Relaxed);
            drop(receiver);
        }
        result
    })
}

fn read_json(path: &Path) -> Result<Value> {
    let meta = fs::metadata(path)?;
    ensure!(
        meta.len() <= MAX_METADATA,
        "Eagle metadata exceeds size limit"
    );
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn int(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_f64().map(|f| f as i64))
}
fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}
fn u16_le(b: &[u8], at: usize) -> u32 {
    u16::from_le_bytes([b[at], b[at + 1]]) as u32
}
fn u24_le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], 0])
}
/// Image format of thumbnail bytes by magic (Eagle writes WebP into `_thumbnail.png`):
/// (width, height, mime, file extension).
fn image_format(b: &[u8]) -> Option<(u32, u32, &'static str, &'static str)> {
    if b.len() >= 24 && &b[..8] == b"\x89PNG\r\n\x1a\n" && &b[12..16] == b"IHDR" {
        let be = |at: usize| u32::from_be_bytes(b[at..at + 4].try_into().unwrap());
        return Some((be(16), be(20), "image/png", "png"));
    }
    if b.len() >= 10 && (b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a")) {
        return Some((u16_le(b, 6), u16_le(b, 8), "image/gif", "gif"));
    }
    if b.len() >= 30 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        let (w, h) = match &b[12..16] {
            b"VP8X" => (u24_le(b, 24) + 1, u24_le(b, 27) + 1),
            b"VP8 " if b[23..26] == [0x9d, 0x01, 0x2a] => {
                (u16_le(b, 26) & 0x3fff, u16_le(b, 28) & 0x3fff)
            }
            b"VP8L" if b[20] == 0x2f => {
                let bits = u32::from_le_bytes(b[21..25].try_into().unwrap());
                ((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1)
            }
            _ => return None,
        };
        return Some((w, h, "image/webp", "webp"));
    }
    if b.starts_with(&[0xff, 0xd8]) {
        let mut at = 2;
        while at + 9 < b.len() {
            if b[at] != 0xff {
                return None;
            }
            let marker = b[at + 1];
            if marker == 0xff {
                at += 1;
                continue;
            }
            let len = u16::from_be_bytes([b[at + 2], b[at + 3]]) as usize;
            if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
                let h = u16::from_be_bytes([b[at + 5], b[at + 6]]) as u32;
                let w = u16::from_be_bytes([b[at + 7], b[at + 8]]) as u32;
                return Some((w, h, "image/jpeg", "jpg"));
            }
            at += 2 + len;
        }
    }
    None
}
fn eagle_color(value: &Value) -> Option<[u8; 3]> {
    let c = value.as_array()?;
    let channel = |i: usize| c.get(i).and_then(Value::as_u64).map(|v| v.min(255) as u8);
    Some([channel(0)?, channel(1)?, channel(2)?])
}

/// Original and thumbnail inside one `<ID>.info` directory: a single listing (one Drive
/// round trip; stats of listed entries are then served from the provider's cache).
fn locate(dir: &Path, name: &str, ext: &str) -> Result<(PathBuf, Option<PathBuf>)> {
    let preferred = format!("{name}.{ext}");
    let thumbnail = format!("{name}_thumbnail.png");
    let mut entries = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|f| f != "metadata.json" && !f.starts_with('.'))
        .collect::<Vec<_>>();
    entries.sort();
    let pick = |exact: &str, fallback: &dyn Fn(&str) -> bool| {
        entries
            .iter()
            .find(|f| *f == exact)
            .or_else(|| entries.iter().find(|f| fallback(f)))
            .map(|f| dir.join(f))
    };
    let thumb = pick(&thumbnail, &|f| f.ends_with("_thumbnail.png"));
    let original = pick(&preferred, &|f| {
        !f.ends_with("_thumbnail.png")
            && Path::new(f)
                .extension()
                .is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case(ext))
    })
    .context("original file missing from Eagle item")?;
    Ok((original, thumb))
}

/// Snapshot of a running Eagle import, reported after each phase step.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ImportProgress {
    /// scanning | folders | items | trash | hashing | done
    pub phase: &'static str,
    /// Units finished in this phase (items, trashed items or hashed originals).
    pub done: u64,
    pub total: u64,
    pub created: u64,
    pub updated: u64,
    pub trashed: u64,
}

impl Library {
    /// Import or incrementally sync an Eagle library (see contract `native.eagle_import`).
    /// `hash_after` computes the pending fingerprints afterwards (reads every original).
    pub fn import_eagle(&mut self, root: &Path, hash_after: bool) -> Result<Value> {
        self.import_eagle_with(root, hash_after, &mut |_| {})
    }
    /// [`Library::import_eagle`] reporting progress after every batch of up to 500 items.
    pub fn import_eagle_with(
        &mut self,
        root: &Path,
        hash_after: bool,
        progress: &mut dyn FnMut(&ImportProgress),
    ) -> Result<Value> {
        let mut report = |phase: &'static str, done: u64, total: u64, counts: &Counts| {
            progress(&ImportProgress {
                phase,
                done,
                total,
                created: counts.created,
                updated: counts.updated,
                trashed: counts.trashed,
            })
        };
        report("scanning", 0, 0, &Counts::default());
        ensure!(
            self.context.remote.is_none(),
            "Eagle import runs on the authority, not on replicas"
        );
        let root = root.canonicalize().context("Eagle library unavailable")?;
        let images = root.join("images");
        ensure!(
            images.is_dir(),
            "not an Eagle library (missing images/): {}",
            root.display()
        );
        ensure!(
            !self.state_dir.starts_with(&root) && !root.starts_with(&self.state_dir),
            "Eagle library and local catalog state must not contain each other"
        );
        let root_text = path_text(&root)?;
        let library_meta = match root.join("metadata.json") {
            p if p.is_file() => read_json(&p)?,
            _ => json!({}),
        };
        let tags_meta = match root.join("tags.json") {
            p if p.is_file() => read_json(&p)?,
            _ => json!({}),
        };
        let state_key = format!("eagle:{root_text}");
        let previous: LibraryState = self
            .conn
            .query_row(
                "SELECT document FROM settings WHERE key=?1",
                [&state_key],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .map(parse)
            .transpose()?
            .unwrap_or_default();

        // ---- folders, smart folders, tag groups ----
        report("folders", 0, 0, &Counts::default());
        let tx = self.conn.transaction()?;
        let (folder_map, folder_counts) = sync_folders(
            &tx,
            &self.context,
            &library_meta["folders"],
            &previous.folders,
        )?;
        let smart_ids = sync_smart_folders(
            &tx,
            &self.context,
            &library_meta["smartFolders"],
            &previous.smart_folders,
            &folder_map,
        )?;
        let tag_groups = sync_tag_groups(&tx, &self.context, &library_meta, &tags_meta)?;
        set_setting(
            &tx,
            &state_key,
            &LibraryState {
                folders: folder_map.keys().cloned().collect(),
                smart_folders: smart_ids.clone(),
            },
        )?;
        tx.commit()?;
        let eagle_folders: HashSet<String> = folder_map.values().cloned().collect();

        // ---- items ----
        let known: HashMap<String, Known> = {
            let mut stmt = self.conn.prepare(
                "SELECT id, substr(source_id,7), json_extract(document,'$.metadata.extra.eagle.stamp'), coalesce(json_extract(document,'$.metadata.extra.eagle.deleted'),0), json_extract(document,'$.metadata.extra.eagle.mt') FROM assets WHERE deleted=0 AND source_id LIKE 'eagle:%' AND json_extract(document,'$.metadata.extra.eagle.library')=?1",
            )?;
            stmt.query_map([&root_text], |r| {
                Ok((
                    r.get::<_, String>(1)?,
                    Known {
                        id: r.get(0)?,
                        stamp: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        eagle_deleted: r.get(3)?,
                        mt: r.get(4)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<_>>()?
        };
        // Eagle's id → mtime index: unchanged items are skipped without opening them.
        let mtimes: HashMap<String, i64> = match read_json(&root.join("mtime.json")) {
            Ok(Value::Object(map)) => map
                .into_iter()
                .filter_map(|(k, v)| Some((k, int(&v)?)))
                .collect(),
            _ => HashMap::new(),
        };
        let mut dirs = fs::read_dir(&images)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "info"))
            .collect::<Vec<_>>();
        dirs.sort();
        let mut counts = Counts::default();
        let mut seen: HashSet<String> = HashSet::with_capacity(dirs.len());
        let total = dirs.len() as u64;
        report("items", 0, total, &counts);
        let mut flush = |batch: &mut Vec<Scan>, counts: &mut Counts| -> Result<()> {
            let tx = self.conn.transaction()?;
            for scanned in batch.drain(..) {
                counts.items += 1;
                let (dir, result) = match scanned {
                    Scan::Skipped(eid) => {
                        counts.unchanged += 1;
                        seen.insert(eid);
                        continue;
                    }
                    Scan::Unchanged(eid, mt) => {
                        counts.unchanged += 1;
                        if let Some(k) = known.get(&eid).filter(|k| k.mt != mt) {
                            tx.execute(
                                "UPDATE assets SET document=json_set(document,'$.metadata.extra.eagle.mt',?2) WHERE id=?1",
                                params![k.id, mt],
                            )?;
                        }
                        seen.insert(eid);
                        continue;
                    }
                    Scan::Failed(dir, e) => {
                        // Unreadable right now (e.g. a Drive hiccup) is not "removed from Eagle".
                        if let Some(stem) = dir.file_stem() {
                            seen.insert(stem.to_string_lossy().into_owned());
                        }
                        (dir, Err(e))
                    }
                    Scan::Changed(item) => {
                        seen.insert(item.eid.clone());
                        let existing = known.get(&item.eid);
                        let dir = item.dir.clone();
                        let result = Sink {
                            context: &self.context,
                        }
                        .apply_item(
                            &tx,
                            *item,
                            &root_text,
                            &folder_map,
                            &eagle_folders,
                            existing,
                            counts,
                        );
                        (dir, result)
                    }
                };
                if let Err(e) = result {
                    counts
                        .skipped
                        .push(json!({"item":dir,"error":format!("{e:#}")}));
                }
            }
            tx.commit()?;
            report("items", counts.items, total, counts);
            Ok(())
        };
        let mut batch = Vec::with_capacity(BATCH);
        let mut flushed = Instant::now();
        parallel(
            &dirs,
            |dir| scan(dir, &mtimes, &known),
            |scanned| {
                batch.push(scanned);
                if batch.len() >= BATCH || flushed.elapsed() >= Duration::from_secs(1) {
                    flush(&mut batch, &mut counts)?;
                    flushed = Instant::now();
                }
                Ok(())
            },
        )?;
        flush(&mut batch, &mut counts)?;
        // Items whose .info directory disappeared from Eagle go to the trash.
        let gone: Vec<String> = known
            .into_iter()
            .filter(|(eid, _)| !seen.contains(eid))
            .map(|(_, k)| k.id)
            .collect();
        report("trash", 0, gone.len() as u64, &counts);
        for (index, chunk) in gone.chunks(BATCH).enumerate() {
            let tx = self.conn.transaction()?;
            for id in chunk {
                let mut asset = get_asset(&tx, id)?;
                if asset.deleted_at.is_none() {
                    asset.deleted_at = Some(now());
                    if let Some(eagle) = asset.metadata.extra.get_mut("eagle") {
                        eagle["deleted"] = json!(true);
                        eagle["stamp"] = json!("gone");
                    }
                    bump(&mut asset);
                    save_asset(&tx, &asset)?;
                    emit(
                        &tx,
                        &self.context,
                        "media_trashed",
                        Some(&asset),
                        json!({"original_deleted":false,"reason":"eagle_item_missing"}),
                    )?;
                    counts.trashed += 1;
                }
            }
            tx.commit()?;
            report(
                "trash",
                ((index + 1) * BATCH).min(gone.len()) as u64,
                gone.len() as u64,
                &counts,
            );
        }
        let hashed = if hash_after {
            let pending: u64 = self.conn.query_row(
                "SELECT count(*) FROM assets WHERE deleted=0 AND hash_state='pending'",
                [],
                |r| r.get::<_, i64>(0).map(|n| n as u64),
            )?;
            let mut finished = 0u64;
            let mut total = 0u64;
            report("hashing", 0, pending, &counts);
            loop {
                let result = self.hash_pending(100)?;
                let done = result["hashed"].as_u64().unwrap_or(0)
                    + result["changed"].as_u64().unwrap_or(0);
                total += done;
                finished += done + result["failed"].as_array().map_or(0, Vec::len) as u64;
                report("hashing", finished.min(pending), pending, &counts);
                if done == 0 || result["remaining"] == 0 {
                    break;
                }
            }
            Some(total)
        } else {
            None
        };
        report("done", counts.items, counts.items, &counts);
        Ok(json!({
            "library": root_text,
            "items": counts.items,
            "created": counts.created,
            "updated": counts.updated,
            "unchanged": counts.unchanged,
            "trashed": counts.trashed,
            "restored": counts.restored,
            "skipped": counts.skipped,
            "folders": folder_counts,
            "smart_folders": smart_ids.len(),
            "tag_groups": tag_groups,
            "queued_thumbnails": counts.queued_thumbnails,
            "queued_generate": counts.queued_generate,
            "missing_thumbnails": counts.missing_thumbnails,
            "pending_thumbnails": self.pending_eagle_thumbnails()?,
            "read_originals": hashed.unwrap_or(0),
            "hashed": hashed,
        }))
    }
}

/// Item writes inside an open transaction (borrows only the library's identity).
struct Sink<'a> {
    context: &'a Context,
}
impl Sink<'_> {
    #[allow(clippy::too_many_arguments)]
    fn apply_item(
        &self,
        tx: &Transaction,
        input: Prepared,
        root: &str,
        folder_map: &HashMap<String, String>,
        eagle_folders: &HashSet<String>,
        existing: Option<&Known>,
        counts: &mut Counts,
    ) -> Result<()> {
        let raw = &input.raw;
        let name = input.name;
        let original = input.original;
        let thumb = input.thumb;
        let mut tags = strings(&raw["tags"]);
        normalize_tags(&mut tags);
        let mut colors = raw["palettes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| {
                let ratio = p["ratio"].as_f64()?;
                Some(swatch(
                    eagle_color(&p["color"])?,
                    (ratio / 100.0).clamp(0.0, 1.0),
                ))
            })
            .collect::<Vec<_>>();
        colors.sort_by(|a, b| b.ratio.total_cmp(&a.ratio));
        let dimension = |key: &str| {
            raw[key]
                .as_u64()
                .filter(|v| *v > 0 && *v <= u32::MAX as u64)
                .map(|v| v as u32)
        };
        let mut metadata = Metadata {
            description: text(&raw["annotation"]),
            tags,
            source_url: text(&raw["url"]),
            source_id: Some(format!("eagle:{}", input.eid)),
            rating: int(&raw["star"]).unwrap_or(0).clamp(0, 5) as u8,
            colors,
            extra: BTreeMap::new(),
        };
        metadata.extra.insert(
            "eagle".into(),
            json!({"id":input.eid,"library":root,"stamp":input.stamp,"deleted":input.deleted,"mt":input.mt}),
        );
        let imported_at = int(&raw["modificationTime"])
            .and_then(millis_rfc3339)
            .unwrap_or_else(now);
        let folders: Vec<String> = strings(&raw["folders"])
            .iter()
            .filter_map(|f| folder_map.get(f).cloned())
            .collect();
        let mut original = original;
        original.import_root = Some(root.to_owned());
        let (asset, created) = match existing {
            Some(known) => {
                let mut asset = get_asset(tx, &known.id)?;
                let path_changed = asset.original.path != original.path
                    || asset.original.byte_size != original.byte_size
                    || asset.original.modified_ns != original.modified_ns;
                if path_changed {
                    // A different file than cataloged: new (pending) source identity.
                    tx.execute("DELETE FROM representations WHERE asset_id=?1", [&asset.id])?;
                    tx.execute(
                        "UPDATE assets SET path=?2 WHERE id=?1",
                        params![asset.id, original.path],
                    )?;
                    let storage = std::mem::take(&mut asset.original.storage);
                    asset.original = Original {
                        storage,
                        ..original
                    };
                }
                // VAMP-only extras (e.g. clip data) survive; Eagle-owned fields are replaced.
                let mut extra = std::mem::take(&mut asset.metadata.extra);
                extra.extend(metadata.extra);
                metadata.extra = extra;
                asset.name = name;
                asset.metadata = metadata;
                asset.width = dimension("width");
                asset.height = dimension("height");
                asset.original_created_at = int(&raw["btime"]).and_then(millis_rfc3339);
                asset.original_modified_at = int(&raw["mtime"]).and_then(millis_rfc3339);
                if input.deleted && asset.deleted_at.is_none() {
                    asset.deleted_at = Some(now());
                    counts.trashed += 1;
                } else if !input.deleted && known.eagle_deleted && asset.deleted_at.is_some() {
                    asset.deleted_at = None;
                    counts.restored += 1;
                }
                bump(&mut asset);
                save_asset(tx, &asset)?;
                emit(
                    tx,
                    self.context,
                    "media_updated",
                    Some(&asset),
                    json!({"change":"eagle_sync"}),
                )?;
                counts.updated += 1;
                (asset, false)
            }
            None => {
                let taken: Option<String> = tx
                    .query_row(
                        "SELECT id FROM assets WHERE reference_path=?1 ORDER BY deleted,id LIMIT 1",
                        [&original.path],
                        |r| r.get(0),
                    )
                    .optional()?;
                ensure!(
                    taken.is_none(),
                    "original already cataloged as another asset: {}",
                    original.path
                );
                set_added_by(&mut metadata, &current_actor());
                let asset = Asset {
                    id: id(),
                    revision: 1,
                    name,
                    original,
                    metadata,
                    created_at: imported_at.clone(),
                    updated_at: imported_at,
                    width: dimension("width"),
                    height: dimension("height"),
                    original_created_at: int(&raw["btime"]).and_then(millis_rfc3339),
                    original_modified_at: int(&raw["mtime"]).and_then(millis_rfc3339),
                    last_viewed_at: None,
                    deleted_at: input.deleted.then(now),
                };
                tx.prepare_cached("INSERT INTO assets(id,path,hash,document) VALUES(?1,?2,?3,?4)")?
                    .execute(params![
                        asset.id,
                        asset.original.path,
                        asset.original.source_key(),
                        json_text(&asset)?
                    ])?;
                reindex(tx, &asset)?;
                emit(
                    tx,
                    self.context,
                    "media_added",
                    Some(&asset),
                    json!({"storage":"referenced","source":"eagle"}),
                )?;
                counts.created += 1;
                if input.deleted {
                    counts.trashed += 1;
                }
                (asset, true)
            }
        };
        // Folder memberships from Eagle replace previous Eagle ones; VAMP folders stay.
        if !created {
            let mut stmt =
                tx.prepare_cached("SELECT collection_id FROM memberships WHERE asset_id=?1")?;
            let current = stmt
                .query_map([&asset.id], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for collection in current {
                if eagle_folders.contains(&collection) && !folders.contains(&collection) {
                    tx.execute(
                        "DELETE FROM memberships WHERE collection_id=?1 AND asset_id=?2",
                        params![collection, asset.id],
                    )?;
                }
            }
        }
        let mut add = tx.prepare_cached(
            "INSERT OR IGNORE INTO memberships(collection_id,asset_id) VALUES(?1,?2)",
        )?;
        for collection in &folders {
            add.execute(params![collection, asset.id])?;
        }
        // Phase B fills the thumbnail later; the grid shows a placeholder meanwhile.
        let order_key = int(&raw["modificationTime"]).unwrap_or(0);
        let source = match &thumb {
            Some(thumb) => Some(("copy", path_text(thumb)?)),
            None if asset.original.mime.starts_with("image/") => {
                Some(("generate", asset.original.path.clone()))
            }
            None => None,
        };
        match source {
            Some((kind, path)) => {
                tx.prepare_cached("INSERT INTO eagle_thumbnails(asset_id,kind,path,revision,order_key) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(asset_id) DO UPDATE SET kind=excluded.kind, path=excluded.path, revision=excluded.revision, order_key=excluded.order_key, attempts=0, error=NULL")?
                    .execute(params![asset.id, kind, path, asset.revision as i64, order_key])?;
                if kind == "copy" {
                    counts.queued_thumbnails += 1;
                } else {
                    counts.queued_generate += 1;
                }
            }
            None => {
                tx.execute(
                    "DELETE FROM eagle_thumbnails WHERE asset_id=?1",
                    [&asset.id],
                )?;
            }
        }
        if thumb.is_none() {
            counts.missing_thumbnails += 1;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct QueuedThumbnail {
    asset_id: String,
    generate: bool,
    path: PathBuf,
    revision: i64,
}
/// A thumbnail read from Eagle and written into state (not yet attached).
struct CopiedThumbnail {
    relative: String,
    width: u32,
    height: u32,
    mime: &'static str,
    byte_size: u64,
}

/// Reads Eagle's thumbnail (small; hydrates only the thumbnail) and writes it into state.
fn copy_thumbnail(state_dir: &Path, queued: &QueuedThumbnail) -> Result<CopiedThumbnail> {
    let bytes = fs::read(&queued.path)?;
    let (width, height, mime, ext) =
        image_format(&bytes).context("Eagle thumbnail is not a PNG, JPEG, GIF or WebP image")?;
    let relative = format!(
        "representations/eagle/{}-{}.{ext}",
        queued.asset_id,
        id::simple()
    );
    fs::write(state_dir.join(&relative), &bytes)?;
    Ok(CopiedThumbnail {
        relative,
        width,
        height,
        mime,
        byte_size: bytes.len() as u64,
    })
}
/// Materializes an original (Drive placeholder) so generation afterwards reads local bytes.
fn hydrate(path: &Path) -> Result<()> {
    let mut file = fs::File::open(path)?;
    let mut buffer = vec![0u8; 256 * 1024];
    while file.read(&mut buffer)? > 0 {
        crate::io::tick();
    }
    Ok(())
}

impl Library {
    /// Eagle thumbnails still waiting for phase B (excluding ones that failed too often).
    pub fn pending_eagle_thumbnails(&self) -> Result<u64> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM eagle_thumbnails WHERE attempts<?1",
            [THUMB_ATTEMPTS],
            |r| r.get::<_, i64>(0),
        )? as u64)
    }

    /// Moves these assets' queued thumbnails ahead of everything else (latest call wins).
    pub fn prioritize_thumbnails(&mut self, asset_ids: &[String]) -> Result<Value> {
        let tx = self.conn.transaction()?;
        let top: i64 = tx.query_row(
            "SELECT coalesce(max(priority),0)+1 FROM eagle_thumbnails",
            [],
            |r| r.get(0),
        )?;
        let mut queued = 0;
        {
            let mut stmt =
                tx.prepare("UPDATE eagle_thumbnails SET priority=?2 WHERE asset_id=?1")?;
            for id in asset_ids {
                queued += stmt.execute(params![id, top])?;
            }
        }
        tx.commit()?;
        Ok(json!({"queued":queued,"pending":self.pending_eagle_thumbnails()?}))
    }

    /// Assets whose thumbnail became ready after event cursor `since`. Without `since`,
    /// returns only the current cursor (start polling from now).
    pub fn thumbnail_updates(&self, since: Option<u64>) -> Result<Value> {
        let latest: i64 =
            self.conn
                .query_row("SELECT coalesce(max(sequence),0) FROM events", [], |r| {
                    r.get(0)
                })?;
        let Some(since) = since else {
            return Ok(
                json!({"cursor":latest,"asset_ids":[],"pending":self.pending_eagle_thumbnails()?}),
            );
        };
        let mut stmt = self.conn.prepare(
            "SELECT sequence, json_extract(document,'$.asset_id') FROM events WHERE sequence>?1 AND sequence<=?2 AND json_extract(document,'$.kind')='representation_ready' AND json_extract(document,'$.payload.kind')='thumbnail' ORDER BY sequence LIMIT 1000",
        )?;
        let rows = stmt
            .query_map(params![since as i64, latest], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let cursor = if rows.len() == 1000 {
            rows.last().map_or(latest, |r| r.0)
        } else {
            latest.max(since as i64)
        };
        let mut seen = HashSet::new();
        let ids: Vec<String> = rows
            .into_iter()
            .filter_map(|r| r.1)
            .filter(|id| seen.insert(id.clone()))
            .collect();
        Ok(json!({"cursor":cursor,"asset_ids":ids,"pending":self.pending_eagle_thumbnails()?}))
    }

    /// Phase B of an Eagle import: fills up to `limit` queued thumbnails, prioritized items
    /// first, then Eagle thumbnail copies before generated ones, newest first. Drive reads run
    /// in parallel; copies of each small batch are attached in one transaction, so an
    /// interrupted run resumes where it stopped. Items without an Eagle thumbnail are
    /// hydrated in parallel, then rendered by the vamp-media helper one by one. Only one
    /// copier runs per library (`busy: true` when another holds the lock).
    pub fn copy_eagle_thumbnails(
        &mut self,
        limit: usize,
        progress: &mut dyn FnMut(&Value),
    ) -> Result<Value> {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.state_dir.join("eagle-thumbnails.lock"))?;
        if lock.try_lock_exclusive().is_err() {
            return Ok(
                json!({"busy":true,"copied":0,"generated":0,"failed":0,"remaining":self.pending_eagle_thumbnails()?}),
            );
        }
        fs::create_dir_all(self.state_dir.join("representations/eagle"))?;
        let helper = media_helper();
        let (mut copied, mut generated, mut failed, mut done) = (0u64, 0u64, 0u64, 0u64);
        let mut total = self.pending_eagle_thumbnails()?;
        // Drive stopped responding: leave the rest queued (no attempt spent) for a later run.
        let mut stalled = false;
        while (done as usize) < limit && !stalled {
            let queued: Vec<QueuedThumbnail> = {
                let mut stmt = self.conn.prepare_cached(
                    "SELECT asset_id, kind='generate', path, revision FROM eagle_thumbnails WHERE kind!='download' AND attempts<?1 ORDER BY priority DESC, kind, order_key DESC LIMIT ?2",
                )?;
                stmt.query_map(
                    params![
                        THUMB_ATTEMPTS,
                        THUMB_BATCH.min(limit - done as usize) as i64
                    ],
                    |r| {
                        Ok(QueuedThumbnail {
                            asset_id: r.get(0)?,
                            generate: r.get(1)?,
                            path: PathBuf::from(r.get::<_, String>(2)?),
                            revision: r.get(3)?,
                        })
                    },
                )?
                .collect::<rusqlite::Result<_>>()?
            };
            if queued.is_empty() {
                break;
            }
            // Reads finish in any order; attach (and announce) in queue order.
            let mut results: Vec<Option<Result<Option<CopiedThumbnail>>>> =
                queued.iter().map(|_| None).collect();
            let state_dir = &self.state_dir;
            let indices: Vec<usize> = (0..queued.len()).collect();
            parallel(
                &indices,
                |&i| {
                    let q = queued[i].clone();
                    let result = if q.generate && helper.is_none() {
                        Err(anyhow::anyhow!("vamp-media helper not found"))
                    } else {
                        let state_dir = state_dir.clone();
                        crate::io::background().try_run(
                            format!("eagle thumbnail {}", q.asset_id),
                            crate::io::DRIVE_DEADLINE,
                            move || {
                                if q.generate {
                                    hydrate(&q.path).map(|()| None)
                                } else {
                                    copy_thumbnail(&state_dir, &q).map(Some)
                                }
                            },
                        )
                    };
                    (i, result)
                },
                |(i, r)| {
                    results[i] = Some(r);
                    Ok(())
                },
            )?;
            let tx = self.conn.transaction()?;
            let mut obsolete = Vec::new();
            let mut to_generate = Vec::new();
            let fail = |tx: &Transaction, q: &QueuedThumbnail, e: anyhow::Error| {
                tx.execute(
                    "UPDATE eagle_thumbnails SET attempts=attempts+1, error=?3 WHERE asset_id=?1 AND revision=?2",
                    params![q.asset_id, q.revision, format!("{e:#}")],
                )
            };
            for (queued, result) in queued.into_iter().zip(results.into_iter().flatten()) {
                done += 1;
                let copy = match result {
                    Ok(Some(copy)) => copy,
                    Ok(None) => {
                        to_generate.push(queued);
                        continue;
                    }
                    Err(e) if crate::io::is_stalled(&e) => {
                        stalled = true;
                        continue;
                    }
                    Err(e) => {
                        failed += 1;
                        fail(&tx, &queued, e)?;
                        continue;
                    }
                };
                // Gone if a newer sync re-queued (or dropped) this asset meanwhile.
                let current = tx.execute(
                    "DELETE FROM eagle_thumbnails WHERE asset_id=?1 AND revision=?2",
                    params![queued.asset_id, queued.revision],
                )? == 1;
                let asset = get_asset(&tx, &queued.asset_id)?;
                if !current || asset.revision as i64 != queued.revision {
                    obsolete.push(copy.relative);
                    continue;
                }
                let previous: Option<String> = tx
                    .query_row(
                        "SELECT json_extract(document,'$.path') FROM representations WHERE asset_id=?1 AND kind='thumbnail'",
                        [&asset.id],
                        |r| r.get(0),
                    )
                    .optional()?;
                let rep = Representation {
                    kind: "thumbnail".into(),
                    path: copy.relative,
                    source_hash: asset.original.source_key(),
                    width: copy.width,
                    height: copy.height,
                    mime: copy.mime.into(),
                    byte_size: copy.byte_size,
                };
                attach_representation(&tx, &self.context, &asset, &rep)?;
                obsolete.extend(previous.filter(|p| p.starts_with("representations/eagle/")));
                copied += 1;
            }
            tx.commit()?;
            for relative in obsolete {
                let _ = fs::remove_file(self.state_dir.join(relative));
            }
            // Originals are local now; the helper renders them (its own transactions).
            for queued in to_generate {
                if stalled {
                    break;
                }
                let result = match &helper {
                    None => Err(anyhow::anyhow!("vamp-media helper not found")),
                    Some(helper) => {
                        let revision: Option<i64> = self
                            .conn
                            .query_row(
                                "SELECT revision FROM eagle_thumbnails WHERE asset_id=?1",
                                [&queued.asset_id],
                                |r| r.get(0),
                            )
                            .optional()?;
                        if revision != Some(queued.revision) {
                            continue;
                        }
                        self.generate(&queued.asset_id, "thumbnail", helper)
                    }
                };
                let tx = self.conn.transaction()?;
                match result {
                    Err(e) if crate::io::is_stalled(&e) => stalled = true,
                    Ok(_) => {
                        tx.execute(
                            "DELETE FROM eagle_thumbnails WHERE asset_id=?1 AND revision=?2",
                            params![queued.asset_id, queued.revision],
                        )?;
                        generated += 1;
                    }
                    Err(e) => {
                        failed += 1;
                        fail(&tx, &queued, e)?;
                    }
                }
                tx.commit()?;
            }
            // Phase A may queue more while this runs.
            total = total.max(done + self.pending_eagle_thumbnails()?);
            progress(
                &json!({"done":done,"total":total,"copied":copied,"generated":generated,"failed":failed,"finished":false}),
            );
        }
        Ok(json!({
            "copied": copied,
            "generated": generated,
            "failed": failed,
            "remaining": self.pending_eagle_thumbnails()?,
        }))
    }
}

/// Upserts Eagle folders (nested `children`) keyed by `eagle:<id>`; deletes previously
/// imported ones that disappeared. Returns Eagle id → collection id and counts.
fn sync_folders(
    tx: &Transaction,
    context: &Context,
    tree: &Value,
    previous: &[String],
) -> Result<(HashMap<String, String>, Value)> {
    let existing: HashMap<String, Collection> = catalog::collection_rows(tx)?
        .into_iter()
        .filter_map(|c| Some((c.source_id.clone()?.strip_prefix("eagle:")?.to_owned(), c)))
        .collect();
    let mut map = HashMap::new();
    let (mut created, mut updated) = (0u64, 0u64);
    #[allow(clippy::too_many_arguments)]
    fn walk(
        tx: &Transaction,
        context: &Context,
        nodes: &Value,
        parent: Option<&str>,
        existing: &HashMap<String, Collection>,
        map: &mut HashMap<String, String>,
        created: &mut u64,
        updated: &mut u64,
    ) -> Result<()> {
        for (index, node) in nodes.as_array().into_iter().flatten().enumerate() {
            let Some(eid) = text(&node["id"]) else {
                continue;
            };
            if map.contains_key(&eid) {
                continue;
            }
            let previous = existing.get(&eid);
            let folder = Collection {
                id: previous.map(|c| c.id.clone()).unwrap_or_else(id),
                name: text(&node["name"]).unwrap_or_else(|| eid.clone()),
                asset_ids: vec![],
                parent_id: parent.map(str::to_owned),
                sort_order: index as i64,
                description: text(&node["description"]),
                color: text(&node["iconColor"]),
                icon: text(&node["icon"]),
                source_id: Some(format!("eagle:{eid}")),
            };
            let changed = previous.is_none_or(|p| {
                (
                    &p.name,
                    &p.parent_id,
                    p.sort_order,
                    &p.description,
                    &p.color,
                    &p.icon,
                ) != (
                    &folder.name,
                    &folder.parent_id,
                    folder.sort_order,
                    &folder.description,
                    &folder.color,
                    &folder.icon,
                )
            });
            if changed {
                insert_collection(tx, &folder)?;
                let kind = if previous.is_some() {
                    *updated += 1;
                    "collection_updated"
                } else {
                    *created += 1;
                    "collection_created"
                };
                emit(
                    tx,
                    context,
                    kind,
                    None,
                    json!({"collection_id":folder.id,"name":folder.name,"operation":"eagle_sync"}),
                )?;
            }
            map.insert(eid, folder.id.clone());
            walk(
                tx,
                context,
                &node["children"],
                Some(&folder.id),
                existing,
                map,
                created,
                updated,
            )?;
        }
        Ok(())
    }
    walk(
        tx,
        context,
        tree,
        None,
        &existing,
        &mut map,
        &mut created,
        &mut updated,
    )?;
    let mut removed = 0u64;
    for eid in previous {
        if map.contains_key(eid) {
            continue;
        }
        let Some(folder) = existing.get(eid) else {
            continue;
        };
        tx.execute(
            "DELETE FROM memberships WHERE collection_id=?1",
            [&folder.id],
        )?;
        tx.execute(
            "UPDATE collections SET parent_id=?2 WHERE parent_id=?1",
            params![folder.id, folder.parent_id],
        )?;
        tx.execute("DELETE FROM collections WHERE id=?1", [&folder.id])?;
        renumber(tx, folder.parent_id.as_deref())?;
        emit(
            tx,
            context,
            "collection_deleted",
            None,
            json!({"collection_id":folder.id,"name":folder.name,"reason":"eagle_folder_removed"}),
        )?;
        removed += 1;
    }
    Ok((
        map,
        json!({"created":created,"updated":updated,"removed":removed}),
    ))
}

/// Eagle smartFolders → smart folders keyed by `eagle:<id>`.
fn sync_smart_folders(
    tx: &Transaction,
    context: &Context,
    tree: &Value,
    previous: &[String],
    folder_map: &HashMap<String, String>,
) -> Result<Vec<String>> {
    let existing: HashMap<String, SmartFolder> =
        documents::<SmartFolder>(tx, "SELECT document FROM smart_folders", [])?
            .into_iter()
            .filter_map(|f| Some((f.source_id.clone()?.strip_prefix("eagle:")?.to_owned(), f)))
            .collect();
    let mut seen = Vec::new();
    let mut changed = false;
    let mut stack: Vec<(Value, Option<String>, usize)> = tree
        .as_array()
        .into_iter()
        .flatten()
        .cloned()
        .enumerate()
        .map(|(i, v)| (v, None, i))
        .collect();
    stack.reverse();
    while let Some((node, parent, index)) = stack.pop() {
        let Some(eid) = text(&node["id"]) else {
            continue;
        };
        let (rules, unsupported) = translate_conditions(&node["conditions"], folder_map);
        let folder = SmartFolder {
            id: existing.get(&eid).map(|f| f.id.clone()).unwrap_or_else(id),
            name: text(&node["name"]).unwrap_or_else(|| eid.clone()),
            rules,
            parent_id: parent,
            sort_order: index as i64,
            source_id: Some(format!("eagle:{eid}")),
            unsupported,
        };
        if existing
            .get(&eid)
            .is_none_or(|old| json_text(old).ok() != json_text(&folder).ok())
        {
            save_smart_folder(tx, &folder)?;
            changed = true;
        }
        let children = node["children"].as_array().cloned().unwrap_or_default();
        for (i, child) in children.into_iter().enumerate().rev() {
            stack.push((child, Some(folder.id.clone()), i));
        }
        seen.push(eid);
    }
    for eid in previous {
        if !seen.contains(eid)
            && let Some(old) = existing.get(eid)
        {
            tx.execute("DELETE FROM smart_folders WHERE id=?1", [&old.id])?;
            changed = true;
        }
    }
    if changed {
        emit(
            tx,
            context,
            "smart_folder_updated",
            None,
            json!({"operation":"eagle_sync"}),
        )?;
    }
    Ok(seen)
}

/// Best-effort translation of Eagle smart-folder conditions. One condition maps to
/// match all/any; further conditions with a different match, negated conditions
/// (`boolean: "FALSE"`) and unknown properties/methods are kept as `unsupported`.
fn translate_conditions(
    conditions: &Value,
    folder_map: &HashMap<String, String>,
) -> (SmartRules, Vec<Value>) {
    let mut rules = SmartRules {
        match_: "all".into(),
        rules: vec![],
    };
    let mut unsupported = Vec::new();
    for (index, condition) in conditions.as_array().into_iter().flatten().enumerate() {
        let match_ = if condition["match"].as_str() == Some("OR") {
            "any"
        } else {
            "all"
        };
        let negated = condition["boolean"].as_str() == Some("FALSE");
        if index == 0 && !negated {
            rules.match_ = match_.into();
        } else if negated || match_ != rules.match_ {
            unsupported.push(condition.clone());
            continue;
        }
        for rule in condition["rules"].as_array().into_iter().flatten() {
            match translate_rule(rule, folder_map) {
                Some(r)
                    if catalog::validate_rules(&SmartRules {
                        match_: "all".into(),
                        rules: vec![r.clone()],
                    })
                    .is_ok() =>
                {
                    rules.rules.push(r)
                }
                _ => unsupported.push(rule.clone()),
            }
        }
    }
    (rules, unsupported)
}
fn translate_rule(rule: &Value, folder_map: &HashMap<String, String>) -> Option<SmartRule> {
    let property = rule["property"].as_str()?;
    let method = rule["method"].as_str()?;
    let value = rule["value"].clone();
    let first = |v: &Value| match v {
        Value::Array(items) => items.first().cloned().unwrap_or(Value::Null),
        other => other.clone(),
    };
    let text_op = |m: &str| {
        Some(match m {
            "contain" => "contains",
            "uncontain" => "not_contains",
            "equal" | "is" => "is",
            "unequal" | "isNot" => "is_not",
            "startWith" | "startsWith" => "starts_with",
            "endWith" | "endsWith" => "ends_with",
            "empty" => "empty",
            "not-empty" | "notEmpty" => "not_empty",
            _ => return None,
        })
    };
    let number_op = |m: &str| {
        Some(match m {
            "=" | "equal" => "eq",
            "!=" | "unequal" => "ne",
            ">" => "gt",
            ">=" => "gte",
            "<" => "lt",
            "<=" => "lte",
            "between" => "between",
            _ => return None,
        })
    };
    let (field, op, value) = match property {
        "name" | "annotation" | "url" => (
            match property {
                "annotation" => "description",
                other => other,
            },
            text_op(method)?,
            first(&value),
        ),
        "tags" => (
            "tags",
            match method {
                "union" | "contain" => "any",
                "intersection" | "identity" => "all",
                "uncontain" => "not_contains",
                "empty" => "none",
                _ => return None,
            },
            value,
        ),
        "type" | "ext" => (
            "ext",
            match method {
                "equal" | "contain" | "union" => "in",
                "unequal" | "uncontain" => "is_not",
                _ => return None,
            },
            value,
        ),
        "rating" | "star" | "width" | "height" | "size" | "fileSize" => {
            let field = match property {
                "star" => "rating",
                "fileSize" => "size",
                other => other,
            };
            let op = number_op(method)?;
            let value = if op == "between" {
                value
            } else {
                first(&value)
            };
            (field, op, value)
        }
        "createTime" | "btime" | "mtime" | "modificationTime" | "importTime" => {
            let field = match property {
                "mtime" => "modified",
                "modificationTime" | "importTime" => "imported",
                _ => "created",
            };
            let date = |v: &Value| match v {
                Value::Number(n) => n.as_i64().and_then(millis_rfc3339).map(Value::String),
                Value::String(_) => Some(v.clone()),
                _ => None,
            };
            match method {
                "within" => (field, "within_days", first(&value)),
                "before" => (field, "before", date(&first(&value))?),
                "after" => (field, "after", date(&first(&value))?),
                "between" => {
                    let items = value.as_array()?;
                    (
                        field,
                        "between",
                        json!([date(items.first()?)?, date(items.get(1)?)?]),
                    )
                }
                _ => return None,
            }
        }
        "color" => {
            let hex = match first(&value) {
                Value::String(s) => s,
                Value::Array(c) => {
                    let c = eagle_color(&Value::Array(c))?;
                    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
                }
                _ => return None,
            };
            ("color", "similar", json!(hex))
        }
        "shape" => (
            "shape",
            match method {
                "equal" | "is" => "is",
                "unequal" | "isNot" => "is_not",
                _ => return None,
            },
            first(&value),
        ),
        "folders" | "folder" => {
            let ids: Vec<String> = match &value {
                Value::Array(items) => items
                    .iter()
                    .filter_map(Value::as_str)
                    .filter_map(|f| folder_map.get(f).cloned())
                    .collect(),
                Value::String(f) => folder_map.get(f).cloned().into_iter().collect(),
                _ => vec![],
            };
            match method {
                "empty" => ("folder", "none", Value::Null),
                "contain" | "union" | "equal" if !ids.is_empty() => ("folder", "in", json!(ids)),
                "uncontain" | "unequal" if !ids.is_empty() => ("folder", "not_in", json!(ids)),
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(SmartRule {
        field: field.into(),
        op: op.into(),
        value,
    })
}

/// tagsGroups → tag groups `eagle:<id>`; tags.json starredTags → starred tags.
fn sync_tag_groups(
    tx: &Transaction,
    context: &Context,
    library: &Value,
    tags: &Value,
) -> Result<usize> {
    let groups: Vec<TagGroup> = library["tagsGroups"]
        .as_array()
        .or(library["tagGroups"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|g| {
            let mut tags = strings(&g["tags"]);
            normalize_tags(&mut tags);
            Some(TagGroup {
                id: format!("eagle:{}", text(&g["id"])?),
                name: text(&g["name"]).unwrap_or_default(),
                tags,
                color: text(&g["color"]),
            })
        })
        .collect();
    let before = documents::<TagGroup>(
        tx,
        "SELECT document FROM tag_groups WHERE id LIKE 'eagle:%' ORDER BY sort_order",
        [],
    )?;
    let mut starred = strings(&tags["starredTags"]);
    normalize_tags(&mut starred);
    let previous_starred: Option<String> = tx
        .query_row(
            "SELECT document FROM settings WHERE key='starred_tags'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let starred_changed = previous_starred.as_deref() != Some(json_text(&starred)?.as_str());
    if json_text(&before)? != json_text(&groups)? || starred_changed {
        tx.execute("DELETE FROM tag_groups WHERE id LIKE 'eagle:%'", [])?;
        for (index, group) in groups.iter().enumerate() {
            tx.execute(
                "INSERT INTO tag_groups(id,sort_order,document) VALUES(?1,?2,?3)",
                params![group.id, index as i64, json_text(group)?],
            )?;
        }
        set_setting(tx, "starred_tags", &starred)?;
        emit(tx, context, "tag_groups_updated", None, json!({}))?;
    }
    Ok(groups.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn translates_eagle_rules() {
        let folders = HashMap::from([("F1".to_owned(), "c1".to_owned())]);
        let (rules, unsupported) = translate_conditions(
            &json!([{"match":"AND","boolean":"TRUE","rules":[
                {"property":"rating","method":">=","value":[4]},
                {"property":"type","method":"equal","value":"png"},
                {"property":"folders","method":"contain","value":["F1"]},
                {"property":"comments","method":"contain","value":"x"}
            ]}]),
            &folders,
        );
        assert_eq!(rules.match_, "all");
        assert_eq!(rules.rules.len(), 3);
        assert_eq!(rules.rules[0].op, "gte");
        assert_eq!(rules.rules[2].value, json!(["c1"]));
        assert_eq!(unsupported.len(), 1);
    }
}
