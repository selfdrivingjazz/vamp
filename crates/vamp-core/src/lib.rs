//! Local authoritative catalog. Originals are read-only references; the only exception is the
//! opt-in `delete_local_after_upload` eviction in `storage` after a verified cloud copy exists.
/// Opt this process (and children it spawns) into downloading dataless cloud placeholders
/// (Google Drive / iCloud / Dropbox File Provider). A parent may launch us with
/// materialization disabled, in which case every placeholder read fails with EDEADLK.
/// Idempotent; no-op off macOS. Entry points call this before touching originals.
pub fn allow_cloud_placeholder_reads() {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn setiopolicy_np(iotype: i32, scope: i32, policy: i32) -> i32;
        }
        const IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES: i32 = 3;
        const IOPOL_SCOPE_PROCESS: i32 = 0;
        const IOPOL_MATERIALIZE_DATALESS_FILES_ON: i32 = 2;
        // Failure leaves the inherited policy; reads then surface the provider's error.
        unsafe {
            setiopolicy_np(
                IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
                IOPOL_SCOPE_PROCESS,
                IOPOL_MATERIALIZE_DATALESS_FILES_ON,
            );
        }
    }
}
mod catalog;
pub use catalog::{ADDED_BY, SidebarFolder, added_by, browse_date, color_distance, hex_to_lab, is_agent};
mod eagle;
pub use eagle::ImportProgress;
mod embeddings;
mod galaxy;
pub mod energy;
mod processing;
pub use embeddings::{EmbedderSpec, EmbeddingState, EmbeddingUnavailable, Scored};
pub use processing::{ClaimedJob, LEASE_SECONDS, MAX_LEASE_ATTEMPTS};
mod tools;
#[cfg(unix)]
pub use tools::run_served;
pub use tools::run_tool;
mod storage;
pub mod fetching;
pub mod io;
pub use storage::{Backend, OriginalLookup, StorageConfig, Upload, store_helper};
#[cfg(feature = "native")]
pub mod native;
#[cfg(all(feature = "native", feature = "remote"))]
mod native_remote;
#[cfg(feature = "remote")]
pub mod remote;
#[cfg(feature = "remote")]
pub mod cache;
pub mod thumbs;
pub mod card;
pub mod explore;
pub mod originals;
pub use originals::OriginalMissing;

use anyhow::{Context as _, Result, ensure};
use chrono::Utc;
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;
use vamp_protocol::*;

fn now() -> String {
    Utc::now().to_rfc3339()
}

thread_local! {
    static ACTOR: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}
/// Runs `f` with this thread's catalog events attributed to `actor` (vamp-server: the
/// requesting tailnet peer) instead of `VAMP_ACTOR`.
pub fn with_actor<R>(actor: Option<String>, f: impl FnOnce() -> R) -> R {
    let previous = ACTOR.with(|a| a.replace(actor));
    let result = f();
    ACTOR.with(|a| *a.borrow_mut() = previous);
    result
}
/// Who catalog events written now are attributed to: the [`with_actor`] override, else
/// `VAMP_ACTOR`, else `local`.
pub fn current_actor() -> String {
    ACTOR
        .with(|a| a.borrow().clone())
        .or_else(|| std::env::var("VAMP_ACTOR").ok().filter(|a| !a.trim().is_empty()))
        .unwrap_or_else(|| "local".into())
}
fn id() -> String {
    Uuid::new_v4().to_string()
}
fn json_text<T: Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}
fn parse<T: DeserializeOwned>(value: String) -> Result<T> {
    Ok(serde_json::from_str(&value)?)
}
fn absolute(path: &Path) -> Result<PathBuf> {
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    })
}
fn path_text(path: &Path) -> Result<String> {
    Ok(path.to_str().context("path is not UTF-8")?.to_owned())
}
/// `<state_dir>` file holding where vamp-server serves an authority library
/// ([`Library::server_url`]).
const SERVER_URL_FILE: &str = "server-url.json";
fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "name must contain 1–64 ASCII letters, digits, hyphens or underscores"
    );
    Ok(())
}
fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("missing parent directory")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".{}.tmp", id()));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        // Config may hold remote bearer tokens; catalog metadata is owner-only too.
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&temp)?;
        file.write_all(&serde_json::to_vec_pretty(value)?)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
fn config_lock(home: &Path) -> Result<File> {
    fs::create_dir_all(home)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(home.join("config.lock"))?;
    file.lock_exclusive()?;
    Ok(file)
}
pub fn default_home() -> PathBuf {
    std::env::var_os("VAMP_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local/share/vamp")
        })
}
pub fn load_config(home: &Path) -> Result<Config> {
    match fs::read(home.join("config.json")) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).context("invalid VAMP config")?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e.into()),
    }
}

// App sandboxes may move on update. Keep managed catalogs relative to their
// configuration home; explicitly attached external catalogs stay absolute.
fn stored_context(home: &Path, context: &Context) -> Result<Context> {
    let home = home.canonicalize()?;
    let state = Path::new(&context.state_dir);
    Ok(Context {
        library_id: context.library_id.clone(),
        state_dir: path_text(state.strip_prefix(&home).unwrap_or(state))?,
        original_root: context.original_root.clone(),
        remote: context.remote.clone(),
    })
}
pub fn select_context(home: &Path, name: &str) -> Result<Config> {
    let _lock = config_lock(home)?;
    let mut config = load_config(home)?;
    ensure!(
        config.contexts.contains_key(name),
        "context not found: {name}"
    );
    config.current = Some(name.into());
    atomic_json(&home.join("config.json"), &config)?;
    Ok(config)
}
pub fn attach_context(home: &Path, name: &str, state: &Path) -> Result<Context> {
    validate_name(name)?;
    let _lock = config_lock(home)?;
    let mut config = load_config(home)?;
    ensure!(
        !config.contexts.contains_key(name),
        "context already exists: {name}"
    );
    let state = state
        .canonicalize()
        .context("library state directory unavailable")?;
    let manifest: Value = serde_json::from_slice(&fs::read(state.join("library.json"))?)?;
    ensure!(
        manifest["protocol_version"] == PROTOCOL_VERSION,
        "unsupported library protocol version"
    );
    ensure!(
        state.join("catalog.sqlite3").is_file(),
        "library catalog not found"
    );
    let context = Context {
        library_id: manifest["library_id"]
            .as_str()
            .context("library manifest missing identity")?
            .into(),
        state_dir: path_text(&state)?,
        original_root: None,
        remote: None,
    };
    config
        .contexts
        .insert(name.into(), stored_context(home, &context)?);
    if config.current.is_none() {
        config.current = Some(name.into());
    }
    atomic_json(&home.join("config.json"), &config)?;
    Ok(context)
}

pub struct Library {
    pub context: Context,
    pub state_dir: PathBuf,
    pub(crate) conn: Connection,
}
impl Library {
    pub fn init(
        home: &Path,
        name: &str,
        original_root: Option<&Path>,
        state_dir: Option<&Path>,
    ) -> Result<Self> {
        validate_name(name)?;
        let home = destination(home)?;
        let root = original_root
            .map(Path::canonicalize)
            .transpose()
            .context("original root unavailable")?;
        if let Some(root) = &root {
            ensure!(root.is_dir(), "original root must be a directory");
            ensure!(
                !home.starts_with(root),
                "configuration must be outside the original storage root"
            );
        }
        let _lock = config_lock(&home)?;
        let mut config = load_config(&home)?;
        ensure!(
            !config.contexts.contains_key(name),
            "context already exists: {name}"
        );
        let library_id = id();
        let state = destination(
            &state_dir
                .map(Path::to_path_buf)
                .unwrap_or_else(|| home.join("libraries").join(&library_id)),
        )?;
        if let Some(root) = &root {
            ensure!(
                !state.starts_with(root),
                "local state must be outside the original storage root"
            );
        }
        ensure!(
            !state.exists() || fs::read_dir(&state)?.next().is_none(),
            "state directory must be new or empty; use context add to attach an existing library"
        );
        fs::create_dir_all(&state)?;
        let state = state.canonicalize()?;
        let context = Context {
            library_id,
            state_dir: path_text(&state)?,
            original_root: root.as_ref().map(|p| path_text(p)).transpose()?,
            remote: None,
        };
        atomic_json(
            &state.join("library.json"),
            &json!({"protocol_version": PROTOCOL_VERSION, "library_id": context.library_id, "storage": "referenced", "writer_model": "local_authority"}),
        )?;
        let lib = Self::connect(context)?;
        config
            .contexts
            .insert(name.into(), stored_context(&home, &lib.context)?);
        config.current = Some(name.into());
        atomic_json(&home.join("config.json"), &config)?;
        Ok(lib)
    }
    pub fn open(home: &Path, name: Option<&str>) -> Result<Self> {
        let config = load_config(home)?;
        let name = name
            .or(config.current.as_deref())
            .context("no active context; run vamp init")?;
        let mut context = config
            .contexts
            .get(name)
            .with_context(|| format!("context not found: {name}"))?
            .clone();
        if Path::new(&context.state_dir).is_relative() {
            context.state_dir = path_text(&home.canonicalize()?.join(&context.state_dir))?;
        }
        let manifest: Value = serde_json::from_slice(
            &fs::read(Path::new(&context.state_dir).join("library.json"))
                .context("local library manifest unavailable")?,
        )?;
        ensure!(
            manifest["protocol_version"] == PROTOCOL_VERSION,
            "unsupported library protocol version"
        );
        ensure!(
            manifest["library_id"] == context.library_id,
            "library identity differs from context"
        );
        ensure!(
            Path::new(&context.state_dir)
                .join("catalog.sqlite3")
                .is_file(),
            "local catalog unavailable; refusing to create an empty replacement"
        );
        Self::connect(context)
    }
    /// Independent connection to this already-open library (background filesystem work).
    pub fn reopen(&self) -> Result<Self> {
        Self::connect(self.context.clone())
    }
    /// Where vamp-server serves this authority library (http base URL), as recorded in
    /// `<state_dir>/server-url.json` by `vamp contexts url`; `vamp explore link` bounces
    /// through it when no URL is given. Replicas use their authority URL instead.
    pub fn server_url(&self) -> Result<Option<String>> {
        match fs::read(self.state_dir.join(SERVER_URL_FILE)) {
            Ok(bytes) => {
                let value: Value = serde_json::from_slice(&bytes)
                    .with_context(|| format!("invalid {}", self.state_dir.join(SERVER_URL_FILE).display()))?;
                Ok(value["url"].as_str().map(str::to_owned))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    /// Record (or with `None` forget) [`Library::server_url`]; refused on replicas.
    pub fn set_server_url(&self, url: Option<&str>) -> Result<Option<String>> {
        ensure!(
            self.context.remote.is_none(),
            "this context is a replica; its links already use its authority's URL"
        );
        let path = self.state_dir.join(SERVER_URL_FILE);
        let Some(url) = url else {
            match fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => return Ok(None),
            }
        };
        let url = url.trim().trim_end_matches('/');
        ensure!(
            url.split_once("://").is_some_and(|(scheme, host)| {
                matches!(scheme, "http" | "https") && !host.is_empty() && !host.contains(char::is_whitespace)
            }),
            "server URL must be an http(s) URL: {url}"
        );
        atomic_json(&path, &json!({"url": url}))?;
        Ok(Some(url.to_owned()))
    }
    fn connect(context: Context) -> Result<Self> {
        let state_dir = PathBuf::from(&context.state_dir);
        let mut conn = Connection::open(state_dir.join("catalog.sqlite3"))?;
        conn.busy_timeout(Duration::from_secs(10))?;
        // Other processes (vamp-server, the CLI, the app) write the same catalog: a write
        // transaction takes the write lock when it begins, waiting out the busy timeout, instead
        // of failing with SQLITE_BUSY when it upgrades from a read another writer outdated.
        conn.set_transaction_behavior(TransactionBehavior::Immediate);
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS assets(id TEXT PRIMARY KEY, path TEXT NOT NULL UNIQUE, hash TEXT NOT NULL, deleted INTEGER NOT NULL DEFAULT 0, document TEXT NOT NULL);
CREATE VIRTUAL TABLE IF NOT EXISTS search USING fts5(asset_id UNINDEXED, text, tokenize='unicode61');
CREATE TABLE IF NOT EXISTS representations(asset_id TEXT NOT NULL REFERENCES assets(id), kind TEXT NOT NULL, source_hash TEXT NOT NULL, document TEXT NOT NULL, PRIMARY KEY(asset_id,kind));
CREATE TABLE IF NOT EXISTS observations(id TEXT PRIMARY KEY,asset_id TEXT NOT NULL REFERENCES assets(id),source_hash TEXT NOT NULL,document TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS events(sequence INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE, document TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS collections(id TEXT PRIMARY KEY,name TEXT NOT NULL UNIQUE);
CREATE TABLE IF NOT EXISTS memberships(collection_id TEXT NOT NULL REFERENCES collections(id),asset_id TEXT NOT NULL REFERENCES assets(id),PRIMARY KEY(collection_id,asset_id));
CREATE TABLE IF NOT EXISTS processors(name TEXT PRIMARY KEY,document TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS jobs(id TEXT PRIMARY KEY,asset_id TEXT NOT NULL REFERENCES assets(id),processor TEXT NOT NULL,event_id TEXT NOT NULL,status TEXT NOT NULL,document TEXT NOT NULL,UNIQUE(processor,event_id));
CREATE INDEX IF NOT EXISTS jobs_status ON jobs(status);
CREATE INDEX IF NOT EXISTS jobs_asset ON jobs(asset_id,processor);
CREATE INDEX IF NOT EXISTS observations_asset ON observations(asset_id);")?;
        conn.execute_batch(embeddings::SCHEMA)?;
        conn.execute_batch(galaxy::SCHEMA)?;
        conn.execute_batch(storage::SCHEMA)?;
        conn.execute_batch(eagle::SCHEMA)?;
        conn.execute_batch(thumbs::SCHEMA)?;
        thumbs::migrate(&conn)?;
        catalog::migrate(&conn)?;
        Ok(Self {
            context,
            state_dir,
            conn,
        })
    }
    pub fn add(
        &mut self,
        path: &Path,
        name: Option<&str>,
        mut metadata: Metadata,
    ) -> Result<Asset> {
        let source = if path.is_absolute() {
            path.to_owned()
        } else if let Some(root) = &self.context.original_root {
            Path::new(root).join(path)
        } else {
            absolute(path)?
        };
        let source = source
            .canonicalize()
            .context("original unavailable for referenced ingestion")?;
        ensure!(
            !source.starts_with(&self.state_dir),
            "originals must not be inside local catalog state"
        );
        let original = fingerprint(&source)?;
        let mut original = original;
        original.import_root = self.context.original_root.clone().or_else(|| {
            source.parent().and_then(|p| p.to_str()).map(str::to_owned)
        });
        normalize_tags(&mut metadata.tags);
        let existing: Option<(String, bool)> = self
            .conn
            .query_row(
                "SELECT document,deleted FROM assets WHERE reference_path=?1 ORDER BY deleted,id LIMIT 1",
                [&original.path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((document, deleted)) = existing {
            let asset: Asset = parse(document)?;
            ensure!(
                !deleted,
                "reference is tombstoned; use a different library or retain the existing deletion record"
            );
            if asset.original.pending() {
                // Bytes were just read: resolve the lazy fingerprint instead of re-adding.
                return self.apply_fingerprint(&asset.id, original);
            }
            ensure!(
                asset.original.blake3 == original.blake3,
                "original changed; use media refresh to create a new revision"
            );
            return Ok(asset);
        }
        let tx = self.conn.transaction()?;
        let timestamp = now();
        catalog::set_added_by(&mut metadata, &current_actor());
        let mut asset = Asset {
            id: id(),
            revision: 1,
            name: name.map(str::to_owned).unwrap_or_else(|| {
                source
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            }),
            original,
            metadata,
            created_at: timestamp.clone(),
            updated_at: timestamp,
            width: None,
            height: None,
            original_created_at: None,
            original_modified_at: None,
            last_viewed_at: None,
            deleted_at: None,
        };
        let cloud = storage::config(&tx)?.is_some();
        if cloud {
            asset.original.storage = "uploading".into();
        }
        tx.execute(
            "INSERT INTO assets(id,path,hash,document) VALUES(?1,?2,?3,?4)",
            params![
                asset.id,
                asset.original.path,
                asset.original.source_key(),
                json_text(&asset)?
            ],
        )?;
        reindex(&tx, &asset)?;
        if cloud {
            storage::enqueue_upload(&tx, &asset)?;
        }
        emit(
            &tx,
            &self.context,
            "media_added",
            Some(&asset),
            json!({"storage":"referenced"}),
        )?;
        tx.commit()?;
        Ok(asset)
    }
    /// Directory receiving explicitly ingested new bytes (clips, pastes, uploads).
    /// Inside original_root when configured, else a sibling of the catalog state.
    pub fn inbox_dir(&self) -> PathBuf {
        match &self.context.original_root {
            Some(root) => Path::new(root).join("Inbox"),
            None => {
                let mut name = self.state_dir.file_name().unwrap_or_default().to_owned();
                name.push(".inbox");
                self.state_dir.with_file_name(name)
            }
        }
    }
    /// Write new bytes to the inbox as an immutable original, then reference it normally.
    pub fn ingest_bytes(
        &mut self,
        bytes: &[u8],
        filename_hint: &str,
        metadata: Metadata,
    ) -> Result<Asset> {
        let (path, display) = self.write_inbox(bytes, filename_hint)?;
        self.add(&path, display.as_deref(), metadata)
            .inspect_err(|_| {
                let _ = fs::remove_file(&path);
            })
    }
    /// Writes `bytes` to a new inbox file named after `filename_hint`'s extension; returns
    /// its path and the display name the hint suggests.
    fn write_inbox(&self, bytes: &[u8], filename_hint: &str) -> Result<(PathBuf, Option<String>)> {
        ensure!(!bytes.is_empty(), "ingested content is empty");
        let hint = Path::new(filename_hint);
        let extension = hint
            .extension()
            .and_then(|e| e.to_str())
            .filter(|e| {
                !e.is_empty() && e.len() <= 8 && e.chars().all(|c| c.is_ascii_alphanumeric())
            })
            .map(str::to_ascii_lowercase);
        let stem = hint
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let month = Utc::now().format("%Y/%m").to_string();
        let dir = self.inbox_dir().join(month);
        fs::create_dir_all(&dir)?;
        let file = match &extension {
            Some(ext) => format!("{}.{ext}", id()),
            None => id(),
        };
        let path = dir.join(file);
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let written = out.write_all(bytes).and_then(|()| out.sync_all());
        drop(out);
        if let Err(error) = written {
            let _ = fs::remove_file(&path);
            return Err(error.into());
        }
        let display = stem.map(|s| match &extension {
            Some(ext) => format!("{s}.{ext}"),
            None => s.to_owned(),
        });
        Ok((path, display))
    }
    /// Live assets in insertion order, or ranked by a full-text `query`; `added_by` is the
    /// browse filter (`owner`, `agents`, `actor:<actor>`).
    pub fn list(
        &self,
        query: Option<&str>,
        limit: usize,
        offset: usize,
        added_by: Option<&str>,
    ) -> Result<Vec<Asset>> {
        ensure!(
            (1..=1000).contains(&limit),
            "limit must be between 1 and 1000"
        );
        ensure!(offset <= i64::MAX as usize, "offset too large");
        let words = query
            .unwrap_or("")
            .split_whitespace()
            .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" AND ");
        let ranked = !words.is_empty();
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        let mut sql = if ranked {
            args.push(words.into());
            "SELECT a.document FROM search CROSS JOIN assets a ON a.id=search.asset_id WHERE search MATCH ? AND a.deleted=0 AND a.trashed=0".to_owned()
        } else {
            "SELECT a.document FROM assets a WHERE a.deleted=0 AND a.trashed=0".to_owned()
        };
        if let Some(added_by) = added_by {
            let clause = catalog::added_by_clause(added_by)?;
            sql.push_str(" AND (");
            sql.push_str(&clause.sql);
            sql.push(')');
            args.extend(clause.args);
        }
        sql.push_str(if ranked {
            " ORDER BY bm25(search),a.id LIMIT ? OFFSET ?"
        } else {
            " ORDER BY a.rowid LIMIT ? OFFSET ?"
        });
        args.push((limit as i64).into());
        args.push((offset as i64).into());
        let mut stmt = self.conn.prepare(&sql)?;
        let values = stmt
            .query_map(rusqlite::params_from_iter(args), |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        values.into_iter().map(parse).collect()
    }
    pub fn inspect(&self, id: &str) -> Result<AssetView> {
        let asset = get_asset(&self.conn, id)?;
        let key = asset.original.source_key();
        let representations = documents::<Representation>(
            &self.conn,
            "SELECT document FROM representations WHERE asset_id=?1 AND source_hash=?2 ORDER BY kind",
            params![id, key],
        )?;
        let observations = documents::<Observation>(
            &self.conn,
            "SELECT document FROM observations WHERE asset_id=?1 AND source_hash=?2 ORDER BY rowid",
            params![id, key],
        )?;
        let suggested_tags = embeddings::suggested_tags(&asset, &observations);
        Ok(AssetView {
            asset,
            representations,
            observations,
            suggested_tags,
        })
    }
    pub fn update(
        &mut self,
        id: &str,
        patch: MetadataPatch,
        expected_revision: Option<u64>,
    ) -> Result<Asset> {
        let tx = self.conn.transaction()?;
        let mut asset = get_asset(&tx, id)?;
        check_revision(&asset, expected_revision)?;
        if let Some(name) = patch.name {
            asset.name = name;
        }
        if let Some(description) = patch.description {
            asset.metadata.description = Some(description);
        }
        if let Some(mut tags) = patch.tags {
            normalize_tags(&mut tags);
            asset.metadata.tags = tags;
        }
        if let Some(add) = patch.add_tags {
            asset.metadata.tags.extend(add);
            normalize_tags(&mut asset.metadata.tags);
        }
        if let Some(remove) = patch.remove_tags {
            let remove: Vec<&str> = remove.iter().map(|t| t.trim()).collect();
            asset
                .metadata
                .tags
                .retain(|t| !remove.contains(&t.as_str()));
        }
        if let Some(url) = patch.source_url {
            asset.metadata.source_url = Some(url);
        }
        if let Some(rating) = patch.rating {
            ensure!(rating <= 5, "rating must be 0–5");
            asset.metadata.rating = rating;
        }
        bump(&mut asset);
        save_asset(&tx, &asset)?;
        emit(
            &tx,
            &self.context,
            "media_updated",
            Some(&asset),
            json!({"change":"metadata"}),
        )?;
        tx.commit()?;
        Ok(asset)
    }
    pub fn refresh(&mut self, id: &str, expected_revision: Option<u64>) -> Result<Asset> {
        let snapshot = get_asset(&self.conn, id)?;
        check_revision(&snapshot, expected_revision)?;
        ensure!(
            !snapshot.original.path.is_empty(),
            "original is stored only in cloud storage ({}); there is no local file to refresh — fetch it and add the edited file as a new asset",
            snapshot.original.locator.as_deref().unwrap_or("no locator")
        );
        let mut original = fingerprint(Path::new(&snapshot.original.path))?;
        original.import_root = snapshot.original.import_root.clone();
        if snapshot.original.pending() {
            return self.apply_fingerprint(id, original);
        }
        let tx = self.conn.transaction()?;
        let mut asset = get_asset(&tx, id)?;
        check_revision(&asset, Some(snapshot.revision))?;
        if asset.original.blake3 == original.blake3 {
            return Ok(asset);
        }
        let cloud = storage::config(&tx)?.is_some();
        if cloud {
            original.storage = "uploading".into();
        }
        asset.original = original;
        bump(&mut asset);
        tx.execute("DELETE FROM representations WHERE asset_id=?1", [id])?;
        save_asset(&tx, &asset)?;
        if cloud {
            storage::enqueue_upload(&tx, &asset)?;
        }
        emit(
            &tx,
            &self.context,
            "media_updated",
            Some(&asset),
            json!({"change":"original","previous_hash":snapshot.original.blake3}),
        )?;
        tx.commit()?;
        Ok(asset)
    }
    /// Moves the asset to the trash (restorable); see [`Library::empty_trash`] for tombstoning.
    pub fn remove(&mut self, id: &str, expected_revision: Option<u64>) -> Result<()> {
        self.trash(&[id.to_owned()], expected_revision).map(|_| ())
    }
    pub fn fetch(&self, id: &str, kind: &str) -> Result<PathBuf> {
        let asset = get_asset(&self.conn, id)?;
        if kind == "original" {
            let result = self.fetch_original(&asset.original);
            if let Err(error) = &result {
                if originals::is_not_found(error) {
                    self.record_original_check(&asset.id, &asset.original.path, originals::Check::Missing)?;
                    return Err(OriginalMissing { asset_id: asset.id, path: asset.original.path }.into());
                }
            }
            return result;
        }
        let path = {
            ensure!(
                matches!(kind, "thumbnail" | "preview" | "loop"),
                "representation must be original, thumbnail, preview or loop"
            );
            let rep: Representation = self.conn.query_row("SELECT document FROM representations WHERE asset_id=?1 AND kind=?2 AND source_hash=?3",params![id,kind,asset.original.source_key()],|r|r.get::<_,String>(0)).optional()?.map(parse).transpose()?.with_context(||format!("representation unavailable: {kind}; generate it while the original is accessible"))?;
            local_representation(&self.state_dir, &rep.path)?
        };
        ensure!(
            path.is_file(),
            "representation unavailable: {}",
            path.display()
        );
        Ok(path)
    }
    /// `fetch`, generating a missing thumbnail/preview from the original with `helper` first
    /// (processor inputs and remote workers need it; the Eagle import only brings thumbnails).
    pub fn fetch_or_generate(
        &mut self,
        id: &str,
        kind: &str,
        helper: Option<&Path>,
    ) -> Result<PathBuf> {
        match (self.fetch(id, kind), helper) {
            (Err(error), Some(helper))
                if matches!(kind, "thumbnail" | "preview" | "loop")
                    && format!("{error:#}").contains("representation unavailable") =>
            {
                let rep = self.generate(id, kind, helper)?;
                local_representation(&self.state_dir, &rep.path)
            }
            (result, _) => result,
        }
    }
    /// Thumbnail/preview from the original. Rendering (which reads the original) runs on the
    /// background I/O pool: a stalled cloud folder fails with [`io::Stalled`] (retry later).
    pub fn generate(&mut self, id: &str, kind: &str, helper: &Path) -> Result<Representation> {
        match self.prepare_generation(id, kind)? {
            Generation::Ready(rep) => Ok(rep),
            Generation::Pending(job) => {
                let helper = helper.to_owned();
                let rendered = io::background().try_run(
                    format!("generate {kind} {id}"),
                    io::DRIVE_DEADLINE,
                    move || job.render(&helper),
                )?;
                self.attach_generated(rendered)
            }
        }
    }
    /// First phase of `generate` (catalog reads only, no filesystem access to the original;
    /// callers may hold a lock): the current representation when its file exists, otherwise
    /// the work `GenerationJob::render` does without the library (reading/hydrating the
    /// original and running the helper).
    pub fn prepare_generation(&self, id: &str, kind: &str) -> Result<Generation> {
        ensure!(
            matches!(kind, "thumbnail" | "preview" | "loop"),
            "only thumbnail, preview or loop can be generated"
        );
        let view = self.inspect(id)?;
        if let Some(rep) = view.representations.into_iter().find(|r| r.kind == kind)
            && local_representation(&self.state_dir, &rep.path)?.is_file()
        {
            return Ok(Generation::Ready(rep));
        }
        Ok(Generation::Pending(Box::new(GenerationJob {
            source: self.original_lookup(id)?,
            asset: view.asset,
            kind: kind.into(),
            state_dir: self.state_dir.clone(),
            library_id: self.context.library_id.clone(),
        })))
    }
    /// Last phase of `generate`: settles a lazy fingerprint with the digest read while
    /// rendering, then records the representation (and derived size/palette).
    pub fn attach_generated(&mut self, rendered: Rendered) -> Result<Representation> {
        let Rendered {
            asset,
            original,
            rep,
            source_size,
            palette,
        } = rendered;
        let id = asset.id.as_str();
        if asset.original.pending() {
            // A changed original becomes a refresh (new revision) and fails the check below.
            self.apply_fingerprint(id, original.clone())?;
        }
        let tx = self.conn.transaction()?;
        let mut current = get_asset(&tx, id)?;
        ensure!(
            current.original.blake3 == original.blake3,
            "stale representation: original revision changed"
        );
        // Derived facts: recorded without a revision bump (representation_ready syncs them).
        let mut derived = false;
        if current.width.is_none()
            && let Some((w, h)) = source_size
        {
            current.width = Some(w);
            current.height = Some(h);
            derived = true;
        }
        if let Some(colors) = palette.filter(|c| !c.is_empty())
            && current.metadata.colors.is_empty()
        {
            current.metadata.colors = colors;
            derived = true;
        }
        if derived {
            save_asset(&tx, &current)?;
        }
        attach_representation(&tx, &self.context, &current, &rep)?;
        tx.commit()?;
        Ok(rep)
    }
}

/// `Library::prepare_generation`: the representation exists, or work to render it.
pub enum Generation {
    Ready(Representation),
    Pending(Box<GenerationJob>),
}

/// A thumbnail/preview to render without holding the library (see `generate`).
pub struct GenerationJob {
    source: OriginalLookup,
    asset: Asset,
    kind: String,
    state_dir: PathBuf,
    library_id: String,
}

/// A rendered representation file waiting for `Library::attach_generated`.
pub struct Rendered {
    asset: Asset,
    /// The original as read while rendering (settles a lazy fingerprint).
    original: Original,
    rep: Representation,
    source_size: Option<(u32, u32)>,
    palette: Option<Vec<ColorSwatch>>,
}

impl GenerationJob {
    /// Reads (hydrating when needed) the original and runs the media helper. May block on a
    /// stalled cloud folder: run it on an [`io`] pool.
    pub fn render(self, helper: &Path) -> Result<Rendered> {
        let Self {
            source,
            mut asset,
            kind,
            state_dir,
            library_id,
        } = self;
        let kind = kind.as_str();
        let input = source.fetch()?;
        let mut original = fingerprint(&input)?;
        // A cache copy keeps the catalog's reference path.
        original.path = asset.original.path.clone();
        if asset.original.pending() {
            asset.original = original.clone();
        }
        ensure!(
            original.blake3 == asset.original.blake3,
            "original changed; run media refresh before generating derivatives"
        );
        let extension = if kind == "loop" { "mp4" } else { "jpg" };
        let relative = format!(
            "representations/{}/{kind}-{}.{extension}",
            asset.original.blake3,
            id::simple()
        );
        let output = state_dir.join(&relative);
        fs::create_dir_all(output.parent().unwrap())?;
        let request = ToolRequest {
            operation: if kind == "loop" { "loop".into() } else { "generate".into() },
            input_path: Some(path_text(&input)?),
            output_path: Some(path_text(&output)?),
            max_edge: Some(match kind { "thumbnail" => 512, "loop" => 480, _ => 1600 }),
            asset: Some(asset.clone()),
            library_id: Some(library_id),
            ..ToolRequest::default()
        };
        // The helper has its own timeout; the inactivity deadline must not cut it short.
        io::expect(Duration::from_secs(330));
        let result = run_tool(&[path_text(helper)?], &request, 300)?;
        io::tick();
        ensure!(
            fingerprint(&input)?.blake3 == asset.original.blake3,
            "original changed during generation; refresh and try again"
        );
        let output_rep = result
            .representation
            .context("media helper returned no representation")?;
        ensure!(
            Path::new(&output_rep.path) == output,
            "helper returned an unexpected output path"
        );
        ensure!(output.is_file(), "helper did not create representation");
        let rep = Representation {
            kind: kind.into(),
            path: relative,
            source_hash: asset.original.blake3.clone(),
            width: output_rep.width,
            height: output_rep.height,
            mime: output_rep.mime,
            byte_size: fs::metadata(&output)?.len(),
        };
        // A fresh thumbnail is the cheap palette source for assets without one.
        let palette = if kind == "thumbnail" && asset.metadata.colors.is_empty() {
            io::expect(Duration::from_secs(330));
            catalog::run_palette(helper, &output).ok()
        } else {
            None
        };
        Ok(Rendered {
            source_size: output_rep.source_width.zip(output_rep.source_height),
            asset,
            original,
            rep,
            palette,
        })
    }
}

impl Library {
    /// Creates a folder, last among its siblings.
    pub fn collection_create(&mut self, name: &str, parent_id: Option<&str>) -> Result<Collection> {
        ensure!(!name.trim().is_empty(), "collection name cannot be empty");
        let tx = self.conn.transaction()?;
        if let Some(parent) = parent_id {
            catalog::collection_exists(&tx, parent)?;
        }
        let sort_order: i64 = tx.query_row(
            "SELECT coalesce(max(sort_order)+1,0) FROM collections WHERE parent_id IS ?1",
            [parent_id],
            |r| r.get(0),
        )?;
        let collection = Collection {
            id: id(),
            name: name.into(),
            asset_ids: vec![],
            parent_id: parent_id.map(str::to_owned),
            sort_order,
            description: None,
            color: None,
            icon: None,
            source_id: None,
        };
        catalog::insert_collection(&tx, &collection)?;
        emit(
            &tx,
            &self.context,
            "collection_created",
            None,
            json!(&collection),
        )?;
        tx.commit()?;
        Ok(collection)
    }
    /// Every folder, parents before children, siblings by `sort_order`.
    pub fn collections(&self) -> Result<Vec<Collection>> {
        let mut collections = catalog::collection_rows(&self.conn)?;
        let mut stmt = self
            .conn
            .prepare("SELECT asset_id FROM memberships WHERE collection_id=?1 ORDER BY rowid")?;
        for collection in &mut collections {
            collection.asset_ids = stmt
                .query_map([&collection.id], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }
        Ok(collections)
    }
    /// Errors with `collection not found: <id>` unless the folder exists.
    pub fn ensure_collection(&self, id: &str) -> Result<()> {
        catalog::collection_exists(&self.conn, id)
    }
    pub fn collection_membership(
        &mut self,
        collection_id: &str,
        asset_id: &str,
        add: bool,
    ) -> Result<Collection> {
        self.change_membership(collection_id, asset_id, add)?;
        self.collections()?
            .into_iter()
            .find(|c| c.id == collection_id)
            .context("collection not found")
    }
    /// Files an asset into a folder without loading every folder's members (clippers).
    pub fn collection_add(&mut self, collection_id: &str, asset_id: &str) -> Result<()> {
        self.change_membership(collection_id, asset_id, true)
    }
    fn change_membership(&mut self, collection_id: &str, asset_id: &str, add: bool) -> Result<()> {
        let tx = self.conn.transaction()?;
        get_asset(&tx, asset_id)?;
        catalog::collection_exists(&tx, collection_id)?;
        let changed = if add {
            tx.execute(
                "INSERT OR IGNORE INTO memberships(collection_id,asset_id) VALUES(?1,?2)",
                params![collection_id, asset_id],
            )?
        } else {
            tx.execute(
                "DELETE FROM memberships WHERE collection_id=?1 AND asset_id=?2",
                params![collection_id, asset_id],
            )?
        };
        if changed > 0 {
            emit(
                &tx,
                &self.context,
                "collection_updated",
                None,
                json!({"collection_id":collection_id,"asset_id":asset_id,"operation":if add {"add"} else {"remove"}}),
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn collection_rename(&mut self, collection_id: &str, name: &str) -> Result<Collection> {
        ensure!(!name.trim().is_empty(), "collection name cannot be empty");
        let tx = self.conn.transaction()?;
        let changed = tx.execute(
            "UPDATE collections SET name=?2 WHERE id=?1",
            params![collection_id, name],
        )?;
        ensure!(changed == 1, "collection not found");
        emit(
            &tx,
            &self.context,
            "collection_updated",
            None,
            json!({"collection_id":collection_id,"name":name,"operation":"rename"}),
        )?;
        tx.commit()?;
        self.collections()?
            .into_iter()
            .find(|c| c.id == collection_id)
            .context("collection not found")
    }
    /// Sets (or clears, with `None`/blank) a folder's description.
    pub fn collection_describe(
        &mut self,
        collection_id: &str,
        description: Option<&str>,
    ) -> Result<Collection> {
        let description = description.map(str::trim).filter(|d| !d.is_empty());
        let tx = self.conn.transaction()?;
        let changed = tx.execute(
            "UPDATE collections SET description=?2 WHERE id=?1",
            params![collection_id, description],
        )?;
        ensure!(changed == 1, "collection not found");
        emit(
            &tx,
            &self.context,
            "collection_updated",
            None,
            json!({"collection_id":collection_id,"operation":"describe"}),
        )?;
        tx.commit()?;
        self.collections()?
            .into_iter()
            .find(|c| c.id == collection_id)
            .context("collection not found")
    }
    /// Deletes the folder and its memberships; member assets are untouched and child
    /// folders move up to the deleted folder's parent.
    pub fn collection_delete(&mut self, collection_id: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        let (name, parent): (String, Option<String>) = tx
            .query_row(
                "SELECT name,parent_id FROM collections WHERE id=?1",
                [collection_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .context("collection not found")?;
        tx.execute(
            "DELETE FROM memberships WHERE collection_id=?1",
            [collection_id],
        )?;
        tx.execute(
            "UPDATE collections SET parent_id=?2 WHERE parent_id=?1",
            params![collection_id, parent],
        )?;
        tx.execute("DELETE FROM collections WHERE id=?1", [collection_id])?;
        catalog::renumber(&tx, parent.as_deref())?;
        emit(
            &tx,
            &self.context,
            "collection_deleted",
            None,
            json!({"collection_id":collection_id,"name":name}),
        )?;
        tx.commit()?;
        Ok(())
    }
    /// Live (non-trashed) asset tag usage, most used first.
    pub fn tag_counts(&self) -> Result<Vec<(String, u64)>> {
        let mut stmt = self.conn.prepare_cached("SELECT t.tag,count(*) FROM asset_tags t JOIN assets a ON a.id=t.asset_id WHERE a.deleted=0 AND a.trashed=0 GROUP BY t.tag ORDER BY count(*) DESC, t.tag")?;
        Ok(stmt
            .query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn events(&self, after: u64, limit: usize) -> Result<Vec<Event>> {
        ensure!(
            after <= i64::MAX as u64 && (1..=1000).contains(&limit),
            "invalid event cursor or limit (1–1000)"
        );
        documents(
            &self.conn,
            "SELECT document FROM events WHERE sequence>?1 ORDER BY sequence LIMIT ?2",
            params![after as i64, limit as i64],
        )
    }
    /// Highest event sequence (0 when there are none); cheap enough to poll.
    pub fn last_event(&self) -> Result<u64> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(sequence),0) FROM events", [], |r| {
                r.get::<_, i64>(0)
            })? as u64)
    }
    /// Replica bootstrap: every non-tombstoned asset view (live and trashed), folders with
    /// memberships, smart folders, tag groups and starred tags, plus the event `cursor` they
    /// reflect, all read in one transaction. Replicas continue incremental sync from `cursor`.
    pub fn snapshot(&self) -> Result<Value> {
        let tx = read_snapshot(&self.conn)?;
        let cursor = self.last_event()?;
        let ids = tx
            .prepare("SELECT id FROM assets WHERE deleted=0 ORDER BY rowid")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let assets = ids
            .iter()
            .map(|id| self.inspect(id))
            .collect::<Result<Vec<_>>>()?;
        let value = json!({
            "library_id": self.context.library_id,
            "cursor": cursor,
            "assets": assets,
            "collections": self.collections()?,
            "smart_folders": self.smart_folders()?,
            "tags": {"groups": self.tag_groups()?, "starred": self.starred_tags()?},
        });
        tx.commit()?;
        Ok(value)
    }
    pub fn export(&self) -> Result<Value> {
        let assets = documents::<Asset>(
            &self.conn,
            "SELECT document FROM assets WHERE deleted=0 ORDER BY rowid",
            [],
        )?;
        let tombstones = documents::<Asset>(
            &self.conn,
            "SELECT document FROM assets WHERE deleted=1 ORDER BY rowid",
            [],
        )?;
        let mut stmt = self.conn.prepare("SELECT representations.asset_id,representations.document FROM representations JOIN assets ON assets.id=representations.asset_id WHERE assets.deleted=0")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let representations = rows
            .into_iter()
            .map(|(asset_id, document)| {
                Ok(json!({"asset_id":asset_id,"representation":parse::<Representation>(document)?}))
            })
            .collect::<Result<Vec<_>>>()?;
        let observations = documents::<Observation>(
            &self.conn,
            "SELECT document FROM observations ORDER BY rowid",
            [],
        )?;
        let events = documents::<Event>(
            &self.conn,
            "SELECT document FROM events ORDER BY sequence",
            [],
        )?;
        Ok(
            json!({"protocol_version":PROTOCOL_VERSION,"library_id":self.context.library_id,"export_kind":"metadata_and_locators","includes_original_bytes":false,"representation_base":self.context.state_dir,"assets":assets,"tombstones":tombstones,"representations":representations,"observations":observations,"collections":self.collections()?,"events":events}),
        )
    }
}

mod id {
    pub fn simple() -> String {
        uuid::Uuid::new_v4().simple().to_string()
    }
}
/// A read-only transaction: one consistent snapshot without taking the write lock.
fn read_snapshot(conn: &Connection) -> rusqlite::Result<Transaction<'_>> {
    Transaction::new_unchecked(conn, TransactionBehavior::Deferred)
}
fn documents<T: DeserializeOwned>(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<T>> {
    let mut stmt = conn.prepare(sql)?;
    let values = stmt
        .query_map(params, |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    values.into_iter().map(parse).collect()
}
fn get_asset(conn: &Connection, id: &str) -> Result<Asset> {
    conn.query_row(
        "SELECT document FROM assets WHERE id=?1 AND deleted=0",
        [id],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .map(parse)
    .transpose()?
    .with_context(|| format!("asset not found: {id}"))
}
fn check_revision(asset: &Asset, expected: Option<u64>) -> Result<()> {
    if let Some(expected) = expected {
        ensure!(
            asset.revision == expected,
            "revision conflict: expected {expected}, current {}",
            asset.revision
        );
    }
    Ok(())
}
fn bump(asset: &mut Asset) {
    asset.revision += 1;
    asset.updated_at = now();
}
fn normalize_tags(tags: &mut Vec<String>) {
    for tag in tags.iter_mut() {
        *tag = tag.trim().into();
    }
    tags.retain(|t| !t.is_empty());
    tags.sort();
    tags.dedup();
}
fn save_asset(tx: &Transaction, asset: &Asset) -> Result<()> {
    tx.prepare_cached("UPDATE assets SET hash=?2,document=?3 WHERE id=?1")?
        .execute(params![
            asset.id,
            asset.original.source_key(),
            json_text(asset)?
        ])?;
    reindex(tx, asset)
}
/// Rebuilds the asset's full-text row and its `asset_tags` rows.
fn reindex(tx: &Transaction, asset: &Asset) -> Result<()> {
    tx.prepare_cached("DELETE FROM search WHERE asset_id=?1")?
        .execute([&asset.id])?;
    tx.prepare_cached("DELETE FROM asset_tags WHERE asset_id=?1")?
        .execute([&asset.id])?;
    index(tx, asset)
}
/// Inserts the asset's full-text and `asset_tags` rows (callers clear stale ones first).
fn index(tx: &Transaction, asset: &Asset) -> Result<()> {
    let observations = documents::<Observation>(
        tx,
        "SELECT document FROM observations WHERE asset_id=?1 AND source_hash=?2",
        params![asset.id, asset.original.source_key()],
    )?;
    let mut text = format!(
        "{} {} {} {}",
        asset.name,
        asset.metadata.description.as_deref().unwrap_or(""),
        asset.metadata.tags.join(" "),
        asset.metadata.source_url.as_deref().unwrap_or("")
    );
    for observation in observations {
        text.push(' ');
        text.push_str(&serde_json::to_string(&observation.value)?);
    }
    tx.prepare_cached("INSERT INTO search(asset_id,text) VALUES(?1,?2)")?
        .execute(params![asset.id, text])?;
    let mut insert =
        tx.prepare_cached("INSERT OR IGNORE INTO asset_tags(tag,asset_id) VALUES(?1,?2)")?;
    for tag in &asset.metadata.tags {
        insert.execute(params![tag, asset.id])?;
    }
    Ok(())
}
/// Drops an asset's derived index rows when it is tombstoned.
fn unindex(tx: &Transaction, id: &str) -> Result<()> {
    tx.execute("DELETE FROM search WHERE asset_id=?1", [id])?;
    tx.execute("DELETE FROM asset_tags WHERE asset_id=?1", [id])?;
    tx.execute("DELETE FROM memberships WHERE asset_id=?1", [id])?;
    Ok(())
}

fn destination(path: &Path) -> Result<PathBuf> {
    let path = absolute(path)?;
    ensure!(
        !path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir)),
        "configuration paths cannot contain '..'"
    );
    let mut ancestor = path.as_path();
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        suffix.push(
            ancestor
                .file_name()
                .context("invalid destination")?
                .to_owned(),
        );
        ancestor = ancestor
            .parent()
            .context("destination has no existing parent")?;
    }
    let mut resolved = ancestor.canonicalize()?;
    for part in suffix.into_iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}
fn emit(
    tx: &Transaction,
    context: &Context,
    kind: &str,
    asset: Option<&Asset>,
    payload: Value,
) -> Result<Event> {
    let mut event = Event {
        sequence: 0,
        id: id(),
        library_id: context.library_id.clone(),
        kind: kind.into(),
        asset_id: asset.map(|a| a.id.clone()),
        revision: asset.map(|a| a.revision),
        actor: current_actor(),
        created_at: now(),
        payload,
    };
    tx.execute(
        "INSERT INTO events(id,document) VALUES(?1,?2)",
        params![event.id, json_text(&event)?],
    )?;
    event.sequence = tx.last_insert_rowid() as u64;
    tx.execute(
        "UPDATE events SET document=?2 WHERE id=?1",
        params![event.id, json_text(&event)?],
    )?;
    if matches!(
        kind,
        "media_added" | "media_updated" | "representation_ready"
    ) && let Some(asset) = asset
    {
        for processor in
            documents::<ProcessorSpec>(tx, "SELECT document FROM processors ORDER BY name", [])?
        {
            if processor.events.iter().any(|e| e == kind)
                && (kind != "representation_ready" || event.payload["kind"] == processor.input)
            {
                processing::insert_job(tx, &processor, asset, &event.id)?;
            }
        }
    }
    Ok(event)
}
fn fingerprint(path: &Path) -> Result<Original> {
    let mut file =
        File::open(path).with_context(|| format!("original unavailable: {}", path.display()))?;
    let before = file.metadata()?;
    ensure!(before.is_file(), "original must be a regular file");
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut size = 0;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        size += n as u64;
        io::tick();
    }
    let after = file.metadata()?;
    ensure!(
        size == before.len()
            && before.len() == after.len()
            && before.modified()? == after.modified()?,
        "original changed during hashing"
    );
    Ok(Original {
        path: path_text(path)?,
        blake3: hasher.finalize().to_hex().to_string(),
        byte_size: size,
        mime: mime_for(path).into(),
        locator: None,
        storage: "local".into(),
        hash_state: "hashed".into(),
        modified_ns: modified_ns(&after),
        availability: OriginalAvailability { last_checked_at: Some(now()), ..OriginalAvailability::default() },
        import_root: None,
    })
}
/// Lazy fingerprint from `stat` alone: never opens the file (cloud placeholders stay
/// unhydrated). Rejects directories, FIFOs and other non-regular files.
fn stat_original(path: &Path) -> Result<Original> {
    let meta =
        fs::metadata(path).with_context(|| format!("original unavailable: {}", path.display()))?;
    ensure!(meta.is_file(), "original must be a regular file");
    Ok(Original {
        path: path_text(path)?,
        blake3: String::new(),
        byte_size: meta.len(),
        mime: mime_for(path).into(),
        locator: None,
        storage: "local".into(),
        hash_state: "pending".into(),
        modified_ns: modified_ns(&meta),
        availability: OriginalAvailability { last_checked_at: Some(now()), ..OriginalAvailability::default() },
        import_root: None,
    })
}
fn modified_ns(meta: &fs::Metadata) -> Option<i64> {
    meta.modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
}
fn mime_for(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "tif" | "tiff" => "image/tiff",
        "bmp" => "image/bmp",
        "heic" | "heif" => "image/heic",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        "psd" => "image/vnd.adobe.photoshop",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}
fn local_representation(state: &Path, relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    ensure!(
        !path.is_absolute()
            && path
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_))),
        "invalid local representation locator"
    );
    let joined = state.join(path);
    if joined.exists() {
        ensure!(
            joined.canonicalize()?.starts_with(state.canonicalize()?),
            "representation escapes local state"
        );
    }
    Ok(joined)
}
fn attach_representation(
    tx: &Transaction,
    context: &Context,
    asset: &Asset,
    rep: &Representation,
) -> Result<()> {
    tx.execute("INSERT INTO representations(asset_id,kind,source_hash,document) VALUES(?1,?2,?3,?4) ON CONFLICT(asset_id,kind) DO UPDATE SET source_hash=excluded.source_hash,document=excluded.document",params![asset.id,rep.kind,rep.source_hash,json_text(rep)?])?;
    emit(
        tx,
        context,
        "representation_ready",
        Some(asset),
        json!({"kind":rep.kind,"path":rep.path}),
    )?;
    Ok(())
}

/// launchd agent (macOS) keeping a long-running vamp process alive (`vamp-server serve`,
/// `vamp sync --watch`).
pub fn launch_agent_plist(
    label: &str,
    program: &Path,
    args: &[String],
    env: &[(&str, String)],
    log: &Path,
) -> String {
    fn escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }
    let mut arguments = format!(
        "\t\t<string>{}</string>\n",
        escape(&program.to_string_lossy())
    );
    for arg in args {
        arguments.push_str(&format!("\t\t<string>{}</string>\n", escape(arg)));
    }
    let mut environment = String::new();
    for (key, value) in env {
        environment.push_str(&format!(
            "\t\t<key>{}</key>\n\t\t<string>{}</string>\n",
            escape(key),
            escape(value)
        ));
    }
    let log = escape(&log.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
{arguments}	</array>
	<key>EnvironmentVariables</key>
	<dict>
{environment}	</dict>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#,
        label = escape(label)
    )
}

/// Optional vamp-media helper: `VAMP_MEDIA_HELPER`, beside the running executable,
/// or in the enclosing macOS app bundle's Resources.
pub fn media_helper() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("VAMP_MEDIA_HELPER") {
        return Some(PathBuf::from(path)).filter(|p| p.is_file());
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [dir.join("vamp-media"), dir.join("../Resources/vamp-media")]
        .into_iter()
        .find(|p| p.is_file())
}
