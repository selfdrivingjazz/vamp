//! Eagle-style organization over the catalog: trash, nested folders, smart folders, tag
//! groups, lazy fingerprints, sidebar counts, original export and palette helpers.
//!
//! Browse/sidebar filters run on indexed virtual columns of `assets` (generated from the
//! JSON document, so every write path keeps them current) and on `asset_tags`, which
//! `reindex` maintains.
use super::*;
use rusqlite::functions::FunctionFlags;
use rusqlite::types::Value as Sql;
use std::collections::{BTreeMap, HashMap, HashSet};

/// The filesystem phase of a lazy fingerprint ([`Library::hash_job`]): owns what it needs,
/// so it can run on an [`crate::io`] pool without the library.
pub struct HashJob {
    lookup: OriginalLookup,
    path: String,
}

impl HashJob {
    /// Reads (hydrating when needed) and fingerprints the original. May block on a stalled
    /// cloud folder.
    pub fn run(self) -> Result<Original> {
        let mut original = fingerprint(&self.lookup.fetch()?)?;
        // A cache copy keeps the catalog's reference path.
        original.path = self.path;
        Ok(original)
    }
}

/// BLAKE3 digest and byte size of each intake candidate file, in order, hashed on up to
/// eight threads (drops can hold hundreds of files).
fn digest_files(files: &[PathBuf]) -> Vec<Result<(String, u64)>> {
    let workers = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(8)
        .min(files.len());
    if workers <= 1 {
        return files.iter().map(|path| candidate_digest(path)).collect();
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let done: Vec<(usize, Result<(String, u64)>)> = std::thread::scope(|scope| {
        let handles = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(path) = files.get(i) else { break };
                        done.push((i, candidate_digest(path)));
                    }
                    done
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap_or_default())
            .collect()
    });
    let mut results: Vec<Option<Result<(String, u64)>>> = files.iter().map(|_| None).collect();
    for (i, result) in done {
        results[i] = Some(result);
    }
    results
        .into_iter()
        .map(|r| r.unwrap_or_else(|| Err(anyhow::anyhow!("hashing failed"))))
        .collect()
}
fn candidate_digest(path: &Path) -> Result<(String, u64)> {
    let file = File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    ensure!(file.metadata()?.is_file(), "not a regular file");
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(&file)?;
    Ok((hasher.finalize().to_hex().to_string(), hasher.count()))
}
/// Whether `filename` names `asset` (case-insensitive): its name, its name without the
/// candidate's extension (Eagle names omit it), or its original's file name.
fn same_file_name(asset: &Asset, filename: &str) -> bool {
    let wanted = filename.to_lowercase();
    let stem = Path::new(filename)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase());
    let name = asset.name.to_lowercase();
    let original = match (&asset.original.locator, asset.original.path.is_empty()) {
        (Some(locator), true) => locator.as_str(),
        _ => asset.original.path.as_str(),
    };
    let original = original.rsplit('/').next().unwrap_or_default().to_lowercase();
    name == wanted || stem.as_deref() == Some(name.as_str()) || original == wanted
}

/// `(name, expression)` of the virtual columns added to `assets`.
const PATH: &str = "coalesce(nullif(json_extract(document,'$.original.path'),''),json_extract(document,'$.original.locator'),'')";
fn virtual_columns() -> Vec<(&'static str, String)> {
    let suffix = format!("replace({PATH},rtrim({PATH},replace({PATH},'.','')),'')");
    vec![
        (
            "trashed",
            "json_extract(document,'$.deleted_at') IS NOT NULL".into(),
        ),
        (
            "rating",
            "coalesce(json_extract(document,'$.metadata.rating'),0)".into(),
        ),
        ("width", "json_extract(document,'$.width')".into()),
        ("height", "json_extract(document,'$.height')".into()),
        (
            "ext",
            format!("lower(CASE WHEN instr({suffix},'/')=0 THEN {suffix} ELSE '' END)"),
        ),
        (
            "byte_size",
            "json_extract(document,'$.original.byte_size')".into(),
        ),
        ("name_key", "json_extract(document,'$.name')".into()),
        ("imported", "json_extract(document,'$.created_at')".into()),
        (
            "created_key",
            "coalesce(json_extract(document,'$.original_created_at'),json_extract(document,'$.created_at'))".into(),
        ),
        (
            "modified_key",
            "coalesce(json_extract(document,'$.original_modified_at'),json_extract(document,'$.updated_at'))".into(),
        ),
        (
            "last_viewed",
            "json_extract(document,'$.last_viewed_at')".into(),
        ),
        (
            "source_id",
            "json_extract(document,'$.metadata.source_id')".into(),
        ),
        (
            "hash_state",
            "coalesce(json_extract(document,'$.original.hash_state'),'hashed')".into(),
        ),
        (
            "reference_path",
            "json_extract(document,'$.original.path')".into(),
        ),
        ("colors", "json_extract(document,'$.metadata.colors')".into()),
        (
            "tag_count",
            "coalesce(json_array_length(document,'$.metadata.tags'),0)".into(),
        ),
        (
            "added_by",
            format!("json_extract(document,'$.metadata.extra.{ADDED_BY}')"),
        ),
    ]
}

pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    register_functions(conn)?;
    let collections_sql: String = conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name='collections'",
        [],
        |r| r.get(0),
    )?;
    if !collections_sql.contains("parent_id") {
        // Folder names are unique per parent at most (Eagle allows duplicates); rebuild
        // without the old global UNIQUE(name) and with the nesting columns.
        conn.execute_batch(
            "PRAGMA foreign_keys=OFF;
BEGIN;
CREATE TABLE collections_new(id TEXT PRIMARY KEY, name TEXT NOT NULL, parent_id TEXT, sort_order INTEGER NOT NULL DEFAULT 0, description TEXT, color TEXT, icon TEXT, source_id TEXT);
INSERT INTO collections_new(id,name,sort_order) SELECT id,name,row_number() OVER (ORDER BY name,id)-1 FROM collections;
DROP TABLE collections;
ALTER TABLE collections_new RENAME TO collections;
COMMIT;
PRAGMA foreign_keys=ON;",
        )?;
    }
    let tags_missing: bool = conn.query_row(
        "SELECT count(*)=0 FROM sqlite_master WHERE type='table' AND name='asset_tags'",
        [],
        |r| r.get(0),
    )?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS collections_parent ON collections(parent_id,sort_order);
CREATE INDEX IF NOT EXISTS collections_source ON collections(source_id);
CREATE INDEX IF NOT EXISTS memberships_asset ON memberships(asset_id);
CREATE TABLE IF NOT EXISTS asset_tags(tag TEXT NOT NULL, asset_id TEXT NOT NULL, PRIMARY KEY(tag,asset_id)) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS asset_tags_asset ON asset_tags(asset_id);
CREATE TABLE IF NOT EXISTS smart_folders(id TEXT PRIMARY KEY, document TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tag_groups(id TEXT PRIMARY KEY, sort_order INTEGER NOT NULL, document TEXT NOT NULL);",
    )?;
    if tags_missing {
        conn.execute(
            "INSERT OR IGNORE INTO asset_tags(tag,asset_id) SELECT t.value,a.id FROM assets a, json_each(a.document,'$.metadata.tags') t WHERE a.deleted=0",
            [],
        )?;
    }
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_xinfo('assets')")?;
    let existing: HashSet<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for (name, expression) in virtual_columns() {
        if !existing.contains(name) {
            conn.execute_batch(&format!(
                "ALTER TABLE assets ADD COLUMN {name} GENERATED ALWAYS AS ({expression}) VIRTUAL"
            ))?;
        }
    }
    // Before the index below exists on an upgraded catalog: one table pass, not a per-row
    // index update.
    backfill_added_by(conn)?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS assets_imported ON assets(deleted,trashed,imported);
CREATE INDEX IF NOT EXISTS assets_name ON assets(deleted,trashed,name_key COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS assets_rating ON assets(deleted,trashed,rating);
CREATE INDEX IF NOT EXISTS assets_size ON assets(deleted,trashed,byte_size);
CREATE INDEX IF NOT EXISTS assets_created ON assets(deleted,trashed,created_key);
CREATE INDEX IF NOT EXISTS assets_modified ON assets(deleted,trashed,modified_key);
CREATE INDEX IF NOT EXISTS assets_viewed ON assets(deleted,trashed,last_viewed);
CREATE INDEX IF NOT EXISTS assets_ext ON assets(deleted,trashed,ext);
CREATE INDEX IF NOT EXISTS assets_source ON assets(source_id);
CREATE INDEX IF NOT EXISTS assets_hash_state ON assets(deleted,hash_state);
CREATE INDEX IF NOT EXISTS assets_hash ON assets(hash);
CREATE INDEX IF NOT EXISTS assets_reference_path ON assets(reference_path);
CREATE INDEX IF NOT EXISTS assets_id_live ON assets(id,deleted,trashed);
CREATE INDEX IF NOT EXISTS assets_added_by ON assets(added_by,deleted,trashed);
CREATE INDEX IF NOT EXISTS assets_facets ON assets(deleted,trashed,rating,width,height,ext,byte_size,last_viewed,tag_count,id,colors);",
    )?;
    Ok(())
}

/// Catalogs predating `added_by`: copies each asset's adder from its `media_added` event,
/// once (the `added_by_backfill` setting; assets without such an event stay unknown, i.e.
/// the owner's). Replicas have no authority events; they mirror the field instead
/// (`remote::mirror_added_by`).
fn backfill_added_by(conn: &Connection) -> Result<()> {
    let done = |conn: &Connection| -> Result<bool> {
        Ok(conn
            .query_row(
                "SELECT 1 FROM settings WHERE key='added_by_backfill'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    };
    if done(conn)? {
        return Ok(());
    }
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<usize> {
        // Another process may have finished it while this one waited for the lock.
        if done(conn)? {
            return Ok(0);
        }
        let updated = conn.execute(
            &format!(
                "UPDATE assets SET document=json_set(document,'$.metadata.extra.{ADDED_BY}',e.actor) \
                 FROM (SELECT json_extract(document,'$.asset_id') AS asset_id, json_extract(document,'$.actor') AS actor, min(sequence) \
                       FROM events WHERE json_extract(document,'$.kind')='media_added' GROUP BY 1) AS e \
                 WHERE assets.id=e.asset_id AND assets.added_by IS NULL AND e.actor IS NOT NULL"
            ),
            [],
        )?;
        conn.execute(
            "INSERT OR REPLACE INTO settings(key,document) VALUES('added_by_backfill',json_object('assets',?1))",
            [updated as i64],
        )?;
        Ok(updated)
    })();
    match result {
        Ok(_) => conn.execute_batch("COMMIT")?,
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(error);
        }
    }
    Ok(())
}

/// `vamp_color_distance(colors_json, "rrggbb,…")` and `vamp_shuffle(rowid, seed)`.
fn register_functions(conn: &Connection) -> Result<()> {
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    conn.create_scalar_function("vamp_color_distance", 2, flags, |ctx| {
        let colors: Option<String> = ctx.get(0)?;
        let query: String = ctx.get(1)?;
        let Some(colors) = colors else {
            return Ok(None);
        };
        let swatches: Vec<ColorSwatch> = serde_json::from_str(&colors).unwrap_or_default();
        let targets = query.split(',').filter_map(hex_to_lab).collect::<Vec<_>>();
        Ok(color_distance(&swatches, &targets))
    })?;
    conn.create_scalar_function("vamp_shuffle", 2, flags, |ctx| {
        let row: i64 = ctx.get(0)?;
        let seed: i64 = ctx.get(1)?;
        // splitmix64: a stable pseudo-random permutation key per (row, seed).
        let mut z = (row as u64)
            .wrapping_add((seed as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15))
            .wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        Ok((z ^ (z >> 31)) as i64)
    })?;
    Ok(())
}

/// CIELAB (D65) of `#rrggbb` / `rrggbb`.
pub fn hex_to_lab(hex: &str) -> Option<[f64; 3]> {
    let hex = hex.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some(rgb_to_lab([channel(0)?, channel(2)?, channel(4)?]))
}
fn rgb_to_lab(rgb: [u8; 3]) -> [f64; 3] {
    let linear = |c: u8| {
        let c = f64::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = (linear(rgb[0]), linear(rgb[1]), linear(rgb[2]));
    let x = (0.4124 * r + 0.3576 * g + 0.1805 * b) / 0.95047;
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let z = (0.0193 * r + 0.1192 * g + 0.9505 * b) / 1.08883;
    let f = |t: f64| {
        if t > 0.008856 {
            t.cbrt()
        } else {
            7.787 * t + 16.0 / 116.0
        }
    };
    let (fx, fy, fz) = (f(x), f(y), f(z));
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}
/// Palette distance to the query colors: for each query color the best swatch by
/// ΔE76 + 20·(1 − ratio) (a dominant exact match scores ~0, a speck of it ~20), averaged
/// over query colors. `None` without swatches or query colors.
pub fn color_distance(swatches: &[ColorSwatch], targets: &[[f64; 3]]) -> Option<f64> {
    if swatches.is_empty() || targets.is_empty() {
        return None;
    }
    let labs = swatches
        .iter()
        .filter_map(|s| Some((hex_to_lab(&s.hex)?, s.ratio.clamp(0.0, 1.0))))
        .collect::<Vec<_>>();
    if labs.is_empty() {
        return None;
    }
    let total: f64 = targets
        .iter()
        .map(|t| {
            labs.iter()
                .map(|(lab, ratio)| {
                    let d = ((lab[0] - t[0]).powi(2)
                        + (lab[1] - t[1]).powi(2)
                        + (lab[2] - t[2]).powi(2))
                    .sqrt();
                    d + 20.0 * (1.0 - ratio)
                })
                .fold(f64::INFINITY, f64::min)
        })
        .sum();
    Some(total / targets.len() as f64)
}
pub(crate) fn swatch(rgb: [u8; 3], ratio: f64) -> ColorSwatch {
    ColorSwatch {
        hex: format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2]),
        ratio,
    }
}

/// Normalize a browse/smart-folder date bound for comparison with stored RFC 3339 UTC
/// strings. `YYYY-MM-DD` upper bounds include the whole day.
pub fn browse_date(text: &str, upper: bool) -> Result<String> {
    let text = text.trim();
    if text.len() == 10 && chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").is_ok() {
        // 'U' sorts after the 'T' separator: every time on that day is below it.
        return Ok(if upper {
            format!("{text}U")
        } else {
            text.to_owned()
        });
    }
    let parsed = chrono::DateTime::parse_from_rfc3339(text)
        .with_context(|| format!("invalid date (use YYYY-MM-DD or RFC 3339): {text}"))?;
    Ok(parsed.with_timezone(&Utc).to_rfc3339())
}
pub(crate) fn millis_rfc3339(ms: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.to_rfc3339())
}

/// SQL fragment over `assets a` with positional arguments.
#[derive(Default)]
pub(crate) struct Clause {
    pub sql: String,
    pub args: Vec<Sql>,
}
impl Clause {
    fn new(sql: impl Into<String>, args: Vec<Sql>) -> Self {
        Self {
            sql: sql.into(),
            args,
        }
    }
    /// Appends ` AND (<other>)`.
    pub fn and(&mut self, other: Clause) {
        self.sql.push_str(" AND (");
        self.sql.push_str(&other.sql);
        self.sql.push(')');
        self.args.extend(other.args);
    }
}
fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}
fn texts(values: &[String]) -> Vec<Sql> {
    values.iter().map(|v| Sql::Text(v.clone())).collect()
}
pub(crate) fn tags_all(tags: &[String]) -> Clause {
    let mut clause = Clause::new("1", vec![]);
    for tag in tags {
        clause.and(Clause::new(
            "a.id IN (SELECT asset_id FROM asset_tags WHERE tag=?)",
            vec![Sql::Text(tag.clone())],
        ));
    }
    clause
}
pub(crate) fn tags_any(tags: &[String]) -> Clause {
    Clause::new(
        format!(
            "a.id IN (SELECT asset_id FROM asset_tags WHERE tag IN ({}))",
            placeholders(tags.len())
        ),
        texts(tags),
    )
}
pub(crate) fn untagged() -> Clause {
    Clause::new("a.tag_count=0", vec![])
}
pub(crate) fn uncategorized() -> Clause {
    Clause::new("a.id NOT IN (SELECT asset_id FROM memberships)", vec![])
}
pub(crate) fn in_folders(ids: &[String]) -> Clause {
    Clause::new(
        format!(
            "a.id IN (SELECT asset_id FROM memberships WHERE collection_id IN ({}))",
            placeholders(ids.len())
        ),
        texts(ids),
    )
}
pub(crate) fn exts(exts: &[String]) -> Clause {
    let lower = exts
        .iter()
        .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
        .collect::<Vec<_>>();
    Clause::new(
        format!("a.ext IN ({})", placeholders(lower.len())),
        texts(&lower),
    )
}
pub(crate) fn shape(shape: &str) -> Result<Clause> {
    let sql = match shape {
        "landscape" => "a.width > a.height*1.05 AND a.width < a.height*2",
        "portrait" => "a.height > a.width*1.05 AND a.height < a.width*2",
        "square" => "abs(a.width-a.height) <= 0.05*max(a.width,a.height)",
        "panorama" => "a.width >= a.height*2 OR a.height >= a.width*2",
        other => anyhow::bail!("unknown shape: {other} (landscape|portrait|square|panorama)"),
    };
    Ok(Clause::new(
        format!("a.width > 0 AND a.height > 0 AND ({sql})"),
        vec![],
    ))
}
/// Query colors as the `vamp_color_distance` argument; validates hex.
pub(crate) fn color_query(colors: &[String]) -> Result<String> {
    colors
        .iter()
        .map(|c| {
            ensure!(hex_to_lab(c).is_some(), "invalid color (#rrggbb): {c}");
            Ok(c.trim().trim_start_matches('#').to_ascii_lowercase())
        })
        .collect::<Result<Vec<_>>>()
        .map(|v| v.join(","))
}
pub(crate) const COLOR_DISTANCE: &str = "vamp_color_distance(a.colors,?)";
/// Covering index of every facet column; see `BrowseFilter::uses_facets`.
#[cfg(feature = "native")]
pub(crate) const FACETS_INDEX: &str = "assets_facets";
pub(crate) fn colors_within(query: &str, tolerance: f64) -> Clause {
    Clause::new(
        format!("{COLOR_DISTANCE} <= ?"),
        vec![Sql::Text(query.into()), Sql::Real(tolerance)],
    )
}
pub(crate) fn date_column(field: &str) -> Result<&'static str> {
    Ok(match field {
        "created" => "a.created_key",
        "imported" => "a.imported",
        "modified" => "a.modified_key",
        other => anyhow::bail!("unknown date field: {other} (created|imported|modified)"),
    })
}

// ---- added by ----

/// `metadata.extra` key naming who added an asset: the actor of its `media_added` event,
/// set when the asset is created and backfilled from events by [`migrate`].
pub const ADDED_BY: &str = "added_by";
/// Agents act as `agent:<name>`; every other actor (`local`, `tailnet:<login>/<host>`, an
/// unknown adder) is the owner.
pub fn is_agent(actor: &str) -> bool {
    actor.starts_with("agent:")
}
pub fn added_by(asset: &Asset) -> Option<&str> {
    asset.metadata.extra.get(ADDED_BY).and_then(Value::as_str)
}
pub(crate) fn set_added_by(metadata: &mut Metadata, actor: &str) {
    metadata.extra.insert(ADDED_BY.into(), json!(actor));
}
/// Browse filter `added_by`: `owner`, `agents` or `actor:<exact actor>`. The matching rows
/// come from the narrow `assets_added_by` index (read without the table); testing the
/// virtual column on the outer row would parse every JSON document (~0.6 s per 100k). The
/// unary `+` keeps it a membership test, so it never drives the outer loop (an FTS
/// browse would otherwise probe the whole list per match).
pub(crate) fn added_by_clause(value: &str) -> Result<Clause> {
    let (condition, args) = match value {
        "owner" => ("coalesce(added_by,'') NOT GLOB 'agent:*'", vec![]),
        "agents" => ("added_by GLOB 'agent:*'", vec![]),
        other => match other.strip_prefix("actor:") {
            Some(actor) if !actor.is_empty() => ("added_by = ?", vec![Sql::Text(actor.into())]),
            _ => anyhow::bail!("unknown added_by: {other} (owner|agents|actor:<actor>)"),
        },
    };
    Ok(Clause::new(
        format!("+a.rowid IN (SELECT rowid FROM assets INDEXED BY assets_added_by WHERE {condition})"),
        args,
    ))
}
/// Ids of the non-deleted assets the `added_by` browse filter selects (candidate sets for
/// ranked searches).
pub(crate) fn added_by_ids(conn: &Connection, value: &str) -> Result<HashSet<String>> {
    let clause = added_by_clause(value)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT a.id FROM assets a WHERE a.deleted=0 AND ({})",
        clause.sql
    ))?;
    let ids = stmt
        .query_map(rusqlite::params_from_iter(clause.args), |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// Folder ids plus all their descendants.
pub(crate) fn with_descendants(conn: &Connection, ids: &[String]) -> Result<Vec<String>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let sql = format!(
        "WITH RECURSIVE tree(id) AS (SELECT id FROM collections WHERE id IN ({}) UNION SELECT c.id FROM collections c JOIN tree ON c.parent_id=tree.id) SELECT id FROM tree",
        placeholders(ids.len())
    );
    let mut stmt = conn.prepare(&sql)?;
    Ok(stmt
        .query_map(rusqlite::params_from_iter(ids), |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?)
}

// ---- smart folders ----

const TEXT_OPS: &[&str] = &[
    "contains",
    "not_contains",
    "is",
    "is_not",
    "starts_with",
    "ends_with",
    "empty",
    "not_empty",
];
const NUMBER_OPS: &[&str] = &["eq", "ne", "gt", "gte", "lt", "lte", "between"];
const DATE_OPS: &[&str] = &["after", "before", "between", "within_days"];

fn value_strings(value: &Value) -> Vec<String> {
    match value {
        Value::Array(items) => items.iter().flat_map(value_strings).collect(),
        Value::String(s) => vec![s.clone()],
        Value::Number(n) => vec![n.to_string()],
        _ => vec![],
    }
}
fn value_number(value: &Value, index: usize) -> Result<f64> {
    let item = match value {
        Value::Array(items) => items.get(index),
        other if index == 0 => Some(other),
        _ => None,
    };
    item.and_then(|v| v.as_f64().or_else(|| v.as_str()?.trim().parse().ok()))
        .context("rule value must be a number (or [a, b] for between)")
}
fn value_text(value: &Value, index: usize) -> Result<String> {
    value_strings(value)
        .into_iter()
        .nth(index)
        .context("rule value must be text")
}

/// Rejects unknown fields/ops so stored smart folders always compile.
pub(crate) fn validate_rules(rules: &SmartRules) -> Result<()> {
    ensure!(
        rules.match_ == "all" || rules.match_ == "any",
        "smart folder match must be all or any"
    );
    for rule in &rules.rules {
        let ops: &[&str] = match rule.field.as_str() {
            "name" | "description" | "url" => TEXT_OPS,
            "tags" => &["contains", "not_contains", "any", "all", "none"],
            "ext" => &["is", "is_not", "in"],
            "rating" | "width" | "height" | "size" => NUMBER_OPS,
            "created" | "modified" | "imported" => DATE_OPS,
            "color" => &["similar"],
            "shape" => &["is", "is_not"],
            "folder" => &["in", "not_in", "none"],
            other => anyhow::bail!("unknown smart folder field: {other}"),
        };
        ensure!(
            ops.contains(&rule.op.as_str()),
            "unknown op {} for field {} (one of {})",
            rule.op,
            rule.field,
            ops.join("|")
        );
    }
    Ok(())
}

fn rule_clause(conn: &Connection, rule: &SmartRule) -> Result<Clause> {
    let text_column = |field: &str| match field {
        "name" => "coalesce(a.name_key,'')",
        "description" => "coalesce(json_extract(a.document,'$.metadata.description'),'')",
        _ => "coalesce(json_extract(a.document,'$.metadata.source_url'),'')",
    };
    let v = &rule.value;
    Ok(match rule.field.as_str() {
        "name" | "description" | "url" => {
            let column = text_column(&rule.field);
            match rule.op.as_str() {
                "empty" => Clause::new(format!("{column}=''"), vec![]),
                "not_empty" => Clause::new(format!("{column}!=''"), vec![]),
                op => {
                    let text = Sql::Text(value_text(v, 0)?.to_lowercase());
                    let sql = match op {
                        "contains" => format!("instr(lower({column}),?)>0"),
                        "not_contains" => format!("instr(lower({column}),?)=0"),
                        "is" => format!("lower({column})=?"),
                        "is_not" => format!("lower({column})!=?"),
                        "starts_with" => format!("substr(lower({column}),1,length(?1x))=?1x"),
                        _ => format!("substr(lower({column}),-length(?1x))=?1x"),
                    };
                    if sql.contains("?1x") {
                        Clause::new(sql.replace("?1x", "?"), vec![text.clone(), text])
                    } else {
                        Clause::new(sql, vec![text])
                    }
                }
            }
        }
        "tags" => {
            let tags = value_strings(v);
            match rule.op.as_str() {
                "none" => untagged(),
                "all" => tags_all(&tags),
                "not_contains" => {
                    let any = tags_any(&tags);
                    Clause::new(format!("NOT {}", any.sql), any.args)
                }
                _ => tags_any(&tags),
            }
        }
        "ext" => {
            let clause = exts(&value_strings(v));
            if rule.op == "is_not" {
                Clause::new(format!("NOT ({})", clause.sql), clause.args)
            } else {
                clause
            }
        }
        "rating" | "width" | "height" | "size" => {
            let column = match rule.field.as_str() {
                "rating" => "a.rating",
                "width" => "a.width",
                "height" => "a.height",
                _ => "a.byte_size",
            };
            if rule.op == "between" {
                Clause::new(
                    format!("{column} BETWEEN ? AND ?"),
                    vec![
                        Sql::Real(value_number(v, 0)?),
                        Sql::Real(value_number(v, 1)?),
                    ],
                )
            } else {
                let op = match rule.op.as_str() {
                    "eq" => "=",
                    "ne" => "!=",
                    "gt" => ">",
                    "gte" => ">=",
                    "lt" => "<",
                    _ => "<=",
                };
                Clause::new(
                    format!("{column} {op} ?"),
                    vec![Sql::Real(value_number(v, 0)?)],
                )
            }
        }
        "created" | "modified" | "imported" => {
            let column = date_column(&rule.field)?;
            match rule.op.as_str() {
                "within_days" => {
                    let days = value_number(v, 0)?;
                    let since = Utc::now() - chrono::Duration::seconds((days * 86400.0) as i64);
                    Clause::new(
                        format!("{column} >= ?"),
                        vec![Sql::Text(since.to_rfc3339())],
                    )
                }
                "after" => Clause::new(
                    format!("{column} >= ?"),
                    vec![Sql::Text(browse_date(&value_text(v, 0)?, false)?)],
                ),
                "before" => Clause::new(
                    format!("{column} < ?"),
                    vec![Sql::Text(browse_date(&value_text(v, 0)?, false)?)],
                ),
                _ => Clause::new(
                    format!("{column} >= ? AND {column} <= ?"),
                    vec![
                        Sql::Text(browse_date(&value_text(v, 0)?, false)?),
                        Sql::Text(browse_date(&value_text(v, 1)?, true)?),
                    ],
                ),
            }
        }
        "color" => {
            let hex = value_text(v, 0)?;
            let tolerance = value_number(v, 1).unwrap_or(DEFAULT_COLOR_TOLERANCE);
            colors_within(&color_query(&[hex])?, tolerance)
        }
        "shape" => {
            let clause = shape(&value_text(v, 0)?)?;
            if rule.op == "is_not" {
                Clause::new(format!("NOT ({})", clause.sql), clause.args)
            } else {
                clause
            }
        }
        _ => match rule.op.as_str() {
            "none" => uncategorized(),
            op => {
                let ids = with_descendants(conn, &value_strings(v))?;
                let clause = in_folders(&ids);
                if op == "not_in" {
                    Clause::new(format!("NOT {}", clause.sql), clause.args)
                } else {
                    clause
                }
            }
        },
    })
}
pub(crate) const DEFAULT_COLOR_TOLERANCE: f64 = 25.0;

/// The smart folder's rules as one SQL condition over `assets a`.
pub(crate) fn smart_clause(conn: &Connection, rules: &SmartRules) -> Result<Clause> {
    if rules.rules.is_empty() {
        return Ok(Clause::new("1", vec![]));
    }
    let joiner = if rules.match_ == "any" {
        " OR "
    } else {
        " AND "
    };
    let mut parts = Vec::new();
    let mut args = Vec::new();
    for rule in &rules.rules {
        let clause = rule_clause(conn, rule)?;
        parts.push(format!("({})", clause.sql));
        args.extend(clause.args);
    }
    Ok(Clause::new(parts.join(joiner), args))
}
pub(crate) fn smart_folder(conn: &Connection, id: &str) -> Result<SmartFolder> {
    conn.query_row(
        "SELECT document FROM smart_folders WHERE id=?1",
        [id],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .map(parse)
    .transpose()?
    .with_context(|| format!("smart folder not found: {id}"))
}
pub(crate) fn save_smart_folder(tx: &Transaction, folder: &SmartFolder) -> Result<()> {
    tx.execute(
        "INSERT INTO smart_folders(id,document) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET document=excluded.document",
        params![folder.id, json_text(folder)?],
    )?;
    Ok(())
}

// ---- folders ----

pub(crate) fn collection_exists(conn: &Connection, id: &str) -> Result<()> {
    let found: bool = conn.query_row(
        "SELECT count(*)>0 FROM collections WHERE id=?1",
        [id],
        |r| r.get(0),
    )?;
    ensure!(found, "collection not found: {id}");
    Ok(())
}
pub(crate) fn insert_collection(tx: &Transaction, c: &Collection) -> Result<()> {
    tx.execute(
        "INSERT INTO collections(id,name,parent_id,sort_order,description,color,icon,source_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(id) DO UPDATE SET name=excluded.name,parent_id=excluded.parent_id,sort_order=excluded.sort_order,description=excluded.description,color=excluded.color,icon=excluded.icon,source_id=excluded.source_id",
        params![
            c.id,
            c.name,
            c.parent_id,
            c.sort_order,
            c.description,
            c.color,
            c.icon,
            c.source_id
        ],
    )?;
    Ok(())
}
/// All folders without memberships, parents before children.
pub(crate) fn collection_rows(conn: &Connection) -> Result<Vec<Collection>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id,name,parent_id,sort_order,description,color,icon,source_id FROM collections",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Collection {
                id: r.get(0)?,
                name: r.get(1)?,
                asset_ids: vec![],
                parent_id: r.get(2)?,
                sort_order: r.get(3)?,
                description: r.get(4)?,
                color: r.get(5)?,
                icon: r.get(6)?,
                source_id: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // Depth-first order so a flat list renders as an outline.
    let mut children: BTreeMap<Option<String>, Vec<Collection>> = BTreeMap::new();
    let ids: HashSet<String> = rows.iter().map(|c| c.id.clone()).collect();
    for row in rows {
        // Orphans (parent deleted on another path) surface at the root.
        let parent = row.parent_id.clone().filter(|p| ids.contains(p));
        children.entry(parent).or_default().push(row);
    }
    for siblings in children.values_mut() {
        siblings.sort_by(|a, b| {
            (a.sort_order, a.name.to_lowercase(), &a.id).cmp(&(
                b.sort_order,
                b.name.to_lowercase(),
                &b.id,
            ))
        });
    }
    fn walk(
        parent: Option<String>,
        children: &mut BTreeMap<Option<String>, Vec<Collection>>,
        out: &mut Vec<Collection>,
    ) {
        for c in children.remove(&parent).unwrap_or_default() {
            let id = c.id.clone();
            out.push(c);
            walk(Some(id), children, out);
        }
    }
    let mut out = Vec::new();
    walk(None, &mut children, &mut out);
    Ok(out)
}
/// Rewrites sibling `sort_order` as 0..n in the current order.
pub(crate) fn renumber(tx: &Transaction, parent: Option<&str>) -> Result<()> {
    let mut stmt = tx.prepare(
        "SELECT id FROM collections WHERE parent_id IS ?1 ORDER BY sort_order,lower(name),id",
    )?;
    let ids = stmt
        .query_map([parent], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (index, id) in ids.iter().enumerate() {
        tx.execute(
            "UPDATE collections SET sort_order=?2 WHERE id=?1",
            params![id, index as i64],
        )?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct SidebarFolder {
    pub id: String,
    pub name: String,
    /// Distinct non-trashed assets in this folder and its sub-folders (what the folder view,
    /// which always includes sub-folders, shows).
    pub count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub sort_order: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub children: Vec<SidebarFolder>,
}

const LIVE: &str = "a.deleted=0 AND a.trashed=0";

impl Library {
    // ---- lazy fingerprints ----

    /// Computes a pending fingerprint (reads the original on the background I/O pool).
    /// No-op when already hashed.
    pub fn resolve_hash(&mut self, id: &str) -> Result<Asset> {
        let Some(job) = self.hash_job(id)? else {
            return get_asset(&self.conn, id);
        };
        let original = crate::io::background().try_run(
            format!("hash {id}"),
            crate::io::DRIVE_DEADLINE,
            move || job.run(),
        )?;
        self.finish_hash(id, original)
    }
    /// Catalog phase of [`Library::resolve_hash`]: `None` when already hashed.
    pub fn hash_job(&self, id: &str) -> Result<Option<HashJob>> {
        let asset = get_asset(&self.conn, id)?;
        if !asset.original.pending() {
            return Ok(None);
        }
        Ok(Some(HashJob {
            lookup: self.lookup_of(&asset.original)?,
            path: asset.original.path,
        }))
    }
    /// Last phase of [`Library::resolve_hash`] with the result of [`HashJob::run`].
    pub fn finish_hash(&mut self, id: &str, original: Original) -> Result<Asset> {
        self.apply_fingerprint(id, original)
    }
    /// Pending rows whose original exists (not still being fetched, see [`crate::fetching`]).
    const HAS_BYTES: &str =
        "coalesce(json_extract(document,'$.original.storage'),'local') NOT IN ('fetching','failed')";
    /// Ids of live assets whose fingerprint is still pending (oldest first). Originals that
    /// have not landed yet ([`crate::fetching`]) have no bytes to hash and are skipped.
    pub fn pending_hash_ids(&self, limit: usize) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id FROM assets WHERE deleted=0 AND hash_state='pending' AND {} ORDER BY rowid LIMIT ?1",
            Self::HAS_BYTES
        ))?;
        let ids = stmt
            .query_map([limit as i64], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids)
    }
    pub fn pending_hash_count(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            &format!("SELECT count(*) FROM assets WHERE deleted=0 AND hash_state='pending' AND {}", Self::HAS_BYTES),
            [],
            |r| r.get(0),
        )?)
    }
    /// Duplicate screening for intake (contract `native.find_duplicates`): one candidate per
    /// file, then the bytes, then each digest, with the existing assets each duplicates.
    /// Files are hashed on a few threads; library originals are never read.
    pub fn find_duplicates(
        &self,
        files: &[PathBuf],
        bytes: Option<(&[u8], Option<&str>)>,
        digests: &[String],
    ) -> Result<Vec<DuplicateCandidate>> {
        let mut candidates = Vec::with_capacity(files.len() + digests.len() + 1);
        for (path, digest) in files.iter().zip(digest_files(files)) {
            let path = path.canonicalize().unwrap_or_else(|_| path.clone());
            let mut candidate = DuplicateCandidate {
                path: Some(path.to_string_lossy().into_owned()),
                filename: path.file_name().map(|n| n.to_string_lossy().into_owned()),
                ..DuplicateCandidate::default()
            };
            match digest {
                Ok((blake3, size)) => {
                    candidate.blake3 = Some(blake3);
                    candidate.byte_size = Some(size);
                }
                Err(e) => candidate.error = Some(format!("{e:#}")),
            }
            candidates.push(candidate);
        }
        if let Some((bytes, filename)) = bytes {
            candidates.push(DuplicateCandidate {
                filename: filename.filter(|n| !n.is_empty()).map(str::to_owned),
                blake3: Some(blake3::hash(bytes).to_hex().to_string()),
                byte_size: Some(bytes.len() as u64),
                ..DuplicateCandidate::default()
            });
        }
        for digest in digests {
            candidates.push(DuplicateCandidate {
                blake3: Some(digest.trim().to_ascii_lowercase()),
                ..DuplicateCandidate::default()
            });
        }
        for candidate in &mut candidates {
            candidate.matches = self.duplicates_of(candidate)?;
        }
        Ok(candidates)
    }
    /// Non-tombstoned assets duplicating `candidate`: equal original digest, or (pending
    /// fingerprints, which have no digest) equal byte size and file name. Live before
    /// trashed, then oldest first.
    pub fn duplicates_of(&self, candidate: &DuplicateCandidate) -> Result<Vec<DuplicateMatch>> {
        const MAX_MATCHES: usize = 20;
        let mut found: Vec<(Asset, &str)> = Vec::new();
        if let Some(digest) = candidate.blake3.as_deref().filter(|d| !d.is_empty()) {
            let mut stmt = self
                .conn
                .prepare_cached("SELECT document FROM assets WHERE hash=?1 AND deleted=0")?;
            for document in stmt.query_map([digest], |r| r.get::<_, String>(0))? {
                let asset: Asset = parse(document?)?;
                if !asset.original.pending() {
                    found.push((asset, "blake3"));
                }
            }
        }
        if let (Some(size), Some(name)) = (
            candidate.byte_size,
            candidate.filename.as_deref().filter(|n| !n.is_empty()),
        ) {
            let mut stmt = self.conn.prepare_cached(
                "SELECT document FROM assets WHERE deleted=0 AND trashed IN (0,1) AND byte_size=?1 AND hash_state='pending'",
            )?;
            for document in stmt.query_map([size as i64], |r| r.get::<_, String>(0))? {
                let asset: Asset = parse(document?)?;
                if same_file_name(&asset, name) {
                    found.push((asset, "name_size"));
                }
            }
        }
        found.sort_by(|(a, _), (b, _)| {
            (a.deleted_at.is_some(), &a.created_at).cmp(&(b.deleted_at.is_some(), &b.created_at))
        });
        found.truncate(MAX_MATCHES);
        found
            .into_iter()
            .map(|(asset, basis)| {
                Ok(DuplicateMatch {
                    folder_ids: self.folder_ids(&asset.id)?,
                    same_path: !asset.original.path.is_empty()
                        && candidate.path.as_deref() == Some(asset.original.path.as_str()),
                    trashed: asset.deleted_at.is_some(),
                    basis: basis.into(),
                    asset_id: asset.id,
                })
            })
            .collect()
    }
    /// Settles a pending fingerprint with freshly read bytes. Unchanged size+mtime means
    /// the bytes are the ones cataloged: every source-keyed record is rekeyed to the
    /// digest, without a revision bump. Otherwise the original changed since cataloging
    /// and this is a refresh (revision bump, derivatives invalidated).
    pub(crate) fn apply_fingerprint(&mut self, id: &str, original: Original) -> Result<Asset> {
        let tx = self.conn.transaction()?;
        let mut asset = get_asset(&tx, id)?;
        if !asset.original.pending() {
            return Ok(asset);
        }
        let old = asset.original.source_key();
        let same = original.byte_size == asset.original.byte_size
            && original.modified_ns == asset.original.modified_ns;
        if same {
            rekey(&tx, id, &old, &original.blake3)?;
            asset.original.blake3 = original.blake3;
            asset.original.hash_state = "hashed".into();
            save_asset(&tx, &asset)?;
            emit(
                &tx,
                &self.context,
                "media_hashed",
                Some(&asset),
                json!({"blake3":asset.original.blake3,"previous_key":old}),
            )?;
        } else {
            let storage = std::mem::take(&mut asset.original.storage);
            asset.original = Original {
                storage,
                import_root: asset.original.import_root.clone(),
                ..original
            };
            bump(&mut asset);
            tx.execute("DELETE FROM representations WHERE asset_id=?1", [id])?;
            save_asset(&tx, &asset)?;
            emit(
                &tx,
                &self.context,
                "media_updated",
                Some(&asset),
                json!({"change":"original","previous_hash":old}),
            )?;
        }
        tx.commit()?;
        Ok(asset)
    }
    /// Hashes up to `limit` pending originals (reads them; hydrates cloud placeholders).
    /// Stops early when the originals' storage stalls (the rest stay pending).
    pub fn hash_pending(&mut self, limit: usize) -> Result<Value> {
        ensure!((1..=10_000).contains(&limit), "limit must be 1–10000");
        let (mut hashed, mut changed, mut failed) = (0, 0, Vec::new());
        for id in self.pending_hash_ids(limit)? {
            let before = get_asset(&self.conn, &id)?.revision;
            match self.resolve_hash(&id) {
                Ok(asset) if asset.revision != before => changed += 1,
                Ok(_) => hashed += 1,
                Err(e) => {
                    let stalled = crate::io::is_stalled(&e);
                    failed.push(json!({"asset_id":id,"message":format!("{e:#}")}));
                    if stalled {
                        break;
                    }
                }
            }
        }
        let remaining = self.pending_hash_count()?;
        Ok(json!({"hashed":hashed,"changed":changed,"failed":failed,"remaining":remaining}))
    }

    // ---- trash ----

    /// Moves assets to the trash. `expected_revision` applies when exactly one id is given.
    pub fn trash(&mut self, ids: &[String], expected_revision: Option<u64>) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut count = 0;
        for id in ids {
            let mut asset = get_asset(&tx, id)?;
            if ids.len() == 1 {
                check_revision(&asset, expected_revision)?;
            }
            if asset.deleted_at.is_some() {
                continue;
            }
            asset.deleted_at = Some(now());
            bump(&mut asset);
            save_asset(&tx, &asset)?;
            emit(
                &tx,
                &self.context,
                "media_trashed",
                Some(&asset),
                json!({"original_deleted":false}),
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }
    pub fn restore(&mut self, ids: &[String]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut count = 0;
        for id in ids {
            let mut asset = get_asset(&tx, id)?;
            if asset.deleted_at.take().is_none() {
                continue;
            }
            bump(&mut asset);
            save_asset(&tx, &asset)?;
            emit(
                &tx,
                &self.context,
                "media_restored",
                Some(&asset),
                json!({}),
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }
    /// Tombstones trashed assets (all, or the given trashed ids). Originals are untouched.
    pub fn empty_trash(&mut self, ids: Option<&[String]>) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let targets: Vec<String> = match ids {
            Some(ids) => ids.to_vec(),
            None => {
                let mut stmt = tx.prepare("SELECT id FROM assets WHERE deleted=0 AND trashed=1")?;
                stmt.query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
        };
        let mut count = 0;
        for id in &targets {
            let mut asset = get_asset(&tx, id)?;
            ensure!(
                asset.deleted_at.is_some(),
                "asset is not in the trash: {id}"
            );
            bump(&mut asset);
            tx.execute(
                "UPDATE assets SET deleted=1,document=?2 WHERE id=?1",
                params![id, json_text(&asset)?],
            )?;
            unindex(&tx, id)?;
            emit(
                &tx,
                &self.context,
                "media_removed",
                Some(&asset),
                json!({"original_deleted":false}),
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }
    /// Records a view (Recently Used) without advancing the revision. The event carries the
    /// time, so replicas apply it without fetching the asset.
    pub fn touch(&mut self, id: &str) -> Result<String> {
        let tx = self.conn.transaction()?;
        let mut asset = get_asset(&tx, id)?;
        let at = now();
        asset.last_viewed_at = Some(at.clone());
        save_asset(&tx, &asset)?;
        emit(&tx, &self.context, "media_viewed", Some(&asset), json!({"last_viewed_at": at}))?;
        tx.commit()?;
        Ok(at)
    }

    // ---- folders ----

    /// Moves a folder under `parent_id` (None = root) at `index` among its new siblings.
    pub fn collection_move(
        &mut self,
        id: &str,
        parent_id: Option<&str>,
        index: Option<usize>,
    ) -> Result<Collection> {
        let tx = self.conn.transaction()?;
        let old_parent: Option<String> = tx
            .query_row("SELECT parent_id FROM collections WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?
            .context("collection not found")?;
        if let Some(parent) = parent_id {
            collection_exists(&tx, parent)?;
            let mut cursor = Some(parent.to_owned());
            while let Some(current) = cursor {
                ensure!(
                    current != id,
                    "cannot move a folder into itself or a descendant"
                );
                cursor = tx.query_row(
                    "SELECT parent_id FROM collections WHERE id=?1",
                    [&current],
                    |r| r.get(0),
                )?;
            }
        }
        let mut stmt = tx.prepare(
            "SELECT id FROM collections WHERE parent_id IS ?1 AND id!=?2 ORDER BY sort_order,lower(name),id",
        )?;
        let mut siblings = stmt
            .query_map(params![parent_id, id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        let index = index.unwrap_or(siblings.len()).min(siblings.len());
        siblings.insert(index, id.to_owned());
        tx.execute(
            "UPDATE collections SET parent_id=?2 WHERE id=?1",
            params![id, parent_id],
        )?;
        for (order, sibling) in siblings.iter().enumerate() {
            tx.execute(
                "UPDATE collections SET sort_order=?2 WHERE id=?1",
                params![sibling, order as i64],
            )?;
        }
        if old_parent.as_deref() != parent_id {
            renumber(&tx, old_parent.as_deref())?;
        }
        emit(
            &tx,
            &self.context,
            "collection_updated",
            None,
            json!({"collection_id":id,"parent_id":parent_id,"index":index,"operation":"move"}),
        )?;
        tx.commit()?;
        self.collections()?
            .into_iter()
            .find(|c| c.id == id)
            .context("collection not found")
    }
    /// Live (non-trashed) folder ids of one asset.
    pub fn folder_ids(&self, asset_id: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT collection_id FROM memberships WHERE asset_id=?1 ORDER BY rowid",
        )?;
        Ok(stmt
            .query_map([asset_id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---- smart folders ----

    pub fn smart_folders(&self) -> Result<Vec<SmartFolder>> {
        let mut folders =
            documents::<SmartFolder>(&self.conn, "SELECT document FROM smart_folders", [])?;
        folders.sort_by(|a, b| {
            (&a.parent_id, a.sort_order, a.name.to_lowercase()).cmp(&(
                &b.parent_id,
                b.sort_order,
                b.name.to_lowercase(),
            ))
        });
        Ok(folders)
    }
    pub fn smart_folder_create(
        &mut self,
        name: &str,
        rules: SmartRules,
        parent_id: Option<&str>,
    ) -> Result<SmartFolder> {
        ensure!(!name.trim().is_empty(), "smart folder name cannot be empty");
        validate_rules(&rules)?;
        let tx = self.conn.transaction()?;
        if let Some(parent) = parent_id {
            smart_folder(&tx, parent)?;
        }
        let sort_order: i64 =
            tx.query_row("SELECT count(*) FROM smart_folders", [], |r| r.get(0))?;
        let folder = SmartFolder {
            id: id(),
            name: name.into(),
            rules,
            parent_id: parent_id.map(str::to_owned),
            sort_order,
            source_id: None,
            unsupported: vec![],
        };
        // Compile once so a stored folder always evaluates.
        smart_clause(&tx, &folder.rules)?;
        save_smart_folder(&tx, &folder)?;
        emit(
            &tx,
            &self.context,
            "smart_folder_updated",
            None,
            json!({"smart_folder_id":folder.id,"operation":"create"}),
        )?;
        tx.commit()?;
        Ok(folder)
    }
    pub fn smart_folder_update(
        &mut self,
        id: &str,
        name: Option<&str>,
        rules: Option<SmartRules>,
    ) -> Result<SmartFolder> {
        let tx = self.conn.transaction()?;
        let mut folder = smart_folder(&tx, id)?;
        if let Some(name) = name {
            ensure!(!name.trim().is_empty(), "smart folder name cannot be empty");
            folder.name = name.into();
        }
        if let Some(rules) = rules {
            validate_rules(&rules)?;
            smart_clause(&tx, &rules)?;
            folder.rules = rules;
        }
        save_smart_folder(&tx, &folder)?;
        emit(
            &tx,
            &self.context,
            "smart_folder_updated",
            None,
            json!({"smart_folder_id":id,"operation":"update"}),
        )?;
        tx.commit()?;
        Ok(folder)
    }
    pub fn smart_folder_delete(&mut self, id: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        smart_folder(&tx, id)?;
        tx.execute("DELETE FROM smart_folders WHERE id=?1", [id])?;
        tx.execute(
            "UPDATE smart_folders SET document=json_remove(document,'$.parent_id') WHERE json_extract(document,'$.parent_id')=?1",
            [id],
        )?;
        emit(
            &tx,
            &self.context,
            "smart_folder_updated",
            None,
            json!({"smart_folder_id":id,"operation":"delete"}),
        )?;
        tx.commit()?;
        Ok(())
    }
    /// Live asset count matching a smart folder.
    pub fn smart_folder_count(&self, folder: &SmartFolder) -> Result<i64> {
        let clause = smart_clause(&self.conn, &folder.rules)?;
        Ok(self.conn.query_row(
            &format!(
                "SELECT count(*) FROM assets a INDEXED BY assets_facets WHERE {LIVE} AND ({})",
                clause.sql
            ),
            rusqlite::params_from_iter(clause.args.iter()),
            |r| r.get(0),
        )?)
    }

    // ---- tags ----

    pub fn tag_groups(&self) -> Result<Vec<TagGroup>> {
        documents(
            &self.conn,
            "SELECT document FROM tag_groups ORDER BY sort_order,id",
            [],
        )
    }
    pub fn starred_tags(&self) -> Result<Vec<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT document FROM settings WHERE key='starred_tags'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .map(parse)
            .transpose()?
            .unwrap_or_default())
    }
    /// Renames (merging into an existing `to`) on every live asset and in tag groups.
    pub fn tag_rename(&mut self, from: &str, to: &str) -> Result<usize> {
        let to = to.trim();
        ensure!(!to.is_empty(), "tag cannot be empty");
        self.retag(from, Some(to))
    }
    pub fn tag_delete(&mut self, tag: &str) -> Result<usize> {
        self.retag(tag, None)
    }
    fn retag(&mut self, from: &str, to: Option<&str>) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let ids = {
            let mut stmt = tx.prepare(
                "SELECT t.asset_id FROM asset_tags t JOIN assets a ON a.id=t.asset_id WHERE t.tag=?1 AND a.deleted=0",
            )?;
            stmt.query_map([from], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for id in &ids {
            let mut asset = get_asset(&tx, id)?;
            asset.metadata.tags.retain(|t| t != from);
            if let Some(to) = to {
                asset.metadata.tags.push(to.into());
            }
            normalize_tags(&mut asset.metadata.tags);
            bump(&mut asset);
            save_asset(&tx, &asset)?;
            emit(
                &tx,
                &self.context,
                "media_updated",
                Some(&asset),
                json!({"change":"metadata","tag_from":from,"tag_to":to}),
            )?;
        }
        let mut groups = documents::<TagGroup>(&tx, "SELECT document FROM tag_groups", [])?;
        let mut groups_changed = false;
        for group in &mut groups {
            if group.tags.iter().any(|t| t == from) {
                group.tags.retain(|t| t != from);
                if let Some(to) = to {
                    group.tags.push(to.into());
                }
                normalize_tags(&mut group.tags);
                tx.execute(
                    "UPDATE tag_groups SET document=?2 WHERE id=?1",
                    params![group.id, json_text(group)?],
                )?;
                groups_changed = true;
            }
        }
        let mut starred = {
            let value: Option<String> = tx
                .query_row(
                    "SELECT document FROM settings WHERE key='starred_tags'",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            value
                .map(parse::<Vec<String>>)
                .transpose()?
                .unwrap_or_default()
        };
        if starred.iter().any(|t| t == from) {
            starred.retain(|t| t != from);
            if let Some(to) = to {
                starred.push(to.into());
            }
            normalize_tags(&mut starred);
            set_setting(&tx, "starred_tags", &starred)?;
            groups_changed = true;
        }
        if groups_changed {
            emit(&tx, &self.context, "tag_groups_updated", None, json!({}))?;
        }
        tx.commit()?;
        Ok(ids.len())
    }

    // ---- sidebar ----

    pub fn sidebar(&self) -> Result<Value> {
        let _snapshot = read_snapshot(&self.conn)?;
        let (all, uncategorized, untagged, recently_used): (i64, i64, i64, i64) = self.conn.query_row(
            &format!("SELECT count(*), coalesce(sum(a.id NOT IN (SELECT asset_id FROM memberships)),0), coalesce(sum(a.tag_count=0),0), coalesce(sum(a.last_viewed IS NOT NULL),0) FROM assets a INDEXED BY assets_facets WHERE {LIVE}"),
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let trash: i64 = self.conn.query_row(
            "SELECT count(*) FROM assets WHERE deleted=0 AND trashed=1",
            [],
            |r| r.get(0),
        )?;
        let tags_total: i64 = self.conn.query_row(
            &format!("SELECT count(DISTINCT t.tag) FROM asset_tags t JOIN assets a ON a.id=t.asset_id WHERE {LIVE}"),
            [],
            |r| r.get(0),
        )?;
        let counts: HashMap<String, i64> = {
            let mut stmt = self.conn.prepare(&format!(
                "SELECT m.collection_id,count(*) FROM memberships m JOIN assets a ON a.id=m.asset_id WHERE {LIVE} GROUP BY m.collection_id"
            ))?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        // Folder views include sub-folders, so a folder counts the distinct live assets of
        // its whole subtree. Leaves keep their direct count; only folders with children walk
        // their subtree (UNION, not UNION ALL, so a corrupt parent cycle still terminates).
        let subtree_counts: HashMap<String, i64> = {
            let mut stmt = self.conn.prepare(&format!(
                "WITH RECURSIVE tree(root, id) AS (SELECT id, id FROM collections WHERE id IN (SELECT parent_id FROM collections) \
                 UNION SELECT tree.root, c.id FROM collections c JOIN tree ON c.parent_id=tree.id) \
                 SELECT tree.root, count(DISTINCT m.asset_id) FROM tree JOIN memberships m ON m.collection_id=tree.id \
                 JOIN assets a ON a.id=m.asset_id WHERE {LIVE} GROUP BY tree.root"
            ))?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut nodes: Vec<SidebarFolder> = collection_rows(&self.conn)?
            .into_iter()
            .map(|c| SidebarFolder {
                count: subtree_counts.get(&c.id).or_else(|| counts.get(&c.id)).copied().unwrap_or(0),
                id: c.id,
                name: c.name,
                parent_id: c.parent_id,
                sort_order: c.sort_order,
                color: c.color,
                icon: c.icon,
                description: c.description,
                children: vec![],
            })
            .collect();
        // collection_rows is depth-first: fold children into parents from the end.
        let mut folders: Vec<SidebarFolder> = Vec::new();
        while let Some(node) = nodes.pop() {
            match node
                .parent_id
                .as_ref()
                .and_then(|p| nodes.iter().rposition(|n| &n.id == p))
            {
                Some(parent) => nodes[parent].children.insert(0, node),
                None => folders.insert(0, node),
            }
        }
        let smart = self
            .smart_folders()?
            .into_iter()
            .map(|f| {
                Ok(json!({"id":f.id,"name":f.name,"parent_id":f.parent_id,"count":self.smart_folder_count(&f)?}))
            })
            .collect::<Result<Vec<_>>>()?;
        let pending_thumbnails: i64 = self.conn.query_row(
            "SELECT count(*) FROM eagle_thumbnails WHERE attempts<3",
            [],
            |r| r.get(0),
        )?;
        Ok(json!({
            "pending_thumbnails": pending_thumbnails,
            "all": all,
            "added_by": self.added_by_counts()?,
            "uncategorized": uncategorized,
            "untagged": untagged,
            "recently_used": recently_used,
            "trash": trash,
            "tags_total": tags_total,
            "folders": folders,
            "smart_folders": smart,
        }))
    }

    /// Live assets by adder: `{owner, agents, by_agent:[{actor, count}]}`, agents by count
    /// (then actor). One pass over the `added_by` index.
    pub fn added_by_counts(&self) -> Result<Value> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT added_by, count(*) FROM assets INDEXED BY assets_added_by WHERE deleted=0 AND trashed=0 GROUP BY added_by",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let (mut owner, mut agents) = (0i64, 0i64);
        let mut by_agent = Vec::new();
        for (actor, count) in rows {
            match actor {
                Some(actor) if is_agent(&actor) => {
                    agents += count;
                    by_agent.push((actor, count));
                }
                _ => owner += count,
            }
        }
        by_agent.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let by_agent = by_agent
            .into_iter()
            .map(|(actor, count)| json!({"actor": actor, "count": count}))
            .collect::<Vec<_>>();
        Ok(json!({"owner": owner, "agents": agents, "by_agent": by_agent}))
    }

    /// Ids of every non-deleted asset (trashed included) by adder, for replicas mirroring
    /// `added_by` onto assets cataloged before they knew it (native op `added_by`).
    pub fn added_by_map(&self) -> Result<Value> {
        let mut stmt = self.conn.prepare(
            "SELECT added_by, id FROM assets INDEXED BY assets_added_by WHERE added_by IS NOT NULL AND deleted=0",
        )?;
        let mut actors: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            actors.entry(row.get(0)?).or_default().push(row.get(1)?);
        }
        Ok(json!({ "actors": actors }))
    }

    // ---- export / palette ----

    /// Copies originals (hydrating placeholders / fetching cloud copies) into `dir`.
    pub fn export_originals(&mut self, ids: &[String], dir: &Path) -> Result<Value> {
        ensure!(
            dir.is_dir(),
            "export destination must be an existing directory"
        );
        let dir = dir.canonicalize()?;
        ensure!(
            !dir.starts_with(&self.state_dir),
            "export destination must be outside local catalog state"
        );
        let mut exported = Vec::new();
        let mut errors = Vec::new();
        for id in ids {
            let result = (|| -> Result<PathBuf> {
                let asset = get_asset(&self.conn, id)?;
                let source = self.fetch(id, "original")?;
                let ext = storage::original_extension(&asset.original);
                let stem = Path::new(&asset.name)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| asset.id.clone())
                    .replace(['/', ':', '\0'], "_");
                let stem = if Path::new(&asset.name)
                    .extension()
                    .is_some_and(|e| format!(".{}", e.to_string_lossy()).eq_ignore_ascii_case(&ext))
                {
                    stem
                } else {
                    asset.name.replace(['/', ':', '\0'], "_")
                };
                for n in 0..10_000 {
                    let name = if n == 0 {
                        format!("{stem}{ext}")
                    } else {
                        format!("{stem} ({n}){ext}")
                    };
                    let target = dir.join(name);
                    match OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&target)
                    {
                        Ok(mut out) => {
                            let copied = (|| -> Result<()> {
                                std::io::copy(&mut File::open(&source)?, &mut out)?;
                                out.sync_all()?;
                                Ok(())
                            })();
                            if let Err(e) = copied {
                                let _ = fs::remove_file(&target);
                                return Err(e);
                            }
                            return Ok(target);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                        Err(e) => return Err(e.into()),
                    }
                }
                anyhow::bail!("too many name collisions")
            })();
            match result {
                Ok(path) => {
                    exported.push(path);
                    // Bytes were read: settle a lazy fingerprint while they are local.
                    if get_asset(&self.conn, id).is_ok_and(|a| a.original.pending()) {
                        let _ = self.resolve_hash(id);
                    }
                }
                Err(e) => errors.push(json!({"asset_id":id,"message":format!("{e:#}")})),
            }
        }
        Ok(json!({"exported":exported,"errors":errors}))
    }
    /// Recomputes `metadata.colors` from the current thumbnail (no revision bump).
    pub fn set_palette(&mut self, id: &str, helper: &Path) -> Result<Asset> {
        let thumbnail = self.fetch(id, "thumbnail")?;
        let colors = run_palette(helper, &thumbnail)?;
        let tx = self.conn.transaction()?;
        let mut asset = get_asset(&tx, id)?;
        asset.metadata.colors = colors;
        save_asset(&tx, &asset)?;
        emit(
            &tx,
            &self.context,
            "media_updated",
            Some(&asset),
            json!({"change":"palette"}),
        )?;
        tx.commit()?;
        Ok(asset)
    }
}

/// Moves every source-keyed record of an asset from `old` to `new` (same bytes).
pub(crate) fn rekey(tx: &Transaction, asset_id: &str, old: &str, new: &str) -> Result<()> {
    for table in ["representations", "observations"] {
        tx.execute(
            &format!("UPDATE {table} SET source_hash=?3,document=json_set(document,'$.source_hash',?3) WHERE asset_id=?1 AND source_hash=?2"),
            params![asset_id, old, new],
        )?;
    }
    for table in ["embeddings", "uploads"] {
        tx.execute(
            &format!(
                "UPDATE OR REPLACE {table} SET source_hash=?3 WHERE asset_id=?1 AND source_hash=?2"
            ),
            params![asset_id, old, new],
        )?;
    }
    tx.execute(
        "UPDATE jobs SET document=json_set(document,'$.source_hash',?3) WHERE asset_id=?1 AND json_extract(document,'$.source_hash')=?2",
        params![asset_id, old, new],
    )?;
    Ok(())
}
pub(crate) fn set_setting(tx: &Transaction, key: &str, value: &impl Serialize) -> Result<()> {
    tx.execute(
        "INSERT INTO settings(key,document) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET document=excluded.document",
        params![key, json_text(value)?],
    )?;
    Ok(())
}
/// vamp-media `palette` on a (small) image.
pub(crate) fn run_palette(helper: &Path, image: &Path) -> Result<Vec<ColorSwatch>> {
    let request = ToolRequest {
        operation: "palette".into(),
        input_path: Some(path_text(image)?),
        ..ToolRequest::default()
    };
    run_tool(&[path_text(helper)?], &request, 60)?
        .palette
        .context("media helper returned no palette")
}

#[cfg(test)]
mod duplicate_tests {
    use super::*;
    use tempfile::TempDir;

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn identical_bytes_match_with_trash_flagged_and_tombstones_excluded() {
        let root = TempDir::new().unwrap();
        let mut lib = Library::init(&root.path().join("home"), "dupes", None, None).unwrap();
        let first = write(root.path(), "first.png", b"same pixels");
        let second = write(root.path(), "second.png", b"same pixels");
        let other = write(root.path(), "other.png", b"different pixels");
        let a = lib.add(&first, None, Metadata::default()).unwrap();
        let b = lib.add(&second, None, Metadata::default()).unwrap();
        lib.add(&other, None, Metadata::default()).unwrap();
        let folder = lib.collection_create("Refs", None).unwrap();
        lib.collection_membership(&folder.id, &a.id, true).unwrap();

        let incoming = write(root.path(), "incoming.png", b"same pixels");
        let missing = root.path().join("missing.png");
        let digest = blake3::hash(b"same pixels").to_hex().to_uppercase();
        let found = lib
            .find_duplicates(
                &[incoming.clone(), first.clone(), missing],
                Some((b"same pixels", Some("clip.png"))),
                &[digest],
            )
            .unwrap();
        assert_eq!(found.len(), 5);
        for candidate in [&found[0], &found[1], &found[3], &found[4]] {
            let ids: Vec<_> = candidate.matches.iter().map(|m| m.asset_id.as_str()).collect();
            assert_eq!(ids, [a.id.as_str(), b.id.as_str()]);
            assert!(candidate.matches.iter().all(|m| m.basis == "blake3" && !m.trashed));
        }
        assert_eq!(found[0].byte_size, Some(11));
        assert_eq!(found[0].filename.as_deref(), Some("incoming.png"));
        assert_eq!(found[0].matches[0].folder_ids, [folder.id.clone()]);
        assert!(found[0].matches[1].folder_ids.is_empty());
        assert!(!found[0].matches[0].same_path);
        assert!(found[1].matches[0].same_path && !found[1].matches[1].same_path);
        assert!(found[2].error.is_some() && found[2].matches.is_empty());

        lib.trash(std::slice::from_ref(&a.id), None).unwrap();
        let found = lib.find_duplicates(std::slice::from_ref(&incoming), None, &[]).unwrap();
        let order: Vec<_> = found[0]
            .matches
            .iter()
            .map(|m| (m.asset_id.as_str(), m.trashed))
            .collect();
        assert_eq!(order, [(b.id.as_str(), false), (a.id.as_str(), true)]);
        lib.empty_trash(None).unwrap();
        let found = lib.find_duplicates(&[incoming], None, &[]).unwrap();
        assert_eq!(found[0].matches.len(), 1);
        assert_eq!(found[0].matches[0].asset_id, b.id);
    }

    #[test]
    fn pending_fingerprints_match_on_size_and_name_only() {
        let root = TempDir::new().unwrap();
        let mut lib = Library::init(&root.path().join("home"), "dupes", None, None).unwrap();
        let source = write(root.path(), "Sunset.jpg", b"eagle original");
        let asset = lib.add(&source, None, Metadata::default()).unwrap();
        // As an Eagle import catalogs it: no digest yet, name without extension.
        lib.conn
            .execute(
                "UPDATE assets SET hash='pending:14:1', document=json_set(document,'$.original.hash_state','pending','$.original.blake3','','$.name','Sunset') WHERE id=?1",
                [&asset.id],
            )
            .unwrap();
        let drop = root.path().join("drop");
        fs::create_dir(&drop).unwrap();
        let renamed_case = write(&drop, "sunset.JPG", b"eagle original");
        let other_name = write(&drop, "beach.jpg", b"eagle original");
        let other_size = write(&drop, "Sunset.png", b"eagle original, edited");
        let found = lib
            .find_duplicates(&[renamed_case, other_name, other_size], None, &[])
            .unwrap();
        assert_eq!(found[0].matches.len(), 1);
        assert_eq!(found[0].matches[0].basis, "name_size");
        assert_eq!(found[0].matches[0].asset_id, asset.id);
        assert!(found[1].matches.is_empty() && found[2].matches.is_empty());
        let named = lib
            .find_duplicates(&[], Some((b"eagle original", Some("Sunset.jpg"))), &[])
            .unwrap();
        assert_eq!(named[0].matches.len(), 1);
        let digest = blake3::hash(b"eagle original").to_hex().to_string();
        let anonymous = lib
            .find_duplicates(&[], Some((b"eagle original", None)), &[digest])
            .unwrap();
        assert!(anonymous.iter().all(|c| c.matches.is_empty()));
    }
}
