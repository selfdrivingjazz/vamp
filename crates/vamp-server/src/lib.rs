//! HTTP/1.1 JSON API over one local library (the authority for replica contexts).
//! Every `/v1` request needs `Authorization: Bearer <token>` (the token lives in
//! `<state_dir>/server-token`, 0600) or, with Tailscale auth, an allowlisted tailnet
//! identity (see [`auth`]). Browser-extension origins get CORS.
pub mod auth;
pub mod health;
pub mod push;

use anyhow::{Context as _, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};
use vamp_core::Library;
use vamp_core::io::{Pool, Stalled};
use vamp_protocol::{Metadata, MetadataPatch, PROTOCOL_VERSION};
mod dist;

mod jobs_api;
mod open;

/// Threads that run original/cloud-folder filesystem calls for requests (see `stream`).
const IO_THREADS: usize = 8;
/// How long a request waits for an original without progress before answering 503.
pub const ORIGINAL_DEADLINE: Duration = Duration::from_secs(20);
/// `retry_after` seconds suggested with a 503 `original_unavailable`.
const RETRY_AFTER: u64 = 30;

/// Upper bound for bytes fetched by `/v1/clip` from `image_url`.
pub const MAX_CLIP: u64 = 64 * 1024 * 1024;
/// Upper bound for a raw `/v1/ingest` body.
pub const MAX_INGEST: u64 = 1024 * 1024 * 1024;
const MAX_JSON: u64 = 1024 * 1024;
/// Upper bound for a client-rendered provisional thumbnail (`POST /v1/assets/{id}/preview`).
const MAX_PREVIEW: u64 = 4 * 1024 * 1024;
/// Library-scoped native operations served by `POST /v1/native` (replicas forward these).
/// Excluded: operations taking authority filesystem paths. Exploration media is bounded.
const NATIVE_OPERATIONS: &[&str] = &[
    "browse",
    "similar",
    "embedding_status",
    "embedding_retry",
    "galaxy",
    "galaxy_search",
    "sidebar",
    "added_by",
    "collections",
    "inspect",
    "tags",
    "smart_folders",
    "update",
    "remove",
    "restore",
    "empty_trash",
    "touch",
    "collection_create",
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

/// Read the server token, creating a random one (0600) on first use.
pub fn token(state_dir: &Path) -> Result<String> {
    let path = state_dir.join("server-token");
    match fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => return Ok(text.trim().to_owned()),
        Ok(_) => bail!("empty server token file: {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&path)?;
    file.write_all(token.as_bytes())?;
    file.sync_all()?;
    Ok(token)
}

pub struct App {
    library: Mutex<Library>,
    token: String,
    name: String,
    helper: Option<PathBuf>,
    fetcher: ureq::Agent,
    dist: Option<PathBuf>,
    hub: push::Hub,
    tailnet: Option<auth::TailnetAuth>,
    /// Per asset/kind turn for on-demand generation (see `representation_path`).
    generating: Mutex<std::collections::HashMap<String, Arc<Mutex<()>>>>,
    /// Original/cloud-folder filesystem calls for requests: HTTP workers never make them.
    io: Arc<Pool>,
    io_deadline: Duration,
    health: Arc<health::Health>,
    watchdog: bool,
}

/// The library, with its hold time visible to `/v1/health`.
struct LibraryGuard<'a> {
    guard: std::sync::MutexGuard<'a, Library>,
    health: &'a health::Health,
}

impl std::ops::Deref for LibraryGuard<'_> {
    type Target = Library;
    fn deref(&self) -> &Library {
        &self.guard
    }
}

impl std::ops::DerefMut for LibraryGuard<'_> {
    fn deref_mut(&mut self) -> &mut Library {
        &mut self.guard
    }
}

impl Drop for LibraryGuard<'_> {
    fn drop(&mut self) {
        self.health.lock_since.store(0, Ordering::Relaxed);
    }
}

impl App {
    pub fn new(library: Library, name: &str) -> Result<Self> {
        let token = token(&library.state_dir)?;
        Ok(Self {
            library: Mutex::new(library),
            token,
            name: name.into(),
            hub: push::Hub::default(),
            tailnet: None,
            helper: vamp_core::media_helper(),
            fetcher: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(30)))
                .max_redirects(5)
                // Some image hosts (e.g. Wikimedia) reject requests without an identifying agent.
                .user_agent(concat!("vamp-server/", env!("CARGO_PKG_VERSION")))
                .build()
                .into(),
            dist: None,
            generating: Mutex::default(),
            io: Arc::new(Pool::new("requests", IO_THREADS)),
            io_deadline: ORIGINAL_DEADLINE,
            health: Arc::default(),
            watchdog: false,
        })
    }
    /// How long requests wait for an original without progress (default [`ORIGINAL_DEADLINE`]).
    pub fn with_io_deadline(mut self, deadline: Duration) -> Self {
        self.io_deadline = deadline;
        self
    }
    /// Exit the process (status [`health::WATCHDOG_EXIT`]) when HTTP workers stop making
    /// progress or every request I/O thread is wedged, so a supervisor restarts it.
    pub fn with_watchdog(mut self) -> Self {
        self.watchdog = true;
        self
    }
    /// Liveness shared with background loops (see [`spawn_jobs`]).
    pub fn health(&self) -> Arc<health::Health> {
        self.health.clone()
    }
    /// Serve `GET /install.sh` and `GET /dist/<file>` from `dir` without any authentication.
    pub fn with_dist(mut self, dir: PathBuf) -> Self {
        self.dist = Some(dir);
        self
    }
    /// Also admit allowlisted Tailscale identities (bearer tokens stay valid).
    pub fn with_tailnet(mut self, tailscale: auth::Tailscale) -> Self {
        let state_dir = self.lock().state_dir.clone();
        self.tailnet = Some(auth::TailnetAuth {
            tailscale,
            state_dir,
        });
        self
    }
    fn lock(&self) -> LibraryGuard<'_> {
        let guard = self.library.lock().unwrap_or_else(|e| e.into_inner());
        self.health
            .lock_since
            .store(self.health.http.now() + 1, Ordering::Relaxed);
        LibraryGuard {
            guard,
            health: &self.health,
        }
    }
}

/// Threads answering asset media requests (`/v1/assets/<id>/<kind>`), which wait on the I/O
/// pool; HTTP workers only hand them over, so catalog requests never queue behind them.
const MEDIA_THREADS: usize = 16;
/// Media requests waiting for a media thread beyond which new ones get 503 right away.
const MEDIA_QUEUE: usize = 64;

/// `GET /v1/assets/<id>/<kind>`: original/thumbnail/preview bytes (may wait on storage).
fn is_media(request: &Request) -> bool {
    let path = request.url().split('?').next().unwrap_or_default();
    *request.method() == Method::Get
        && matches!(
            path.trim_matches('/').split('/').collect::<Vec<_>>().as_slice(),
            ["v1", "assets", _, kind] if *kind != "similar"
        )
}

/// Serve with `threads` workers; returns after `server.unblock()` was called once per worker.
pub fn serve(server: Arc<Server>, app: App, threads: usize) {
    let app = Arc::new(app);
    push::spawn_watcher(Arc::downgrade(&app));
    let threads = threads.max(1);
    let first = app.health.http.add_workers(threads);
    if app.watchdog {
        spawn_watchdog(Arc::downgrade(&app));
    }
    let (media, queue) = std::sync::mpsc::sync_channel::<Request>(MEDIA_QUEUE);
    let queue = Arc::new(Mutex::new(queue));
    app.health.media_threads.store(MEDIA_THREADS, Ordering::Relaxed);
    let media_threads = (0..MEDIA_THREADS)
        .map(|_| {
            let (app, queue) = (app.clone(), queue.clone());
            std::thread::spawn(move || {
                loop {
                    let next = queue.lock().unwrap_or_else(|e| e.into_inner()).recv();
                    let Ok(request) = next else { return };
                    app.health.media_busy.fetch_add(1, Ordering::Relaxed);
                    handle(&app, request);
                    app.health.media_busy.fetch_sub(1, Ordering::Relaxed);
                }
            })
        })
        .collect::<Vec<_>>();
    let workers = (first..first + threads)
        .map(|worker| {
            let server = server.clone();
            let app = app.clone();
            let media = media.clone();
            std::thread::spawn(move || {
                while let Ok(request) = server.recv() {
                    if push::is_stream(&request) {
                        push::spawn_stream(app.clone(), request);
                    } else if is_media(&request) {
                        if let Err(std::sync::mpsc::TrySendError::Full(request)) =
                            media.try_send(request)
                        {
                            let _ = request.respond(error_reply(
                                503,
                                "all media threads are waiting on original storage",
                            ));
                        }
                    } else {
                        let progress = &app.health.http;
                        progress.begin(worker, request.url(), progress.now());
                        handle(&app, request);
                        progress.end(worker, progress.now());
                    }
                }
            })
        })
        .collect::<Vec<_>>();
    drop(media);
    for worker in workers {
        let _ = worker.join();
    }
    for thread in media_threads {
        let _ = thread.join();
    }
}

fn spawn_watchdog(app: std::sync::Weak<App>) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(5));
            let Some(app) = app.upgrade() else { return };
            let now = app.health.http.now();
            let io = app.io.stats();
            let reason = app
                .health
                .http
                .check(now, health::HTTP_STALL)
                .or_else(|| {
                    io.wedged(health::IO_WEDGE).then(|| {
                        format!("all {} request I/O threads stuck", io.threads)
                    })
                });
            if let Some(reason) = reason {
                eprintln!("vamp-server: watchdog: {reason}; exiting for restart");
                eprintln!(
                    "vamp-server: watchdog: io={} background={} subsystems={}",
                    serde_json::to_string(&io).unwrap_or_default(),
                    serde_json::to_string(&vamp_core::io::background().stats())
                        .unwrap_or_default(),
                    app.health.subsystems(now)
                );
                std::process::exit(health::WATCHDOG_EXIT);
            }
        }
    });
}

type Reply = Response<Box<dyn Read + Send>>;

struct HttpError {
    status: u16,
    message: String,
    embedding_unavailable: Option<vamp_core::EmbeddingUnavailable>,
}

impl HttpError {
    fn new(status: u16, message: String) -> Self {
        Self { status, message, embedding_unavailable: None }
    }

    fn reply(self) -> Reply {
        if let Some(unavailable) = self.embedding_unavailable {
            return json_reply(409, &json!({"error":unavailable.to_json()}));
        }
        error_reply(self.status, &self.message)
    }
}

fn status_for(error: &anyhow::Error) -> u16 {
    if vamp_core::EmbeddingUnavailable::from_error(error).is_some() {
        // Valid request, but this asset's current derived state cannot answer it yet.
        return 409;
    }
    let message = format!("{error:#}");
    if vamp_core::io::is_stalled(error) {
        503
    } else if message.contains("not found") || message.contains("representation unavailable") {
        404
    } else if message.contains("revision conflict") {
        409
    } else {
        400
    }
}

impl From<anyhow::Error> for HttpError {
    fn from(error: anyhow::Error) -> Self {
        Self {
            status: status_for(&error),
            message: format!("{error:#}"),
            embedding_unavailable: vamp_core::EmbeddingUnavailable::from_error(&error).cloned(),
        }
    }
}

impl From<Stalled> for HttpError {
    fn from(stalled: Stalled) -> Self {
        Self::new(503, stalled.to_string())
    }
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header")
}

fn boxed(data: Vec<u8>) -> Box<dyn Read + Send> {
    Box::new(std::io::Cursor::new(data))
}


/// JSON reply, gzip-encoded when the client accepts it (snapshots shrink ~10x).
fn compressed_json(request: &Request, value: &Value) -> Reply {
    let gzip = request.headers().iter().any(|h| {
        h.field.equiv("Accept-Encoding") && h.value.as_str().contains("gzip")
    });
    if !gzip {
        return json_reply(200, value);
    }
    let mut encoder =
        flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let body = serde_json::to_writer(&mut encoder, value)
        .map_err(std::io::Error::from)
        .and_then(|()| encoder.finish());
    let Ok(body) = body else {
        return json_reply(200, value);
    };
    let len = body.len();
    Response::new(
        StatusCode(200),
        vec![
            header("Content-Type", "application/json"),
            header("Content-Encoding", "gzip"),
        ],
        boxed(body),
        Some(len),
        None,
    )
}
fn json_reply(status: u16, value: &Value) -> Reply {
    let body = value.to_string().into_bytes();
    let len = body.len();
    Response::new(
        StatusCode(status),
        vec![header("Content-Type", "application/json")],
        boxed(body),
        Some(len),
        None,
    )
}

/// 409 for writes that need a clipped original still on its way (see `vamp_core::fetching`).
fn landed_conflict(error: anyhow::Error) -> HttpError {
    let message = format!("{error:#}");
    if message.contains("has landed") || message.contains("already stored") {
        HttpError::new(409, message)
    } else {
        error.into()
    }
}

fn error_code(status: u16) -> &'static str {
    match status {
        401 => "unauthorized",
        404 => "not_found",
        409 => "conflict",
        413 => "too_large",
        // Only stalled original/cloud-folder reads answer 503.
        503 => "original_unavailable",
        _ => "bad_request",
    }
}

fn error_reply(status: u16, message: &str) -> Reply {
    let code = error_code(status);
    if status == 503 {
        let mut reply = json_reply(
            status,
            &json!({"error":{"code":code,"message":message,"retry_after":RETRY_AFTER}}),
        );
        reply.add_header(header("Retry-After", &RETRY_AFTER.to_string()));
        return reply;
    }
    json_reply(status, &json!({"error":{"code":code,"message":message}}))
}

fn extension_origin(request: &Request) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Origin"))
        .map(|h| h.value.as_str().to_owned())
        .filter(|o| {
            [
                "chrome-extension://",
                "moz-extension://",
                "safari-web-extension://",
            ]
            .iter()
            .any(|p| o.starts_with(p))
        })
}

fn request_header<'a>(request: &'a Request, name: &'static str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str())
}

/// `Ok(Some(peer))` for a tailnet identity, `Ok(None)` for a valid bearer token.
fn authorized(app: &App, request: &Request) -> Result<Option<auth::Peer>, String> {
    let given = request_header(request, "Authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    if let Some(given) = given {
        let (a, b) = (given.as_bytes(), app.token.as_bytes());
        if a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0 {
            return Ok(None);
        }
    }
    match &app.tailnet {
        Some(tailnet) => tailnet
            .check(request.remote_addr().map(|a| a.ip()))
            .map(Some)
            .map_err(|e| format!("{e:#}")),
        None if given.is_some() => Err("invalid bearer token".into()),
        None => Err("missing or invalid bearer token".into()),
    }
}

pub(crate) fn handle(app: &Arc<App>, mut request: Request) {
    let origin = extension_origin(&request);
    let public = dist::route(app.dist.as_deref(), &request).or_else(|| open::route(&request));
    let mut reply = if let Some(reply) = public {
        reply
    } else if *request.method() == Method::Options {
        Response::new(
            StatusCode(204),
            vec![
                header("Access-Control-Allow-Methods", "GET, POST, DELETE, OPTIONS"),
                header(
                    "Access-Control-Allow-Headers",
                    "Authorization, Content-Type, Range",
                ),
                header("Access-Control-Max-Age", "600"),
            ],
            boxed(Vec::new()),
            Some(0),
            None,
        )
    } else {
        match authorized(app, &request) {
            Err(message) => error_reply(401, &message),
            // Authenticated clients may identify an agent, but cannot claim to be an
            // owner/device. This attribution does not grant any additional access.
            Ok(peer) => {
                let actor = request_header(&request, "X-Vamp-Actor")
                    .filter(|actor| vamp_core::is_agent(actor) && actor.len() <= 128 && actor.is_ascii() && !actor.chars().any(char::is_control))
                    .map(str::to_owned)
                    .or_else(|| peer.as_ref().map(peer_actor));
                match vamp_core::with_actor(actor, || route(app, &mut request, peer.as_ref())) {
                    Ok(reply) => reply,
                    Err(error) => error.reply(),
                }
            }
        }
    };
    if let Some(origin) = origin {
        reply.add_header(header("Access-Control-Allow-Origin", &origin));
        reply.add_header(header("Vary", "Origin"));
        reply.add_header(header(
            "Access-Control-Expose-Headers",
            "ETag, Content-Range",
        ));
    }
    let _ = request.respond(reply);
}

/// Event actor for a tailnet peer: `tailnet:<login>/<host>`, e.g. `tailnet:alice@example.com/laptop`.
fn peer_actor(peer: &auth::Peer) -> String {
    let host = peer.node.split('.').next().unwrap_or(&peer.node);
    format!("tailnet:{}/{host}", peer.login)
}

struct Query(Vec<(String, String)>);
impl Query {
    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
    fn number<T: std::str::FromStr>(&self, key: &str, default: T) -> Result<T, HttpError> {
        self.get(key)
            .map(|v| {
                v.parse()
                    .map_err(|_| HttpError::new(400, format!("invalid {key}")))
            })
            .transpose()
            .map(|v| v.unwrap_or(default))
    }
    fn tags(&self) -> Vec<String> {
        self.get("tags")
            .map(|t| {
                t.split(',')
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn read_body(request: &mut Request, limit: u64) -> Result<Vec<u8>, HttpError> {
    if request.body_length().is_some_and(|n| n as u64 > limit) {
        return Err(HttpError::new(413, format!("body exceeds {limit} bytes")));
    }
    let mut body = Vec::new();
    request
        .as_reader()
        .take(limit + 1)
        .read_to_end(&mut body)
        .map_err(|e| HttpError::new(400, e.to_string()))?;
    if body.len() as u64 > limit {
        return Err(HttpError::new(413, format!("body exceeds {limit} bytes")));
    }
    Ok(body)
}

fn read_json<T: for<'de> Deserialize<'de>>(request: &mut Request) -> Result<T, HttpError> {
    let body = read_body(request, MAX_JSON)?;
    serde_json::from_slice(&body).map_err(|e| HttpError::new(400, format!("invalid JSON body: {e}")))
}

fn ok(value: impl serde::Serialize) -> Result<Reply, HttpError> {
    Ok(json_reply(
        200,
        &serde_json::to_value(value).map_err(|e| HttpError::new(500, e.to_string()))?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateBody {
    name: Option<String>,
    metadata: Option<MetadataPatch>,
    expected_revision: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClipBody {
    url: String,
    title: Option<String>,
    image_url: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    note: Option<String>,
    /// Accepted for forward compatibility; page snapshots are not stored.
    #[allow(dead_code)]
    html: Option<String>,
    /// Folder the clip is filed into.
    collection_id: Option<String>,
    /// Pixel size of the image, when the clipper knows it (recorded at once).
    width: Option<u32>,
    height: Option<u32>,
}

/// `POST /v1/clip/batch`: items per request.
const MAX_CLIP_BATCH: usize = 200;
/// Image downloads one batch runs at once.
const CLIP_DOWNLOADS: usize = 6;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClipBatchBody {
    items: Vec<ClipBody>,
    /// Folder for items without their own `collection_id`.
    collection_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NameBody {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MembershipBody {
    asset_ids: Vec<String>,
    operation: String,
}

/// `POST /v1/assets/batch`: ids per request (replicas chunk larger change sets).
const MAX_BATCH: usize = 1000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchBody {
    ids: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PackBody {
    ids: Vec<String>,
    /// `grid` (400 px JPEG, else the thumbnail) or `micro`.
    kind: String,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct AcceptTagsBody {
    #[serde(default)]
    tags: Vec<String>,
    expected_revision: Option<u64>,
}

fn flag(query: &Query, key: &str) -> bool {
    matches!(query.get(key), Some("1" | "true"))
}

fn route(
    app: &Arc<App>,
    request: &mut Request,
    peer: Option<&auth::Peer>,
) -> Result<Reply, HttpError> {
    let url = request.url().to_owned();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let query = Query(
        form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect(),
    );
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let method = request.method().clone();
    match (&method, segments.as_slice()) {
        (Method::Get, ["v1", "library"]) => {
            let library = app.lock();
            ok(
                json!({"library_id":library.context.library_id,"protocol_version":PROTOCOL_VERSION,"name":app.name}),
            )
        }
        // Never takes the library lock: answers even when the catalog is wedged.
        (Method::Get, ["v1", "health"]) => ok(health_report(app)),
        (Method::Get, ["v1", "hello"]) => {
            let library = app.lock();
            ok(json!({
                "library_id": library.context.library_id,
                "name": app.name,
                "protocol_version": PROTOCOL_VERSION,
                "auth": if app.tailnet.is_some() { "tailscale" } else { "token" },
                "peer": peer,
                "cursor": library.last_event()?,
            }))
        }
        (Method::Get, ["v1", "snapshot"]) => {
            let snapshot = app.lock().snapshot()?;
            Ok(compressed_json(request, &snapshot))
        }
        (Method::Post, ["v1", "assets", "batch"]) => {
            let body: BatchBody = read_json(request)?;
            if body.ids.len() > MAX_BATCH {
                return Err(HttpError::new(400, format!("at most {MAX_BATCH} ids per batch")));
            }
            let library = app.lock();
            // Missing (tombstoned/unknown) ids are omitted; the caller treats them as deleted.
            let views = body
                .ids
                .iter()
                .filter_map(|id| library.inspect(id).ok())
                .collect::<Vec<_>>();
            drop(library);
            let value = serde_json::to_value(views).map_err(|e| HttpError::new(500, e.to_string()))?;
            Ok(compressed_json(request, &value))
        }
        (Method::Get, ["v1", "events", "stream"]) => {
            let after = query.number("after", 0u64)?;
            let wait = query.number("wait", 30u64)?.min(push::MAX_WAIT);
            let limit = query.number("limit", 500usize)?;
            if !app.hub.wait_past(after, Duration::from_secs(wait)) {
                return ok(Vec::<Value>::new());
            }
            ok(app.lock().events(after, limit)?)
        }
        (Method::Get, ["v1", "events"]) => ok(app.lock().events(
            query.number("after", 0u64)?,
            query.number("limit", 500usize)?,
        )?),
        (Method::Get, ["v1", "search"]) if flag(&query, "semantic") => {
            let library = app.lock();
            let limit = query.number("limit", 100usize)?;
            let text = query.get("q").unwrap_or("").trim();
            let added_by = query.get("added_by");
            // Without an embedder (or query) answer with FTS results and semantic:false.
            if text.is_empty() || library.embedder()?.is_none() {
                let results = library
                    .list(Some(text), limit, 0, added_by)?
                    .into_iter()
                    .map(|asset| json!({"asset":asset,"score":null}))
                    .collect::<Vec<_>>();
                return ok(json!({"semantic":false,"results":results}));
            }
            let results =
                library.semantic_search(text, limit, flag(&query, "hybrid"), added_by)?;
            ok(json!({"semantic":true,"results":results}))
        }
        (Method::Get, ["v1", "search"]) => ok(app.lock().list(
            query.get("q"),
            query.number("limit", 100usize)?,
            query.number("offset", 0usize)?,
            query.get("added_by"),
        )?),
        (Method::Get, ["v1", "tags"]) => ok(app
            .lock()
            .tag_counts()?
            .into_iter()
            .map(|(tag, count)| json!({"tag":tag,"count":count}))
            .collect::<Vec<_>>()),
        (Method::Get, ["v1", "assets", id]) => ok(app.lock().inspect(id)?),
        (Method::Get, ["v1", "assets", id, "similar"]) => {
            ok(app.lock().similar(id, query.number("limit", 40usize)?)?)
        }
        // A client-rendered thumbnail shown until a clipped original lands.
        (Method::Post, ["v1", "assets", id, "preview"]) => {
            let body = read_body(request, MAX_PREVIEW)?;
            ok(app
                .lock()
                .attach_provisional(id, &body)
                .map_err(landed_conflict)?)
        }
        // The client's bytes for a clipped original the server could not (or did not yet)
        // fetch; 409 once an original is stored.
        (Method::Post, ["v1", "assets", id, "original"]) => {
            let body = read_body(request, MAX_INGEST)?;
            let filename = query.get("filename").unwrap_or("clip");
            let asset = app
                .lock()
                .land_original(id, &body, filename)
                .map_err(landed_conflict)?;
            if app.helper.is_some() && visual_media(&asset.original.mime) {
                generate_in_background(app, &asset.id);
            }
            ok(asset)
        }
        (Method::Post, ["v1", "assets", id, "accept-tags"]) => {
            let body: AcceptTagsBody = read_json(request)?;
            ok(app
                .lock()
                .accept_tags(id, &body.tags, body.expected_revision)?)
        }
        (Method::Get, ["v1", "assets", id, "original"]) if flag(&query, "redirect") => {
            // Optional: S3-stored originals may be served directly by the bucket.
            match app.lock().presign_original(id, 900)? {
                Some(url) => Ok(Response::new(
                    StatusCode(302),
                    vec![header("Location", &url)],
                    boxed(Vec::new()),
                    Some(0),
                    None,
                )),
                None => stream(app, request, id, "original"),
            }
        }
        (Method::Get, ["v1", "assets", id, "tier", "loop"]) => {
            // Loop metadata is synced before playback. Never hydrate/generate on this hot path.
            if app.lock().fetch(id, "loop").is_err() {
                return Ok(json_reply(404, &json!({"error":{"code":"tier_missing","message":format!("no loop tier for {id} yet")}})));
            }
            stream(app, request, id, "loop")
        }
        (Method::Get, ["v1", "assets", id, "tier", tier]) => {
            // Generated tier files only: never an original, so never Drive.
            let path = vamp_core::thumbs::tier_path(&app.lock(), id, tier)
                .map_err(|e| HttpError::new(400, format!("{e:#}")))?;
            match path.and_then(|p| std::fs::read(p).ok()) {
                Some(data) => {
                    let len = data.len();
                    Ok(Response::new(
                        StatusCode(200),
                        vec![
                            header("Content-Type", "image/webp"),
                            header("Cache-Control", "private, max-age=3600"),
                        ],
                        boxed(data),
                        Some(len),
                        None,
                    ))
                }
                None => Ok(json_reply(
                    404,
                    &json!({"error":{"code":"tier_missing","message":format!("no {tier} tier for {id} yet")}}),
                )),
            }
        }
        (Method::Get, ["v1", "tiers", "stats"]) => ok(vamp_core::thumbs::tier_stats(&app.lock())?),
        // Embeddings pack for replicas' in-memory similarity browsing (see vamp_core::energy).
        (Method::Get, ["v1", "embeddings", "pack", "meta"]) => {
            let pack = app.lock().energy_pack()?;
            ok(match pack {
                Some((path, header)) => {
                    let mut meta = vamp_core::energy::describe(&path, &header);
                    meta.as_object_mut().map(|m| m.remove("path"));
                    meta
                }
                None => json!({ "count": 0, "version": null }),
            })
        }
        (Method::Get, ["v1", "embeddings", "pack"]) => {
            let library = app.lock();
            let pack = library.energy_pack()?;
            let Some((path, pack_header)) = pack else {
                return Ok(json_reply(404, &json!({"error":{"code":"no_embeddings","message":"the library has no embeddings"}})));
            };
            let etag = format!("\"{}\"", pack_header.version);
            if request_header(request, "If-None-Match").is_some_and(|value| value.split(',').any(|v| v.trim() == "*" || v.trim().trim_start_matches("W/") == etag)) {
                return Ok(Response::new(StatusCode(304), vec![header("ETag", &etag), header("Cache-Control", "private, no-cache")], boxed(Vec::new()), Some(0), None));
            }
            let data = std::fs::read(path).map_err(|e| HttpError::new(500, e.to_string()))?;
            // Another authority connection may have atomically published a newer pack.
            // Derive the response ETag/checkpoint from the actual bytes being returned.
            let current = vamp_core::energy::header_from_bytes(&data)?;
            let etag = format!("\"{}\"", current.version);
            let after = query.get("after").and_then(|s| s.parse::<u64>().ok());
            let compatible = after.is_some_and(|a| a > 0 && a <= current.cursor)
                && query.get("model") == Some(current.model.as_str())
                && query.get("dim").and_then(|s| s.parse::<usize>().ok()) == Some(current.dim)
                && query.get("palette_dim").and_then(|s| s.parse::<usize>().ok()) == Some(current.palette_dim);
            let delta = if compatible {
                query.get("version").map(|base| vamp_core::energy::delta_for(&library, &current, &data, after.unwrap(), base)).transpose()?
            } else { None };
            let delta = delta.map(|d| serde_json::to_vec(&d)).transpose().map_err(anyhow::Error::from)?;
            let (mime, body) = match delta {
                Some(delta) if delta.len() < data.len() => ("application/vnd.vamp.embeddings-delta+json", delta),
                _ => ("application/octet-stream", data),
            };
            let len = body.len();
            Ok(Response::new(StatusCode(200), vec![header("Content-Type", mime), header("ETag", &etag), header("Cache-Control", "private, no-cache")], boxed(body), Some(len), None).with_chunked_threshold(usize::MAX))
        }
        (Method::Get, ["v1", "assets", id, kind]) => stream(app, request, id, kind),
        (Method::Post, ["v1", "assets", id]) => {
            let body: UpdateBody = read_json(request)?;
            let mut patch = body.metadata.unwrap_or_default();
            if body.name.is_some() {
                patch.name = body.name;
            }
            if patch.name.as_deref().is_some_and(|n| n.trim().is_empty()) {
                return Err(HttpError::new(400, "name cannot be empty".into()));
            }
            ok(app.lock().update(id, patch, body.expected_revision)?)
        }
        (Method::Delete, ["v1", "assets", id]) => {
            let expected = query
                .get("expected_revision")
                .map(|v| v.parse::<u64>())
                .transpose()
                .map_err(|_| HttpError::new(400, "invalid expected_revision".into()))?;
            app.lock().remove(id, expected)?;
            ok(json!({"trashed":1}))
        }
        (Method::Post, ["v1", "ingest"]) => {
            let body = read_body(request, MAX_INGEST)?;
            let filename = query
                .get("filename")
                .ok_or_else(|| HttpError::new(400, "filename is required".into()))?;
            let metadata = Metadata {
                description: query
                    .get("note")
                    .or(query.get("description"))
                    .map(str::to_owned),
                tags: query.tags(),
                source_url: query.get("source_url").map(str::to_owned),
                ..Metadata::default()
            };
            ok(ingest(
                app,
                &body,
                filename,
                query.get("name"),
                metadata,
                query.get("collection_id"),
            )?)
        }
        (Method::Post, ["v1", "clip"]) => {
            let body: ClipBody = read_json(request)?;
            ok(clip(app, body)?)
        }
        (Method::Post, ["v1", "clip", "batch"]) => {
            let body: ClipBatchBody = read_json(request)?;
            ok(clip_batch(app, body)?)
        }
        (Method::Get, ["v1", "collections"]) => ok(app.lock().collections()?),
        (Method::Post, ["v1", "collections"]) => {
            let body: NameBody = read_json(request)?;
            ok(app.lock().collection_create(&body.name, None)?)
        }
        (Method::Post, ["v1", "thumbnails", "pack"]) => {
            let body: PackBody = read_json(request)?;
            if body.ids.len() > vamp_core::thumbs::MAX_PACK {
                return Err(HttpError::new(400, format!("at most {} ids per pack", vamp_core::thumbs::MAX_PACK)));
            }
            // Paths resolve under the lock; files are read after it is released.
            let items = vamp_core::thumbs::pack_items(&app.lock(), &body.ids, &body.kind)
                .map_err(|e| HttpError::new(400, format!("{e:#}")))?;
            let data = vamp_core::thumbs::pack_body(items);
            let len = data.len();
            Ok(Response::new(
                StatusCode(200),
                vec![header("Content-Type", "application/x-vamp-pack")],
                boxed(data),
                Some(len),
                None,
            ))
        }
        (Method::Post, ["v1", "native"]) => {
            // Base64 exploration findings may contain up to 64 MiB of media.
            let bytes = read_body(request, (vamp_core::native::MAX_MEDIA + 2) / 3 * 4 + MAX_JSON)?;
            let mut body: Value = serde_json::from_slice(&bytes)
                .map_err(|e| HttpError::new(400, format!("invalid JSON body: {e}")))?;
            let operation = body["operation"].as_str().unwrap_or_default().to_owned();
            if !NATIVE_OPERATIONS.contains(&operation.as_str()) {
                return Err(HttpError::new(400, format!("operation not available over HTTP: {operation}")));
            }
            if let Some(object) = body.as_object_mut() {
                // Library-scoped: the served library, never a client-chosen home/context.
                object.remove("home");
                object.remove("context");
            }
            let native: vamp_core::native::Request = serde_json::from_value(body)
                .map_err(|e| HttpError::new(400, format!("invalid request: {e}")))?;
            if operation == "hash" {
                // Reads originals: on the I/O pool, never under the library lock.
                return ok(hash_pending(app, native.limit.unwrap_or(100))?);
            }
            ok(vamp_core::native::dispatch(&mut app.lock(), native)?)
        }
        (Method::Post, ["v1", "collections", id]) => {
            let body: NameBody = read_json(request)?;
            ok(app.lock().collection_rename(id, &body.name)?)
        }
        (Method::Delete, ["v1", "collections", id]) => {
            app.lock().collection_delete(id)?;
            ok(json!({"deleted":id}))
        }
        (Method::Post, ["v1", "collections", id, "assets"]) => {
            let body: MembershipBody = read_json(request)?;
            let add = match body.operation.as_str() {
                "add" => true,
                "remove" => false,
                _ => return Err(HttpError::new(400, "operation must be add or remove".into())),
            };
            if body.asset_ids.is_empty() {
                return Err(HttpError::new(400, "asset_ids is required".into()));
            }
            let mut library = app.lock();
            let mut collection = None;
            for asset_id in &body.asset_ids {
                collection = Some(library.collection_membership(id, asset_id, add)?);
            }
            ok(collection)
        }
        (_, ["v1", "jobs", ..]) => jobs_api::route(app, &method, &segments, request)
            .unwrap_or_else(|| Err(HttpError::new(404, format!("no route for {method} {path}")))),
        _ => Err(HttpError::new(404, format!("no route for {method} {path}"))),
    }
}

/// Stores uploaded bytes, filed into `collection_id` when given, and answers as soon as the
/// asset is cataloged. Its thumbnail and preview render afterwards (see
/// [`generate_in_background`]), so an upload never waits on rendering or holds the library
/// lock for it.
fn ingest(
    app: &Arc<App>,
    bytes: &[u8],
    filename: &str,
    name: Option<&str>,
    metadata: Metadata,
    collection_id: Option<&str>,
) -> Result<vamp_protocol::Asset> {
    let asset = {
        let mut library = app.lock();
        if let Some(collection_id) = collection_id {
            library.ensure_collection(collection_id)?;
        }
        let mut asset = library.ingest_bytes(bytes, filename, metadata)?;
        if let Some(name) = name
            .map(str::trim)
            .filter(|n| !n.is_empty() && *n != asset.name)
        {
            asset = library.update(
                &asset.id,
                MetadataPatch {
                    name: Some(name.into()),
                    ..MetadataPatch::default()
                },
                Some(asset.revision),
            )?;
        }
        if let Some(collection_id) = collection_id {
            library.collection_add(collection_id, &asset.id)?;
        }
        asset
    };
    // Derivatives are optional: images and video posters use the same representation tiers.
    if app.helper.is_some() && visual_media(&asset.original.mime) {
        generate_in_background(app, &asset.id);
    }
    Ok(asset)
}

fn visual_media(mime: &str) -> bool {
    mime.starts_with("image/") || mime.starts_with("video/")
}

/// Renders a new asset's thumbnail, then preview, off the request. Each lands as a
/// `representation_ready` event, which replicas pick up on their next sync. A replica or
/// worker asking first gets it generated on demand; the per-asset turn in
/// [`representation_path`] keeps that from rendering twice.
fn generate_in_background(app: &Arc<App>, id: &str) {
    let (app, id) = (app.clone(), id.to_owned());
    std::thread::spawn(move || {
        let looping = app.lock().inspect(&id).is_ok_and(|view| {
            let mime = view.asset.original.mime;
            mime.starts_with("video/") || matches!(mime.as_str(), "image/gif" | "image/webp")
        });
        for kind in ["thumbnail", "preview"].into_iter().chain(looping.then_some("loop")) {
            if let Err(HttpError { message, .. }) = representation_path(&app, &id, kind) {
                if kind == "loop" && message.contains("static images do not have loop previews") { continue; }
                eprintln!("vamp-server: {kind} for {id} failed: {message}");
                break;
            }
        }
    });
}

fn web_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

fn image_extension(content_type: &str) -> Option<&'static str> {
    Some(match content_type.split(';').next()?.trim() {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/avif" => "avif",
        "image/heic" => "heic",
        "image/tiff" => "tiff",
        "image/bmp" => "bmp",
        _ => return None,
    })
}

/// An image fetched for a clip, and the filename it is stored under.
struct Download {
    bytes: Vec<u8>,
    filename: String,
}

fn check_clip(body: &ClipBody) -> Result<()> {
    ensure!(web_url(&body.url), "url must be http(s)");
    match &body.image_url {
        Some(image_url) => ensure!(web_url(image_url), "image_url must be http(s)"),
        None => ensure!(
            !body.url.contains(['\r', '\n']),
            "url cannot contain line breaks"
        ),
    }
    Ok(())
}

/// Downloads `image_url` without the catalog lock. The clipped page goes along as
/// `Referer`: hotlink-protected hosts refuse image requests without one.
fn download(app: &App, page_url: &str, image_url: &str) -> Result<Download> {
    let mut response = app
        .fetcher
        .get(image_url)
        .header("Referer", page_url)
        .call()
        .with_context(|| format!("image download failed: {image_url}"))?;
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let bytes = response
        .body_mut()
        .with_config()
        .limit(MAX_CLIP)
        .read_to_vec()
        .context("image download failed or exceeds 64 MiB")?;
    let last = url_filename(image_url);
    let has_ext = Path::new(last)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| {
            (1..=5).contains(&e.len()) && e.chars().all(|c| c.is_ascii_alphanumeric())
        });
    let filename = match (has_ext, image_extension(&content_type)) {
        (true, _) => last.to_owned(),
        (false, Some(ext)) => format!("clip.{ext}"),
        (false, None) => "clip".to_owned(),
    };
    Ok(Download { bytes, filename })
}

/// An image clip cataloged before its original: the background download to run.
struct PendingOriginal {
    id: String,
    page_url: String,
    image_url: String,
}

/// Last path segment of a URL (no query or fragment).
fn url_filename(url: &str) -> &str {
    url.split(['?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("")
}

/// Stores a checked clip. Bookmarks are stored whole; images are cataloged at once with
/// their original still to fetch (see `vamp_core::fetching`) and returned with the
/// download for [`resolve_originals`] to run.
fn store_clip(
    app: &Arc<App>,
    body: ClipBody,
    collection_id: Option<&str>,
) -> Result<(vamp_protocol::Asset, Option<PendingOriginal>)> {
    let title = body
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let mut metadata = Metadata {
        description: body.note.clone(),
        tags: body.tags.clone(),
        source_url: Some(body.url.clone()),
        ..Metadata::default()
    };
    metadata.extra.insert(
        "clip".into(),
        json!({"page_title":title,"image_url":body.image_url}),
    );
    let Some(image_url) = body.image_url else {
        let shortcut = format!("[InternetShortcut]\r\nURL={}\r\n", body.url);
        let asset = ingest(
            app,
            shortcut.as_bytes(),
            "bookmark.url",
            Some(title.unwrap_or(&body.url)),
            metadata,
            collection_id,
        )?;
        return Ok((asset, None));
    };
    let filename = url_filename(&image_url);
    let name = title
        .or(Some(filename).filter(|f| !f.is_empty()))
        .unwrap_or("Image")
        .to_owned();
    let size = body.width.zip(body.height);
    let asset = {
        let mut library = app.lock();
        if let Some(collection_id) = collection_id {
            library.ensure_collection(collection_id)?;
        }
        let asset = library.add_fetching(&name, &image_url, filename, metadata, size)?;
        if let Some(collection_id) = collection_id {
            library.collection_add(collection_id, &asset.id)?;
        }
        asset
    };
    let pending = PendingOriginal {
        id: asset.id.clone(),
        page_url: body.url,
        image_url,
    };
    Ok((asset, Some(pending)))
}

/// Downloads clipped originals off the request, [`CLIP_DOWNLOADS`] at a time, and lands
/// each (then its thumbnail and preview render); a failed download marks the asset failed
/// (`original_error`) so it is visible, and a client upload can still complete it.
fn resolve_originals(app: &Arc<App>, pending: Vec<PendingOriginal>) {
    if pending.is_empty() {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        let next = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..CLIP_DOWNLOADS.min(pending.len()) {
                scope.spawn(|| {
                    while let Some(item) = pending.get(next.fetch_add(1, Ordering::Relaxed)) {
                        resolve_original(&app, item);
                    }
                });
            }
        });
    });
}

fn resolve_original(app: &Arc<App>, item: &PendingOriginal) {
    let landed = download(app, &item.page_url, &item.image_url).and_then(|image| {
        app.lock()
            .land_original(&item.id, &image.bytes, &image.filename)
    });
    match landed {
        Ok(asset) => {
            if app.helper.is_some() && visual_media(&asset.original.mime) {
                generate_in_background(app, &asset.id);
            }
        }
        // The client may have uploaded the bytes meanwhile: then this is a no-op.
        Err(error) => {
            if let Err(e) = app.lock().fail_original(&item.id, &format!("{error:#}")) {
                eprintln!("vamp-server: original of {} failed: {e:#}", item.id);
            }
        }
    }
}

fn clip(app: &Arc<App>, body: ClipBody) -> Result<vamp_protocol::Asset> {
    check_clip(&body)?;
    let collection_id = body.collection_id.clone();
    let (asset, pending) = store_clip(app, body, collection_id.as_deref())?;
    resolve_originals(app, pending.into_iter().collect());
    Ok(asset)
}

/// `POST /v1/clip/batch` (a clipper's "Batch Save"): every item is cataloged at once, in
/// request order (images with their originals still to fetch); the downloads run after the
/// reply (see [`resolve_originals`]). An unknown folder fails the whole request; otherwise
/// each item answers `{asset}` or `{error:{code,message}}`; `instant: true` tells clients
/// the server works this way (and takes `width`/`height`).
fn clip_batch(app: &Arc<App>, body: ClipBatchBody) -> Result<Value, HttpError> {
    let ClipBatchBody {
        items,
        collection_id,
    } = body;
    if items.len() > MAX_CLIP_BATCH {
        return Err(HttpError::new(400, format!("at most {MAX_CLIP_BATCH} items per batch")));
    }
    {
        let library = app.lock();
        let folders = items
            .iter()
            .filter_map(|item| item.collection_id.as_ref())
            .chain(&collection_id);
        for folder in folders {
            library.ensure_collection(folder)?;
        }
    }
    let failure = |code: &str, error: anyhow::Error| {
        json!({"error":{"code":code,"message":format!("{error:#}")}})
    };
    let mut results = Vec::with_capacity(items.len());
    let mut pending = Vec::new();
    for item in items {
        let result = match check_clip(&item) {
            Err(error) => failure("bad_request", error),
            Ok(()) => {
                let folder = item.collection_id.clone().or_else(|| collection_id.clone());
                match store_clip(app, item, folder.as_deref()) {
                    Ok((asset, download)) => {
                        pending.extend(download);
                        json!({ "asset": asset })
                    }
                    Err(error) => failure(error_code(status_for(&error)), error),
                }
            }
        };
        results.push(result);
    }
    resolve_originals(app, pending);
    // `instant`: image clips are cataloged before their originals (clients probe it).
    Ok(json!({ "results": results, "instant": true }))
}

/// `GET /v1/health`: lock-free liveness. `status` is `degraded` when the request I/O pool
/// is saturated or an operation outlived its deadline (originals answer 503 meanwhile).
fn health_report(app: &App) -> Value {
    let now = app.health.http.now();
    let requests = app.io.stats();
    let background = vamp_core::io::background().stats();
    let stuck = |s: &vamp_core::io::PoolStats, deadline: Duration| {
        s.busy >= s.threads || s.oldest_ms() > deadline.as_millis() as u64
    };
    let degraded =
        stuck(&requests, app.io_deadline) || stuck(&background, vamp_core::io::DRIVE_DEADLINE);
    let pool = |s: &vamp_core::io::PoolStats| {
        json!({
            "threads": s.threads,
            "busy": s.busy,
            "queued": s.queued,
            "saturation": s.busy as f64 / s.threads as f64,
            "oldest_op_ms": s.oldest_ms(),
            "completed": s.completed,
            "stalled": s.stalled,
            "running": s.running,
        })
    };
    json!({
        "status": if degraded { "degraded" } else { "ok" },
        "uptime_ms": now,
        "http": app.health.http.report(now),
        "io": {"requests": pool(&requests), "background": pool(&background)},
        "subsystems": app.health.subsystems(now),
    })
}

/// Native `hash` without holding the library lock while reading: each original is hashed
/// on the request I/O pool; a stalled one ends the batch (the rest stay pending).
fn hash_pending(app: &App, limit: usize) -> Result<Value, HttpError> {
    if !(1..=10_000).contains(&limit) {
        return Err(HttpError::new(400, "limit must be 1–10000".into()));
    }
    let ids = app.lock().pending_hash_ids(limit)?;
    let (mut hashed, mut changed, mut failed) = (0, 0, Vec::new());
    for id in ids {
        let (job, before) = {
            let library = app.lock();
            (library.hash_job(&id)?, library.inspect(&id)?.asset.revision)
        };
        let Some(job) = job else { continue };
        let original = match app.io.run(format!("hash {id}"), app.io_deadline, move || job.run())
        {
            Ok(Ok(original)) => original,
            Ok(Err(e)) => {
                failed.push(json!({"asset_id":id,"message":format!("{e:#}")}));
                continue;
            }
            Err(stalled) => {
                failed.push(json!({"asset_id":id,"message":stalled.to_string()}));
                break;
            }
        };
        match app.lock().finish_hash(&id, original) {
            Ok(asset) if asset.revision != before => changed += 1,
            Ok(_) => hashed += 1,
            Err(e) => failed.push(json!({"asset_id":id,"message":format!("{e:#}")})),
        }
    }
    let remaining = app.lock().pending_hash_count()?;
    Ok(json!({"hashed":hashed,"changed":changed,"failed":failed,"remaining":remaining}))
}

/// Local path of an asset's thumbnail/preview. Workers and replicas may ask for one the
/// import never made: it is derived here, with only catalog reads/writes under the library
/// lock; reading (or hydrating) the original and rendering run on the request I/O pool, one
/// at a time per asset and kind. A caller that gives up (503) leaves the generation running;
/// it attaches the result itself, so a retry finds it.
fn representation_path(app: &Arc<App>, id: &str, kind: &str) -> Result<PathBuf, HttpError> {
    let error = match app.lock().fetch(id, kind) {
        Ok(path) => return Ok(path),
        Err(error) => error,
    };
    let Some(helper) = app.helper.clone().filter(|_| {
        matches!(kind, "thumbnail" | "preview" | "loop")
            && format!("{error:#}").contains("representation unavailable")
    }) else {
        return Err(error.into());
    };
    let key = format!("{id}/{kind}");
    let gate = app
        .generating
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(key.clone())
        .or_default()
        .clone();
    let worker = app.clone();
    let (id, kind) = (id.to_owned(), kind.to_owned());
    let generated = app.io.run_capped(
        format!("generate {kind} {id}"),
        app.io_deadline,
        Some(GENERATION_CAP),
        move || {
            let result = (|| {
                let _turn = gate.lock().unwrap_or_else(|e| e.into_inner());
                let prepared = worker.lock().prepare_generation(&id, &kind)?;
                if let vamp_core::Generation::Pending(job) = prepared {
                    let rendered = job.render(&helper)?;
                    worker.lock().attach_generated(rendered)?;
                }
                worker.lock().fetch(&id, &kind)
            })();
            worker
                .generating
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&key);
            result
        },
    )?;
    Ok(generated?)
}

/// Longest a request waits for on-demand generation that keeps making progress.
const GENERATION_CAP: Duration = Duration::from_secs(60);
/// Bytes per pooled original read.
const READ_CHUNK: usize = 256 * 1024;

/// Response body of an original: every read runs on the request I/O pool with a deadline,
/// so a cloud folder stalling mid-transfer aborts the response instead of parking the HTTP
/// worker. The first read shares the first-byte deadline with the open.
struct PooledReader {
    app: Arc<App>,
    label: String,
    file: Option<File>,
    seek: Option<u64>,
    remaining: u64,
    chunk: Vec<u8>,
    offset: usize,
    first_byte_by: Option<std::time::Instant>,
}

impl Read for PooledReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.offset >= self.chunk.len() {
            if self.remaining == 0 {
                return Ok(0);
            }
            let mut file = self
                .file
                .take()
                .ok_or_else(|| std::io::Error::other("original read abandoned"))?;
            let want = self.remaining.min(READ_CHUNK as u64) as usize;
            let seek = self.seek.take();
            let deadline = match self.first_byte_by.take() {
                Some(by) => by
                    .saturating_duration_since(std::time::Instant::now())
                    .max(Duration::from_secs(1)),
                None => self.app.io_deadline,
            };
            let (file, data) = self
                .app
                .io
                .run(self.label.clone(), deadline, move || {
                    let data = (|| {
                        if let Some(start) = seek {
                            file.seek(SeekFrom::Start(start))?;
                        }
                        let mut data = vec![0; want];
                        let n = file.read(&mut data)?;
                        data.truncate(n);
                        Ok::<_, std::io::Error>(data)
                    })();
                    (file, data)
                })
                .map_err(|stalled| {
                    std::io::Error::new(std::io::ErrorKind::TimedOut, stalled.to_string())
                })?;
            self.file = Some(file);
            self.chunk = data?;
            self.offset = 0;
            if self.chunk.is_empty() {
                // Shorter than its size said: end the (truncated) body.
                self.remaining = 0;
                return Ok(0);
            }
            self.remaining -= self.chunk.len() as u64;
            let progress = &self.app.health.http;
            progress.progress(progress.now());
        }
        let n = out.len().min(self.chunk.len() - self.offset);
        out[..n].copy_from_slice(&self.chunk[self.offset..self.offset + n]);
        self.offset += n;
        Ok(n)
    }
}

/// Opens an original on the request I/O pool: (file, size), 503 when storage stalls.
fn open_original(
    app: &App,
    id: &str,
    lookup: vamp_core::OriginalLookup,
) -> Result<(File, u64), HttpError> {
    let opened = app.io.run(format!("open original {id}"), app.io_deadline, move || {
        let path = lookup.fetch()?;
        let file = File::open(&path)
            .with_context(|| format!("original unavailable: {}", path.display()))?;
        let meta = file.metadata()?;
        ensure!(meta.is_file(), "original must be a regular file");
        Ok((file, meta.len()))
    })?;
    opened.map_err(|e| {
        if e.downcast_ref::<std::io::Error>().is_some() {
            HttpError::new(404, format!("{e:#}"))
        } else {
            e.into()
        }
    })
}

fn stream(app: &Arc<App>, request: &Request, id: &str, kind: &str) -> Result<Reply, HttpError> {
    // Only catalog reads happen under the library lock (and on this thread); the original
    // itself is opened and read on the I/O pool.
    let (lookup, etag, mime) = if kind == "original" {
        let library = app.lock();
        let original = library.inspect(id)?.asset.original;
        // A clipped original still downloading: retry later; one that failed: say why.
        if vamp_core::fetching::awaiting_original(&original) {
            if original.storage == vamp_core::fetching::FETCHING {
                return Err(HttpError::new(503, format!("original of {id} is still being fetched")));
            }
            let asset = library.inspect(id)?.asset;
            let reason = asset.metadata.extra.get(vamp_core::fetching::ORIGINAL_ERROR);
            return Err(HttpError::new(404, format!("original of {id} could not be fetched: {}", reason.and_then(Value::as_str).unwrap_or("unknown error"))));
        }
        // Pending (lazy) fingerprints use their surrogate key as the entity tag.
        let etag = original.source_key();
        (Ok(library.original_lookup(id)?), etag, original.mime)
    } else if kind == "loop" {
        let library = app.lock();
        let path = library.fetch(id, "loop")?;
        // A representation's unique persisted path is its version; no per-range hashing.
        let etag = blake3::hash(path.to_string_lossy().as_bytes()).to_hex().to_string();
        (Err(path), etag, "video/mp4".into())
    } else {
        // Representations live in the state dir (never a cloud folder): read them here.
        let path = representation_path(app, id, kind)?;
        let mut hasher = blake3::Hasher::new();
        hasher
            .update_reader(File::open(&path).map_err(|e| HttpError::new(404, e.to_string()))?)
            .map_err(|e| HttpError::new(500, e.to_string()))?;
        let mime = match path.extension().and_then(|e| e.to_str()) {
            Some("png") => "image/png",
            Some("webp") => "image/webp",
            _ => "image/jpeg",
        };
        (Err(path), hasher.finalize().to_hex().to_string(), mime.to_owned())
    };
    let etag = format!("\"{etag}\"");
    if request_header(request, "If-None-Match") == Some(etag.as_str()) {
        return Ok(Response::new(
            StatusCode(304),
            vec![header("ETag", &etag)],
            boxed(Vec::new()),
            Some(0),
            None,
        ));
    }
    let (mut file, size, first_byte_by) = match lookup {
        Ok(lookup) => {
            let by = std::time::Instant::now() + app.io_deadline;
            let (file, size) = open_original(app, id, lookup)?;
            (file, size, Some(by))
        }
        Err(path) => {
            let file = File::open(&path).map_err(|e| HttpError::new(404, e.to_string()))?;
            let size = file
                .metadata()
                .map_err(|e| HttpError::new(500, e.to_string()))?
                .len();
            (file, size, None)
        }
    };
    let mut headers = vec![
        header("Content-Type", &mime),
        header("ETag", &etag),
        header("Accept-Ranges", "bytes"),
        header("Cache-Control", if kind == "loop" { "private, max-age=3600" } else { "private, max-age=31536000, immutable" }),
    ];
    let range = request_header(request, "Range").and_then(|r| parse_range(r, size));
    let (status, start, len) = match range {
        Some(Ok((start, end))) => {
            headers.push(header(
                "Content-Range",
                &format!("bytes {start}-{end}/{size}"),
            ));
            (206, start, end - start + 1)
        }
        Some(Err(())) => {
            return Ok(Response::new(
                StatusCode(416),
                vec![header("Content-Range", &format!("bytes */{size}"))],
                boxed(Vec::new()),
                Some(0),
                None,
            ));
        }
        None => (200, 0, size),
    };
    let body: Box<dyn Read + Send> = match first_byte_by {
        Some(by) => Box::new(PooledReader {
            app: app.clone(),
            label: format!("read original {id}"),
            file: Some(file),
            seek: Some(start),
            remaining: len,
            chunk: Vec::new(),
            offset: 0,
            first_byte_by: Some(by),
        }),
        None => {
            file.seek(SeekFrom::Start(start))
                .map_err(|e| HttpError::new(500, e.to_string()))?;
            Box::new(file.take(len))
        }
    };
    Ok(Response::new(
        StatusCode(status),
        headers,
        body,
        Some(len as usize),
        None,
    )
    // These bodies have an exact length. Preserve Content-Length for progressive
    // players, including ranges larger than tiny_http's default 32 KiB threshold.
    .with_chunked_threshold(usize::MAX))
}

/// Single `bytes=a-b`, `bytes=a-` or `bytes=-n` range; `None` ignores the header.
fn parse_range(value: &str, size: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = value.strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let range = match (a.trim(), b.trim()) {
        ("", n) => {
            let n: u64 = n.parse().ok()?;
            if n == 0 || size == 0 {
                return Some(Err(()));
            }
            (size.saturating_sub(n), size - 1)
        }
        (a, "") => (a.parse().ok()?, size.saturating_sub(1)),
        (a, b) => (
            a.parse().ok()?,
            b.parse::<u64>().ok()?.min(size.saturating_sub(1)),
        ),
    };
    Some(if range.0 <= range.1 && range.0 < size {
        Ok(range)
    } else {
        Err(())
    })
}

/// Drain processor jobs (captions, tags, embeddings) on a separate catalog connection,
/// like `vamp jobs watch`. `run_jobs` takes the library's exclusive worker lock per batch,
/// so an external `vamp jobs watch` simply wins the lock and this worker keeps waiting.
/// Original reads run on the background I/O pool: a stalled cloud folder defers the item
/// (retried later) instead of blocking the loop. Beats `jobs` on `health` per pass.
pub fn spawn_jobs(
    home: PathBuf,
    context: String,
    interval: Duration,
    health: Arc<health::Health>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut library = match Library::open(&home, Some(&context)) {
            Ok(library) => library,
            Err(e) => return eprintln!("vamp-server: job worker disabled: {e:#}"),
        };
        let mut last_error = String::new();
        loop {
            health.beat("jobs");
            match library.run_jobs(8) {
                Ok(result) => {
                    last_error.clear();
                    let done = |key: &str| result.get(key).and_then(Value::as_u64).unwrap_or(0);
                    if done("deferred") > 0 {
                        eprintln!(
                            "vamp-server: jobs deferred={} (original storage not responding)",
                            done("deferred")
                        );
                    }
                    if done("completed") + done("failed") > 0 {
                        eprintln!(
                            "vamp-server: jobs completed={} failed={}",
                            done("completed"),
                            done("failed")
                        );
                        continue;
                    }
                }
                Err(e) => {
                    let message = format!("{e:#}");
                    if message != last_error {
                        eprintln!("vamp-server: job worker waiting: {message}");
                        last_error = message;
                    }
                }
            }
            // Micro/grid thumbnails for new or re-thumbnailed assets (CPU-bound; batched so
            // jobs keep their turn).
            match vamp_core::thumbs::pending(&library, 128) {
                Ok(batch) if !batch.is_empty() => {
                    let rendered = vamp_core::thumbs::render_all(&batch);
                    if let Err(e) = vamp_core::thumbs::store(&mut library, &rendered) {
                        eprintln!("vamp-server: grid thumbnails: {e:#}");
                    } else {
                        continue;
                    }
                }
                Ok(_) => {}
                Err(e) => eprintln!("vamp-server: grid thumbnails: {e:#}"),
            }
            // Hires (2048 px) tiers once grids are done: at most 2 original reads at once on
            // the background I/O pool, none while a stalled one lingers; a stalled Drive
            // defers the asset (retried later). All-busy passes wait out the interval.
            match vamp_core::thumbs::pending_hires(&library, 4) {
                Ok(batch) if !batch.is_empty() => {
                    let outcomes = vamp_core::thumbs::render_hires_all(&batch);
                    match vamp_core::thumbs::store_hires(&mut library, &batch, &outcomes) {
                        Ok(0) => {}
                        Ok(_) => continue,
                        Err(e) => eprintln!("vamp-server: hires tiers: {e:#}"),
                    }
                }
                Ok(_) => {}
                Err(e) => eprintln!("vamp-server: hires tiers: {e:#}"),
            }
            std::thread::sleep(interval);
        }
    })
}
