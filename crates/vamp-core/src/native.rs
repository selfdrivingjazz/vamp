//! Native-client queries and an intentionally preview-only, stored-ZIP interchange format.
use super::*;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Seek, SeekFrom};
use zip::{ZipArchive, ZipWriter, write::SimpleFileOptions};

pub const MAX_ASSETS: usize = 20_000;
pub const MAX_MANIFEST: u64 = 64 * 1024 * 1024;
pub const MAX_MEDIA: u64 = 64 * 1024 * 1024;
pub const MAX_TOTAL: u64 = 2 * 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = MAX_ASSETS * 2 + 1;

#[derive(Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub operation: String,
    #[serde(default)]
    pub home: PathBuf,
    pub context: Option<String>,
    pub query: Option<String>,
    pub collection_id: Option<String>,
    #[serde(default)]
    pub offset: usize,
    pub limit: Option<usize>,
    pub asset_id: Option<String>,
    pub path: Option<PathBuf>,
    pub name: Option<String>,
    #[serde(default)]
    pub paths: Vec<PathBuf>,
    /// `Metadata` for add/ingest, `MetadataPatch` for update.
    pub metadata: Option<Value>,
    pub expected_revision: Option<u64>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub untagged: bool,
    #[serde(default)]
    pub asset_ids: Vec<String>,
    pub sort: Option<String>,
    pub kind: Option<String>,
    pub bytes_base64: Option<String>,
    pub filename: Option<String>,
    pub remote_url: Option<String>,
    pub token: Option<String>,
    #[serde(default)]
    pub semantic: bool,
    #[serde(default)]
    pub tags_any: Vec<String>,
    #[serde(default)]
    pub exts: Vec<String>,
    pub shape: Option<String>,
    pub rating_min: Option<u8>,
    #[serde(default)]
    pub colors: Vec<String>,
    pub color_tolerance: Option<f64>,
    #[serde(default)]
    pub folder_ids: Vec<String>,
    #[serde(default)]
    pub include_subfolders: bool,
    #[serde(default)]
    pub uncategorized: bool,
    pub smart_folder_id: Option<String>,
    #[serde(default)]
    pub trash: bool,
    #[serde(default)]
    pub recently_used: bool,
    pub date_from: Option<String>,
    pub date_to: Option<String>,
    pub date_field: Option<String>,
    pub seed: Option<i64>,
    /// Browse: original byte size bounds (inclusive).
    pub size_min: Option<u64>,
    pub size_max: Option<u64>,
    /// import_status: id returned by import_eagle_start.
    pub job_id: Option<String>,
    /// collection_create / collection_move / smart_folder_create parent (null = root).
    pub parent_id: Option<String>,
    pub index: Option<usize>,
    /// Smart-folder rules `{match, rules:[{field, op, value}]}`.
    pub rules: Option<Value>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub tag: Option<String>,
    /// import_eagle: accepted for clarity; every run is an incremental sync.
    #[serde(default)]
    pub sync: bool,
    /// import_eagle: `lazy` (default; also `none`) or `full` (hash every original after).
    pub hash: Option<String>,
    /// sync_wait: seconds to long-poll the authority for changes (default 25, max 60).
    pub wait: Option<u64>,
    /// thumbnail_updates: event cursor from the previous answer (omit to start now).
    /// With `wait` (seconds, max 60), blocks until a thumbnail lands, the queue drains, or
    /// the wait ends, instead of answering immediately.
    pub since: Option<u64>,
    /// layout: also return every asset's micro-thumbnail.
    #[serde(default)]
    pub micro: bool,
    /// galaxy: recompute the projection in full instead of placing new embeddings.
    /// Omitted when false so forwarded bodies stay readable by older authorities.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rebuild: bool,
    /// cache_policy_set: the replica cache policy (`cache_policy` shape). Omitted when
    /// absent so forwarded bodies stay readable by authorities that predate the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<Value>,
    /// find_duplicates: precomputed BLAKE3 digests (hex). Omitted when empty so forwarded
    /// bodies stay readable by authorities that predate the field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hashes: Vec<String>,
    /// Replica downloads (`representation`, `fetch_grid`): a caller-chosen id naming this
    /// request's transfer for `download_progress`, `download_priority` and `cancel_download`.
    /// Local-only (never forwarded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_id: Option<String>,
    /// Replica downloads: background priority (prefetch); its socket is not read while a
    /// foreground download runs, so the item being shown gets the whole link.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub background: bool,
    /// browse/layout: who added the assets, `owner`, `agents` or `actor:<actor>`. Omitted
    /// when absent so forwarded bodies stay readable by authorities that predate the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_by: Option<String>,
    /// Exploration operations use the same core implementation as `vamp explore`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brief: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peek: Option<bool>,
}

/// Browse selects live assets matching every given filter.
#[derive(Default)]
pub struct BrowseFilter<'a> {
    pub query: Option<&'a str>,
    /// A collection ID, or [`UNCATEGORIZED`] for assets in no collection.
    pub collection: Option<&'a str>,
    /// MIME top-level type, e.g. `image`.
    pub kind: Option<&'a str>,
    /// Every tag must be present.
    pub tags: &'a [String],
    /// At least one tag must be present.
    pub tags_any: &'a [String],
    pub untagged: bool,
    pub exts: &'a [String],
    pub shape: Option<&'a str>,
    pub rating_min: Option<u8>,
    pub colors: &'a [String],
    pub color_tolerance: Option<f64>,
    pub folder_ids: &'a [String],
    pub include_subfolders: bool,
    pub uncategorized: bool,
    pub smart_folder: Option<&'a str>,
    /// Only trashed assets (otherwise trashed assets are excluded).
    pub trash: bool,
    pub recently_used: bool,
    pub date_from: Option<&'a str>,
    pub date_to: Option<&'a str>,
    pub date_field: Option<&'a str>,
    /// See contract `native.browse`; default is relevance for text queries, color
    /// distance with colors, insertion order otherwise.
    pub sort: Option<&'a str>,
    pub seed: Option<i64>,
    pub size_min: Option<u64>,
    pub size_max: Option<u64>,
    /// `owner`, `agents` or `actor:<actor>` (see [`crate::is_agent`]).
    pub added_by: Option<&'a str>,
    pub offset: usize,
    pub limit: usize,
}
impl BrowseFilter<'_> {
    /// Whether any filter reads a facet (virtual) column covered by the facets index.
    pub(crate) fn uses_facets(&self) -> bool {
        self.shape.is_some()
            || self.size_min.is_some()
            || self.size_max.is_some()
            || self.rating_min.is_some_and(|r| r > 0)
            || !self.exts.is_empty()
            || !self.colors.is_empty()
            || self.untagged
            || self.recently_used
            || !self.tags.is_empty()
            || !self.tags_any.is_empty()
            || self.collection.is_some()
            || !self.folder_ids.is_empty()
            || self.uncategorized
            || self.smart_folder.is_some()
    }
}
impl<'a> BrowseFilter<'a> {
    pub fn from_request(request: &'a Request) -> Self {
        Self {
            query: request.query.as_deref(),
            collection: request.collection_id.as_deref(),
            kind: request.kind.as_deref(),
            tags: &request.tags,
            tags_any: &request.tags_any,
            untagged: request.untagged,
            exts: &request.exts,
            shape: request.shape.as_deref(),
            rating_min: request.rating_min,
            colors: &request.colors,
            color_tolerance: request.color_tolerance,
            folder_ids: &request.folder_ids,
            include_subfolders: request.include_subfolders,
            uncategorized: request.uncategorized,
            smart_folder: request.smart_folder_id.as_deref(),
            trash: request.trash,
            recently_used: request.recently_used,
            date_from: request.date_from.as_deref(),
            date_to: request.date_to.as_deref(),
            date_field: request.date_field.as_deref(),
            sort: request.sort.as_deref(),
            seed: request.seed,
            size_min: request.size_min,
            size_max: request.size_max,
            added_by: request.added_by.as_deref(),
            offset: request.offset,
            limit: request.limit.unwrap_or(120),
        }
    }
}

pub const UNCATEGORIZED: &str = "__uncategorized__";
pub const MAX_INGEST: usize = 64 * 1024 * 1024;

fn context_value(home: &Path, name: &str, context: &Context) -> Result<Value> {
    let state = home.canonicalize()?.join(&context.state_dir);
    let manifest: Value = serde_json::from_slice(&fs::read(state.join("library.json"))?)?;
    Ok(
        json!({"name":name,"display_name":manifest["display_name"].as_str().unwrap_or(name),"library_id":context.library_id,"state_dir":state,"preview_only":manifest["preview_only"] == true,"remote_url":context.remote.as_ref().map(|r| &r.url)}),
    )
}

fn metadata<T: DeserializeOwned + Default>(value: Option<Value>) -> Result<T> {
    Ok(value
        .map(serde_json::from_value)
        .transpose()
        .context("invalid metadata")?
        .unwrap_or_default())
}

pub fn request(request: Request) -> Result<Value> {
    ensure!(!request.home.as_os_str().is_empty(), "home is required");
    if request.operation == "contexts" {
        let config = load_config(&request.home)?;
        let contexts = config
            .contexts
            .iter()
            .map(|(name, ctx)| context_value(&request.home, name, ctx))
            .collect::<Result<Vec<_>>>()?;
        return Ok(json!({"contexts":contexts,"current":config.current}));
    }
    if request.operation == "import_preview" {
        let (name, context) = import_preview(
            &request.home,
            request.path.as_deref().context("path is required")?,
            request.name.as_deref(),
        )?;
        return Ok(json!({"context":context_value(&request.home, &name, &context)?}));
    }
    if request.operation == "import_status" {
        let id = request.job_id.as_deref().context("job_id is required")?;
        return import_status(id);
    }
    if request.operation == "init" {
        let name = request.name.as_deref().context("name is required")?;
        let library = Library::init(&request.home, name, request.path.as_deref(), None)?;
        return context_value(&request.home, name, &library.context);
    }
    #[cfg(feature = "remote")]
    if let Some(value) = super::native_remote::request(&request)? {
        return Ok(value);
    }
    let mut library = Library::open(&request.home, request.context.as_deref())?;
    if request.operation == "import_eagle_start" {
        return import_eagle_start(library, &request);
    }
    let resume = (request.operation == "prioritize_thumbnails")
        .then(|| (request.home.clone(), request.context.clone()));
    let result = dispatch(&mut library, request)?;
    if let Some((home, context)) = resume {
        spawn_thumbnail_copier(&home, context.as_deref())?;
    }
    Ok(result)
}

/// Eagle imports running on background threads of this process (the app's bridge is
/// long-lived). Finished jobs stay queryable until the process exits.
static IMPORT_JOBS: std::sync::LazyLock<std::sync::Mutex<BTreeMap<String, Value>>> =
    std::sync::LazyLock::new(Default::default);
fn set_import_job(id: &str, value: Value) {
    IMPORT_JOBS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.to_owned(), value);
}
fn hash_mode(request: &Request) -> Result<bool> {
    Ok(match request.hash.as_deref().unwrap_or("lazy") {
        "lazy" | "none" => false,
        "full" => true,
        other => anyhow::bail!("unknown hash mode: {other} (lazy|none|full)"),
    })
}
fn import_status_value(
    job_id: &str,
    progress: &ImportProgress,
    finished: bool,
    error: Option<String>,
    result: Option<Value>,
    thumbnails: Value,
) -> Value {
    json!({"job_id":job_id,"phase":progress.phase,"done":progress.done,"total":progress.total,"created":progress.created,"updated":progress.updated,"trashed":progress.trashed,"finished":finished,"error":error,"result":result,"thumbnails":thumbnails})
}
fn import_eagle_start(mut library: Library, request: &Request) -> Result<Value> {
    ensure!(
        library.context.remote.is_none(),
        "Eagle import runs on the authority, not on replicas"
    );
    let path = request.path.clone().context("path is required")?;
    let full = hash_mode(request)?;
    let job_id = id();
    let queued = ImportProgress {
        phase: "queued",
        ..ImportProgress::default()
    };
    let idle = json!({"done":0,"total":0,"copied":0,"generated":0,"failed":0,"finished":false});
    set_import_job(
        &job_id,
        import_status_value(&job_id, &queued, false, None, None, idle.clone()),
    );
    let id = job_id.clone();
    std::thread::Builder::new()
        .name(format!("eagle-import-{id}"))
        .spawn(move || {
            let mut last = queued;
            let result = library.import_eagle_with(&path, full, &mut |progress| {
                last = progress.clone();
                set_import_job(
                    &id,
                    import_status_value(&id, progress, false, None, None, idle.clone()),
                );
            });
            let result = match result {
                Ok(result) => result,
                Err(e) => {
                    let error = Some(format!("{e:#}"));
                    set_import_job(
                        &id,
                        import_status_value(&id, &last, true, error, None, idle),
                    );
                    return;
                }
            };
            // Phase A is what the user waits for; thumbnails keep landing afterwards.
            let status = |thumbnails: Value| {
                import_status_value(&id, &last, true, None, Some(result.clone()), thumbnails)
            };
            set_import_job(&id, status(idle.clone()));
            let mut last_thumbnails = idle.clone();
            copy_thumbnails_until_empty(&mut library, &mut |progress| {
                last_thumbnails = progress.clone();
                set_import_job(&id, status(progress.clone()));
            });
            last_thumbnails["done"] = last_thumbnails["total"].clone();
            last_thumbnails["finished"] = json!(true);
            set_import_job(&id, status(last_thumbnails));
        })?;
    Ok(json!({"job_id":job_id}))
}
fn import_status(id: &str) -> Result<Value> {
    IMPORT_JOBS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(id)
        .cloned()
        .with_context(|| format!("import job not found: {id}"))
}

/// State dirs with an Eagle thumbnail copier running on a thread of this process.
static THUMBNAIL_COPIERS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<PathBuf>>,
> = std::sync::LazyLock::new(Default::default);
/// Drains the library's Eagle thumbnail queue unless this process already does.
fn copy_thumbnails_until_empty(library: &mut Library, progress: &mut dyn FnMut(&Value)) {
    let key = library.state_dir.clone();
    if !THUMBNAIL_COPIERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.clone())
    {
        return;
    }
    let mut notify = |value: &Value| {
        thumbnails_landed();
        progress(value)
    };
    if let Err(e) = library.copy_eagle_thumbnails(usize::MAX, &mut notify) {
        eprintln!("eagle thumbnails: {e:#}");
    }
    thumbnails_landed();
    THUMBNAIL_COPIERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
}

/// Bumped by this process's thumbnail copier/downloader after each landed batch, so
/// `thumbnail_updates` with `wait` sleeps on a condvar instead of clients polling.
static THUMBNAILS_LANDED: std::sync::LazyLock<(std::sync::Mutex<u64>, std::sync::Condvar)> =
    std::sync::LazyLock::new(Default::default);
pub(crate) fn thumbnails_landed() {
    let (count, changed) = &*THUMBNAILS_LANDED;
    *count.lock().unwrap_or_else(|e| e.into_inner()) += 1;
    changed.notify_all();
}
/// `thumbnail_updates` that blocks up to `wait` s while thumbnails are pending but none
/// landed. Re-checks every few seconds for landings by other processes (vamp-server).
fn wait_thumbnail_updates(library: &Library, since: u64, wait: u64) -> Result<Value> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(wait.min(60));
    let (count, changed) = &*THUMBNAILS_LANDED;
    loop {
        let seen = *count.lock().unwrap_or_else(|e| e.into_inner());
        let value = library.thumbnail_updates(Some(since))?;
        let now = std::time::Instant::now();
        if value["asset_ids"].as_array().is_some_and(|ids| !ids.is_empty())
            || value["pending"].as_u64() == Some(0)
            || now >= deadline
        {
            return Ok(value);
        }
        let guard = count.lock().unwrap_or_else(|e| e.into_inner());
        if *guard == seen {
            let slice = (deadline - now).min(std::time::Duration::from_secs(5));
            drop(changed.wait_timeout(guard, slice).unwrap_or_else(|e| e.into_inner()));
        }
    }
}
/// Resumes phase B on a background thread (e.g. after an app restart) when work is queued.
fn spawn_thumbnail_copier(home: &Path, context: Option<&str>) -> Result<()> {
    let mut library = Library::open(home, context)?;
    if library.context.remote.is_some()
        || THUMBNAIL_COPIERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&library.state_dir)
        || library.pending_eagle_thumbnails()? == 0
    {
        return Ok(());
    }
    std::thread::Builder::new()
        .name("eagle-thumbnails".into())
        .spawn(move || copy_thumbnails_until_empty(&mut library, &mut |_| {}))?;
    Ok(())
}

/// Library-scoped native operations, also served by vamp-server's `POST /v1/native`.
pub fn dispatch(library: &mut Library, request: Request) -> Result<Value> {
    let asset_id = || request.asset_id.as_deref().context("asset_id is required");
    let asset_ids = || {
        ensure!(!request.asset_ids.is_empty(), "asset_ids is required");
        Ok(&request.asset_ids)
    };
    let collection_id = || {
        request
            .collection_id
            .as_deref()
            .context("collection_id is required")
    };
    let name = || request.name.as_deref().context("name is required");
    let rules = |value: &Option<Value>| -> Result<Option<SmartRules>> {
        value
            .clone()
            .map(serde_json::from_value)
            .transpose()
            .context("invalid rules; expected {match, rules:[{field, op, value}]}")
    };
    match request.operation.as_str() {
        "explore_ensure" => {
            explore::agent_actor()?;
            library.explore_ensure(request.slug.as_deref().context("slug is required")?, request.title.as_deref(), request.brief.as_deref())
        }
        "explore_add" => {
            explore::agent_actor()?;
            let content = if let Some(encoded) = &request.bytes_base64 {
                ensure!(encoded.len() as u64 <= (MAX_MEDIA + 2) / 3 * 4, "exploration media exceeds 64 MiB");
                let bytes = decode_base64(encoded)?;
                ensure!(bytes.len() as u64 <= MAX_MEDIA, "exploration media exceeds 64 MiB");
                explore::Content::Bytes { bytes, filename: request.filename.clone().context("filename is required")?, image_url: request.image_url.clone() }
            } else {
                explore::Content::Card { kind: request.kind.clone().unwrap_or_else(|| if request.url.is_some() { "link" } else { "note" }.into()) }
            };
            library.explore_add(
                request.slug.as_deref().context("slug is required")?,
                explore::Finding { name: name()?.to_owned(), note: request.note.clone(), tags: request.tags.clone(), url: request.url.clone(), folder: request.folder.clone(), content },
                media_helper().as_deref(),
            )
        }
        "explore_summary" => {
            explore::agent_actor()?;
            library.explore_summary(request.slug.as_deref().context("slug is required")?, request.text.as_deref().context("text is required")?)
        }
        "explore_show" => library.explore_show(request.slug.as_deref().context("slug is required")?),
        "explore_list" => library.explore_list(),
        "explore_link" => library.explore_link(request.slug.as_deref().context("slug is required")?, request.asset_id.as_deref(), request.server_url.as_deref().or(library.server_url()?.as_deref())),
        "explore_changes" => library.explore_changes(request.slug.as_deref().context("slug is required")?, request.since, !request.peek.unwrap_or(false)),
        "browse" if request.semantic => embeddings::native::semantic_browse(library, &request),
        "similar" => embeddings::native::similar(library, &request),
        // Authority pack for in-memory similarity browsing (replicas: native_remote).
        "energy_pack" => Ok(match library.energy_pack()? {
            Some((path, header)) => crate::energy::describe(&path, &header),
            None => json!({ "count": 0 }),
        }),
        "accept_tags" => embeddings::native::accept_tags(library, &request),
        "collections" => {
            let mut stmt = library.conn.prepare("SELECT m.collection_id,count(*) FROM memberships m JOIN assets a ON a.id=m.asset_id WHERE a.deleted=0 AND a.trashed=0 GROUP BY m.collection_id")?;
            let counts: BTreeMap<String, i64> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let rows = catalog::collection_rows(&library.conn)?
                .into_iter()
                .map(|c| {
                    json!({"id":c.id,"name":c.name,"count":counts.get(&c.id).copied().unwrap_or(0),"parent_id":c.parent_id,"sort_order":c.sort_order,"color":c.color,"icon":c.icon,"description":c.description,"source_id":c.source_id})
                })
                .collect::<Vec<_>>();
            Ok(json!(rows))
        }
        "browse" => browse(library, &BrowseFilter::from_request(&request)),
        "layout" => layout(library, &BrowseFilter::from_request(&request), request.micro),
        "items" => items(library, &request.asset_ids),
        "galaxy" => library.galaxy(request.rebuild),
        "galaxy_search" => library.galaxy_search(
            request.query.as_deref().unwrap_or(""),
            request.limit.unwrap_or(300).clamp(1, 5000),
        ),
        "micro" => {
            let found = crate::thumbs::micro_for(library, &request.asset_ids)?;
            let micro: serde_json::Map<String, Value> = found
                .into_iter()
                .map(|(id, data)| (id, json!(encode_base64(&data))))
                .collect();
            Ok(json!({ "micro": micro }))
        }
        // Authority: renders missing micro/grid thumbnails now (normally a background job).
        "generate_thumbs" => crate::thumbs::generate_all(library),
        "sidebar" => library.sidebar(),
        // Replicas mirroring `added_by` onto assets they cataloged before it existed.
        "added_by" => library.added_by_map(),
        "inspect" => inspect(library, asset_id()?),
        // The inspector's detail cache: every field it shows for each id, from the local
        // catalog in one call. Ids that no longer resolve are left out.
        "inspect_many" => Ok(json!(
            request
                .asset_ids
                .iter()
                .filter_map(|id| inspect(library, id).ok())
                .collect::<Vec<_>>()
        )),
        "original" => {
            let id = asset_id()?;
            let path = library.fetch(id, "original")?;
            // The bytes are local now: settle a lazy fingerprint. A failure here (e.g. the
            // file changed) must not block viewing; the next read retries.
            if get_asset(&library.conn, id)?.original.pending() {
                let _ = library.resolve_hash(id);
            }
            Ok(json!({"path":path}))
        }
        "touch" => {
            let id = asset_id()?;
            Ok(json!({"asset_id":id,"last_viewed_at":library.touch(id)?}))
        }
        "export_preview" => {
            let path = request.path.as_deref().context("path is required")?;
            export_preview(library, path)?;
            Ok(json!({"path":path}))
        }
        "export" => {
            let path = request.path.as_deref().context("path is required")?;
            library.export_originals(asset_ids()?, path)
        }
        "add" => {
            ensure!(!request.paths.is_empty(), "paths is required");
            let base: Metadata = metadata(request.metadata.clone())?;
            let helper = media_helper();
            let mut assets = Vec::new();
            let mut errors = Vec::new();
            let mut files = Vec::new();
            for path in &request.paths {
                if let Err(e) = expand(path, &mut files) {
                    errors.push(json!({"path":path,"message":format!("{e:#}")}));
                }
            }
            for path in files {
                match library.add(&path, None, base.clone()) {
                    Ok(asset) => {
                        generate_reps(library, &asset, helper.as_deref());
                        assets.push(get_asset(&library.conn, &asset.id)?);
                    }
                    Err(e) => errors.push(json!({"path":path,"message":format!("{e:#}")})),
                }
            }
            Ok(json!({"assets":assets,"errors":errors}))
        }
        "ingest" => {
            let encoded = request
                .bytes_base64
                .as_deref()
                .context("bytes_base64 is required")?;
            ensure!(
                encoded.len() / 4 * 3 <= MAX_INGEST + 3,
                "ingested content exceeds 64 MiB"
            );
            let bytes = decode_base64(encoded)?;
            let filename = request
                .filename
                .as_deref()
                .context("filename is required")?;
            let asset =
                library.ingest_bytes(&bytes, filename, metadata(request.metadata.clone())?)?;
            generate_reps(library, &asset, media_helper().as_deref());
            Ok(serde_json::to_value(get_asset(&library.conn, &asset.id)?)?)
        }
        // Answered from the local catalog in every context: replicas match their mirror.
        "find_duplicates" => {
            let mut files = Vec::new();
            for path in &request.paths {
                // An unreadable directory becomes a candidate with an error.
                if expand(path, &mut files).is_err() && !files.contains(path) {
                    files.push(path.clone());
                }
            }
            let bytes = request
                .bytes_base64
                .as_deref()
                .map(|encoded| {
                    ensure!(
                        encoded.len() / 4 * 3 <= MAX_INGEST + 3,
                        "content exceeds 64 MiB"
                    );
                    decode_base64(encoded)
                })
                .transpose()?;
            ensure!(
                !files.is_empty() || bytes.is_some() || !request.hashes.is_empty(),
                "paths, bytes_base64 or hashes is required"
            );
            let candidates = library.find_duplicates(
                &files,
                bytes.as_deref().map(|b| (b, request.filename.as_deref())),
                &request.hashes,
            )?;
            let mut results = Vec::with_capacity(candidates.len());
            for candidate in candidates {
                let mut value = serde_json::to_value(&candidate)?;
                for (i, found) in candidate.matches.iter().enumerate() {
                    let view = library.inspect(&found.asset_id)?;
                    value["matches"][i]["item"] =
                        media_item(library, &view.asset, &view.representations)?;
                }
                results.push(value);
            }
            Ok(json!({"results":results}))
        }
        "update" => {
            let mut patch: MetadataPatch = metadata(request.metadata.clone())?;
            if let Some(name) = &request.name {
                patch.name = Some(name.clone());
            }
            if let Some(name) = &patch.name {
                ensure!(!name.trim().is_empty(), "name cannot be empty");
            }
            if request.asset_ids.is_empty() {
                let asset = library.update(asset_id()?, patch, request.expected_revision)?;
                return Ok(serde_json::to_value(asset)?);
            }
            let assets = request
                .asset_ids
                .iter()
                .map(|id| library.update(id, patch.clone(), None))
                .collect::<Result<Vec<_>>>()?;
            Ok(json!({"assets":assets}))
        }
        "remove" => {
            let ids = asset_ids()?;
            let expected = if ids.len() == 1 {
                request.expected_revision
            } else {
                None
            };
            Ok(json!({"trashed":library.trash(ids, expected)?}))
        }
        "restore" => Ok(json!({"restored":library.restore(asset_ids()?)?})),
        "empty_trash" => {
            let ids = (!request.asset_ids.is_empty()).then_some(request.asset_ids.as_slice());
            Ok(json!({"deleted":library.empty_trash(ids)?}))
        }
        "collection_create" => Ok(serde_json::to_value(
            library.collection_create(name()?, request.parent_id.as_deref())?,
        )?),
        "collection_rename" => Ok(serde_json::to_value(
            library.collection_rename(collection_id()?, name()?)?,
        )?),
        "collection_delete" => {
            library.collection_delete(collection_id()?)?;
            Ok(json!({"deleted":collection_id()?}))
        }
        "collection_move" => Ok(serde_json::to_value(library.collection_move(
            collection_id()?,
            request.parent_id.as_deref(),
            request.index,
        )?)?),
        "collection_add" | "collection_remove" => {
            let add = request.operation == "collection_add";
            let mut collection = None;
            for id in asset_ids()? {
                collection = Some(library.collection_membership(collection_id()?, id, add)?);
            }
            Ok(serde_json::to_value(collection)?)
        }
        "smart_folders" => {
            let folders = library
                .smart_folders()?
                .into_iter()
                .map(|f| {
                    let count = library.smart_folder_count(&f)?;
                    let mut value = serde_json::to_value(f)?;
                    value["count"] = json!(count);
                    Ok(value)
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(json!(folders))
        }
        "smart_folder_create" => {
            let rules = rules(&request.rules)?.context("rules is required")?;
            Ok(serde_json::to_value(library.smart_folder_create(
                name()?,
                rules,
                request.parent_id.as_deref(),
            )?)?)
        }
        "smart_folder_update" => {
            let id = request
                .smart_folder_id
                .as_deref()
                .context("smart_folder_id is required")?;
            Ok(serde_json::to_value(library.smart_folder_update(
                id,
                request.name.as_deref(),
                rules(&request.rules)?,
            )?)?)
        }
        "smart_folder_delete" => {
            let id = request
                .smart_folder_id
                .as_deref()
                .context("smart_folder_id is required")?;
            library.smart_folder_delete(id)?;
            Ok(json!({"deleted":id}))
        }
        "tags" => Ok(json!({
            "tags": library
                .tag_counts()?
                .into_iter()
                .map(|(tag, count)| json!({"tag":tag,"count":count}))
                .collect::<Vec<_>>(),
            "groups": library.tag_groups()?,
            "starred": library.starred_tags()?,
        })),
        "tag_rename" => {
            let from = request.from.as_deref().context("from is required")?;
            let to = request.to.as_deref().context("to is required")?;
            Ok(json!({"updated":library.tag_rename(from, to)?}))
        }
        "tag_delete" => {
            let tag = request.tag.as_deref().context("tag is required")?;
            Ok(json!({"updated":library.tag_delete(tag)?}))
        }
        "import_eagle" => {
            let path = request.path.as_deref().context("path is required")?;
            library.import_eagle(path, hash_mode(&request)?)
        }
        "copy_thumbnails" => {
            library.copy_eagle_thumbnails(request.limit.unwrap_or(usize::MAX), &mut |_| {})
        }
        "prioritize_thumbnails" => library.prioritize_thumbnails(asset_ids()?),
        "thumbnail_updates" => match (request.since, request.wait) {
            (Some(since), Some(wait)) => wait_thumbnail_updates(library, since, wait),
            (since, _) => library.thumbnail_updates(since),
        },
        "hash" => library.hash_pending(request.limit.unwrap_or(100)),
        "palette" => {
            let helper = media_helper().context("vamp-media helper not found")?;
            Ok(serde_json::to_value(
                library.set_palette(asset_id()?, &helper)?,
            )?)
        }
        _ => anyhow::bail!("unknown operation: {}", request.operation),
    }
}

/// Derivatives are optional and regenerable: failures leave the asset without them.
fn generate_reps(library: &mut Library, asset: &Asset, helper: Option<&Path>) {
    let Some(helper) = helper else { return };
    if !asset.original.mime.starts_with("image/") {
        return;
    }
    for kind in ["thumbnail", "preview"] {
        if library.generate(&asset.id, kind, helper).is_err() {
            return;
        }
    }
}

/// Files are added as given; directories contribute their non-hidden files recursively.
pub(crate) fn expand(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    if !path.is_dir() {
        files.push(path.to_owned());
        return Ok(());
    }
    let mut entries = fs::read_dir(path)?
        .map(|e| Ok(e?.path()))
        .collect::<Result<Vec<_>>>()?;
    entries.sort();
    for entry in entries {
        if entry
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        {
            continue;
        }
        ensure!(
            files.len() < MAX_ASSETS,
            "too many files; add at most {MAX_ASSETS} at once"
        );
        expand(&entry, files)?;
    }
    Ok(())
}

pub(crate) fn encode_base64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub(crate) fn decode_base64(text: &str) -> Result<Vec<u8>> {
    let text = text.trim_end_matches('=');
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'\r' | b'\n' => continue,
            _ => anyhow::bail!("invalid base64"),
        };
        acc = acc << 6 | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    ensure!(bits < 6, "invalid base64 length");
    Ok(out)
}

/// `inspect`: an item plus every detail the inspector shows (all local; replicas mirror it).
fn inspect(library: &Library, id: &str) -> Result<Value> {
    let view = library.inspect(id)?;
    let observations = view.observations.iter().map(|o| json!({"id":o.id,"kind":o.kind,"model":o.model,"processor":o.processor,"text":o.value.as_str().or_else(||o.value.get("text").and_then(Value::as_str)).map(str::to_owned).unwrap_or_else(||o.value.to_string())})).collect::<Vec<_>>();
    let original = &view.asset.original;
    Ok(
        json!({"asset":media_item(library, &view.asset, &view.representations)?,"original_path":if original.path.is_empty() {None} else {Some(&original.path)},"source_url":view.asset.metadata.source_url,"human_description":view.asset.metadata.description,"observations":observations,"suggested_tags":view.suggested_tags,"representations":view.representations,"created_at":view.asset.created_at,"folder_ids":library.folder_ids(&view.asset.id)?,"hash_state":original.hash_state,"blake3":(!original.pending()).then_some(&original.blake3)}),
    )
}

pub(crate) fn media_item(
    library: &Library,
    asset: &Asset,
    representations: &[Representation],
) -> Result<Value> {
    let key = asset.original.source_key();
    let current = |kind: &str| {
        representations
            .iter()
            .find(|r| r.kind == kind && r.source_hash == key)
    };
    let derivative = |kind: &str| -> Result<Option<String>> {
        let Some(rep) = current(kind) else {
            return Ok(None);
        };
        let path = local_representation(&library.state_dir, &rep.path)?;
        if path.is_file() {
            Ok(Some(path_text(&path)?))
        } else {
            Ok(None)
        }
    };
    let thumbnail = derivative("thumbnail")?;
    // The 400 px grid JPEG rendered from the current thumbnail (authority only).
    let grid = match current("thumbnail") {
        Some(rep) => library
            .conn
            .prepare_cached("SELECT path FROM grid_thumbs WHERE asset_id=?1 AND source=?2")?
            .query_row(params![asset.id, rep.source_hash], |r| r.get::<_, String>(0))
            .optional()?
            .map(|p| local_representation(&library.state_dir, &p))
            .transpose()?
            .filter(|p| p.is_file())
            .map(|p| path_text(&p))
            .transpose()?,
        None => None,
    };
    // Replicas cache the authority's preview tier outside the representations table: report
    // it once local so clients show it without asking the engine (and never fetch for it).
    let preview = derivative("preview")?;
    #[cfg(feature = "remote")]
    let preview = match preview {
        None if library.remote().is_some() => {
            let tier = crate::remote::preview_tier_path(library, asset);
            tier.is_file().then(|| path_text(&tier)).transpose()?
        }
        preview => preview,
    };
    let size = current("preview").or_else(|| current("thumbnail"));
    let (width, height) = match (asset.width, asset.height) {
        (Some(w), Some(h)) => (Some(w), Some(h)),
        _ => (size.map(|r| r.width), size.map(|r| r.height)),
    };
    let original = library.original_available_locally(&asset.original);
    let ext = storage::original_extension(&asset.original)
        .trim_start_matches('.')
        .to_owned();
    let mut item = json!({
        "id": asset.id,
        "revision": asset.revision,
        "name": asset.name,
        "ext": ext,
        "mime": asset.original.mime,
        "byte_size": asset.original.byte_size,
        "created_at": asset.created_at,
        "original_created_at": asset.original_created_at,
        "modified": asset.original_modified_at.as_ref().unwrap_or(&asset.updated_at),
        "last_viewed_at": asset.last_viewed_at,
        "deleted_at": asset.deleted_at,
        "width": width,
        "height": height,
        "rating": asset.metadata.rating,
        "colors": asset.metadata.colors,
        "hash_state": asset.original.hash_state,
        "available_locally": {"thumbnail":thumbnail.is_some(),"preview":preview.is_some(),"original":original},
        "storage": asset.original.storage,
        "thumbnail_path": thumbnail,
        "grid_path": grid,
        "preview_path": preview,
        "tags": asset.metadata.tags,
        "description": asset.metadata.description,
        "source_url": asset.metadata.source_url,
        "added_by": crate::added_by(asset),
    });
    // Why the authority could not fetch a clipped original (`storage` "failed").
    if let Some(error) = asset.metadata.extra.get("original_error") {
        item["original_error"] = error.clone();
    }
    Ok(item)
}

/// `" AND …"` SQL conditions over `assets a` for the non-text browse filters, with their
/// positional arguments. Includes the trash scope. Shared by full-text and semantic browse.
pub(crate) fn filter_conditions(
    library: &Library,
    filter: &BrowseFilter,
) -> Result<(String, Vec<rusqlite::types::Value>)> {
    use catalog::*;
    let conn = &library.conn;
    let mut clause = Clause::default();
    clause.and(Clause {
        sql: if filter.trash {
            "a.trashed=1"
        } else {
            "a.trashed=0"
        }
        .into(),
        args: vec![],
    });
    let expand = |ids: &[String]| -> Result<Vec<String>> {
        if filter.include_subfolders {
            with_descendants(conn, ids)
        } else {
            Ok(ids.to_vec())
        }
    };
    match filter.collection {
        Some(UNCATEGORIZED) => clause.and(uncategorized()),
        Some(id) => clause.and(in_folders(&expand(&[id.to_owned()])?)),
        None => {}
    }
    if filter.uncategorized {
        clause.and(uncategorized());
    }
    if !filter.folder_ids.is_empty() {
        clause.and(in_folders(&expand(filter.folder_ids)?));
    }
    if let Some(id) = filter.smart_folder {
        clause.and(smart_clause(conn, &smart_folder(conn, id)?.rules)?);
    }
    if let Some(kind) = filter.kind {
        clause.and(Clause {
            sql: "json_extract(a.document,'$.original.mime') LIKE ?".into(),
            args: vec![format!("{kind}/%").into()],
        });
    }
    if !filter.tags.is_empty() {
        clause.and(tags_all(filter.tags));
    }
    if !filter.tags_any.is_empty() {
        clause.and(tags_any(filter.tags_any));
    }
    if filter.untagged {
        clause.and(untagged());
    }
    if !filter.exts.is_empty() {
        clause.and(exts(filter.exts));
    }
    if let Some(shape_name) = filter.shape {
        clause.and(shape(shape_name)?);
    }
    if let Some(min) = filter.size_min {
        clause.and(Clause {
            sql: "a.byte_size >= ?".into(),
            args: vec![(min.min(i64::MAX as u64) as i64).into()],
        });
    }
    if let Some(max) = filter.size_max {
        clause.and(Clause {
            sql: "a.byte_size <= ?".into(),
            args: vec![(max.min(i64::MAX as u64) as i64).into()],
        });
    }
    if let Some(rating) = filter.rating_min.filter(|r| *r > 0) {
        ensure!(rating <= 5, "rating_min must be 0–5");
        clause.and(Clause {
            sql: "a.rating >= ?".into(),
            args: vec![i64::from(rating).into()],
        });
    }
    if !filter.colors.is_empty() {
        clause.and(colors_within(
            &color_query(filter.colors)?,
            filter.color_tolerance.unwrap_or(DEFAULT_COLOR_TOLERANCE),
        ));
    }
    if filter.recently_used {
        clause.and(Clause {
            sql: "a.last_viewed IS NOT NULL".into(),
            args: vec![],
        });
    }
    if let Some(added_by) = filter.added_by {
        clause.and(added_by_clause(added_by)?);
    }
    let column = date_column(filter.date_field.unwrap_or("created"))?;
    if let Some(from) = filter.date_from {
        clause.and(Clause {
            sql: format!("{column} >= ?"),
            args: vec![browse_date(from, false)?.into()],
        });
    }
    if let Some(to) = filter.date_to {
        let bound = browse_date(to, true)?;
        let op = if bound.ends_with('U') { "<" } else { "<=" };
        clause.and(Clause {
            sql: format!("{column} {op} ?"),
            args: vec![bound.into()],
        });
    }
    // Clause::default() starts empty; every `and` contributed " AND (…)".
    Ok((clause.sql, clause.args))
}

/// The `FROM … WHERE …` and `ORDER BY` of a browse, shared by `browse` (one page) and
/// `layout` (every match), so both list exactly the same assets in the same order.
struct BrowseQuery {
    sql: String,
    args: Vec<rusqlite::types::Value>,
    order: String,
    order_args: Vec<rusqlite::types::Value>,
}

fn browse_query(library: &Library, filter: &BrowseFilter) -> Result<BrowseQuery> {
    let BrowseFilter { query, sort, .. } = *filter;
    ensure!(
        query.unwrap_or("").len() <= 4096,
        "search query exceeds 4096 bytes"
    );
    let words = query
        .unwrap_or("")
        .split_whitespace()
        .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" AND ");
    let (conditions, values) = filter_conditions(library, filter)?;
    let mut args: Vec<rusqlite::types::Value> = Vec::new();
    let mut sql = String::new();
    if !words.is_empty() {
        // CROSS JOIN pins the FTS scan as the outer loop (never one MATCH per asset).
        sql.push_str("FROM search CROSS JOIN assets a ON a.id=search.asset_id WHERE search MATCH ? AND a.deleted=0");
        args.push(words.clone().into());
    } else if filter.uses_facets() {
        // Facet filters read virtual columns; the covering index avoids parsing every
        // JSON document (SQLite does not cost generated columns).
        sql.push_str(&format!(
            "FROM assets a INDEXED BY {} WHERE a.deleted=0",
            catalog::FACETS_INDEX
        ));
    } else {
        sql.push_str("FROM assets a WHERE a.deleted=0");
    }
    sql.push_str(&conditions);
    args.extend(values);
    let color_query = if filter.colors.is_empty() {
        None
    } else {
        Some(catalog::color_query(filter.colors)?)
    };
    let mut order_args: Vec<rusqlite::types::Value> = Vec::new();
    let order: String = match (sort, &color_query) {
        // created_at, not rowid: replica mirrors insert in sync order, not authority order.
        (Some("added_desc"), _) => "a.imported DESC, a.rowid DESC".into(),
        (Some("added_asc"), _) => "a.imported, a.rowid".into(),
        (Some("name_asc"), _) => "a.name_key COLLATE NOCASE, a.rowid".into(),
        (Some("name_desc"), _) => "a.name_key COLLATE NOCASE DESC, a.rowid DESC".into(),
        (Some("rating_desc"), _) => "a.rating DESC, a.imported DESC, a.rowid DESC".into(),
        (Some("size_desc"), _) => "a.byte_size DESC, a.rowid DESC".into(),
        (Some("dimensions_desc"), _) => "coalesce(a.width*a.height,0) DESC, a.rowid DESC".into(),
        (Some("modified_desc"), _) => "a.modified_key DESC, a.rowid DESC".into(),
        (Some("created_desc"), _) => "a.created_key DESC, a.rowid DESC".into(),
        (Some("last_viewed_desc"), _) => {
            "a.last_viewed IS NULL, a.last_viewed DESC, a.rowid DESC".into()
        }
        (Some("random"), _) => {
            order_args.push(filter.seed.unwrap_or(0).into());
            "vamp_shuffle(a.rowid, ?), a.rowid".into()
        }
        (Some("color") | None, Some(colors)) => {
            order_args.push(colors.clone().into());
            format!("{} , a.rowid", catalog::COLOR_DISTANCE)
        }
        (Some("color"), None) => anyhow::bail!("sort color requires colors"),
        (Some(other), _) => anyhow::bail!("unknown sort: {other}"),
        (None, None) if words.is_empty() => "a.rowid".into(),
        (None, None) => "bm25(search),a.id".into(),
    };
    Ok(BrowseQuery {
        sql,
        args,
        order,
        order_args,
    })
}

/// Every asset a browse with the same filter/sort would list, as compact parallel arrays
/// (`ids`, `widths`, `heights`; 0 = unknown) so a client can lay out the whole result at
/// once and fetch cell details (`items`) only for what is on screen. `micro:true` adds
/// `micro`: base64 micro-thumbnails (or null) in the same order.
pub fn layout(library: &mut Library, filter: &BrowseFilter, micro: bool) -> Result<Value> {
    let BrowseQuery {
        sql,
        mut args,
        order,
        order_args,
    } = browse_query(library, filter)?;
    args.extend(order_args);
    let (mut ids, mut widths, mut heights) = (Vec::new(), Vec::new(), Vec::new());
    {
        let _snapshot = read_snapshot(&library.conn)?;
        // Assets without catalog dimensions take their preview/thumbnail's.
        let mut stmt = library.conn.prepare(&format!(
            "SELECT a.id, coalesce(a.width,(SELECT json_extract(r.document,'$.width') FROM representations r WHERE r.asset_id=a.id ORDER BY r.kind DESC LIMIT 1),0), coalesce(a.height,(SELECT json_extract(r.document,'$.height') FROM representations r WHERE r.asset_id=a.id ORDER BY r.kind DESC LIMIT 1),0) {sql} ORDER BY {order}"
        ))?;
        let mut rows = stmt.query(rusqlite::params_from_iter(args.iter()))?;
        while let Some(row) = rows.next()? {
            ids.push(row.get::<_, String>(0)?);
            widths.push(row.get::<_, i64>(1)?.clamp(0, u32::MAX as i64) as u32);
            heights.push(row.get::<_, i64>(2)?.clamp(0, u32::MAX as i64) as u32);
        }
    }
    let mut value = json!({"total":ids.len(),"widths":widths,"heights":heights});
    if micro {
        let blobs = crate::thumbs::micro_for(library, &ids)?;
        value["micro"] = json!(
            ids.iter()
                .map(|id| blobs.get(id).map(|b| encode_base64(b)))
                .collect::<Vec<_>>()
        );
    }
    value["ids"] = json!(ids);
    Ok(value)
}

/// `items`: browse cells for `ids`, in the given order; unknown/deleted ids are skipped.
pub fn items(library: &Library, ids: &[String]) -> Result<Value> {
    ensure!(ids.len() <= 2000, "at most 2000 ids per items request");
    let _snapshot = read_snapshot(&library.conn)?;
    let mut asset_stmt = library
        .conn
        .prepare_cached("SELECT document FROM assets WHERE id=?1 AND deleted=0")?;
    let mut reps_stmt = library.conn.prepare_cached(
        "SELECT document FROM representations WHERE asset_id=?1 AND source_hash=?2",
    )?;
    let mut items = Vec::with_capacity(ids.len());
    for id in ids {
        let Some(document) = asset_stmt
            .query_row([id], |r| r.get::<_, String>(0))
            .optional()?
        else {
            continue;
        };
        let asset: Asset = parse(document)?;
        let reps = reps_stmt
            .query_map(params![asset.id, asset.original.source_key()], |r| {
                r.get::<_, String>(0)
            })?
            .map(|row| parse::<Representation>(row?))
            .collect::<Result<Vec<_>>>()?;
        items.push(media_item(library, &asset, &reps)?);
    }
    Ok(json!({ "items": items }))
}

pub fn browse(library: &Library, filter: &BrowseFilter) -> Result<Value> {
    let BrowseFilter { offset, limit, .. } = *filter;
    ensure!(
        (1..=500).contains(&limit) && offset <= i64::MAX as usize,
        "invalid pagination; limit must be 1–500"
    );
    let BrowseQuery {
        sql,
        mut args,
        order,
        order_args,
    } = browse_query(library, filter)?;
    let _snapshot = read_snapshot(&library.conn)?;
    let total: i64 = library.conn.query_row(
        &format!("SELECT count(*) {sql}"),
        rusqlite::params_from_iter(args.iter()),
        |r| r.get(0),
    )?;
    args.extend(order_args);
    args.push((limit as i64).into());
    args.push((offset as i64).into());
    let assets = documents::<Asset>(
        &library.conn,
        &format!("SELECT a.document {sql} ORDER BY {order} LIMIT ? OFFSET ?"),
        rusqlite::params_from_iter(args.iter()),
    )?;
    let targets = filter
        .colors
        .iter()
        .filter_map(|c| catalog::hex_to_lab(c))
        .collect::<Vec<_>>();
    let mut reps_stmt = library.conn.prepare_cached(
        "SELECT document FROM representations WHERE asset_id=?1 AND source_hash=?2",
    )?;
    let mut items = Vec::with_capacity(assets.len());
    for asset in assets {
        let reps = reps_stmt
            .query_map(params![asset.id, asset.original.source_key()], |r| {
                r.get::<_, String>(0)
            })?
            .map(|row| parse::<Representation>(row?))
            .collect::<Result<Vec<_>>>()?;
        let mut item = media_item(library, &asset, &reps)?;
        if !targets.is_empty() {
            item["color_distance"] =
                json!(catalog::color_distance(&asset.metadata.colors, &targets));
        }
        items.push(item);
    }
    Ok(
        json!({"has_more":(offset as u64 + items.len() as u64)<total as u64,"items":items,"total":total,"offset":offset}),
    )
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewManifest {
    format: String,
    version: u32,
    preview_only: bool,
    origin_library_id: String,
    assets: Vec<AssetView>,
    collections: Vec<Collection>,
    media: BTreeMap<String, MediaDigest>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MediaDigest {
    byte_size: u64,
    blake3: String,
}

fn safe_entry(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 240
            && !name.contains('\\')
            && !name.contains(':')
            && !name.contains('\0')
            && name.split('/').all(|c| !c.is_empty()
                && c != "."
                && c != ".."
                && c.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')),
        "unsafe archive path: {name}"
    );
    Ok(())
}
fn web_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}
fn scrub(asset: &mut Asset) {
    asset.original.path.clear();
    asset.metadata.extra.clear();
    if asset
        .metadata
        .source_url
        .as_deref()
        .is_some_and(|url| !web_url(url))
    {
        asset.metadata.source_url = None;
    }
}
fn copy_digest(reader: &mut impl Read, writer: &mut impl Write, expected: u64) -> Result<String> {
    ensure!(expected <= MAX_MEDIA, "media exceeds 64 MiB");
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        ensure!(total <= expected, "media exceeds declared length");
        hasher.update(&buffer[..n]);
        writer.write_all(&buffer[..n])?;
    }
    ensure!(total == expected, "media length mismatch");
    Ok(hasher.finalize().to_hex().to_string())
}

/// Export a read transaction's active catalog and only its current thumbnail/preview bytes.
/// The destination must not exist. A failed export removes only the file it created.
pub fn export_preview(library: &Library, path: &Path) -> Result<()> {
    ensure!(
        path.extension().is_some_and(|e| e == "vamp"),
        "preview archive must have .vamp extension"
    );
    let _snapshot = read_snapshot(&library.conn)?;
    let count: usize = library
        .conn
        .query_row("SELECT count(*) FROM assets WHERE deleted=0", [], |r| {
            r.get::<_, i64>(0)
        })?
        .try_into()?;
    ensure!(count <= MAX_ASSETS, "preview archive exceeds 20000 assets");
    let assets = documents::<Asset>(
        &library.conn,
        "SELECT document FROM assets WHERE deleted=0 ORDER BY rowid",
        [],
    )?;
    let mut manifest = PreviewManifest {
        format: "vamp-preview".into(),
        version: 1,
        preview_only: true,
        origin_library_id: library.context.library_id.clone(),
        assets: Vec::with_capacity(count),
        collections: library.collections()?,
        media: BTreeMap::new(),
    };
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let result = (|| -> Result<()> {
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(0o600);
        let mut total = 0u64;
        for asset in assets {
            let mut view = library.inspect(&asset.id)?;
            scrub(&mut view.asset);
            view.representations
                .retain(|r| r.kind == "thumbnail" || r.kind == "preview");
            ensure!(
                view.representations.iter().any(|r| r.kind == "preview"),
                "asset {} has no current preview",
                asset.id
            );
            for rep in &mut view.representations {
                let source = local_representation(&library.state_dir, &rep.path)?;
                ensure!(
                    !fs::symlink_metadata(&source)?.file_type().is_symlink(),
                    "symlink derivative refused"
                );
                let mut source = File::open(source)?;
                ensure!(
                    source.metadata()?.is_file(),
                    "derivative is not a regular file"
                );
                let size = source.metadata()?.len();
                ensure!(
                    size == rep.byte_size && size > 0,
                    "derivative length differs from catalog"
                );
                total = total.checked_add(size).context("archive length overflow")?;
                ensure!(total <= MAX_TOTAL, "archive exceeds 2 GiB");
                let name = format!("media/{}-{}.jpg", id::simple(), rep.kind);
                zip.start_file(&name, options)?;
                let digest = copy_digest(&mut source, &mut zip, size)?;
                rep.path = name.clone();
                manifest.media.insert(
                    name,
                    MediaDigest {
                        byte_size: size,
                        blake3: digest,
                    },
                );
            }
            manifest.assets.push(view);
        }
        let bytes = serde_json::to_vec(&manifest)?;
        ensure!(
            bytes.len() as u64 <= MAX_MANIFEST,
            "manifest exceeds 64 MiB"
        );
        zip.start_file("manifest.json", options)?;
        zip.write_all(&bytes)?;
        zip.finish()?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

fn read_manifest(zip: &mut ZipArchive<File>) -> Result<PreviewManifest> {
    ensure!(zip.len() <= MAX_ENTRIES, "too many archive entries");
    let mut names = BTreeSet::new();
    let mut total = 0u64;
    for index in 0..zip.len() {
        let entry = zip.by_index(index)?;
        safe_entry(entry.name())?;
        ensure!(
            names.insert(entry.name().to_owned()),
            "duplicate archive entry"
        );
        ensure!(
            entry.compression() == zip::CompressionMethod::Stored && !entry.is_dir(),
            "only stored regular files are supported"
        );
        if let Some(mode) = entry.unix_mode() {
            ensure!(
                mode & 0o170000 == 0 || mode & 0o170000 == 0o100000,
                "non-regular archive entry"
            );
        }
        ensure!(
            entry.size()
                <= if entry.name() == "manifest.json" {
                    MAX_MANIFEST
                } else {
                    MAX_MEDIA
                },
            "archive entry exceeds size limit"
        );
        total = total
            .checked_add(entry.size())
            .context("archive size overflow")?;
        ensure!(
            total <= MAX_TOTAL + MAX_MANIFEST,
            "archive exceeds total size limit"
        );
    }
    let entry = zip.by_name("manifest.json")?;
    let mut bytes = Vec::new();
    entry.take(MAX_MANIFEST + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_MANIFEST,
        "manifest exceeds size limit"
    );
    let manifest: PreviewManifest = serde_json::from_slice(&bytes)?;
    ensure!(
        manifest.format == "vamp-preview" && manifest.version == 1 && manifest.preview_only,
        "unsupported preview archive"
    );
    ensure!(
        manifest.assets.len() <= MAX_ASSETS && manifest.collections.len() <= MAX_ASSETS,
        "catalog exceeds entry limit"
    );
    ensure!(
        manifest.media.len() + 1 == names.len(),
        "unlisted archive entries"
    );
    let mut referenced = BTreeSet::new();
    let mut assets = BTreeSet::new();
    let mut observations = BTreeSet::new();
    for view in &manifest.assets {
        ensure!(
            !view.asset.id.is_empty() && assets.insert(view.asset.id.clone()),
            "duplicate or empty asset id"
        );
        ensure!(
            view.asset.original.path.is_empty() && view.asset.metadata.extra.is_empty(),
            "preview contains original locators or extra metadata"
        );
        ensure!(
            view.asset
                .metadata
                .source_url
                .as_deref()
                .is_none_or(web_url),
            "non-web source locator refused"
        );
        let mut kinds = BTreeSet::new();
        for rep in &view.representations {
            ensure!(
                (rep.kind == "thumbnail" || rep.kind == "preview")
                    && kinds.insert(rep.kind.as_str()),
                "invalid or duplicate representation"
            );
            safe_entry(&rep.path)?;
            ensure!(
                rep.path.starts_with("media/")
                    && names.contains(&rep.path)
                    && referenced.insert(rep.path.clone()),
                "invalid or shared media entry"
            );
            let digest = manifest
                .media
                .get(&rep.path)
                .context("missing media digest")?;
            ensure!(
                digest.byte_size == rep.byte_size
                    && rep.byte_size > 0
                    && digest.byte_size <= MAX_MEDIA
                    && digest.blake3.len() == 64
                    && digest.blake3.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid media digest or size"
            );
            ensure!(
                rep.source_hash == view.asset.original.source_key(),
                "stale representation"
            );
        }
        ensure!(kinds.contains("preview"), "asset has no preview");
        for observation in &view.observations {
            ensure!(
                observation.asset_id == view.asset.id
                    && observation.source_hash == view.asset.original.source_key()
                    && observations.insert(observation.id.clone()),
                "invalid observation reference or duplicate id"
            );
        }
    }
    ensure!(
        referenced.len() == manifest.media.len(),
        "unreferenced media"
    );
    let mut collections = BTreeSet::new();
    for collection in &manifest.collections {
        ensure!(collections.insert(&collection.id), "duplicate collection");
    }
    for collection in &manifest.collections {
        ensure!(
            collection
                .parent_id
                .as_ref()
                .is_none_or(|p| p != &collection.id && collections.contains(p)),
            "invalid collection parent"
        );
        let mut members = BTreeSet::new();
        ensure!(
            collection
                .asset_ids
                .iter()
                .all(|id| assets.contains(id) && members.insert(id)),
            "invalid collection membership"
        );
    }
    Ok(manifest)
}

// Bound the central directory before ZipArchive can allocate from untrusted counts.
// This small format deliberately excludes ZIP64, split archives, and trailing bytes.
fn preflight(file: &mut File) -> Result<()> {
    let length = file.metadata()?.len();
    ensure!(
        (22..=MAX_TOTAL + MAX_MANIFEST + 32 * 1024 * 1024).contains(&length),
        "archive file exceeds size limit"
    );
    let tail_len = length.min(65_557) as usize;
    file.seek(SeekFrom::End(-(tail_len as i64)))?;
    let mut tail = vec![0; tail_len];
    file.read_exact(&mut tail)?;
    let pos = (0..=tail_len - 22)
        .rev()
        .find(|&i| {
            tail[i..i + 4] == *b"PK\x05\x06"
                && i + 22 + u16::from_le_bytes([tail[i + 20], tail[i + 21]]) as usize == tail_len
        })
        .context("missing ZIP end record")?;
    let u16_at = |n| u16::from_le_bytes([tail[pos + n], tail[pos + n + 1]]);
    let u32_at = |n| u32::from_le_bytes(tail[pos + n..pos + n + 4].try_into().unwrap());
    ensure!(
        u16_at(4) == 0 && u16_at(6) == 0 && u16_at(8) == u16_at(10),
        "split archive refused"
    );
    ensure!(
        u16_at(10) as usize <= MAX_ENTRIES,
        "archive entry count exceeds limit"
    );
    let directory_size = u32_at(12) as u64;
    let directory_offset = u32_at(16) as u64;
    ensure!(
        directory_size <= 32 * 1024 * 1024
            && directory_offset + directory_size == length - tail_len as u64 + pos as u64,
        "invalid, oversized or ZIP64 central directory"
    );
    file.seek(SeekFrom::Start(0))?;
    Ok(())
}

/// Validate before publication. Nothing is registered until all bytes and the ordinary
/// SQLite catalog are committed; failure removes only this import's new UUID directory.
pub fn import_preview(
    home: &Path,
    path: &Path,
    requested_name: Option<&str>,
) -> Result<(String, Context)> {
    let mut file = File::open(path)?;
    preflight(&mut file)?;
    let mut zip = ZipArchive::new(file)?;
    let manifest = read_manifest(&mut zip)?;
    let library_id = id();
    let name = requested_name
        .map(str::to_owned)
        .unwrap_or_else(|| format!("preview-{}", &library_id[..8]));
    validate_name(&name)?;
    let home = destination(home)?;
    let _lock = config_lock(&home)?;
    let mut config = load_config(&home)?;
    ensure!(
        !config.contexts.contains_key(&name),
        "context already exists: {name}"
    );
    let parent = home.join("libraries");
    fs::create_dir_all(&parent)?;
    let state = parent.canonicalize()?.join(&library_id);
    fs::create_dir(&state)?;
    let result = (|| -> Result<Context> {
        let context = Context {
            library_id: library_id.clone(),
            state_dir: path_text(&state)?,
            original_root: None,
            remote: None,
        };
        fs::create_dir(state.join("media"))?;
        for (name, digest) in &manifest.media {
            let mut entry = zip.by_name(name)?;
            ensure!(
                entry.size() == digest.byte_size,
                "media length differs from manifest"
            );
            // The format is intentionally flat: no nested directories or extracted config.
            ensure!(
                name.strip_prefix("media/")
                    .is_some_and(|tail| !tail.contains('/')),
                "nested media path refused"
            );
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(state.join(name))?;
            let actual = copy_digest(&mut entry, &mut output, digest.byte_size)?;
            ensure!(actual == digest.blake3, "media digest mismatch");
            output.sync_all()?;
        }
        let mut library = Library::connect(context.clone())?;
        let tx = library.conn.transaction()?;
        for view in &manifest.assets {
            // Empty original paths make core fetch(original) unavailable on every host.
            // The SQL uniqueness key is not a filesystem locator and never enters Asset.
            tx.execute(
                "INSERT INTO assets(id,path,hash,document) VALUES(?1,?2,?3,?4)",
                params![
                    view.asset.id,
                    format!("preview-only:{}", view.asset.id),
                    view.asset.original.source_key(),
                    json_text(&view.asset)?
                ],
            )?;
            for rep in &view.representations {
                tx.execute("INSERT INTO representations(asset_id,kind,source_hash,document) VALUES(?1,?2,?3,?4)", params![view.asset.id,rep.kind,rep.source_hash,json_text(rep)?])?;
            }
            for observation in &view.observations {
                tx.execute("INSERT INTO observations(id,asset_id,source_hash,document) VALUES(?1,?2,?3,?4)", params![observation.id,observation.asset_id,observation.source_hash,json_text(observation)?])?;
            }
            reindex(&tx, &view.asset)?;
        }
        for collection in &manifest.collections {
            catalog::insert_collection(&tx, collection)?;
            for asset in &collection.asset_ids {
                tx.execute(
                    "INSERT INTO memberships(collection_id,asset_id) VALUES(?1,?2)",
                    params![collection.id, asset],
                )?;
            }
        }
        tx.commit()?;
        library
            .conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        drop(library);
        atomic_json(
            &state.join("library.json"),
            &json!({"protocol_version":PROTOCOL_VERSION,"library_id":library_id,"preview_only":true,"origin_library_id":manifest.origin_library_id,"storage":"preview_only","writer_model":"local_authority","display_name":requested_name.or_else(||path.file_stem().and_then(|s|s.to_str())).unwrap_or(&name)}),
        )?;
        config
            .contexts
            .insert(name.clone(), stored_context(&home, &context)?);
        config.current = Some(name.clone());
        atomic_json(&home.join("config.json"), &config)?;
        Ok(context)
    })();
    match result {
        Ok(context) => Ok((name, context)),
        Err(error) => {
            let _ = fs::remove_dir_all(&state);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture(root: &Path) -> (Library, String, String) {
        let original = root.join("source.jpg");
        fs::write(&original, b"source bytes never exported").unwrap();
        let mut library = Library::init(&root.join("source-home"), "source", None, None).unwrap();
        let asset = library
            .add(
                &original,
                Some("Offline photograph"),
                Metadata {
                    tags: vec!["architecture".into()],
                    extra: BTreeMap::from([("eagle_library".into(), json!("/private/source"))]),
                    ..Metadata::default()
                },
            )
            .unwrap();
        fs::create_dir(library.state_dir.join("representations")).unwrap();
        let bytes = b"local preview bytes";
        fs::write(library.state_dir.join("representations/preview.jpg"), bytes).unwrap();
        let rep = Representation {
            kind: "preview".into(),
            path: "representations/preview.jpg".into(),
            source_hash: asset.original.blake3.clone(),
            width: 10,
            height: 10,
            mime: "image/jpeg".into(),
            byte_size: bytes.len() as u64,
        };
        library.conn.execute("INSERT INTO representations(asset_id,kind,source_hash,document) VALUES(?1,?2,?3,?4)", params![asset.id, rep.kind, rep.source_hash, json_text(&rep).unwrap()]).unwrap();
        let collection = library.collection_create("Buildings", None).unwrap();
        library
            .collection_membership(&collection.id, &asset.id, true)
            .unwrap();
        fs::remove_file(original).unwrap();
        (library, asset.id, collection.id)
    }

    #[test]
    fn preview_roundtrip_preserves_offline_core_search_fetch_and_identity() {
        let root = TempDir::new().unwrap();
        let (library, asset_id, collection_id) = fixture(root.path());
        let archive = root.path().join("library.vamp");
        export_preview(&library, &archive).unwrap();
        let home = root.path().join("import-home");
        let (_, context) = import_preview(&home, &archive, Some("offline")).unwrap();
        assert_ne!(context.library_id, library.context.library_id);
        let imported = Library::open(&home, Some("offline")).unwrap();
        assert_eq!(
            imported.list(Some("architecture"), 10, 0, None).unwrap()[0].id,
            asset_id
        );
        assert_eq!(
            imported.collections().unwrap()[0].asset_ids,
            vec![asset_id.clone()]
        );
        assert_eq!(
            fs::read(imported.fetch(&asset_id, "preview").unwrap()).unwrap(),
            b"local preview bytes"
        );
        assert!(imported.fetch(&asset_id, "original").is_err());
        let inspected = imported.inspect(&asset_id).unwrap();
        assert!(inspected.asset.original.path.is_empty());
        assert!(inspected.asset.metadata.extra.is_empty());
        let filter = |query, offset| BrowseFilter {
            query,
            collection: Some(collection_id.as_str()),
            offset,
            limit: 1,
            ..BrowseFilter::default()
        };
        let page = browse(&imported, &filter(Some("architecture"), 0)).unwrap();
        assert_eq!(page["items"][0]["id"], asset_id);
        assert_eq!(page["total"], 1);
        assert_eq!(
            browse(&imported, &filter(Some("absent"), 0)).unwrap()["total"],
            0
        );
        assert_eq!(
            browse(&imported, &filter(None, 1)).unwrap()["items"],
            json!([])
        );
        let second = root.path().join("second.vamp");
        export_preview(&imported, &second).unwrap();
        assert!(import_preview(&home, &archive, Some("offline")).is_err());
        assert_eq!(
            Library::open(&home, Some("offline"))
                .unwrap()
                .context
                .library_id,
            context.library_id
        );
    }

    #[test]
    fn imported_catalog_survives_home_relocation_and_config_writes() {
        let root = TempDir::new().unwrap();
        let (library, asset_id, _) = fixture(root.path());
        let archive = root.path().join("library.vamp");
        export_preview(&library, &archive).unwrap();
        let home = root.path().join("old-sandbox");
        attach_context(&home, "external", &library.state_dir).unwrap();
        import_preview(&home, &archive, Some("offline")).unwrap();
        let moved = root.path().join("new-sandbox");
        fs::rename(&home, &moved).unwrap();
        select_context(&moved, "offline").unwrap();
        import_preview(&moved, &archive, Some("another")).unwrap();
        for name in ["offline", "another", "external"] {
            let reopened = Library::open(&moved, Some(name)).unwrap();
            assert_eq!(
                reopened.list(Some("architecture"), 10, 0, None).unwrap()[0].id,
                asset_id
            );
            assert_eq!(
                fs::read(reopened.fetch(&asset_id, "preview").unwrap()).unwrap(),
                b"local preview bytes"
            );
        }
        let response =
            request(serde_json::from_value(json!({"operation":"contexts","home":moved})).unwrap())
                .unwrap();
        assert_eq!(response["contexts"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn two_imported_assets_remain_independently_editable() {
        let root = TempDir::new().unwrap();
        let (library, first_id, _) = fixture(root.path());
        let mut second = library.inspect(&first_id).unwrap();
        second.asset.id = id();
        let second_id = second.asset.id.clone();
        library
            .conn
            .execute(
                "INSERT INTO assets(id,path,hash,document) VALUES(?1,?2,?3,?4)",
                params![
                    second_id,
                    "other-source",
                    second.asset.original.blake3,
                    json_text(&second.asset).unwrap()
                ],
            )
            .unwrap();
        let rep = &second.representations[0];
        library.conn.execute("INSERT INTO representations(asset_id,kind,source_hash,document) VALUES(?1,?2,?3,?4)", params![second_id,rep.kind,rep.source_hash,json_text(rep).unwrap()]).unwrap();
        let archive = root.path().join("two.vamp");
        export_preview(&library, &archive).unwrap();
        let home = root.path().join("imported");
        import_preview(&home, &archive, Some("two")).unwrap();
        let mut imported = Library::open(&home, Some("two")).unwrap();
        imported
            .update(
                &first_id,
                MetadataPatch {
                    name: Some("First renamed".into()),
                    ..MetadataPatch::default()
                },
                None,
            )
            .unwrap();
        imported
            .update(
                &second_id,
                MetadataPatch {
                    name: Some("Second renamed".into()),
                    ..MetadataPatch::default()
                },
                None,
            )
            .unwrap();
        assert_eq!(imported.list(Some("First"), 10, 0, None).unwrap()[0].id, first_id);
        assert_eq!(
            imported.list(Some("Second"), 10, 0, None).unwrap()[0].id,
            second_id
        );
        assert!(imported.fetch(&first_id, "original").is_err());
        assert!(imported.fetch(&second_id, "original").is_err());
    }

    fn rewrite_archive(source: &Path, target: &Path, tamper: bool, extra: Option<&str>) {
        let mut archive = ZipArchive::new(File::open(source).unwrap()).unwrap();
        let mut output = ZipWriter::new(File::create(target).unwrap());
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).unwrap();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            if tamper && entry.name().starts_with("media/") {
                bytes[0] ^= 1;
            }
            output.start_file(entry.name(), options).unwrap();
            output.write_all(&bytes).unwrap();
        }
        if let Some(name) = extra {
            output.start_file(name, options).unwrap();
            output.write_all(b"unexpected payload").unwrap();
        }
        output.finish().unwrap();
    }

    #[test]
    fn digest_tamper_rolls_back_directory_and_preserves_existing_config() {
        let root = TempDir::new().unwrap();
        let (library, _, _) = fixture(root.path());
        let valid = root.path().join("valid.vamp");
        let bad = root.path().join("bad.vamp");
        export_preview(&library, &valid).unwrap();
        rewrite_archive(&valid, &bad, true, None);
        let home = root.path().join("destination");
        let (_, existing) = import_preview(&home, &valid, Some("kept")).unwrap();
        let before = fs::read(home.join("config.json")).unwrap();
        assert!(import_preview(&home, &bad, Some("rejected")).is_err());
        assert_eq!(fs::read(home.join("config.json")).unwrap(), before);
        assert_eq!(fs::read_dir(home.join("libraries")).unwrap().count(), 1);
        assert_eq!(
            Library::open(&home, Some("kept"))
                .unwrap()
                .context
                .library_id,
            existing.library_id
        );
    }

    #[test]
    fn traversal_and_executable_config_are_rejected_without_publication() {
        let root = TempDir::new().unwrap();
        let (library, _, _) = fixture(root.path());
        let valid = root.path().join("valid.vamp");
        export_preview(&library, &valid).unwrap();
        for (index, entry) in ["../escaped", "/absolute", "media\\escape", "config.json"]
            .iter()
            .enumerate()
        {
            let bad = root.path().join(format!("bad-{index}.vamp"));
            rewrite_archive(&valid, &bad, false, Some(entry));
            let home = root.path().join(format!("destination-{index}"));
            assert!(import_preview(&home, &bad, None).is_err());
            assert!(load_config(&home).unwrap().contexts.is_empty());
            assert!(!root.path().join("escaped").exists());
        }
    }

    fn call(home: &Path, body: Value) -> Result<Value> {
        let mut body = body;
        body["home"] = json!(home);
        request(serde_json::from_value(body)?)
    }

    fn catalog(root: &Path) -> (PathBuf, Vec<String>) {
        let home = root.join("home");
        Library::init(&home, "main", None, None).unwrap();
        let media = root.join("media");
        fs::create_dir_all(media.join("nested")).unwrap();
        for (file, bytes) in [
            ("b-cat.png", "1"),
            ("A-dog.jpg", "2"),
            ("nested/c-note.pdf", "3"),
            (".hidden.png", "4"),
        ] {
            fs::write(media.join(file), bytes).unwrap();
        }
        let added = call(&home, json!({"operation":"add","paths":[media, root.join("missing.png")],"metadata":{"tags":["pet"]}})).unwrap();
        assert_eq!(added["errors"].as_array().unwrap().len(), 1);
        let ids = added["assets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        (home, ids)
    }

    fn names(page: &Value) -> Vec<&str> {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["name"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn add_expands_directories_and_browse_sorts_and_filters() {
        let root = TempDir::new().unwrap();
        let (home, ids) = catalog(root.path());
        assert_eq!(ids.len(), 3, "hidden files skipped, directories recursed");
        let browse = |extra: Value| {
            let mut body = json!({"operation":"browse"});
            body.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            call(&home, body).unwrap()
        };
        assert_eq!(
            names(&browse(json!({"sort":"name_asc"}))),
            ["A-dog.jpg", "b-cat.png", "c-note.pdf"]
        );
        assert_eq!(
            names(&browse(json!({"sort":"name_desc"}))),
            ["c-note.pdf", "b-cat.png", "A-dog.jpg"]
        );
        assert_eq!(
            names(&browse(json!({"sort":"added_asc"}))),
            ["A-dog.jpg", "b-cat.png", "c-note.pdf"]
        );
        assert_eq!(
            names(&browse(json!({"sort":"added_desc"}))),
            ["c-note.pdf", "b-cat.png", "A-dog.jpg"]
        );
        let images = browse(json!({"kind":"image","sort":"name_asc"}));
        assert_eq!(names(&images), ["A-dog.jpg", "b-cat.png"]);
        assert_eq!(images["total"], 2);
        let item = &images["items"][0];
        assert_eq!(item["revision"], 1);
        assert_eq!(item["mime"], "image/jpeg");
        assert_eq!(item["byte_size"], 1);
        assert_eq!(item["tags"], json!(["pet"]));
        assert_eq!(
            item["available_locally"],
            json!({"thumbnail":false,"preview":false,"original":true})
        );
        let dog = &ids[0];
        call(
            &home,
            json!({"operation":"update","asset_id":dog,"metadata":{"tags":["pet","dog"]}}),
        )
        .unwrap();
        call(
            &home,
            json!({"operation":"update","asset_id":&ids[2],"metadata":{"tags":[]}}),
        )
        .unwrap();
        assert_eq!(names(&browse(json!({"tags":["pet","dog"]}))), ["A-dog.jpg"]);
        assert_eq!(browse(json!({"tags":["dog","absent"]}))["total"], 0);
        assert_eq!(names(&browse(json!({"untagged":true}))), ["c-note.pdf"]);
        assert_eq!(
            call(&home, json!({"operation":"tags"})).unwrap()["tags"],
            json!([{"tag":"pet","count":2},{"tag":"dog","count":1}])
        );
        let removed = call(&home, json!({"operation":"remove","asset_ids":[dog]})).unwrap();
        assert_eq!(removed["trashed"], 1);
        assert_eq!(
            call(&home, json!({"operation":"tags"})).unwrap()["tags"],
            json!([{"tag":"pet","count":1}])
        );
    }

    #[test]
    fn unknown_sort_is_rejected() {
        let root = TempDir::new().unwrap();
        let (home, _) = catalog(root.path());
        assert!(call(&home, json!({"operation":"browse","sort":"shuffle"})).is_err());
    }

    #[test]
    fn rename_bumps_revision_emits_event_and_honors_precondition() {
        let root = TempDir::new().unwrap();
        let (home, ids) = catalog(root.path());
        let renamed = call(&home, json!({"operation":"update","asset_id":&ids[0],"name":"Good dog","expected_revision":1})).unwrap();
        assert_eq!(renamed["name"], "Good dog");
        assert_eq!(renamed["revision"], 2);
        assert!(call(&home, json!({"operation":"update","asset_id":&ids[0],"name":"Stale","expected_revision":1})).is_err());
        assert!(
            call(
                &home,
                json!({"operation":"update","asset_id":&ids[0],"name":"  "})
            )
            .is_err()
        );
        let library = Library::open(&home, None).unwrap();
        let last = library.events(0, 1000).unwrap().pop().unwrap();
        assert_eq!(last.kind, "media_updated");
        assert_eq!(last.revision, Some(2));
        assert_eq!(library.list(Some("Good"), 10, 0, None).unwrap()[0].id, ids[0]);
    }

    #[test]
    fn collections_rename_delete_and_uncategorized_filter() {
        let root = TempDir::new().unwrap();
        let (home, ids) = catalog(root.path());
        let created = call(
            &home,
            json!({"operation":"collection_create","name":"Pets"}),
        )
        .unwrap();
        let cid = created["id"].as_str().unwrap();
        let added = call(
            &home,
            json!({"operation":"collection_add","collection_id":cid,"asset_ids":[&ids[0],&ids[1]]}),
        )
        .unwrap();
        assert_eq!(added["asset_ids"], json!([&ids[0], &ids[1]]));
        let uncategorized = call(
            &home,
            json!({"operation":"browse","collection_id":UNCATEGORIZED}),
        )
        .unwrap();
        assert_eq!(uncategorized["total"], 1);
        assert_eq!(uncategorized["items"][0]["id"], ids[2]);
        call(
            &home,
            json!({"operation":"collection_remove","collection_id":cid,"asset_ids":[&ids[1]]}),
        )
        .unwrap();
        assert_eq!(
            call(
                &home,
                json!({"operation":"browse","collection_id":UNCATEGORIZED})
            )
            .unwrap()["total"],
            2
        );

        let renamed = call(
            &home,
            json!({"operation":"collection_rename","collection_id":cid,"name":"Animals"}),
        )
        .unwrap();
        assert_eq!(renamed["name"], "Animals");
        assert_eq!(renamed["asset_ids"], json!([&ids[0]]));
        assert!(
            call(
                &home,
                json!({"operation":"collection_rename","collection_id":"absent","name":"X"})
            )
            .is_err()
        );
        call(
            &home,
            json!({"operation":"collection_delete","collection_id":cid}),
        )
        .unwrap();
        assert_eq!(
            call(&home, json!({"operation":"collections"})).unwrap(),
            json!([])
        );
        assert_eq!(
            call(
                &home,
                json!({"operation":"browse","collection_id":UNCATEGORIZED})
            )
            .unwrap()["total"],
            3
        );
        assert!(
            call(
                &home,
                json!({"operation":"collection_delete","collection_id":cid})
            )
            .is_err()
        );
        let events = Library::open(&home, None).unwrap().events(0, 1000).unwrap();
        let kinds = events
            .iter()
            .rev()
            .take(2)
            .map(|e| (e.kind.as_str(), e.payload.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            kinds[0],
            (
                "collection_deleted",
                json!({"collection_id":cid,"name":"Animals"})
            )
        );
        assert_eq!(
            kinds[1],
            (
                "collection_updated",
                json!({"collection_id":cid,"name":"Animals","operation":"rename"})
            )
        );
    }

    #[test]
    fn ingest_decodes_base64_into_inbox_original() {
        let root = TempDir::new().unwrap();
        let home = root.path().join("home");
        Library::init(&home, "main", None, None).unwrap();
        let asset = call(&home, json!({"operation":"ingest","bytes_base64":"aGVsbG8gdmFtcCE=","filename":"Clip.PNG","metadata":{"source_url":"https://example.com/a"}})).unwrap();
        assert_eq!(asset["name"], "Clip.png");
        assert_eq!(asset["original"]["mime"], "image/png");
        assert_eq!(asset["metadata"]["source_url"], "https://example.com/a");
        let path = PathBuf::from(asset["original"]["path"].as_str().unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"hello vamp!");
        let library = Library::open(&home, None).unwrap();
        assert!(path.starts_with(library.inbox_dir().canonicalize().unwrap()));
        assert!(
            call(
                &home,
                json!({"operation":"ingest","bytes_base64":"a$==","filename":"x.png"})
            )
            .is_err()
        );
        assert_eq!(decode_base64("AAECAw").unwrap(), [0, 1, 2, 3]);
    }

    #[test]
    fn find_duplicates_expands_folders_and_returns_browse_items() {
        let root = TempDir::new().unwrap();
        let home = root.path().join("home");
        Library::init(&home, "main", None, None).unwrap();
        let original = root.path().join("photo.png");
        fs::write(&original, b"duplicate me").unwrap();
        let added = call(&home, json!({"operation":"add","paths":[original]})).unwrap();
        let id = added["assets"][0]["id"].clone();
        let drop = root.path().join("drop");
        fs::create_dir(&drop).unwrap();
        fs::write(drop.join("copy.png"), b"duplicate me").unwrap();
        fs::write(drop.join("new.png"), b"something new").unwrap();
        fs::write(drop.join(".hidden.png"), b"duplicate me").unwrap();
        let found = call(
            &home,
            json!({"operation":"find_duplicates","paths":[drop],"bytes_base64":encode_base64(b"duplicate me"),"filename":"clip.png"}),
        )
        .unwrap();
        let results = found["results"].as_array().unwrap();
        assert_eq!(results.len(), 3);
        assert!(results[0]["path"].as_str().unwrap().ends_with("copy.png"));
        assert_eq!(results[0]["matches"][0]["asset_id"], id);
        assert_eq!(results[0]["matches"][0]["basis"], "blake3");
        assert_eq!(results[0]["matches"][0]["item"]["name"], "photo.png");
        assert_eq!(results[0]["matches"][0]["item"]["byte_size"], 12);
        assert_eq!(results[1]["matches"], json!([]));
        assert_eq!(results[2]["matches"][0]["asset_id"], id);
        assert!(call(&home, json!({"operation":"find_duplicates"})).is_err());
    }

    /// Replicas screen intake against their mirror: the authority here is unreachable.
    #[cfg(feature = "remote")]
    #[test]
    fn replica_finds_duplicates_in_its_mirror_without_the_authority() {
        let root = TempDir::new().unwrap();
        let original = root.path().join("photo.png");
        fs::write(&original, b"replica bytes").unwrap();
        let mut authority =
            Library::init(&root.path().join("authority"), "auth", None, None).unwrap();
        let asset = authority
            .add(&original, None, Metadata::default())
            .unwrap();
        let view = authority.inspect(&asset.id).unwrap();
        // The mirror remote::attach + sync leave behind, pointing at a closed port.
        let home = root.path().join("replica-home");
        let state = home.join("replicas").join("mirror");
        fs::create_dir_all(&state).unwrap();
        let state = state.canonicalize().unwrap();
        atomic_json(
            &state.join("library.json"),
            &json!({"protocol_version": PROTOCOL_VERSION, "library_id": authority.context.library_id, "storage": "replica", "writer_model": "remote_authority"}),
        )
        .unwrap();
        let context = Context {
            library_id: authority.context.library_id.clone(),
            state_dir: path_text(&state).unwrap(),
            original_root: None,
            remote: Some(Remote {
                url: "http://127.0.0.1:9".into(),
                token: "unused".into(),
                drive_roots: BTreeMap::new(),
            }),
        };
        let mut mirror = Library::connect(context.clone()).unwrap();
        let tx = mirror.conn.transaction().unwrap();
        crate::remote::store_view(&tx, &mirror.state_dir, &view).unwrap();
        tx.commit().unwrap();
        atomic_json(
            &home.join("config.json"),
            &Config {
                current: Some("mirror".into()),
                contexts: BTreeMap::from([("mirror".into(), context)]),
            },
        )
        .unwrap();
        let incoming = root.path().join("incoming.png");
        fs::write(&incoming, b"replica bytes").unwrap();
        let found = call(
            &home,
            json!({"operation":"find_duplicates","context":"mirror","paths":[incoming]}),
        )
        .unwrap();
        let matched = &found["results"][0]["matches"][0];
        assert_eq!(matched["asset_id"], asset.id);
        assert_eq!(matched["basis"], "blake3");
        assert_eq!(matched["item"]["name"], "photo.png");
    }
}
