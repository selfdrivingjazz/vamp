//! Native ops for replica contexts. Dispatched before local ops: replicas answer
//! reads from their mirror, but writes go to the authority and are then synced
//! (`add`/`ingest` mirror the created assets directly; their events follow on the next sync).
use super::native::{Request, decode_base64, expand};
use super::cache::{self, CachePolicy, Previews};
use super::remote::{
    Transfer, attach_remote, download_previews, download_thumbnails, fetch_grid,
    representation_with, sync, sync_wait, sync_with,
};
use super::*;

/// Name/description/tags/source_url from `metadata` (Metadata or MetadataPatch shape) plus `name`.
fn patch(request: &Request) -> Result<MetadataPatch> {
    let mut patch: MetadataPatch = request
        .metadata
        .clone()
        .map(serde_json::from_value)
        .transpose()
        .context("invalid metadata")?
        .unwrap_or_default();
    if request.name.is_some() {
        patch.name.clone_from(&request.name);
    }
    Ok(patch)
}

/// Library-scoped writes a replica forwards verbatim to the authority's `POST /v1/native`.
const FORWARDED: &[&str] = &[
    "restore",
    "empty_trash",
    "collection_move",
    "smart_folder_create",
    "smart_folder_update",
    "smart_folder_delete",
    "tag_rename",
    "tag_delete",
    "palette",
    "hash",
    "explore_ensure",
    "explore_add",
    "explore_summary",
    "explore_show",
    "explore_list",
    "explore_link",
    "explore_changes",
];
fn forward_body(request: &Request) -> Result<Value> {
    let mut body = serde_json::to_value(request)?;
    if let Some(object) = body.as_object_mut() {
        for local in ["home", "context", "transfer_id", "background"] {
            object.remove(local);
        }
    }
    Ok(body)
}

/// Replica `energy_pack`: the mirrored pack, fetched now when none is mirrored yet;
/// otherwise answered at once while a background check picks up a newer authority pack.
fn energy_pack(request: &Request) -> Result<Option<Value>> {
    let library = Library::open(&request.home, request.context.as_deref())?;
    if library.remote().is_none() {
        return Ok(None);
    }
    let path = energy::pack_path(&library.state_dir);
    if let Ok(header) = energy::read_header(&path) {
        energy::replica::refresh_in_background(&library);
        return Ok(Some(energy::describe(&path, &header)));
    }
    energy::replica::refresh(&library)?;
    Ok(Some(match energy::read_header(&path) {
        Ok(header) => energy::describe(&path, &header),
        Err(_) => json!({ "count": 0 }),
    }))
}

/// Views recorded on this replica and not yet forwarded to the authority.
const TOUCHES: &str =
    "CREATE TABLE IF NOT EXISTS replica_touches(asset_id TEXT PRIMARY KEY, at TEXT NOT NULL)";
static TOUCH_FLUSHES: std::sync::Mutex<std::collections::BTreeSet<PathBuf>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// Replica `touch`: recorded in the mirror at once (Recently Used, offline too) and queued
/// for the authority, so viewing an item never waits on the network.
fn touch(library: &Library, id: &str) -> Result<Value> {
    let mut asset = get_asset(&library.conn, id)?;
    let at = now();
    asset.last_viewed_at = Some(at.clone());
    library.conn.execute_batch(TOUCHES)?;
    // The view time is not indexed text, so the document alone changes (no reindex).
    library.conn.execute(
        "UPDATE assets SET document=?2 WHERE id=?1",
        params![id, json_text(&asset)?],
    )?;
    library.conn.execute(
        "INSERT OR REPLACE INTO replica_touches(asset_id,at) VALUES(?1,?2)",
        params![id, at],
    )?;
    flush_touches_in_background(library);
    Ok(json!({"asset_id":id,"last_viewed_at":at}))
}

/// Forwards the touch queue on a background thread unless one runs for this replica.
fn flush_touches_in_background(library: &Library) {
    let key = library.state_dir.clone();
    if !locked(&TOUCH_FLUSHES).insert(key.clone()) {
        return;
    }
    let context = library.context.clone();
    let spawned = std::thread::Builder::new()
        .name("replica-touches".into())
        .spawn(move || {
            let result = Library::connect(context).and_then(|library| flush_touches(&library));
            locked(&TOUCH_FLUSHES).remove(&key);
            if let Err(e) = result {
                eprintln!("replica views (kept for the next sync): {e:#}");
            }
        });
    if spawned.is_err() {
        locked(&TOUCH_FLUSHES).remove(&library.state_dir);
    }
}

/// Sends queued views oldest first; each leaves the queue once the authority has it (or
/// rejects it for good, e.g. the item was deleted). Transient failures stop the flush.
fn flush_touches(library: &Library) -> Result<()> {
    library.conn.execute_batch(TOUCHES)?;
    let client = library.remote_client()?;
    loop {
        let queued: Vec<(String, String)> = library
            .conn
            .prepare("SELECT asset_id,at FROM replica_touches ORDER BY at LIMIT 32")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        if queued.is_empty() {
            return Ok(());
        }
        for (id, at) in queued {
            if let Err(e) = client.native(&json!({"operation":"touch","asset_id":id}))
                && super::remote::is_transient(&e)
            {
                return Err(e);
            }
            library.conn.execute(
                "DELETE FROM replica_touches WHERE asset_id=?1 AND at=?2",
                params![id, at],
            )?;
        }
    }
}

/// `Ok(None)` means "not a remote concern"; the local dispatcher handles it.
pub(crate) fn request(request: &Request) -> Result<Option<Value>> {
    let home = &request.home;
    if request.operation == "remote_attach" {
        let name = request.name.as_deref().context("name is required")?;
        let context = attach_remote(
            home,
            name,
            request
                .remote_url
                .as_deref()
                .context("remote_url is required")?,
            // Absent/empty: the authority admits this machine by Tailscale identity.
            request.token.as_deref().unwrap_or(""),
        )?;
        // Returns once the catalog metadata is mirrored; thumbnails keep downloading.
        let mut library = Library::open(home, Some(name))?;
        let sync = tracked_sync(&mut library)?;
        return Ok(Some(
            json!({"name":name,"library_id":context.library_id,"state_dir":context.state_dir,"remote_url":context.remote.map(|r| r.url),"sync":sync}),
        ));
    }
    let operation = request.operation.as_str();
    if operation == "sync_status" {
        return sync_status(request).map(Some);
    }
    if operation == "energy_pack" {
        return energy_pack(request);
    }
    if operation == "remote_detach" {
        let name = request.context.as_deref().context("context is required")?;
        super::remote::detach(home, name)?;
        return Ok(Some(json!({"removed":name})));
    }
    if operation == "download_progress" {
        let transfer = locked(&TRANSFERS).get(&transfer_key(request)?).cloned();
        return Ok(Some(transfer.map_or_else(
            || json!({"bytes":0,"total":null,"active":false}),
            |t| json!({"bytes":t.bytes(),"total":t.total(),"active":true,"background":t.is_background()}),
        )));
    }
    if operation == "cancel_download" || operation == "download_priority" {
        let key = transfer_key(request)?;
        let cancel = operation == "cancel_download";
        let transfer = locked(&TRANSFERS).get(&key).cloned();
        match &transfer {
            Some(transfer) if cancel => transfer.cancel(),
            Some(transfer) => transfer.set_background(request.background),
            // The request has not reached the engine yet: applied when it registers.
            None => {
                let mut early = locked(&EARLY);
                early.retain(|_, e| e.at.elapsed() < EARLY_TTL);
                let entry = early.entry(key).or_insert_with(|| Early {
                    at: std::time::Instant::now(),
                    cancel: false,
                    background: None,
                });
                if cancel {
                    entry.cancel = true;
                } else {
                    entry.background = Some(request.background);
                }
            }
        }
        return Ok(Some(if cancel {
            json!({"cancelled":transfer.is_some()})
        } else {
            json!({"found":transfer.is_some(),"background":request.background})
        }));
    }
    if !matches!(
        operation,
        "sync"
            | "sync_wait"
            | "representation"
            | "original"
            | "update"
            | "remove"
            | "ingest"
            | "add"
            | "collection_create"
            | "collection_add"
            | "collection_remove"
            | "collection_rename"
            | "collection_delete"
            | "similar"
            | "galaxy"
            | "galaxy_search"
            | "accept_tags"
            | "prioritize_thumbnails"
            | "cache_policy"
            | "cache_policy_set"
            | "cache_stats"
            | "touch"
            | "fetch_grid"
    ) && !FORWARDED.contains(&operation)
        && !(operation == "browse" && request.semantic)
    {
        return Ok(None);
    }
    let mut library = Library::open(home, request.context.as_deref())?;
    let asset_id = || request.asset_id.as_deref().context("asset_id is required");
    if operation == "prioritize_thumbnails" {
        if library.remote().is_none() {
            return Ok(None);
        }
        ensure!(!request.asset_ids.is_empty(), "asset_ids is required");
        // Visible tiles are retried now even if they failed before.
        {
            let mut stmt = library
                .conn
                .prepare("UPDATE eagle_thumbnails SET attempts=0 WHERE asset_id=?1 AND kind='download' AND attempts>0")?;
            for id in &request.asset_ids {
                stmt.execute([id])?;
            }
        }
        let value = library.prioritize_thumbnails(&request.asset_ids)?;
        spawn_downloader(&library, false)?;
        return Ok(Some(value));
    }
    if operation == "sync" {
        return tracked_sync(&mut library).map(Some);
    }
    if operation == "sync_wait" {
        flush_touches_in_background(&library);
        let value = sync_wait(&mut library, request.wait.unwrap_or(25))?;
        spawn_downloader(&library, false)?;
        return Ok(Some(value));
    }
    if operation == "representation" || operation == "original" {
        let kind = match operation {
            "original" => "original",
            _ => request.kind.as_deref().context("kind is required")?,
        };
        if library.remote().is_none() {
            if operation == "original" {
                return Ok(None);
            }
            return Ok(Some(
                json!({"path":representation_with(&library, asset_id()?, kind, &Transfer::default())?}),
            ));
        }
        let id = asset_id()?;
        let path = with_transfer(request, |transfer| representation_with(&library, id, kind, transfer))?;
        return Ok(Some(json!({"path":path})));
    }
    if operation == "fetch_grid" {
        ensure!(library.remote().is_some(), "context is not a remote replica");
        ensure!(!request.asset_ids.is_empty(), "asset_ids is required");
        return with_transfer(request, |transfer| fetch_grid(&library, &request.asset_ids, transfer)).map(Some);
    }
    if operation.starts_with("cache_") {
        ensure!(library.remote().is_some(), "context is not a remote replica");
        return Ok(Some(match operation {
            "cache_policy" => json!(CachePolicy::load(&library.state_dir)?),
            "cache_policy_set" => {
                let policy: CachePolicy = serde_json::from_value(
                    request.policy.clone().context("policy is required")?,
                )
                .context("invalid policy")?;
                cache::set_policy(&library, &policy)?;
                spawn_downloader(&library, false)?;
                json!(policy)
            }
            _ => cache::stats(&library)?,
        }));
    }
    if library.remote().is_none() {
        return Ok(None);
    }
    if operation == "touch" {
        return touch(&library, asset_id()?).map(Some);
    }
    let client = library.remote_client()?;
    let value = match operation {
        // Embeddings live on the authority. Offline, the local dispatcher answers with
        // full-text browse over the mirror and "semantic":false.
        "browse" => return Ok(semantic_browse(&library, &client, request).ok()),
        // The authority owns embeddings, so it owns the projection; the last layout is kept
        // for offline use. Offline search answers `semantic:false` and the client falls back
        // to its lexical match.
        "galaxy" => {
            let cache = library.state_dir.join("galaxy.json");
            return match client.native(&forward_body(request)?) {
                Ok(value) => {
                    atomic_json(&cache, &value)?;
                    Ok(Some(value))
                }
                Err(error) => match fs::read(&cache) {
                    Ok(bytes) => {
                        let mut value: Value = serde_json::from_slice(&bytes)?;
                        value["offline"] = json!(format!("{error:#}"));
                        Ok(Some(value))
                    }
                    Err(_) => Err(error),
                },
            };
        }
        "galaxy_search" => {
            return Ok(Some(client.native(&forward_body(request)?).unwrap_or_else(
                |error| json!({"semantic":false,"ids":[],"scores":[],"offline":format!("{error:#}")}),
            )));
        }
        "similar" => {
            let ranked = client.similar(asset_id()?, request.limit.unwrap_or(40))?;
            return Ok(Some(json!({"items":mirror_items(&library, &ranked)?})));
        }
        "accept_tags" => {
            json!(client.accept_tags(asset_id()?, &request.tags, request.expected_revision)?)
        }
        "update" if !request.asset_ids.is_empty() => client.native(&forward_body(request)?)?,
        "collection_create" if request.parent_id.is_some() => {
            client.native(&forward_body(request)?)?
        }
        op if FORWARDED.contains(&op) => client.native(&forward_body(request)?)?,
        "update" => {
            json!(client.update(asset_id()?, &patch(request)?, request.expected_revision)?)
        }
        "remove" => {
            let ids = if request.asset_ids.is_empty() {
                vec![asset_id()?.to_owned()]
            } else {
                request.asset_ids.clone()
            };
            let expected = if ids.len() == 1 {
                request.expected_revision
            } else {
                None
            };
            for id in &ids {
                client.remove(id, expected)?;
            }
            json!({"trashed":ids.len()})
        }
        "ingest" => {
            let bytes = decode_base64(
                request
                    .bytes_base64
                    .as_deref()
                    .context("bytes_base64 is required")?,
            )?;
            let filename = request
                .filename
                .as_deref()
                .context("filename is required")?;
            let asset = client.ingest(&bytes, filename, &patch(request)?)?;
            super::remote::record_created(&mut library, std::slice::from_ref(&asset))?;
            return Ok(Some(json!(asset)));
        }
        "add" => {
            ensure!(!request.paths.is_empty(), "paths is required");
            let mut patch = patch(request)?;
            // Each file keeps its own name on the authority.
            patch.name = None;
            let mut errors = Vec::new();
            let mut files = Vec::new();
            for path in &request.paths {
                if let Err(e) = expand(path, &mut files) {
                    errors.push(json!({"path":path,"message":format!("{e:#}")}));
                }
            }
            let mut assets = Vec::new();
            for (path, result) in files.iter().zip(upload_all(&client, &files, &patch)) {
                match result {
                    Ok(asset) => assets.push(asset),
                    Err(e) => errors.push(json!({"path":path,"message":format!("{e:#}")})),
                }
            }
            super::remote::record_created(&mut library, &assets)?;
            return Ok(Some(json!({"assets":assets,"errors":errors})));
        }
        "collection_create" => {
            json!(client.collection_create(request.name.as_deref().context("name is required")?)?)
        }
        "collection_rename" => json!(
            client.collection_rename(
                request
                    .collection_id
                    .as_deref()
                    .context("collection_id is required")?,
                request.name.as_deref().context("name is required")?,
            )?
        ),
        "collection_delete" => {
            let id = request
                .collection_id
                .as_deref()
                .context("collection_id is required")?;
            client.collection_delete(id)?;
            json!({"deleted":id})
        }
        "collection_add" | "collection_remove" => {
            ensure!(!request.asset_ids.is_empty(), "asset_ids is required");
            json!(
                client.collection_membership(
                    request
                        .collection_id
                        .as_deref()
                        .context("collection_id is required")?,
                    &request.asset_ids,
                    operation == "collection_add",
                )?
            )
        }
        _ => unreachable!("filtered above"),
    };
    sync(&mut library)?;
    spawn_downloader(&library, false)?;
    Ok(Some(value))
}

/// Files a replica `add` uploads at once: several in flight keep a slow, high-latency link
/// busy (each upload ends in a round trip the next one would otherwise wait for).
const PARALLEL_UPLOADS: usize = 4;

/// Uploads `files` to the authority's inbox, [`PARALLEL_UPLOADS`] at a time; results are in
/// the order of `files`.
fn upload_all(
    client: &super::remote::Client,
    files: &[PathBuf],
    patch: &MetadataPatch,
) -> Vec<Result<Asset>> {
    let upload = |path: &PathBuf| -> Result<Asset> {
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .context("path has no UTF-8 file name")?;
        client.ingest(&fs::read(path)?, filename, patch)
    };
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<std::sync::Mutex<Option<Result<Asset>>>> =
        files.iter().map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..PARALLEL_UPLOADS.min(files.len()) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(path) = files.get(index) else { break };
                    *locked(&results[index]) = Some(upload(path));
                }
            });
        }
    });
    results
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .unwrap_or_else(|e| e.into_inner())
                .unwrap_or_else(|| Err(anyhow::anyhow!("upload did not run")))
        })
        .collect()
}

/// Browse items (from the mirror) for authority-ranked assets; unsynced ids are skipped.
fn mirror_items(library: &Library, ranked: &[super::remote::Ranked]) -> Result<Vec<Value>> {
    let mut items = Vec::with_capacity(ranked.len());
    for (id, score) in ranked {
        let Ok(asset) = get_asset(&library.conn, id) else {
            continue;
        };
        let reps = documents::<Representation>(
            &library.conn,
            "SELECT document FROM representations WHERE asset_id=?1 AND source_hash=?2",
            params![asset.id, asset.original.source_key()],
        )?;
        let mut item = super::native::media_item(library, &asset, &reps)?;
        item["score"] = json!(score);
        items.push(item);
    }
    Ok(items)
}

/// Semantic browse ranked by the authority, rendered from the mirror.
fn semantic_browse(
    library: &Library,
    client: &super::remote::Client,
    request: &Request,
) -> Result<Value> {
    let limit = request.limit.unwrap_or(120);
    ensure!(
        (1..=500).contains(&limit),
        "invalid pagination; limit must be 1–500"
    );
    let query = request.query.as_deref().unwrap_or("").trim();
    ensure!(!query.is_empty(), "semantic browse needs a query");
    let added_by = request.added_by.as_deref();
    let (semantic, mut ranked) = client.semantic_search(query, 1000, false, added_by)?;
    ensure!(semantic, "authority has no embedder");
    if let Some(collection) = request.collection_id.as_deref() {
        let mut stmt = library
            .conn
            .prepare("SELECT asset_id FROM memberships WHERE collection_id=?1")?;
        let members = stmt
            .query_map([collection], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<std::collections::HashSet<_>>>()?;
        ranked.retain(|(id, _)| members.contains(id));
    }
    // Also against the mirror: authorities predating the filter ignore it.
    if let Some(added_by) = added_by {
        let matching = crate::catalog::added_by_ids(&library.conn, added_by)?;
        ranked.retain(|(id, _)| matching.contains(id));
    }
    let mut items = mirror_items(library, &ranked)?;
    let total = items.len();
    let items: Vec<Value> = items.drain(..).skip(request.offset).take(limit).collect();
    Ok(
        json!({"has_more":request.offset + items.len() < total,"items":items,"total":total,"offset":request.offset,"semantic":true}),
    )
}

/// Replica state dirs with a sync in progress in this process: `{phase, done, total}`.
static SYNCS: std::sync::LazyLock<std::sync::Mutex<std::collections::BTreeMap<PathBuf, Value>>> =
    std::sync::LazyLock::new(Default::default);
/// Replica state dirs with a downloader thread running in this process, with the token of
/// that thread (so a finished thread never unregisters its successor).
static DOWNLOADERS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, u64>>,
> = std::sync::LazyLock::new(Default::default);
static NEXT_DOWNLOADER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Bumped to cut downloader naps short (new priorities, sync, policy change).
static WAKE: std::sync::LazyLock<(std::sync::Mutex<u64>, std::sync::Condvar)> =
    std::sync::LazyLock::new(Default::default);
/// Replica downloads in progress, by `(home, context, transfer_id)` of the request.
type TransferKey = (PathBuf, String, String);
static TRANSFERS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<TransferKey, std::sync::Arc<Transfer>>>,
> = std::sync::LazyLock::new(Default::default);
/// `cancel_download` / `download_priority` that arrived before their request registered
/// (the app cancels as soon as an item leaves the window); applied when it does.
struct Early {
    at: std::time::Instant,
    cancel: bool,
    background: Option<bool>,
}
static EARLY: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<TransferKey, Early>>> =
    std::sync::LazyLock::new(Default::default);
const EARLY_TTL: Duration = Duration::from_secs(60);

/// Runs a replica download under a fresh [`Transfer`] (background when the request says
/// so), registered under its `transfer_id` while it runs so `download_progress`,
/// `download_priority` and `cancel_download` reach it. Without an id it runs unregistered.
fn with_transfer<T>(request: &Request, run: impl FnOnce(&Transfer) -> Result<T>) -> Result<T> {
    let transfer = std::sync::Arc::new(if request.background {
        Transfer::background()
    } else {
        Transfer::default()
    });
    let Some(key) = request.transfer_id.as_ref().map(|_| transfer_key(request)).transpose()? else {
        return run(&transfer);
    };
    if let Some(early) = locked(&EARLY).remove(&key) {
        if early.cancel {
            transfer.cancel();
        }
        if let Some(background) = early.background {
            transfer.set_background(background);
        }
    }
    locked(&TRANSFERS).insert(key.clone(), transfer.clone());
    let result = run(&transfer);
    let mut transfers = locked(&TRANSFERS);
    if transfers
        .get(&key)
        .is_some_and(|t| std::sync::Arc::ptr_eq(t, &transfer))
    {
        transfers.remove(&key);
    }
    result
}

/// Failed thumbnails (attempts exhausted) are retried this often while the thread lives.
const REARM: Duration = Duration::from_secs(180);
const BACKOFF_MIN: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// Preview tiers fetched between checks of the (higher priority) thumbnail queue.
const PREVIEW_ROUND: usize = 16;

fn locked<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn transfer_key(request: &Request) -> Result<TransferKey> {
    Ok((
        request.home.clone(),
        request.context.clone().unwrap_or_default(),
        request
            .transfer_id
            .clone()
            .context("transfer_id is required")?,
    ))
}

fn wake_downloaders() {
    let (generation, changed) = &*WAKE;
    *locked(generation) += 1;
    changed.notify_all();
}

/// Sleeps for `duration` or until [`wake_downloaders`].
fn nap(duration: Duration) {
    let (generation, changed) = &*WAKE;
    let guard = locked(generation);
    let start = *guard;
    let _ = changed.wait_timeout_while(guard, duration, |g| *g == start);
}

/// `base` ±25%, so many replicas do not retry an overloaded authority in lockstep.
fn jittered(base: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    base.mul_f64(0.75 + f64::from(nanos % 1000) / 2000.0)
}

/// `sync` with progress visible to `sync_status`; afterwards thumbnails download in the
/// background (retrying earlier failures, e.g. after being offline).
fn tracked_sync(library: &mut Library) -> Result<Value> {
    let key = library.state_dir.clone();
    locked(&SYNCS).insert(key.clone(), json!({"phase":"starting","done":0,"total":0}));
    let result = sync_with(library, &mut |phase, done, total| {
        locked(&SYNCS).insert(
            key.clone(),
            json!({"phase":phase,"done":done,"total":total}),
        );
    });
    locked(&SYNCS).remove(&key);
    let mut value = result?;
    // Micro-thumbnails arrive with the catalog (~1 KB each), so every tile has an image
    // before the first real thumbnail downloads. Best effort: older authorities lack packs.
    match crate::thumbs::sync_micros(library) {
        Ok(micro) => value["micro"] = micro,
        Err(e) => eprintln!("micro thumbnails: {e:#}"),
    }
    spawn_downloader(library, true)?;
    Ok(value)
}

/// `sync_status`: `{syncing, phase?, done?, total?, downloading, pending_thumbnails}`.
fn sync_status(request: &Request) -> Result<Value> {
    let library = Library::open(&request.home, request.context.as_deref())?;
    let mut value = locked(&SYNCS)
        .get(&library.state_dir)
        .cloned()
        .map_or_else(|| json!({"syncing":false}), |mut v| {
            v["syncing"] = json!(true);
            v
        });
    value["downloading"] = json!(locked(&DOWNLOADERS).contains_key(&library.state_dir));
    value["pending_thumbnails"] = json!(library.pending_eagle_thumbnails()?);
    Ok(value)
}

/// Anything left for the downloader: queued thumbnails (including ones waiting for a
/// re-arm after failing) or, under policy `all`, missing previews within the cap.
fn has_work(library: &Library) -> Result<bool> {
    let queued: i64 = library.conn.query_row(
        "SELECT count(*) FROM eagle_thumbnails WHERE kind='download'",
        [],
        |r| r.get(0),
    )?;
    if queued > 0 {
        return Ok(true);
    }
    let policy = CachePolicy::load(&library.state_dir)?;
    Ok(policy.previews == Previews::All
        && policy
            .preview_cap_bytes
            .is_none_or(|cap| cache::total(library, cache::PREVIEW).is_ok_and(|t| t < cap))
        && !cache::missing_previews(library, 1)?.is_empty())
}

/// Downloads the replica's thumbnail queue (and, under policy `all`, previews) on a
/// background thread unless one already runs in this process. `retry` re-arms items that
/// exhausted their attempts. A running thread is woken to pick up new priorities.
fn spawn_downloader(library: &Library, retry: bool) -> Result<()> {
    if library.remote().is_none() {
        return Ok(());
    }
    if retry {
        library.conn.execute(
            "UPDATE eagle_thumbnails SET attempts=0 WHERE kind='download' AND attempts>0",
            [],
        )?;
    }
    wake_downloaders();
    if !has_work(library)? {
        return Ok(());
    }
    let key = library.state_dir.clone();
    let token = NEXT_DOWNLOADER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    {
        let mut running = locked(&DOWNLOADERS);
        if running.contains_key(&key) {
            return Ok(());
        }
        running.insert(key.clone(), token);
    }
    let registration = Registration { key, token };
    let context = library.context.clone();
    std::thread::Builder::new()
        .name("replica-downloads".into())
        .spawn(move || {
            let registration = registration;
            let result = Library::connect(context)
                .and_then(|mut library| run_downloader(&mut library, &registration));
            super::native::thumbnails_landed();
            if let Err(e) = result {
                eprintln!("replica downloads: {e:#}");
            }
        })?;
    Ok(())
}

/// A [`DOWNLOADERS`] entry, removed when its thread ends, however it ends (panics too).
struct Registration {
    key: PathBuf,
    token: u64,
}

impl Registration {
    fn release(&self, running: &mut std::collections::HashMap<PathBuf, u64>) {
        if running.get(&self.key) == Some(&self.token) {
            running.remove(&self.key);
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.release(&mut locked(&DOWNLOADERS));
    }
}

/// The downloader thread: thumbnails first, then policy-`all` previews. Transient
/// failures back off (2 s doubling to 60 s, jittered) and retry; exhausted thumbnails are
/// re-armed every [`REARM`]. Ends only when nothing is left (checked under the
/// [`DOWNLOADERS`] lock, so a concurrent `spawn_downloader` either sees this thread or
/// starts a new one) or the replica is detached.
fn run_downloader(library: &mut Library, registration: &Registration) -> Result<()> {
    let mut backoff = BACKOFF_MIN;
    let mut rearmed = std::time::Instant::now();
    let mut reconciled = false;
    loop {
        if !library.state_dir.join("catalog.sqlite3").is_file() {
            return Ok(());
        }
        if rearmed.elapsed() >= REARM {
            library.conn.execute(
                "UPDATE eagle_thumbnails SET attempts=0 WHERE kind='download' AND attempts>0",
                [],
            )?;
            rearmed = std::time::Instant::now();
        }
        let mut failure = match download_thumbnails(library, &mut |_, _| {
            super::native::thumbnails_landed()
        }) {
            Ok(round) => round["transient"].as_str().map(str::to_owned),
            Err(e) => Some(format!("{e:#}")),
        };
        if failure.is_none() && library.pending_eagle_thumbnails()? == 0 {
            if !reconciled {
                cache::reconcile(library, cache::PREVIEW)?;
                reconciled = true;
            }
            match download_previews(library, PREVIEW_ROUND) {
                Ok((_, true)) => {
                    backoff = BACKOFF_MIN;
                    continue;
                }
                Ok((_, false)) => {}
                Err(e) => failure = Some(format!("{e:#}")),
            }
        }
        if let Some(error) = failure {
            eprintln!("replica downloads (retrying in {}s): {error}", backoff.as_secs());
            nap(jittered(backoff));
            backoff = (backoff * 2).min(BACKOFF_MAX);
            continue;
        }
        backoff = BACKOFF_MIN;
        if library.pending_eagle_thumbnails()? > 0 {
            continue;
        }
        {
            let mut running = locked(&DOWNLOADERS);
            if !has_work(library)? {
                registration.release(&mut running);
                return Ok(());
            }
        }
        // Only exhausted items remain: wait for their re-arm (or new priorities).
        nap(REARM.saturating_sub(rearmed.elapsed()));
    }
}
