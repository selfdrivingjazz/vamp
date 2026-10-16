//! Cloud storage for originals: durable upload queue, folder backend, `vamp-store` S3 helper,
//! on-demand rehydration, and the opt-in (and only) path where VAMP deletes an original.
use super::*;
use serde::Deserialize;
use std::process::{Command, Stdio};

/// Name used in `folder://<backend>/<relpath>` locators. One backend per library; the name
/// keeps locators valid when the synced folder moves (re-run `storage set folder --root`).
pub const FOLDER_BACKEND: &str = "default";
const CREDENTIALS: &str = "storage-credentials.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "lowercase")]
pub enum Backend {
    Folder {
        root: String,
    },
    S3 {
        bucket: String,
        #[serde(default)]
        prefix: String,
        endpoint: String,
        region: String,
        #[serde(default)]
        path_style: bool,
    },
}
/// Per-library backend configuration (catalog `settings` table). Never contains credentials.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StorageConfig {
    #[serde(flatten)]
    pub backend: Backend,
    /// Delete the local original once it is verified in cloud storage and a thumbnail and
    /// preview exist for its hash. Default off. The only code path that deletes an original.
    #[serde(default)]
    pub delete_local_after_upload: bool,
    /// Also allow deletion outside original_root and the inbox. Default off.
    #[serde(default)]
    pub delete_local_anywhere: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Upload {
    pub id: String,
    pub asset_id: String,
    pub source_hash: String,
    /// pending, running, completed, failed, or skipped (asset removed or original replaced).
    pub status: String,
    pub attempts: u32,
    pub error: Option<String>,
    pub created_at: String,
}
#[derive(Serialize, Deserialize)]
struct Credentials {
    access_key_id: String,
    secret_access_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_token: Option<String>,
}

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS uploads(id TEXT PRIMARY KEY, asset_id TEXT NOT NULL REFERENCES assets(id), source_hash TEXT NOT NULL, status TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, error TEXT, created_at TEXT NOT NULL, UNIQUE(asset_id,source_hash));
CREATE INDEX IF NOT EXISTS uploads_status ON uploads(status);";

pub(crate) fn config(conn: &Connection) -> Result<Option<StorageConfig>> {
    conn.query_row(
        "SELECT document FROM settings WHERE key='storage'",
        [],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .map(parse)
    .transpose()
}
/// Durable: a pending row survives crashes; completed rows are never re-queued for a hash.
pub(crate) fn enqueue_upload(tx: &Transaction, asset: &Asset) -> Result<()> {
    tx.execute(
        "INSERT INTO uploads(id,asset_id,source_hash,status,attempts,created_at) VALUES(?1,?2,?3,'pending',0,?4) ON CONFLICT(asset_id,source_hash) DO UPDATE SET status='pending',error=NULL WHERE uploads.status!='completed'",
        params![id(), asset.id, asset.original.source_key(), now()],
    )?;
    Ok(())
}
fn upload_row(r: &rusqlite::Row) -> rusqlite::Result<Upload> {
    Ok(Upload {
        id: r.get(0)?,
        asset_id: r.get(1)?,
        source_hash: r.get(2)?,
        status: r.get(3)?,
        attempts: r.get(4)?,
        error: r.get(5)?,
        created_at: r.get(6)?,
    })
}
fn set_upload(conn: &Connection, id: &str, status: &str, error: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE uploads SET status=?2,error=?3 WHERE id=?1",
        params![id, status, error],
    )?;
    Ok(())
}

/// Extension for cloud keys and cache files, from the local path or the stored locator.
pub(crate) fn original_extension(original: &Original) -> String {
    [Some(original.path.as_str()), original.locator.as_deref()]
        .into_iter()
        .flatten()
        .filter_map(|p| Path::new(p).extension().and_then(|e| e.to_str()))
        .find(|e| !e.is_empty() && e.len() <= 8 && e.bytes().all(|b| b.is_ascii_alphanumeric()))
        .map(|e| format!(".{}", e.to_ascii_lowercase()))
        .unwrap_or_default()
}
fn hash_file(path: &Path) -> Result<(String, u64)> {
    let mut hasher = blake3::Hasher::new();
    let mut file = File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    let size = std::io::copy(&mut file, &mut hasher)?;
    Ok((hasher.finalize().to_hex().to_string(), size))
}
fn relative_locator(rel: &str) -> Result<&Path> {
    let path = Path::new(rel);
    ensure!(
        !rel.is_empty()
            && path
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_))),
        "invalid storage locator path: {rel}"
    );
    Ok(path)
}
fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}
/// Copy through a same-directory temp file, fsync, then rename: readers never see partial bytes.
fn copy_atomic(src: &Path, dest: &Path) -> Result<()> {
    let parent = dest.parent().context("destination has no parent")?;
    fs::create_dir_all(parent)?;
    let name = dest.file_name().unwrap_or_default().to_string_lossy();
    let prefix = format!(".{name}.vamp-tmp-");
    // Residue of an interrupted copy of this object.
    for entry in fs::read_dir(parent)?.filter_map(|e| e.ok()) {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = fs::remove_file(entry.path());
        }
    }
    let tmp = parent.join(format!("{prefix}{}", id::simple()));
    let result = (|| -> Result<()> {
        let mut out = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        std::io::copy(&mut File::open(src)?, &mut out)?;
        out.sync_all()?;
        fs::rename(&tmp, dest)?;
        sync_dir(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// `vamp-store`: `VAMP_STORE_HELPER`, beside the running executable (or its parent, for test
/// binaries in target/*/deps), or in the enclosing macOS app bundle's Resources.
pub fn store_helper() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("VAMP_STORE_HELPER") {
        return Some(PathBuf::from(path)).filter(|p| p.is_file());
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [
        dir.join("vamp-store"),
        dir.join("../Resources/vamp-store"),
        dir.join("../vamp-store"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

impl Library {
    pub fn storage_config(&self) -> Result<Option<StorageConfig>> {
        config(&self.conn)
    }
    pub fn storage_set(&mut self, mut storage: StorageConfig) -> Result<StorageConfig> {
        ensure!(
            self.context.remote.is_none(),
            "storage backends are configured on the authority, not on replicas"
        );
        let tx = self.conn.transaction()?;
        let count = |pattern: &str| -> Result<i64> {
            Ok(tx.query_row(
                "SELECT count(*) FROM assets WHERE json_extract(document,'$.original.locator') LIKE ?1",
                [pattern],
                |r| r.get(0),
            )?)
        };
        match &mut storage.backend {
            Backend::Folder { root } => {
                let dir = Path::new(root.as_str())
                    .canonicalize()
                    .with_context(|| format!("storage folder unavailable: {root}"))?;
                ensure!(dir.is_dir(), "storage root must be a directory");
                ensure!(
                    !dir.starts_with(&self.state_dir) && !self.state_dir.starts_with(&dir),
                    "storage folder must not overlap local catalog state"
                );
                *root = path_text(&dir)?;
                ensure!(
                    count("s3://%")? == 0,
                    "originals are stored in S3; switching backends would strand them"
                );
            }
            Backend::S3 {
                bucket,
                prefix,
                endpoint,
                region,
                ..
            } => {
                ensure!(
                    !bucket.is_empty()
                        && bucket
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.'),
                    "invalid bucket name"
                );
                ensure!(
                    endpoint.starts_with("https://") || endpoint.starts_with("http://"),
                    "S3 endpoint must be an http(s) URL"
                );
                ensure!(!region.is_empty(), "S3 region is required");
                ensure!(
                    !prefix.starts_with('/') && !prefix.contains(".."),
                    "invalid S3 prefix"
                );
                if !prefix.is_empty() && !prefix.ends_with('/') {
                    prefix.push('/');
                }
                ensure!(
                    count("folder://%")? == 0,
                    "originals are stored in a folder backend; switching backends would strand them"
                );
            }
        }
        tx.execute(
            "INSERT INTO settings(key,document) VALUES('storage',?1) ON CONFLICT(key) DO UPDATE SET document=excluded.document",
            [json_text(&storage)?],
        )?;
        tx.commit()?;
        Ok(storage)
    }
    pub fn storage_unset(&mut self) -> Result<()> {
        let cloud_only: i64 = self.conn.query_row(
            "SELECT count(*) FROM assets WHERE deleted=0 AND json_extract(document,'$.original.storage')='cloud'",
            [],
            |r| r.get(0),
        )?;
        ensure!(
            cloud_only == 0,
            "{cloud_only} originals exist only in cloud storage; unsetting the backend would make them unreachable"
        );
        self.conn
            .execute("DELETE FROM settings WHERE key='storage'", [])?;
        self.conn.execute(
            "DELETE FROM uploads WHERE status IN ('pending','failed')",
            [],
        )?;
        Ok(())
    }
    /// Persist S3 credentials in the state directory with mode 0600. They never enter the
    /// catalog, events, or exports. Environment variables take precedence at use.
    pub fn storage_save_credentials(
        &self,
        access_key_id: &str,
        secret_access_key: &str,
        session_token: Option<&str>,
    ) -> Result<PathBuf> {
        ensure!(
            !access_key_id.is_empty() && !secret_access_key.is_empty(),
            "credentials are empty"
        );
        let path = self.state_dir.join(CREDENTIALS);
        let tmp = self
            .state_dir
            .join(format!(".{CREDENTIALS}.{}", id::simple()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&tmp)?;
        let credentials = Credentials {
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
            session_token: session_token.map(str::to_owned),
        };
        let written = file
            .write_all(&serde_json::to_vec(&credentials)?)
            .and_then(|()| file.sync_all())
            .and_then(|()| fs::rename(&tmp, &path));
        if let Err(e) = written {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(path)
    }
    /// Upload records per status, e.g. {"pending":2,"completed":10}.
    pub fn upload_counts(&self) -> Result<Value> {
        let mut stmt = self
            .conn
            .prepare("SELECT status,count(*) FROM uploads GROUP BY status")?;
        let counts = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Value::Object(
            counts.into_iter().map(|(k, v)| (k, json!(v))).collect(),
        ))
    }
    pub fn uploads(&self, status: Option<&str>, limit: usize) -> Result<Vec<Upload>> {
        ensure!((1..=1000).contains(&limit), "upload limit must be 1–1000");
        let mut stmt = self.conn.prepare("SELECT id,asset_id,source_hash,status,attempts,error,created_at FROM uploads WHERE ?1 IS NULL OR status=?1 ORDER BY rowid LIMIT ?2")?;
        Ok(stmt
            .query_map(params![status, limit as i64], upload_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    /// Queue uploads for existing local originals (and re-queue failed ones).
    pub fn storage_migrate(&mut self, limit: usize) -> Result<Value> {
        ensure!((1..=100_000).contains(&limit), "limit must be 1–100000");
        ensure!(
            config(&self.conn)?.is_some(),
            "no storage backend configured; run vamp storage set"
        );
        let tx = self.conn.transaction()?;
        let assets = documents::<Asset>(
            &tx,
            "SELECT document FROM assets WHERE deleted=0 AND json_extract(document,'$.original.path')!='' AND coalesce(json_extract(document,'$.original.storage'),'local') IN ('local','uploading') ORDER BY rowid LIMIT ?1",
            [limit as i64],
        )?;
        let mut queued = Vec::with_capacity(assets.len());
        for mut asset in assets {
            if asset.original.storage != "uploading" {
                asset.original.storage = "uploading".into();
                bump(&mut asset);
                save_asset(&tx, &asset)?;
                emit(
                    &tx,
                    &self.context,
                    "original_upload_queued",
                    Some(&asset),
                    json!({"blake3":asset.original.blake3}),
                )?;
            }
            enqueue_upload(&tx, &asset)?;
            queued.push(asset.id);
        }
        tx.commit()?;
        Ok(json!({"queued":queued.len(),"asset_ids":queued}))
    }
    /// Drain uploads and sweep evictions. Caller holds the library worker lock.
    pub(crate) fn run_uploads(&mut self, limit: usize) -> Result<Value> {
        // Uploads are content-addressed and idempotent: an interrupted one is simply redone.
        self.conn.execute(
            "UPDATE uploads SET status='pending' WHERE status='running'",
            [],
        )?;
        let Some(storage) = config(&self.conn)? else {
            return Ok(json!({"completed":0,"failed":0,"uploads":[],"evicted":[]}));
        };
        let (mut completed, mut failed) = (0, 0);
        let mut uploads = Vec::new();
        for _ in 0..limit {
            let Some(mut upload) = self
                .conn
                .query_row(
                    "SELECT id,asset_id,source_hash,status,attempts,error,created_at FROM uploads WHERE status='pending' ORDER BY rowid LIMIT 1",
                    [],
                    upload_row,
                )
                .optional()?
            else {
                break;
            };
            upload.attempts += 1;
            self.conn.execute(
                "UPDATE uploads SET status='running',attempts=?2 WHERE id=?1",
                params![upload.id, upload.attempts],
            )?;
            match self.upload_one(&storage, &upload) {
                Ok(status) => {
                    upload.status = status.into();
                    upload.error = None;
                    completed += 1;
                }
                Err(e) if crate::io::is_stalled(&e) => {
                    // Cloud-synced folder not responding: retry on a later run.
                    upload.attempts -= 1;
                    self.conn.execute(
                        "UPDATE uploads SET status='pending',attempts=?2,error=?3 WHERE id=?1",
                        params![upload.id, upload.attempts, format!("{e:#}")],
                    )?;
                    break;
                }
                Err(e) => {
                    upload.status = "failed".into();
                    upload.error = Some(format!("{e:#}"));
                    failed += 1;
                }
            }
            set_upload(
                &self.conn,
                &upload.id,
                &upload.status,
                upload.error.as_deref(),
            )?;
            uploads.push(upload);
        }
        let evicted = self.sweep_evictions(&storage, limit)?;
        Ok(json!({"completed":completed,"failed":failed,"uploads":uploads,"evicted":evicted}))
    }
    fn upload_one(&mut self, storage: &StorageConfig, upload: &Upload) -> Result<&'static str> {
        let Ok(mut asset) = get_asset(&self.conn, &upload.asset_id) else {
            return Ok("skipped");
        };
        if asset.original.source_key() != upload.source_hash {
            return Ok("skipped");
        }
        if asset.original.pending() {
            // Content-addressed keys need the digest: settle the lazy fingerprint (the
            // upload row is rekeyed with it; changed bytes enqueue a fresh upload).
            let revision = asset.revision;
            asset = self.resolve_hash(&asset.id)?;
            if asset.revision != revision {
                return Ok("skipped");
            }
        }
        if asset.original.locator.is_some()
            && matches!(asset.original.storage.as_str(), "cloud" | "cloud+local")
        {
            return Ok("completed");
        }
        ensure!(
            !asset.original.path.is_empty(),
            "original has no local copy to upload"
        );
        let local = PathBuf::from(&asset.original.path);
        let digest = {
            let local = local.clone();
            crate::io::background().try_run(
                "verify original before upload",
                crate::io::DRIVE_DEADLINE,
                move || fingerprint(&local),
            )?
        };
        ensure!(
            digest.blake3 == asset.original.blake3,
            "local original changed since it was cataloged; run vamp media refresh"
        );
        let local = local.as_path();
        let locator = self.backend_put(storage, &asset.original, local)?;
        self.backend_verify(storage, &locator, &asset.original)?;
        let tx = self.conn.transaction()?;
        let mut current = get_asset(&tx, &asset.id)?;
        ensure!(
            current.original.blake3 == asset.original.blake3,
            "original changed during upload"
        );
        current.original.locator = Some(locator.clone());
        current.original.storage = "cloud+local".into();
        bump(&mut current);
        save_asset(&tx, &current)?;
        emit(
            &tx,
            &self.context,
            "original_stored",
            Some(&current),
            json!({"locator":locator,"blake3":current.original.blake3,"byte_size":current.original.byte_size}),
        )?;
        tx.commit()?;
        Ok("completed")
    }
    /// The only place VAMP deletes an original. Requires delete_local_after_upload, a verified
    /// cloud copy, current thumbnail+preview, an allowed location, and unchanged local bytes.
    fn sweep_evictions(&mut self, storage: &StorageConfig, limit: usize) -> Result<Vec<String>> {
        if !storage.delete_local_after_upload {
            return Ok(vec![]);
        }
        let allowed_roots = [
            self.context.original_root.as_deref().map(PathBuf::from),
            self.inbox_dir().canonicalize().ok(),
        ];
        let candidates = documents::<Asset>(
            &self.conn,
            "SELECT document FROM assets WHERE deleted=0 AND json_extract(document,'$.original.storage')='cloud+local' ORDER BY rowid",
            [],
        )?;
        let mut evicted = Vec::new();
        for asset in candidates {
            if evicted.len() >= limit {
                break;
            }
            if asset.original.pending() {
                continue;
            }
            let hash = &asset.original.blake3;
            let derivatives = ["thumbnail", "preview"].iter().all(|kind| {
                self.conn
                    .query_row(
                        "SELECT document FROM representations WHERE asset_id=?1 AND kind=?2 AND source_hash=?3",
                        params![asset.id, kind, hash],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()
                    .ok()
                    .flatten()
                    .and_then(|d| parse::<Representation>(d).ok())
                    .and_then(|rep| local_representation(&self.state_dir, &rep.path).ok())
                    .is_some_and(|p| p.is_file())
            });
            if !derivatives || asset.original.path.is_empty() {
                continue;
            }
            let path = Path::new(&asset.original.path);
            if !storage.delete_local_anywhere
                && !allowed_roots
                    .iter()
                    .flatten()
                    .any(|root| path.starts_with(root))
            {
                continue;
            }
            // Only bytes identical to the verified cloud copy are ever deleted.
            let removed = {
                let (path, size, hash) = (path.to_owned(), asset.original.byte_size, hash.clone());
                crate::io::background().try_run(
                    "evict original",
                    crate::io::DRIVE_DEADLINE,
                    move || -> Result<bool> {
                        if !path.exists() {
                            return Ok(true);
                        }
                        let unchanged = fs::metadata(&path).is_ok_and(|m| m.len() == size)
                            && fingerprint(&path).is_ok_and(|o| o.blake3 == hash);
                        if !unchanged {
                            return Ok(false);
                        }
                        fs::remove_file(&path).with_context(|| {
                            format!("cannot delete local original {}", path.display())
                        })?;
                        Ok(true)
                    },
                )
            };
            match removed {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) if crate::io::is_stalled(&e) => break,
                Err(e) => return Err(e),
            }
            let tx = self.conn.transaction()?;
            let mut current = get_asset(&tx, &asset.id)?;
            if &current.original.blake3 != hash {
                continue;
            }
            let previous = std::mem::take(&mut current.original.path);
            current.original.storage = "cloud".into();
            bump(&mut current);
            save_asset(&tx, &current)?;
            emit(
                &tx,
                &self.context,
                "original_evicted",
                Some(&current),
                json!({"previous_path":previous,"locator":current.original.locator}),
            )?;
            tx.commit()?;
            evicted.push(asset.id);
        }
        Ok(evicted)
    }
    fn object_key(&self, original: &Original) -> String {
        format!(
            "{}/originals/{}/{}{}",
            self.context.library_id,
            &original.blake3[..2],
            original.blake3,
            original_extension(original)
        )
    }
    fn backend_put(
        &self,
        storage: &StorageConfig,
        original: &Original,
        src: &Path,
    ) -> Result<String> {
        let key = self.object_key(original);
        match &storage.backend {
            Backend::Folder { root } => {
                let dest = Path::new(root).join(&key);
                let present = dest.is_file()
                    && hash_file(&dest)? == (original.blake3.clone(), original.byte_size);
                if !present {
                    copy_atomic(src, &dest)?;
                }
                Ok(format!("folder://{FOLDER_BACKEND}/{key}"))
            }
            Backend::S3 { bucket, prefix, .. } => {
                let key = format!("{prefix}{key}");
                self.store_call(storage, "put", bucket, &key, |r| {
                    r.path = Some(path_text(src)?);
                    r.blake3 = Some(original.blake3.clone());
                    r.content_type = Some(original.mime.clone());
                    Ok(())
                })?;
                Ok(format!("s3://{bucket}/{key}"))
            }
        }
    }
    fn backend_verify(
        &self,
        storage: &StorageConfig,
        locator: &str,
        original: &Original,
    ) -> Result<()> {
        match resolve(storage, locator)? {
            Target::Folder(path) => {
                let (hash, size) = hash_file(&path)?;
                ensure!(
                    hash == original.blake3 && size == original.byte_size,
                    "stored copy failed verification: {locator}"
                );
            }
            Target::S3 { bucket, key } => {
                let head = self.store_call(storage, "head", &bucket, &key, |_| Ok(()))?;
                ensure!(
                    head.exists
                        && head.byte_size == Some(original.byte_size)
                        && head.blake3.as_deref() == Some(original.blake3.as_str()),
                    "stored object failed verification: {locator}"
                );
            }
        }
        Ok(())
    }
}
fn resolve(storage: &StorageConfig, locator: &str) -> Result<Target> {
    {
        if let Some(rest) = locator.strip_prefix("folder://") {
            let (name, rel) = rest.split_once('/').context("invalid folder locator")?;
            ensure!(name == FOLDER_BACKEND, "unknown folder backend: {name}");
            let Backend::Folder { root } = &storage.backend else {
                anyhow::bail!("{locator} requires a folder storage backend");
            };
            return Ok(Target::Folder(Path::new(root).join(relative_locator(rel)?)));
        }
        if let Some(rest) = locator.strip_prefix("s3://") {
            ensure!(
                matches!(storage.backend, Backend::S3 { .. }),
                "{locator} requires an S3 storage backend"
            );
            let (bucket, key) = rest.split_once('/').context("invalid s3 locator")?;
            relative_locator(key)?;
            return Ok(Target::S3 {
                bucket: bucket.into(),
                key: key.into(),
            });
        }
        anyhow::bail!("unsupported original locator: {locator}")
    }
}
fn store_call(
    state_dir: &Path,
    storage: &StorageConfig,
    operation: &str,
    bucket: &str,
    key: &str,
    fill: impl FnOnce(&mut StoreRequest) -> Result<()>,
) -> Result<StoreResponse> {
    {
        let Backend::S3 {
            endpoint,
            region,
            path_style,
            ..
        } = &storage.backend
        else {
            anyhow::bail!("S3 operation without an S3 backend");
        };
        let mut request = StoreRequest {
            operation: operation.into(),
            endpoint: endpoint.clone(),
            region: region.clone(),
            bucket: bucket.into(),
            key: key.into(),
            path_style: *path_style,
            path: None,
            blake3: None,
            content_type: None,
            expires_seconds: None,
        };
        fill(&mut request)?;
        let helper = store_helper().context(
            "vamp-store helper unavailable; install it beside vamp or set VAMP_STORE_HELPER",
        )?;
        let mut command = Command::new(helper);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let file = state_dir.join(CREDENTIALS);
        if std::env::var_os("AWS_ACCESS_KEY_ID").is_none() && file.is_file() {
            let credentials: Credentials = serde_json::from_slice(&fs::read(&file)?)
                .context("invalid storage credentials file")?;
            command
                .env("AWS_ACCESS_KEY_ID", credentials.access_key_id)
                .env("AWS_SECRET_ACCESS_KEY", credentials.secret_access_key);
            if let Some(token) = credentials.session_token {
                command.env("AWS_SESSION_TOKEN", token);
            }
        }
        let mut child = command.spawn().context("cannot start vamp-store")?;
        let mut stdin = child.stdin.take().context("vamp-store stdin unavailable")?;
        stdin.write_all(&serde_json::to_vec(&request)?)?;
        drop(stdin);
        let output = child.wait_with_output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = stderr
                .lines()
                .find_map(|line| serde_json::from_str::<ErrorResponse>(line).ok())
                .map(|e| e.error.message)
                .unwrap_or_else(|| stderr.trim().to_owned());
            anyhow::bail!("vamp-store {operation} failed: {message}");
        }
        serde_json::from_slice(&output.stdout).context("vamp-store returned invalid JSON")
    }
}
/// Where an original's bytes come from, taken from the catalog alone: building one never
/// touches the filesystem (safe under a lock), and it owns everything [`OriginalLookup::fetch`]
/// needs, so callers can run that (stat/open, hydration) on an [`crate::io`] pool instead.
#[derive(Clone)]
pub struct OriginalLookup {
    state_dir: PathBuf,
    storage: Option<StorageConfig>,
    original: Original,
    cache: PathBuf,
}
struct Hydration {
    state_dir: PathBuf,
    storage: StorageConfig,
    locator: String,
    original: Original,
    cache: PathBuf,
}
impl OriginalLookup {
    /// Local bytes of the original: the local copy, else the pinned cache, else a verified
    /// download from cloud storage into state_dir/cache/originals. May block indefinitely on
    /// a stalled cloud-synced folder.
    pub fn fetch(self) -> Result<PathBuf> {
        let Self {
            state_dir,
            storage,
            original,
            cache,
        } = self;
        // Not `is_file`: callers open (and type-check) it on an I/O pool, where a placeholder
        // or special file may block.
        if !original.path.is_empty() && fs::metadata(&original.path).is_ok_and(|m| !m.is_dir()) {
            return Ok(PathBuf::from(&original.path));
        }
        let Some(locator) = original.locator.clone() else {
            anyhow::bail!(
                "representation unavailable: {}",
                if original.path.is_empty() {
                    "original has no local copy or cloud locator"
                } else {
                    &original.path
                }
            );
        };
        if fs::metadata(&cache).is_ok_and(|m| m.len() == original.byte_size) {
            return Ok(cache);
        }
        let storage = storage.with_context(|| {
            format!("original is in cloud storage ({locator}) but no storage backend is configured")
        })?;
        Hydration {
            state_dir,
            storage,
            locator,
            original,
            cache,
        }
        .run()
    }
}
impl Hydration {
    /// Download into the pinned cache and verify blake3. A per-hash file lock dedupes
    /// concurrent downloads (threads or processes); later waiters reuse the first result.
    fn run(self) -> Result<PathBuf> {
        let Self {
            state_dir,
            storage,
            locator,
            original,
            cache,
        } = self;
        let dir = cache.parent().unwrap();
        fs::create_dir_all(dir)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(format!(".{}.lock", original.blake3)))?;
        lock.lock_exclusive()?;
        if fs::metadata(&cache).is_ok_and(|m| m.len() == original.byte_size) {
            return Ok(cache);
        }
        let tmp = dir.join(format!(".{}.part-{}", original.blake3, id::simple()));
        let result = (|| -> Result<()> {
            match resolve(&storage, &locator)? {
                Target::Folder(path) => {
                    let mut out = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
                    std::io::copy(
                        &mut File::open(&path)
                            .with_context(|| format!("stored original unavailable: {locator}"))?,
                        &mut out,
                    )?;
                    out.sync_all()?;
                }
                Target::S3 { bucket, key } => {
                    store_call(&state_dir, &storage, "get", &bucket, &key, |r| {
                        r.path = Some(path_text(&tmp)?);
                        Ok(())
                    })?;
                }
            }
            let (hash, size) = hash_file(&tmp)?;
            ensure!(
                hash == original.blake3 && size == original.byte_size,
                "downloaded original failed blake3 verification: {locator}"
            );
            fs::rename(&tmp, &cache)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result?;
        Ok(cache)
    }
}
impl Library {
    fn store_call(
        &self,
        storage: &StorageConfig,
        operation: &str,
        bucket: &str,
        key: &str,
        fill: impl FnOnce(&mut StoreRequest) -> Result<()>,
    ) -> Result<StoreResponse> {
        store_call(&self.state_dir, storage, operation, bucket, key, fill)
    }
    fn original_cache(&self, original: &Original) -> PathBuf {
        self.state_dir.join(format!(
            "cache/originals/{}{}",
            original.blake3,
            original_extension(original)
        ))
    }
    /// Catalog-only description of an asset's original (see [`OriginalLookup`]).
    pub fn original_lookup(&self, id: &str) -> Result<OriginalLookup> {
        self.lookup_of(&get_asset(&self.conn, id)?.original)
    }
    pub(crate) fn lookup_of(&self, original: &Original) -> Result<OriginalLookup> {
        Ok(OriginalLookup {
            state_dir: self.state_dir.clone(),
            storage: config(&self.conn)?,
            original: original.clone(),
            cache: self.original_cache(original),
        })
    }
    /// [`OriginalLookup::fetch`] on the background I/O pool: a stalled cloud folder yields
    /// a [`crate::io::Stalled`] error instead of blocking the caller.
    pub(crate) fn fetch_original(&self, original: &Original) -> Result<PathBuf> {
        let lookup = self.lookup_of(original)?;
        crate::io::background().try_run("fetch original", crate::io::DRIVE_DEADLINE, move || {
            lookup.fetch()
        })
    }
    /// Whether the original's bytes are on this machine: the cataloged local copy or the
    /// rehydrated cache (authority `cache/originals`, replica `cache/original`). Never stats
    /// the local copy: it may sit on a cloud-synced folder whose `stat` can hang (browse
    /// calls this per item), so a cataloged path counts as local.
    pub fn original_available_locally(&self, original: &Original) -> bool {
        let cached = if self.context.remote.is_some() {
            self.state_dir.join(format!(
                "cache/original/{}{}",
                original.source_key().replace(':', "-"),
                original_extension(original)
            ))
        } else {
            if !original.path.is_empty() {
                return true;
            }
            if original.pending() {
                return false;
            }
            self.original_cache(original)
        };
        fs::metadata(cached).is_ok_and(|m| m.len() == original.byte_size)
    }
    /// Time-limited direct URL for an S3-stored original; None for folder or local-only.
    pub fn presign_original(&self, id: &str, expires_seconds: u64) -> Result<Option<String>> {
        let asset = get_asset(&self.conn, id)?;
        let (Some(storage), Some(locator)) = (config(&self.conn)?, asset.original.locator) else {
            return Ok(None);
        };
        let Target::S3 { bucket, key } = resolve(&storage, &locator)? else {
            return Ok(None);
        };
        Ok(self
            .store_call(&storage, "presign", &bucket, &key, |r| {
                r.expires_seconds = Some(expires_seconds);
                Ok(())
            })?
            .url)
    }
    /// Remove rehydrated originals (authority cache/originals, replica cache/original).
    pub fn cache_prune(&self) -> Result<Value> {
        let (mut files, mut bytes) = (0u64, 0u64);
        for dir in ["cache/originals", "cache/original"] {
            let Ok(entries) = fs::read_dir(self.state_dir.join(dir)) else {
                continue;
            };
            for entry in entries.filter_map(|e| e.ok()) {
                let meta = entry.metadata()?;
                // Per-hash download locks may be held by an in-flight download.
                if meta.is_file() && !entry.file_name().to_string_lossy().ends_with(".lock") {
                    fs::remove_file(entry.path())?;
                    files += 1;
                    bytes += meta.len();
                }
            }
        }
        Ok(json!({"removed_files":files,"removed_bytes":bytes}))
    }
}
enum Target {
    Folder(PathBuf),
    S3 { bucket: String, key: String },
}
