//! Replica cache policy: how much of the authority's previews and originals a replica
//! keeps (`cache_policy.json` in its state dir), least-recently-used accounting in the
//! mirror catalog (`replica_cache`), eviction and disk usage stats.
use super::*;
use serde::Deserialize;

/// `replica_cache.kind` (and `cache/<kind>` directory) of preview files (tier WebP and
/// older preview representations).
pub const PREVIEW: &str = "preview";
/// `replica_cache.kind` (and `cache/<kind>` directory) of downloaded originals.
pub const ORIGINAL: &str = "original";
const POLICY_FILE: &str = "cache_policy.json";
/// A preview the authority has not generated is not asked for again for this long.
const MISSING_RETRY_MS: i64 = 3600 * 1000;

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS replica_cache(path TEXT PRIMARY KEY, kind TEXT NOT NULL, asset_id TEXT NOT NULL, bytes INTEGER NOT NULL, last_used INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS replica_cache_lru ON replica_cache(kind,last_used);
CREATE INDEX IF NOT EXISTS replica_cache_asset ON replica_cache(asset_id,kind);
CREATE TABLE IF NOT EXISTS replica_cache_missing(asset_id TEXT PRIMARY KEY, until INTEGER NOT NULL);";

/// Which previews a replica keeps.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum Previews {
    /// Only previews already cached are shown.
    Off,
    /// Downloaded when viewed.
    #[default]
    OnDemand,
    /// Every preview, downloaded in the background after the grid thumbnails.
    All,
    /// On demand, least recently used evicted beyond `preview_cap_bytes`.
    Cap,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct CachePolicy {
    pub previews: Previews,
    /// Preview cache bound (any mode; required by `cap`). `None`: unbounded.
    pub preview_cap_bytes: Option<u64>,
    /// Downloaded-originals cache bound. `None`: unbounded.
    pub originals_cap_bytes: Option<u64>,
}

impl Default for CachePolicy {
    fn default() -> Self {
        Self {
            previews: Previews::OnDemand,
            preview_cap_bytes: None,
            originals_cap_bytes: Some(10 << 30),
        }
    }
}

impl CachePolicy {
    pub fn load(state_dir: &Path) -> Result<Self> {
        match fs::read(state_dir.join(POLICY_FILE)) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).context("invalid cache_policy.json")?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }
    pub fn save(&self, state_dir: &Path) -> Result<()> {
        ensure!(
            self.previews != Previews::Cap || self.preview_cap_bytes.is_some(),
            "previews cap requires preview_cap_bytes"
        );
        let temp = state_dir.join(format!(".{POLICY_FILE}.part"));
        fs::write(&temp, serde_json::to_vec_pretty(self)?)?;
        fs::rename(&temp, state_dir.join(POLICY_FILE))?;
        Ok(())
    }
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn relative(library: &Library, path: &Path) -> Result<String> {
    path_text(path.strip_prefix(&library.state_dir).unwrap_or(path))
}

/// Records a use of the cached file `path` (`kind` [`PREVIEW`] or [`ORIGINAL`]).
pub fn used(library: &Library, kind: &str, asset_id: &str, path: &Path) -> Result<()> {
    library.conn.execute_batch(SCHEMA)?;
    let bytes = fs::metadata(path).map_or(0, |m| m.len());
    library.conn.execute(
        "INSERT INTO replica_cache(path,kind,asset_id,bytes,last_used) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(path) DO UPDATE SET bytes=excluded.bytes,last_used=excluded.last_used",
        params![relative(library, path)?, kind, asset_id, bytes as i64, now_ms()],
    )?;
    Ok(())
}

/// Bytes of tracked `kind` files.
pub fn total(library: &Library, kind: &str) -> Result<u64> {
    library.conn.execute_batch(SCHEMA)?;
    Ok(library.conn.query_row(
        "SELECT coalesce(sum(bytes),0) FROM replica_cache WHERE kind=?1",
        [kind],
        |r| r.get::<_, i64>(0),
    )? as u64)
}

/// Files under `cache/<kind>` (temporary `.part` files excluded): `(path, bytes, mtime ms)`.
fn files(library: &Library, kind: &str) -> Vec<(PathBuf, u64, i64)> {
    let Ok(entries) = fs::read_dir(library.state_dir.join("cache").join(kind)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|e| {
            let meta = e.metadata().ok().filter(|m| m.is_file())?;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_millis() as i64);
            Some((e.path(), meta.len(), mtime))
        })
        .collect()
}

/// Brings `replica_cache` in line with `cache/<kind>`: untracked files (cached before
/// tracking) count as last used at their mtime; rows of vanished files are dropped.
pub fn reconcile(library: &Library, kind: &str) -> Result<()> {
    library.conn.execute_batch(SCHEMA)?;
    let present = files(library, kind);
    let tx = library.conn.unchecked_transaction()?;
    let mut keep = std::collections::HashSet::new();
    for (path, bytes, mtime) in &present {
        let rel = relative(library, path)?;
        // Previews are `<asset id>-<source>.<ext>`; original names carry no asset id.
        let asset_id = match kind {
            PREVIEW => path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.rsplit_once('-'))
                .map_or("", |(id, _)| id),
            _ => "",
        };
        tx.prepare_cached(
            "INSERT OR IGNORE INTO replica_cache(path,kind,asset_id,bytes,last_used) VALUES(?1,?2,?3,?4,?5)",
        )?
        .execute(params![rel, kind, asset_id, *bytes as i64, mtime])?;
        keep.insert(rel);
    }
    let tracked: Vec<String> = tx
        .prepare("SELECT path FROM replica_cache WHERE kind=?1")?
        .query_map([kind], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for path in tracked.iter().filter(|p| !keep.contains(*p)) {
        tx.execute("DELETE FROM replica_cache WHERE path=?1", [path])?;
    }
    tx.commit()?;
    Ok(())
}

/// Deletes least recently used `kind` files (never `keep`) until they fit in `cap` bytes.
/// Returns `(files, bytes)` removed.
pub fn evict(library: &Library, kind: &str, cap: u64, keep: &Path) -> Result<(u64, u64)> {
    reconcile(library, kind)?;
    let mut over = total(library, kind)?.saturating_sub(cap);
    let keep = relative(library, keep)?;
    let oldest: Vec<(String, u64)> = library
        .conn
        .prepare("SELECT path, bytes FROM replica_cache WHERE kind=?1 ORDER BY last_used")?
        .query_map([kind], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)))?
        .collect::<rusqlite::Result<_>>()?;
    let (mut files, mut bytes) = (0, 0);
    for (path, size) in oldest {
        if over == 0 {
            break;
        }
        if path == keep {
            continue;
        }
        match fs::remove_file(local_representation(&library.state_dir, &path)?) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        library
            .conn
            .execute("DELETE FROM replica_cache WHERE path=?1", [&path])?;
        over = over.saturating_sub(size);
        files += 1;
        bytes += size;
    }
    Ok((files, bytes))
}

/// Up to `limit` live assets, newest first, with no cached preview that were not recently
/// found missing on the authority.
pub fn missing_previews(library: &Library, limit: usize) -> Result<Vec<String>> {
    library.conn.execute_batch(SCHEMA)?;
    let ids = library
        .conn
        .prepare(
            "SELECT a.id FROM assets a WHERE a.deleted=0
             AND NOT EXISTS (SELECT 1 FROM replica_cache c WHERE c.asset_id=a.id AND c.kind='preview')
             AND NOT EXISTS (SELECT 1 FROM replica_cache_missing m WHERE m.asset_id=a.id AND m.until>?1)
             ORDER BY a.rowid DESC LIMIT ?2",
        )?
        .query_map(params![now_ms(), limit as i64], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// The authority has no preview tier for `asset_id` (yet): skip it for a while.
pub fn mark_missing(library: &Library, asset_id: &str) -> Result<()> {
    library.conn.execute_batch(SCHEMA)?;
    library.conn.execute(
        "INSERT OR REPLACE INTO replica_cache_missing(asset_id,until) VALUES(?1,?2)",
        params![asset_id, now_ms() + MISSING_RETRY_MS],
    )?;
    Ok(())
}

/// Stores a replica's policy and evicts down to its caps right away.
pub fn set_policy(library: &Library, policy: &CachePolicy) -> Result<()> {
    ensure!(library.remote().is_some(), "context is not a remote replica");
    policy.save(&library.state_dir)?;
    for (kind, cap) in [
        (PREVIEW, policy.preview_cap_bytes),
        (ORIGINAL, policy.originals_cap_bytes),
    ] {
        if let Some(cap) = cap {
            evict(library, kind, cap, Path::new(""))?;
        }
    }
    Ok(())
}

/// `{"grid":{count,bytes},"previews":{count,bytes},"originals":{count,bytes},"server":…,
/// "tiers":{…}}`: files on disk per cache, the authority's `/v1/tiers/stats` (null when
/// unreachable), and how complete each always-local tier is (`tiers`: catalog items and
/// sync cursor, micro and grid thumbnails present / queued / upgraded to the authority's grid
/// tier on demand, previews and originals held, views waiting to reach the authority).
pub fn stats(library: &Library) -> Result<Value> {
    let usage = |kinds: &[&str]| {
        let files: Vec<_> = kinds.iter().flat_map(|kind| files(library, kind)).collect();
        json!({"count":files.len(),"bytes":files.iter().map(|f| f.1).sum::<u64>()})
    };
    let server = library.remote_client().ok().and_then(|client| {
        client
            .get_quick::<Value>("/tiers/stats", Duration::from_secs(5))
            .ok()
            .flatten()
    });
    Ok(json!({
        "grid": usage(&["thumbnail", "grid"]),
        "previews": usage(&[PREVIEW]),
        "originals": usage(&[ORIGINAL]),
        "server": server,
        "tiers": tiers(library)?,
    }))
}

fn tiers(library: &Library) -> Result<Value> {
    library.conn.execute_batch(SCHEMA)?;
    library.conn.execute_batch(crate::thumbs::SCHEMA)?;
    let count = |sql: &str| -> Result<u64> {
        Ok(library
            .conn
            .query_row(sql, [], |r| r.get::<_, i64>(0))
            .optional()?
            .unwrap_or(0)
            .max(0) as u64)
    };
    let items = count("SELECT count(*) FROM assets WHERE deleted=0")?;
    let cursor = count("SELECT value FROM replica_state WHERE key='cursor'").unwrap_or(0);
    let touches = count("SELECT count(*) FROM replica_touches").unwrap_or(0);
    Ok(json!({
        "metadata": {"items": items, "cursor": cursor, "unsent_views": touches},
        "micro": {"local": count("SELECT count(*) FROM assets a WHERE a.deleted=0 AND EXISTS (SELECT 1 FROM micro_thumbs m WHERE m.asset_id=a.id AND length(m.data)>0)")?, "total": items},
        "grid": {"local": count("SELECT count(*) FROM assets a WHERE a.deleted=0 AND NOT EXISTS (SELECT 1 FROM eagle_thumbnails t WHERE t.asset_id=a.id AND t.kind='download')")?, "queued": count("SELECT count(*) FROM eagle_thumbnails WHERE kind='download'")?, "upgraded": count("SELECT count(*) FROM grid_thumbs")?, "total": items},
        "preview": {"local": count("SELECT count(DISTINCT asset_id) FROM replica_cache WHERE kind='preview'")?, "total": items},
        "original": {"local": count("SELECT count(DISTINCT asset_id) FROM replica_cache WHERE kind='original'")?, "total": items},
    }))
}
