//! Authority-only original health and fingerprint relinking. Never writes source files.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize)]
pub struct OriginalMissing {
    pub asset_id: String,
    pub path: String,
}
impl OriginalMissing {
    pub fn from_error(error: &anyhow::Error) -> Option<&Self> {
        error.chain().find_map(|e| e.downcast_ref::<Self>())
    }
    pub fn to_json(&self) -> Value {
        json!({"code":"original_missing","message":self.to_string(),"asset_id":self.asset_id,"path":self.path})
    }
}
impl std::fmt::Display for OriginalMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "original_missing: original not found at {}", self.path)
    }
}
impl std::error::Error for OriginalMissing {}
pub fn is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|e| {
        e.downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    })
}

#[derive(Debug, Clone)]
pub enum Check {
    Present,
    Missing,
    Unreadable(String),
}
/// Stat only: never opens, hashes, or hydrates an original.
pub fn check(path: &Path) -> Check {
    match fs::metadata(path) {
        Ok(meta) if meta.is_file() => Check::Present,
        Ok(_) => Check::Unreadable("not a regular file".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Check::Missing,
        Err(e) => Check::Unreadable(e.to_string()),
    }
}
/// Permission/ACL preflight for doctor, without opening or materializing file bytes.
#[cfg(unix)]
fn readable(path: &str, buffer: &mut Vec<u8>) -> Check {
    buffer.clear();
    buffer.extend_from_slice(path.as_bytes());
    buffer.push(0);
    // `check` already accepted this filesystem path (so it cannot contain NUL).
    // The buffer is NUL-terminated and lives for the duration of faccessat.
    if unsafe {
        libc::faccessat(
            libc::AT_FDCWD,
            buffer.as_ptr().cast(),
            libc::R_OK,
            libc::AT_EACCESS,
        )
    } == 0
    {
        Check::Present
    } else {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            Check::Missing
        } else {
            Check::Unreadable(error.to_string())
        }
    }
}

fn referenced(original: &Original) -> bool {
    !original.path.is_empty()
        && original.locator.is_none()
        && !crate::fetching::awaiting_original(original)
}

impl Library {
    /// Save health through the same asset document and event stream as metadata sync.
    /// The path guard prevents a stale in-flight stat overwriting a concurrent relink.
    pub fn record_original_check(&self, id: &str, path: &str, check: Check) -> Result<()> {
        ensure!(
            self.context.remote.is_none(),
            "only the authority checks original paths"
        );
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let mut asset = get_asset(&tx, id)?;
        if asset.original.path != path || !referenced(&asset.original) {
            return Ok(());
        }
        let before = asset.original.availability.clone();
        let health = &mut asset.original.availability;
        match check {
            Check::Present => {
                health.state = OriginalState::Present;
                health.check_error = None;
            }
            Check::Missing => {
                health.state = OriginalState::Missing;
                health.check_error = None;
            }
            Check::Unreadable(error) => health.check_error = Some(error),
        }
        health.last_checked_at = Some(now());
        // Health does not change content identity, revision, timestamps, or derived records.
        tx.execute(
            "UPDATE assets SET document=?2 WHERE id=?1",
            params![id, json_text(&asset)?],
        )?;
        emit(
            &tx,
            &self.context,
            "original_checked",
            Some(&asset),
            json!({"previous":before,"availability":asset.original.availability}),
        )?;
        tx.commit()?;
        Ok(())
    }
    /// Bounded, indexed cursor for the server's low-priority stat sweep. No filesystem I/O.
    pub fn original_check_batch(&self, after: &str, limit: usize) -> Result<Vec<(String, String)>> {
        ensure!(
            self.context.remote.is_none(),
            "only the authority checks originals"
        );
        let assets = documents::<Asset>(
            &self.conn,
            "SELECT document FROM assets WHERE deleted=0 AND id>?1 ORDER BY id LIMIT ?2",
            params![after, limit.min(128) as i64],
        )?;
        // Keep cursor rows even when not referenced, allowing a whole catalog pass.
        Ok(assets
            .into_iter()
            .map(|a| {
                (
                    a.id,
                    if referenced(&a.original) {
                        a.original.path
                    } else {
                        String::new()
                    },
                )
            })
            .collect())
    }
    pub fn remember_import_root(&self, path: &Path) -> Result<()> {
        ensure!(
            self.context.remote.is_none(),
            "only the authority records import roots"
        );
        let root = path.canonicalize()?;
        ensure!(root.is_dir(), "import root must be a directory");
        let mut roots = self.import_roots()?;
        roots.insert(path_text(&root)?);
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        catalog::set_setting(&tx, "original_import_roots", &roots)?;
        tx.commit()?;
        Ok(())
    }
    fn import_roots(&self) -> Result<BTreeSet<String>> {
        let mut roots: BTreeSet<String> = self
            .conn
            .query_row(
                "SELECT document FROM settings WHERE key='original_import_roots'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .map(parse)
            .transpose()?
            .unwrap_or_default();
        if let Some(root) = &self.context.original_root {
            roots.insert(root.clone());
        }
        let mut stmt = self.conn.prepare("SELECT DISTINCT coalesce(json_extract(document,'$.original.import_root'),json_extract(document,'$.metadata.extra.eagle.library')) FROM assets WHERE deleted=0")?;
        for root in stmt.query_map([], |r| r.get::<_, Option<String>>(0))? {
            if let Some(root) = root? {
                roots.insert(root);
            }
        }
        Ok(roots)
    }
    /// Explicit path repair: verify exact bytes and recheck identity under the write lock.
    pub fn relink_original(&mut self, id: &str, path: &Path) -> Result<Asset> {
        ensure!(
            self.context.remote.is_none(),
            "relink the authority, not a replica path"
        );
        let snapshot = get_asset(&self.conn, id)?;
        ensure!(
            referenced(&snapshot.original),
            "relink requires a referenced original"
        );
        ensure!(
            !snapshot.original.pending() && !snapshot.original.blake3.is_empty(),
            "cannot relink without a recorded content hash; original fingerprint is pending"
        );
        ensure!(
            matches!(check(Path::new(&snapshot.original.path)), Check::Missing),
            "original is not missing or its location is unreadable"
        );
        let path = path.canonicalize().context("relink candidate unavailable on the authority; choose an authority-accessible file or configure a shared-drive mapping")?;
        ensure!(
            !path.starts_with(&self.state_dir),
            "relink candidate cannot be in catalog state"
        );
        ensure!(
            fs::metadata(&path)?.len() == snapshot.original.byte_size,
            "relink candidate size does not match recorded original"
        );
        let candidate = fingerprint(&path)?;
        ensure!(
            candidate.blake3 == snapshot.original.blake3
                && candidate.byte_size == snapshot.original.byte_size,
            "relink candidate content hash does not match recorded original"
        );
        let tx = self.conn.transaction()?;
        let mut asset = get_asset(&tx, id)?;
        ensure!(
            asset.original.path == snapshot.original.path
                && asset.original.source_key() == snapshot.original.source_key(),
            "original changed while verifying relink"
        );
        let previous_path = std::mem::replace(&mut asset.original.path, candidate.path);
        let timestamp = now();
        asset.original.modified_ns = candidate.modified_ns;
        asset.original.availability = OriginalAvailability {
            state: OriginalState::Present,
            last_checked_at: Some(timestamp.clone()),
            relinked_at: Some(timestamp),
            check_error: None,
        };
        // No source change: keep revision, representation/cache keys, metadata, memberships.
        // The SQL reference key is distinct even if two catalog identities relink to one
        // remaining file. Physical paths are indexed separately for intake deduplication.
        tx.execute(
            "UPDATE assets SET path=?2,document=?3 WHERE id=?1",
            params![id, format!("relinked:{id}"), json_text(&asset)?],
        )?;
        emit(
            &tx,
            &self.context,
            "original_relinked",
            Some(&asset),
            json!({"previous_path":previous_path,"path":asset.original.path,"blake3":asset.original.blake3}),
        )?;
        tx.commit()?;
        Ok(asset)
    }
    /// Read-only unless `fix`: stat all referenced originals; search only missing ones.
    /// A single filesystem walk and one hash per size-matching path serve the whole report.
    pub fn doctor(&mut self, fix: bool, search: &[PathBuf]) -> Result<Value> {
        ensure!(
            self.context.remote.is_none(),
            "doctor checks the authority; use its server doctor API from a replica"
        );
        let assets = documents::<Asset>(
            &self.conn,
            "SELECT document FROM assets WHERE deleted=0 ORDER BY id",
            [],
        )?;
        let mut missing = Vec::new();
        let mut unreadable = Vec::new();
        let mut present = 0usize;
        let mut tier_gaps = Vec::new();
        #[cfg(unix)]
        let mut access_buffer = Vec::new();
        for asset in &assets {
            if referenced(&asset.original) {
                let outcome = check(Path::new(&asset.original.path));
                #[cfg(unix)]
                let outcome = match outcome {
                    Check::Present => readable(&asset.original.path, &mut access_buffer),
                    outcome => outcome,
                };
                if fix {
                    self.record_original_check(&asset.id, &asset.original.path, outcome.clone())?;
                }
                match outcome {
                    Check::Present => present += 1,
                    Check::Missing => missing.push(asset),
                    Check::Unreadable(error) => unreadable.push(json!({"id":asset.id,"name":asset.name,"path":asset.original.path,"error":error})),
                }
            }
            if asset.original.mime.starts_with("image/")
                || asset.original.mime.starts_with("video/")
            {
                let reps = documents::<Representation>(
                    &self.conn,
                    "SELECT document FROM representations WHERE asset_id=?1 AND source_hash=?2",
                    params![asset.id, asset.original.source_key()],
                )?;
                let mut gaps = ["thumbnail", "preview"]
                    .into_iter()
                    .filter(|kind| {
                        !reps.iter().any(|r| {
                            r.kind == *kind
                                && local_representation(&self.state_dir, &r.path)
                                    .is_ok_and(|p| p.is_file())
                        })
                    })
                    .collect::<Vec<_>>();
                let micro: bool = self.conn.prepare_cached("SELECT EXISTS(SELECT 1 FROM micro_thumbs WHERE asset_id=?1 AND source=?2 AND length(data)>0)")?
                    .query_row(params![asset.id, asset.original.source_key()], |r| r.get(0))?;
                if !micro {
                    gaps.push("micro");
                }
                for (sql, kind) in [
                    (
                        "SELECT path FROM grid_thumbs WHERE asset_id=?1 AND source=?2",
                        "grid",
                    ),
                    (
                        "SELECT path FROM hires_thumbs WHERE asset_id=?1 AND source=?2",
                        "preview_tier",
                    ),
                ] {
                    let path: Option<String> = self
                        .conn
                        .prepare_cached(sql)?
                        .query_row(params![asset.id, asset.original.source_key()], |r| r.get(0))
                        .optional()?;
                    if !path.is_some_and(|p| {
                        local_representation(&self.state_dir, &p).is_ok_and(|p| p.is_file())
                    }) {
                        gaps.push(kind);
                    }
                }
                if !gaps.is_empty() {
                    tier_gaps.push(json!({"id":asset.id,"missing":gaps}));
                }
            }
        }
        let mut roots: BTreeSet<PathBuf> = if search.is_empty() {
            self.import_roots()?
                .into_iter()
                .map(PathBuf::from)
                .collect()
        } else {
            search.iter().map(|p| absolute(p)).collect::<Result<_>>()?
        };
        if search.is_empty() {
            for asset in &missing {
                if let Some(parent) = Path::new(&asset.original.path).parent() {
                    // With no recorded boundary, search the parent only, never / or the home.
                    roots.insert(parent.to_owned());
                    let boundary = asset
                        .original
                        .import_root
                        .as_deref()
                        .or(self.context.original_root.as_deref())
                        .or_else(|| {
                            asset
                                .metadata
                                .extra
                                .get("eagle")
                                .and_then(|e| e["library"].as_str())
                        });
                    if let Some(boundary) = boundary {
                        let boundary = Path::new(boundary);
                        for ancestor in parent.ancestors().take_while(|p| p.starts_with(boundary)) {
                            roots.insert(ancestor.to_owned());
                        }
                    }
                }
            }
        }
        // Canonicalize search boundaries once, deduplicating overlapping spellings.
        roots = roots
            .into_iter()
            .map(|root| root.canonicalize().unwrap_or(root))
            .collect();
        let sizes: BTreeSet<u64> = missing
            .iter()
            .filter(|a| !a.original.pending())
            .map(|a| a.original.byte_size)
            .collect();
        let mut matches: BTreeMap<(u64, String), Vec<String>> = BTreeMap::new();
        let mut search_errors = Vec::new();
        if !sizes.is_empty() {
            let mut visited = BTreeSet::new();
            let mut stack = roots.iter().cloned().collect::<Vec<_>>();
            while let Some(path) = stack.pop() {
                if path.starts_with(&self.state_dir) || !visited.insert(path.clone()) {
                    continue;
                }
                let meta = match fs::symlink_metadata(&path) {
                    Ok(meta) => meta,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => {
                        search_errors.push(json!({"path":path,"error":e.to_string()}));
                        continue;
                    }
                };
                // Never follow directory symlinks outside the selected search boundaries.
                if meta.is_symlink() {
                    continue;
                }
                if meta.is_dir() {
                    match fs::read_dir(&path) {
                        Ok(entries) => {
                            for entry in entries {
                                match entry {
                                    Ok(entry) => stack.push(entry.path()),
                                    Err(e) => search_errors
                                        .push(json!({"path":path,"error":e.to_string()})),
                                }
                            }
                        }
                        Err(e) => search_errors.push(json!({"path":path,"error":e.to_string()})),
                    }
                } else if meta.is_file() && sizes.contains(&meta.len()) {
                    match fingerprint(&path) {
                        Ok(original) => matches
                            .entry((original.byte_size, original.blake3))
                            .or_default()
                            .push(original.path),
                        Err(e) => search_errors.push(json!({"path":path,"error":format!("{e:#}")})),
                    }
                }
            }
        }
        let mut items = Vec::new();
        let (mut relinkable, mut ambiguous, mut relinked) = (0usize, 0usize, 0usize);
        for asset in &missing {
            let mut candidates = matches
                .get(&(asset.original.byte_size, asset.original.blake3.clone()))
                .cloned()
                .unwrap_or_default();
            candidates.sort();
            candidates.dedup();
            let status = if asset.original.pending() || asset.original.blake3.is_empty() {
                "fingerprint_pending"
            } else if candidates.len() > 1 {
                ambiguous += 1;
                "ambiguous"
            } else if candidates.len() == 1 && search_errors.is_empty() {
                relinkable += 1;
                "relinkable"
            } else if !search_errors.is_empty() {
                "search_incomplete"
            } else {
                "missing"
            };
            let mut row = json!({"id":asset.id,"name":asset.name,"path":asset.original.path,"status":status,"candidates":candidates});
            if fix && status == "relinkable" {
                match self.relink_original(&asset.id, Path::new(&candidates[0])) {
                    Ok(_) => {
                        relinked += 1;
                        row["status"] = json!("relinked");
                    }
                    Err(e) => row["error"] = json!(format!("{e:#}")),
                }
            }
            items.push(row);
        }
        Ok(
            json!({"read_only":!fix,"checked_at":now(),"counts":{"items":assets.len(),"present":present,"missing":missing.len(),"relinkable":relinkable,"ambiguous":ambiguous,"unreadable":unreadable.len(),"tier_gaps":tier_gaps.len(),"relinked":relinked,"remaining_missing":missing.len()-relinked},"search_roots":roots,"search_errors":search_errors,"missing":items,"unreadable":unreadable,"tier_gaps":tier_gaps}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_relinks_and_ambiguity_preserve_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let originals = tmp.path().join("originals");
        fs::create_dir(&originals).unwrap();
        let a = originals.join("a");
        fs::create_dir(&a).unwrap();
        let b = originals.join("b");
        fs::create_dir(&b).unwrap();
        let source = a.join("image.png");
        fs::write(&source, b"exact original bytes").unwrap();
        let mut library =
            Library::init(&tmp.path().join("home"), "test", Some(&originals), None).unwrap();
        let asset = library.add(&source, None, Metadata::default()).unwrap();
        let cursor = library.last_event().unwrap();
        fs::rename(&source, b.join("renamed.png")).unwrap();
        let report = library.doctor(false, &[]).unwrap();
        assert_eq!(report["counts"]["relinkable"], 1);
        assert_eq!(
            library.last_event().unwrap(),
            cursor,
            "doctor default is read-only"
        );
        assert_eq!(
            library.inspect(&asset.id).unwrap().asset.original.path,
            source.to_str().unwrap()
        );
        fs::copy(b.join("renamed.png"), b.join("duplicate.png")).unwrap();
        assert_eq!(library.doctor(true, &[]).unwrap()["counts"]["ambiguous"], 1);
        assert_eq!(
            library
                .inspect(&asset.id)
                .unwrap()
                .asset
                .original
                .availability
                .state,
            OriginalState::Missing
        );
        fs::remove_file(b.join("duplicate.png")).unwrap();
        assert_eq!(library.doctor(true, &[]).unwrap()["counts"]["relinked"], 1);
        let repaired = library.inspect(&asset.id).unwrap().asset;
        assert_eq!(repaired.revision, asset.revision);
        assert_eq!(repaired.original.blake3, asset.original.blake3);
        assert!(repaired.original.availability.relinked_at.is_some());
        fs::remove_file(b.join("renamed.png")).unwrap();
        let error = library.fetch(&asset.id, "original").unwrap_err();
        assert!(error.downcast_ref::<OriginalMissing>().is_some());
        assert_eq!(library.doctor(false, &[]).unwrap()["counts"]["missing"], 1);
        assert!(library.inspect(&asset.id).is_ok());
    }
    #[test]
    fn explicit_relink_verifies_hash_and_stale_stat_cannot_undo_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("files");
        fs::create_dir(&root).unwrap();
        let source = root.join("source");
        fs::write(&source, b"original").unwrap();
        let mut lib = Library::init(&tmp.path().join("home"), "test", None, None).unwrap();
        let asset = lib.add(&source, None, Metadata::default()).unwrap();
        fs::remove_file(&source).unwrap();
        let other = tmp.path().join("other");
        fs::create_dir(&other).unwrap();
        let candidate = other.join("new");
        fs::write(&candidate, b"mismatch").unwrap();
        assert!(lib.relink_original(&asset.id, &candidate).is_err());
        fs::write(&candidate, b"original").unwrap();
        assert_eq!(
            lib.doctor(false, std::slice::from_ref(&other)).unwrap()["counts"]["relinkable"],
            1
        );
        lib.relink_original(&asset.id, &candidate).unwrap();
        lib.record_original_check(&asset.id, source.to_str().unwrap(), Check::Missing)
            .unwrap();
        assert_eq!(
            lib.inspect(&asset.id)
                .unwrap()
                .asset
                .original
                .availability
                .state,
            OriginalState::Present
        );
    }
    #[test]
    fn unreadable_pending_and_shared_candidate_are_not_destructive() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("files");
        fs::create_dir(&root).unwrap();
        let source = root.join("source.jpg");
        fs::write(&source, b"original").unwrap();
        let mut lib = Library::init(&tmp.path().join("home"), "test", Some(&root), None).unwrap();
        let first = lib.add(&source, None, Metadata::default()).unwrap();
        lib.record_original_check(
            &first.id,
            &first.original.path,
            Check::Unreadable("permission denied".into()),
        )
        .unwrap();
        assert_eq!(
            lib.inspect(&first.id)
                .unwrap()
                .asset
                .original
                .availability
                .state,
            OriginalState::Present
        );
        fs::remove_file(&source).unwrap();
        // A different catalog identity may already refer to the only remaining copy.
        let target = root.join("target.jpg");
        fs::write(&target, b"original").unwrap();
        let second = lib.add(&target, None, Metadata::default()).unwrap();
        assert_eq!(lib.doctor(true, &[]).unwrap()["counts"]["relinked"], 1);
        assert!(lib.inspect(&first.id).is_ok() && lib.inspect(&second.id).is_ok());
        assert_eq!(
            lib.add(&target, None, Metadata::default())
                .unwrap()
                .original
                .path,
            target.to_str().unwrap()
        );
        let tx = lib.conn.transaction().unwrap();
        let mut asset = get_asset(&tx, &first.id).unwrap();
        asset.original.hash_state = "pending".into();
        asset.original.blake3.clear();
        save_asset(&tx, &asset).unwrap();
        tx.commit().unwrap();
        fs::remove_file(&target).unwrap();
        assert!(lib.relink_original(&first.id, &source).is_err());
        let report = lib.doctor(true, &[]).unwrap();
        assert_eq!(report["counts"]["relinked"], 0);
        assert!(
            report["missing"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["id"] == first.id && row["status"] == "fingerprint_pending")
        );
    }
    #[cfg(feature = "native")]
    #[test]
    fn native_folder_intake_retains_its_search_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("imported");
        let old = root.join("old");
        fs::create_dir_all(&old).unwrap();
        let mut lib = Library::init(&tmp.path().join("home"), "test", None, None).unwrap();
        native::dispatch(
            &mut lib,
            serde_json::from_value(json!({"operation":"remember_import_root","path":root}))
                .unwrap(),
        )
        .unwrap();
        let file = old.join("image.jpg");
        fs::write(&file, b"original").unwrap();
        lib.add(&file, None, Metadata::default()).unwrap();
        let sibling = root.join("new");
        fs::create_dir(&sibling).unwrap();
        fs::rename(&file, sibling.join("renamed.jpg")).unwrap();
        assert_eq!(lib.doctor(false, &[]).unwrap()["counts"]["relinkable"], 1);
    }
    #[cfg(unix)]
    #[test]
    fn doctor_reports_permission_denial_without_opening_originals() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("original.jpg");
        fs::write(&file, b"original").unwrap();
        let mut lib = Library::init(&tmp.path().join("home"), "test", None, None).unwrap();
        lib.add(&file, None, Metadata::default()).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0)).unwrap();
        let before = lib.last_event().unwrap();
        let report = lib.doctor(false, &[]).unwrap();
        assert_eq!(report["counts"]["unreadable"], 1);
        assert_eq!(report["counts"]["missing"], 0);
        assert_eq!(before, lib.last_event().unwrap());
    }
}
