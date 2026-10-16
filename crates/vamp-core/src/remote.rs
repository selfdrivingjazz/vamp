//! Replica contexts: a local mirror catalog of a `vamp-server` authority.
//!
//! The mirror uses the ordinary catalog schema so browse/inspect/collections run
//! offline. It is written only by `sync`; writes are forwarded to the authority.
//! Thumbnails download in the background (`download_thumbnails`), previews and
//! originals on first request.
use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;

const PAGE: usize = 1000;
/// Events a replica may lag before it re-bootstraps from a snapshot instead of replaying.
const SNAPSHOT_LAG: u64 = 5000;
/// Asset ids per `POST /assets/batch`.
const BATCH: usize = 500;
/// Concurrent thumbnail pack requests, and thumbnails per pack.
const THUMBNAIL_WORKERS: usize = 8;
const PACK_CHUNK: usize = 32;
use crate::eagle::THUMB_ATTEMPTS;
use serde::Deserialize;

/// Blocking HTTP client for one authority's `/v1` API.
#[derive(Clone)]
pub struct Client {
    agent: ureq::Agent,
    base: String,
    token: String,
}

type Response = ureq::http::Response<ureq::Body>;

/// Body read bound for ordinary requests (JSON, thumbnail packs): no infinite hangs.
const BODY_TIMEOUT: Duration = Duration::from_secs(30);
/// Body read bound for bulk responses (the bootstrap snapshot).
const BULK_TIMEOUT: Duration = Duration::from_secs(600);
/// A download that receives no bytes this long is abandoned (see [`Client::download_with`]).
pub const STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Slack past a long-poll's `wait` before its answer counts as lost (see [`Client::wait_events`]).
const STREAM_GRACE: Duration = Duration::from_secs(10);

/// A non-2xx authority answer. `code` is the authority's `error.code` (e.g.
/// `original_unavailable` for a 503 while its Drive stalls).
#[derive(Debug)]
pub struct RemoteStatus {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for RemoteStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.status == 401 {
            return write!(f, "remote rejected credentials (401): {}", self.message);
        }
        write!(f, "remote error {}", self.status)?;
        if !self.code.is_empty() {
            write!(f, " ({})", self.code)?;
        }
        write!(f, ": {}", self.message)
    }
}

impl std::error::Error for RemoteStatus {}

/// A download that received no bytes for the stall timeout.
#[derive(Debug)]
pub struct Stalled(pub Duration);

impl std::fmt::Display for Stalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "download stalled: no data for {}s", self.0.as_secs())
    }
}

impl std::error::Error for Stalled {}

/// Failures worth retrying after a pause (authority overloaded or its storage stalled,
/// network trouble, timeouts) as opposed to answers that will not change (404, bad data).
pub fn is_transient(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        if let Some(status) = cause.downcast_ref::<RemoteStatus>() {
            return matches!(status.status, 408 | 429 | 500..=599);
        }
        if let Some(error) = cause.downcast_ref::<ureq::Error>() {
            return matches!(
                error,
                ureq::Error::Timeout(_)
                    | ureq::Error::Io(_)
                    | ureq::Error::ConnectionFailed
                    | ureq::Error::HostNotFound
                    | ureq::Error::BodyStalled
                    | ureq::Error::Protocol(_)
            );
        }
        cause.is::<Stalled>() || cause.is::<std::io::Error>()
    })
}

/// Progress, cancellation and priority of one download request (shared with whoever asked
/// for it; several downloads may share one, e.g. a `fetch_grid` batch).
#[derive(Default)]
pub struct Transfer {
    bytes: std::sync::atomic::AtomicU64,
    /// Content-Length + 1 (0 = unknown).
    total: std::sync::atomic::AtomicU64,
    cancel: std::sync::atomic::AtomicBool,
    lane: std::sync::Mutex<Lane>,
}

/// Downloads running under a [`Transfer`] and whether they yield to foreground ones; one
/// lock so [`FOREGROUND`] stays exact when a transfer is promoted mid-download.
#[derive(Default)]
struct Lane {
    active: usize,
    background: bool,
}

/// Foreground downloads running in this process. Background downloads stop reading their
/// socket while it is non-zero (TCP backpressure pauses the authority's send), so the item
/// being shown gets the whole link and no background bytes are lost.
static FOREGROUND: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl Transfer {
    /// A transfer that yields to foreground downloads (prefetch, bulk tiers).
    pub fn background() -> Self {
        let transfer = Self::default();
        transfer.lane().background = true;
        transfer
    }
    pub fn bytes(&self) -> u64 {
        self.bytes.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn total(&self) -> Option<u64> {
        self.total
            .load(std::sync::atomic::Ordering::Relaxed)
            .checked_sub(1)
    }
    pub fn cancel(&self) {
        self.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn is_background(&self) -> bool {
        self.lane().background
    }
    /// Promotes (`false`) or demotes (`true`) this transfer, including running downloads.
    pub fn set_background(&self, background: bool) {
        use std::sync::atomic::Ordering::SeqCst;
        let mut lane = self.lane();
        if lane.background != background && lane.active > 0 {
            if background {
                FOREGROUND.fetch_sub(lane.active, SeqCst);
            } else {
                FOREGROUND.fetch_add(lane.active, SeqCst);
            }
        }
        lane.background = background;
    }
    /// A background transfer while a foreground download runs.
    fn held(&self) -> bool {
        self.is_background() && FOREGROUND.load(std::sync::atomic::Ordering::SeqCst) > 0
    }
    fn lane(&self) -> std::sync::MutexGuard<'_, Lane> {
        self.lane.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn begin(&self) {
        let mut lane = self.lane();
        lane.active += 1;
        if !lane.background {
            FOREGROUND.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    fn end(&self) {
        let mut lane = self.lane();
        lane.active -= 1;
        if !lane.background {
            FOREGROUND.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

enum Chunk {
    Head(Option<u64>),
    Data(Vec<u8>),
    Done,
    Failed(anyhow::Error),
}

fn safe_id(id: &str) -> Result<&str> {
    ensure!(
        !id.is_empty()
            && id.len() <= 64
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "invalid identifier: {id}"
    );
    Ok(id)
}

impl Client {
    pub fn new(remote: &Remote) -> Result<Self> {
        ensure!(
            remote.url.starts_with("http://") || remote.url.starts_with("https://"),
            "remote url must be http:// or https://"
        );
        // An empty token relies on the authority's Tailscale identity auth.
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_recv_response(Some(Duration::from_secs(120)))
            .timeout_recv_body(Some(BODY_TIMEOUT))
            .build()
            .into();
        Ok(Self {
            agent,
            base: format!("{}/v1", remote.url.trim_end_matches('/')),
            token: remote.token.trim().to_owned(),
        })
    }
    fn authorize<B>(&self, request: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
        if self.token.is_empty() {
            request
        } else {
            request.header("Authorization", format!("Bearer {}", self.token))
        }
    }
    fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Response> {
        self.get_bounded(path, query, BODY_TIMEOUT)
    }
    fn get_bounded(&self, path: &str, query: &[(&str, String)], body: Duration) -> Result<Response> {
        let mut request = self.authorize(self.agent.get(format!("{}{path}", self.base)));
        for (key, value) in query {
            request = request.query(*key, value);
        }
        request
            .config()
            .timeout_recv_body(Some(body))
            .build()
            .call()
            .with_context(|| format!("remote unavailable: {}", self.base))
    }
    fn checked(mut response: Response) -> Result<Response> {
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(response);
        }
        let body: Value = response
            .body_mut()
            .with_config()
            .limit(1024 * 1024)
            .read_json()
            .unwrap_or(Value::Null);
        Err(RemoteStatus {
            status,
            code: body["error"]["code"].as_str().unwrap_or("").to_owned(),
            message: body["error"]["message"]
                .as_str()
                .unwrap_or("request failed")
                .to_owned(),
        }
        .into())
    }
    fn json<T: DeserializeOwned>(response: Response) -> Result<T> {
        Ok(Self::checked(response)?
            .into_body()
            .with_config()
            .limit(256 * 1024 * 1024)
            .read_json()?)
    }
    pub fn get_json<T: DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> Result<T> {
        Self::json(self.get(path, query)?)
    }
    /// `None` when the authority answers 404.
    pub fn get_optional<T: DeserializeOwned>(&self, path: &str) -> Result<Option<T>> {
        Self::optional(self.get(path, &[])?)
    }
    /// [`Self::get_optional`] for bulk bodies (the snapshot), read for up to 10 minutes.
    fn get_optional_bulk<T: DeserializeOwned>(&self, path: &str) -> Result<Option<T>> {
        Self::optional(self.get_bounded(path, &[], BULK_TIMEOUT)?)
    }
    fn optional<T: DeserializeOwned>(response: Response) -> Result<Option<T>> {
        if response.status().as_u16() == 404 {
            return Ok(None);
        }
        Self::json(response).map(Some)
    }
    /// `GET` answered within `limit` overall (status panels must not wait on a busy
    /// authority); `None` on 404.
    pub fn get_quick<T: DeserializeOwned>(&self, path: &str, limit: Duration) -> Result<Option<T>> {
        Self::optional(
            self.authorize(self.agent.get(format!("{}{path}", self.base)))
                .config()
                .timeout_global(Some(limit))
                .build()
                .call()
                .with_context(|| format!("remote unavailable: {}", self.base))?,
        )
    }
    pub fn post_json<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T> {
        Self::json(
            self.authorize(self.agent.post(format!("{}{path}", self.base)))
                .send_json(body)
                .with_context(|| format!("remote unavailable: {}", self.base))?,
        )
    }
    /// POST JSON, read a binary body (thumbnail packs).
    pub fn post_bytes(&self, path: &str, body: &Value) -> Result<Vec<u8>> {
        let response = self
            .authorize(self.agent.post(format!("{}{path}", self.base)))
            .send_json(body)
            .with_context(|| format!("remote unavailable: {}", self.base))?;
        Ok(Self::checked(response)?
            .into_body()
            .with_config()
            .limit(512 * 1024 * 1024)
            .read_to_vec()?)
    }
    pub fn delete(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        let mut request = self.authorize(self.agent.delete(format!("{}{path}", self.base)));
        for (key, value) in query {
            request = request.query(*key, value);
        }
        Self::json(
            request
                .call()
                .with_context(|| format!("remote unavailable: {}", self.base))?,
        )
    }
    pub fn library(&self) -> Result<Value> {
        self.get_json("/library", &[])
    }
    /// `GET /v1/hello`: library id/name and the identity the authority admitted us as.
    pub fn hello(&self) -> Result<Value> {
        self.get_json("/hello", &[])
    }
    /// Long-poll `GET /v1/events/stream`: returns once events after `after` exist (a page
    /// like `GET /v1/events`, up to [`PAGE`]) or `wait` seconds pass (then the list is
    /// empty). The answer is due after `wait` seconds, so it gets [`STREAM_GRACE`] more
    /// instead of the agent's 120 s: a connection that died silently fails fast.
    pub fn wait_events(&self, after: u64, wait: u64) -> Result<Vec<Event>> {
        let response = self
            .authorize(self.agent.get(format!("{}/events/stream", self.base)))
            .query("after", after.to_string())
            .query("wait", wait.to_string())
            .query("limit", PAGE.to_string())
            .config()
            .timeout_recv_response(Some(Duration::from_secs(wait) + STREAM_GRACE))
            .build()
            .call()
            .with_context(|| format!("remote unavailable: {}", self.base))?;
        Self::json(response)
    }
    /// Library-scoped native operation on the authority (`POST /v1/native`).
    pub fn native(&self, body: &Value) -> Result<Value> {
        self.post_json("/native", body)
    }
    pub fn update(&self, id: &str, patch: &MetadataPatch, expected: Option<u64>) -> Result<Asset> {
        self.post_json(
            &format!("/assets/{}", safe_id(id)?),
            &json!({"metadata":patch,"expected_revision":expected}),
        )
    }
    pub fn remove(&self, id: &str, expected: Option<u64>) -> Result<()> {
        let query = expected
            .map(|r| vec![("expected_revision", r.to_string())])
            .unwrap_or_default();
        self.delete(&format!("/assets/{}", safe_id(id)?), &query)?;
        Ok(())
    }
    /// Upload new bytes; the authority stores them in its inbox and references them.
    pub fn ingest(&self, bytes: &[u8], filename: &str, patch: &MetadataPatch) -> Result<Asset> {
        let mut request = self
            .authorize(self.agent.post(format!("{}/ingest", self.base)))
            .content_type("application/octet-stream")
            .query("filename", filename);
        if let Some(name) = &patch.name {
            request = request.query("name", name);
        }
        if let Some(description) = &patch.description {
            request = request.query("description", description);
        }
        if let Some(url) = &patch.source_url {
            request = request.query("source_url", url);
        }
        if let Some(tags) = &patch.tags {
            request = request.query("tags", tags.join(","));
        }
        Self::json(
            request
                .send(bytes)
                .with_context(|| format!("remote unavailable: {}", self.base))?,
        )
    }
    pub fn collection_rename(&self, id: &str, name: &str) -> Result<Collection> {
        self.post_json(
            &format!("/collections/{}", safe_id(id)?),
            &json!({"name":name}),
        )
    }
    pub fn collection_delete(&self, id: &str) -> Result<()> {
        self.delete(&format!("/collections/{}", safe_id(id)?), &[])?;
        Ok(())
    }
    /// `(asset_id, score)` ranked by the authority's embedder; `semantic` is false when
    /// the authority has none and answered with full-text matches.
    pub fn semantic_search(
        &self,
        text: &str,
        limit: usize,
        hybrid: bool,
        added_by: Option<&str>,
    ) -> Result<(bool, Vec<Ranked>)> {
        let mut query = vec![
            ("q", text.to_owned()),
            ("limit", limit.to_string()),
            ("semantic", "1".into()),
            ("hybrid", if hybrid { "1" } else { "0" }.into()),
        ];
        if let Some(added_by) = added_by {
            query.push(("added_by", added_by.to_owned()));
        }
        let value: Value = self.get_json("/search", &query)?;
        Ok((value["semantic"] == true, ranked(&value["results"])))
    }
    pub fn similar(&self, id: &str, limit: usize) -> Result<Vec<Ranked>> {
        let value: Value = self.get_json(
            &format!("/assets/{}/similar", safe_id(id)?),
            &[("limit", limit.to_string())],
        )?;
        Ok(ranked(&value))
    }
    pub fn accept_tags(&self, id: &str, tags: &[String], expected: Option<u64>) -> Result<Asset> {
        self.post_json(
            &format!("/assets/{}/accept-tags", safe_id(id)?),
            &json!({"tags":tags,"expected_revision":expected}),
        )
    }
    pub fn collection_create(&self, name: &str) -> Result<Collection> {
        self.post_json("/collections", &json!({"name":name}))
    }
    pub fn collection_membership(
        &self,
        id: &str,
        asset_ids: &[String],
        add: bool,
    ) -> Result<Collection> {
        self.post_json(
            &format!("/collections/{}/assets", safe_id(id)?),
            &json!({"asset_ids":asset_ids,"operation":if add {"add"} else {"remove"}}),
        )
    }
    /// Stream a representation to `dest` atomically; verify blake3 when known.
    pub fn download(&self, path: &str, dest: &Path, expected_blake3: Option<&str>) -> Result<()> {
        self.download_with(path, dest, expected_blake3, &Transfer::default(), STALL_TIMEOUT)
    }
    /// [`Self::download`] reporting progress into `transfer` and abandoned when it is
    /// cancelled or no bytes (headers included) arrive for `stall`. The socket is read on a
    /// helper thread, so a stalled authority never blocks the caller past `stall`. A
    /// background `transfer` neither starts nor reads while a foreground download runs
    /// (time held does not count as stalled).
    pub fn download_with(
        &self,
        path: &str,
        dest: &Path,
        expected_blake3: Option<&str>,
        transfer: &Transfer,
        stall: Duration,
    ) -> Result<()> {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering::{Relaxed, SeqCst};
        use std::sync::mpsc::{RecvTimeoutError, sync_channel};
        let parent = dest.parent().context("missing cache directory")?;
        fs::create_dir_all(parent)?;
        transfer.begin();
        /// Ends the transfer's lane and releases a held helper thread, however we return.
        struct Running<'a>(&'a Transfer, Arc<AtomicBool>);
        impl Drop for Running<'_> {
            fn drop(&mut self) {
                self.1.store(true, SeqCst);
                self.0.end();
            }
        }
        let (hold, gone) = (Arc::new(AtomicBool::new(transfer.held())), Arc::new(AtomicBool::new(false)));
        let _running = Running(transfer, gone.clone());
        let (tx, rx) = sync_channel::<Chunk>(8);
        let request = self
            .authorize(self.agent.get(format!("{}{path}", self.base)))
            .config()
            // Bounds the helper thread if the authority stalls forever after we give up.
            .timeout_recv_body(Some(Duration::from_secs(6 * 3600)))
            .build();
        let base = self.base.clone();
        let helper_hold = hold.clone();
        std::thread::Builder::new()
            .name("replica-download".into())
            .spawn(move || {
                // Held: wait (no request, or no socket reads) until released or abandoned.
                let wait = || {
                    while helper_hold.load(SeqCst) {
                        if gone.load(SeqCst) {
                            return false;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    !gone.load(SeqCst)
                };
                if !wait() {
                    return;
                }
                let response = request
                    .call()
                    .with_context(|| format!("remote unavailable: {base}"))
                    .and_then(Self::checked);
                let response = match response {
                    Ok(response) => response,
                    Err(e) => {
                        let _ = tx.send(Chunk::Failed(e));
                        return;
                    }
                };
                let total = response
                    .headers()
                    .get("content-length")
                    .and_then(|v| v.to_str().ok()?.parse().ok());
                if tx.send(Chunk::Head(total)).is_err() {
                    return;
                }
                let mut reader = response.into_body().into_reader();
                loop {
                    if !wait() {
                        return;
                    }
                    let mut buffer = vec![0u8; 256 * 1024];
                    let chunk = match reader.read(&mut buffer) {
                        Ok(0) => Chunk::Done,
                        Ok(n) => {
                            buffer.truncate(n);
                            Chunk::Data(buffer)
                        }
                        Err(e) => Chunk::Failed(e.into()),
                    };
                    let last = !matches!(chunk, Chunk::Data(_));
                    if tx.send(chunk).is_err() || last {
                        return;
                    }
                }
            })?;
        let temp = parent.join(format!(".{}.part", id()));
        let result = (|| -> Result<()> {
            let mut out = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            let mut hasher = blake3::Hasher::new();
            let mut last = std::time::Instant::now();
            loop {
                ensure!(!transfer.cancelled(), "download cancelled");
                let held = transfer.held();
                hold.store(held, SeqCst);
                if held {
                    last = std::time::Instant::now();
                }
                match rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(Chunk::Head(total)) => {
                        transfer.total.store(total.map_or(0, |t: u64| t + 1), Relaxed);
                        last = std::time::Instant::now();
                    }
                    Ok(Chunk::Data(data)) => {
                        hasher.update(&data);
                        out.write_all(&data)?;
                        transfer.bytes.fetch_add(data.len() as u64, Relaxed);
                        last = std::time::Instant::now();
                    }
                    Ok(Chunk::Done) => break,
                    Ok(Chunk::Failed(e)) => return Err(e),
                    Err(RecvTimeoutError::Timeout) => {
                        if last.elapsed() >= stall {
                            return Err(Stalled(stall).into());
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => anyhow::bail!("download ended early"),
                }
            }
            out.sync_all()?;
            if let Some(expected) = expected_blake3 {
                ensure!(
                    hasher.finalize().to_hex().as_str() == expected,
                    "downloaded bytes do not match the catalog hash"
                );
            }
            fs::rename(&temp, dest)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
}

impl Library {
    pub fn remote(&self) -> Option<&Remote> {
        self.context.remote.as_ref()
    }
    /// Client for this replica's authority; errors for local authority catalogs.
    pub fn remote_client(&self) -> Result<Client> {
        Client::new(self.remote().context("context is not a remote replica")?)
    }
}

/// Register a replica context mirroring the authority at `url`.
/// Verifies the token (empty = Tailscale identity auth), creates an empty mirror catalog;
/// run `sync` to populate.
pub fn attach_remote(home: &Path, name: &str, url: &str, token: &str) -> Result<Context> {
    attach(
        home,
        name,
        Remote {
            url: join_url(url),
            token: token.trim().to_owned(),
            drive_roots: BTreeMap::new(),
        },
    )
}

/// Authority URL for `vamp join`: a full `http(s)://` URL, or `host[:port]` (default 7777).
pub fn join_url(target: &str) -> String {
    let target = target.trim().trim_end_matches('/');
    if target.starts_with("http://") || target.starts_with("https://") {
        return target.to_owned();
    }
    let has_port = match target.rsplit_once(':') {
        // Bracketed IPv6 (`[::1]:7777`) or host:port; a bare IPv6 has several colons.
        Some((host, port)) => {
            port.parse::<u16>().is_ok() && (host.ends_with(']') || !host.contains(':'))
        }
        None => false,
    };
    if has_port {
        format!("http://{target}")
    } else if target.contains(':') && !target.starts_with('[') {
        format!("http://[{target}]:7777")
    } else {
        format!("http://{target}:7777")
    }
}

/// `vamp join`: attach a replica without a token (the authority admits this machine by
/// its Tailscale identity). `name` defaults to the authority's context name.
pub fn join(
    home: &Path,
    target: &str,
    name: Option<&str>,
    drive_roots: BTreeMap<String, String>,
) -> Result<(String, Context)> {
    let remote = Remote {
        url: join_url(target),
        token: String::new(),
        drive_roots,
    };
    let hello = Client::new(&remote)?.hello()?;
    let name = match name {
        Some(name) => name.to_owned(),
        None => hello["name"]
            .as_str()
            .context("authority did not report a library name; pass --name")?
            .to_owned(),
    };
    let context = attach(home, &name, remote)?;
    Ok((name, context))
}

fn attach(home: &Path, name: &str, remote: Remote) -> Result<Context> {
    validate_name(name)?;
    let url = remote.url.clone();
    let info = Client::new(&remote)?.library()?;
    ensure!(
        info["protocol_version"] == PROTOCOL_VERSION,
        "unsupported remote protocol version"
    );
    let library_id = info["library_id"]
        .as_str()
        .context("remote library has no identity")?
        .to_owned();
    let home = destination(home)?;
    let _lock = config_lock(&home)?;
    let mut config = load_config(&home)?;
    if let Some(existing) = config.contexts.get(name) {
        // Re-attaching the same authority resumes (an interrupted attach syncs on).
        ensure!(
            existing.library_id == library_id
                && existing.remote.as_ref().is_some_and(|r| r.url == url),
            "context already exists: {name}"
        );
        drop(_lock);
        return Ok(Library::open(&home, Some(name))?.context);
    }
    let state = home.join("replicas").join(id());
    fs::create_dir_all(&state)?;
    let state = state.canonicalize()?;
    let context = Context {
        library_id: library_id.clone(),
        state_dir: path_text(&state)?,
        original_root: None,
        remote: Some(remote),
    };
    atomic_json(
        &state.join("library.json"),
        &json!({"protocol_version": PROTOCOL_VERSION, "library_id": library_id, "storage": "replica", "writer_model": "remote_authority", "display_name": name, "remote_url": url}),
    )?;
    Library::connect(context.clone())?;
    config
        .contexts
        .insert(name.into(), stored_context(&home, &context)?);
    if config.current.is_none() {
        config.current = Some(name.into());
    }
    atomic_json(&home.join("config.json"), &config)?;
    Ok(context)
}

/// Removes a replica context and its mirror (catalog and caches: nothing that is not on
/// the authority). Local-authority contexts are refused.
pub fn detach(home: &Path, name: &str) -> Result<()> {
    let home = destination(home)?;
    let state = {
        let library = Library::open(&home, Some(name));
        let _lock = config_lock(&home)?;
        let mut config = load_config(&home)?;
        let context = config
            .contexts
            .remove(name)
            .with_context(|| format!("context not found: {name}"))?;
        ensure!(context.remote.is_some(), "only server libraries can be removed here");
        if config.current.as_deref() == Some(name) {
            config.current = config.contexts.keys().next().cloned();
        }
        atomic_json(&home.join("config.json"), &config)?;
        // A half-attached mirror may not open; its state dir is still under replicas/.
        library.map_or_else(
            |_| {
                let dir = PathBuf::from(&context.state_dir);
                if dir.is_relative() { home.join(dir) } else { dir }
            },
            |l| l.state_dir.clone(),
        )
    };
    if state.starts_with(home.join("replicas")) || state.starts_with(home.canonicalize()?.join("replicas")) {
        fs::remove_dir_all(&state)?;
    }
    Ok(())
}

/// Asset id and optional similarity score from a `[{asset, score}]` response.
pub type Ranked = (String, Option<f64>);
fn ranked(results: &Value) -> Vec<Ranked> {
    results
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| Some((r["asset"]["id"].as_str()?.to_owned(), r["score"].as_f64())))
        .collect()
}

fn cache_path(kind: &str, asset_id: &str, source_hash: &str, mime: &str) -> String {
    let ext = match mime {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "bin",
    };
    let hash = &source_hash[..source_hash.len().min(16)];
    format!("cache/{kind}/{asset_id}-{hash}.{ext}")
}

fn cursor(conn: &Connection) -> Result<u64> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS replica_state(key TEXT PRIMARY KEY, value INTEGER NOT NULL)",
    )?;
    let value: Option<i64> = conn
        .query_row(
            "SELECT value FROM replica_state WHERE key='cursor'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(value.unwrap_or(0) as u64)
}

fn apply_view(tx: &Transaction, state: &Path, id: &str, view: Option<&AssetView>) -> Result<()> {
    let Some(view) = view else {
        tx.execute("UPDATE assets SET deleted=1 WHERE id=?1", [id])?;
        tx.execute("DELETE FROM eagle_thumbnails WHERE asset_id=?1", [id])?;
        unindex(tx, id)?;
        return Ok(());
    };
    ensure!(view.asset.id == id, "remote returned a different asset");
    store_view(tx, state, view)?;
    reindex(tx, &view.asset)
}

/// Mirrors assets the authority just created for this replica (`add`/`ingest`), so they
/// show without waiting for a sync. Representations are left out: the next sync replays
/// the creation and `representation_ready` events and stores the full views. Ids already
/// mirrored (the authority answered with an existing asset) are left alone.
pub(crate) fn record_created(library: &mut Library, assets: &[Asset]) -> Result<()> {
    let state = library.state_dir.clone();
    let tx = library.conn.transaction()?;
    for asset in assets {
        let mirrored = tx
            .query_row("SELECT 1 FROM assets WHERE id=?1", [&asset.id], |_| Ok(()))
            .optional()?
            .is_some();
        if mirrored {
            continue;
        }
        let view = AssetView {
            asset: asset.clone(),
            representations: Vec::new(),
            observations: Vec::new(),
            suggested_tags: Vec::new(),
        };
        apply_view(&tx, &state, &asset.id, Some(&view))?;
    }
    tx.commit()?;
    Ok(())
}

/// Upserts an asset's document, current observations and representations (paths mapped
/// into the replica cache), and queues its thumbnail download when not cached yet.
/// Full-text indexing is the caller's job.
pub(crate) fn store_view(tx: &Transaction, state: &Path, view: &AssetView) -> Result<()> {
    let asset = &view.asset;
    let id = asset.id.as_str();
    // Evicted originals have no authority path; the SQL uniqueness key must stay distinct.
    let key = if asset.original.path.is_empty() {
        format!("cloud-only:{id}")
    } else {
        asset.original.path.clone()
    };
    tx.prepare_cached(
        "INSERT INTO assets(id,path,hash,deleted,document) VALUES(?1,?2,?3,0,?4) ON CONFLICT(id) DO UPDATE SET path=excluded.path,hash=excluded.hash,deleted=0,document=excluded.document",
    )?
    .execute(params![asset.id, key, asset.original.source_key(), json_text(asset)?])?;
    tx.prepare_cached("DELETE FROM observations WHERE asset_id=?1")?
        .execute([id])?;
    for observation in &view.observations {
        tx.prepare_cached(
            "INSERT OR REPLACE INTO observations(id,asset_id,source_hash,document) VALUES(?1,?2,?3,?4)",
        )?
        .execute(params![observation.id, id, observation.source_hash, json_text(observation)?])?;
    }
    tx.prepare_cached("DELETE FROM representations WHERE asset_id=?1")?
        .execute([id])?;
    let mut thumbnail = None;
    for rep in &view.representations {
        let rep = Representation {
            path: cache_path(&rep.kind, id, &rep.source_hash, &rep.mime),
            ..rep.clone()
        };
        tx.prepare_cached(
            "INSERT INTO representations(asset_id,kind,source_hash,document) VALUES(?1,?2,?3,?4)",
        )?
        .execute(params![id, rep.kind, rep.source_hash, json_text(&rep)?])?;
        if rep.kind == "thumbnail" && rep.source_hash == asset.original.source_key() {
            thumbnail = Some(rep.path);
        }
    }
    match thumbnail {
        Some(path) if !local_representation(state, &path)?.is_file() => {
            // Newest first by default; `prioritize_thumbnails` moves visible ones ahead.
            let order = chrono::DateTime::parse_from_rfc3339(&asset.created_at)
                .map_or(0, |t| t.timestamp_millis());
            tx.prepare_cached(
                "INSERT INTO eagle_thumbnails(asset_id,kind,path,revision,order_key) VALUES(?1,'download',?2,0,?3) ON CONFLICT(asset_id) DO UPDATE SET kind='download',path=excluded.path,order_key=excluded.order_key,attempts=0,error=NULL",
            )?
            .execute(params![id, path, order])?;
        }
        _ => {
            tx.prepare_cached("DELETE FROM eagle_thumbnails WHERE asset_id=?1")?
                .execute([id])?;
        }
    }
    Ok(())
}

fn apply_collections(tx: &Transaction, collections: &[Collection]) -> Result<()> {
    tx.execute("DELETE FROM memberships", [])?;
    tx.execute("DELETE FROM collections", [])?;
    for collection in collections {
        crate::catalog::insert_collection(tx, collection)?;
        for asset_id in &collection.asset_ids {
            tx.execute(
                "INSERT OR IGNORE INTO memberships(collection_id,asset_id) SELECT ?1,id FROM assets WHERE id=?2 AND deleted=0",
                params![collection.id, asset_id],
            )?;
        }
    }
    Ok(())
}

/// Mirrors the authority's smart folders and tag groups (library-level, not per asset).
fn apply_organization(tx: &Transaction, smart: Option<&Value>, tags: Option<&Value>) -> Result<()> {
    if let Some(smart) = smart {
        tx.execute("DELETE FROM smart_folders", [])?;
        for folder in smart.as_array().into_iter().flatten() {
            let mut folder = folder.clone();
            if let Some(object) = folder.as_object_mut() {
                object.remove("count");
            }
            let folder: SmartFolder = serde_json::from_value(folder)?;
            crate::catalog::save_smart_folder(tx, &folder)?;
        }
    }
    if let Some(tags) = tags {
        tx.execute("DELETE FROM tag_groups", [])?;
        let groups: Vec<TagGroup> = serde_json::from_value(tags["groups"].clone())?;
        for (index, group) in groups.iter().enumerate() {
            tx.execute(
                "INSERT INTO tag_groups(id,sort_order,document) VALUES(?1,?2,?3)",
                params![group.id, index as i64, json_text(group)?],
            )?;
        }
        let starred: Vec<String> = serde_json::from_value(tags["starred"].clone())?;
        // Absent and empty are equivalent; keep the row only when it says something.
        if starred.is_empty() {
            tx.execute("DELETE FROM settings WHERE key='starred_tags'", [])?;
        } else {
            crate::catalog::set_setting(tx, "starred_tags", &starred)?;
        }
    }
    Ok(())
}

/// Authority catalog state from `GET /v1/snapshot` (see `Library::snapshot`).
#[derive(Deserialize)]
struct Snapshot {
    library_id: String,
    cursor: u64,
    assets: Vec<AssetView>,
    collections: Vec<Collection>,
    smart_folders: Value,
    tags: Value,
}

/// Sync progress: `phase` is `snapshot` (downloading), `applying` (snapshot rows written)
/// or `events` (incremental replay); `done`/`total` count assets or events.
pub type Progress<'a> = &'a mut dyn FnMut(&str, u64, u64);

fn set_cursor(tx: &Transaction, cursor: u64) -> Result<()> {
    tx.execute(
        "INSERT INTO replica_state(key,value) VALUES('cursor',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [cursor as i64],
    )?;
    Ok(())
}

/// Replaces the mirror with an authority snapshot in one transaction (FTS rebuilt once).
/// Assets absent from the snapshot are tombstoned. `None` for authorities without the route.
fn bootstrap(library: &mut Library, client: &Client, progress: Progress) -> Result<Option<u64>> {
    progress("snapshot", 0, 0);
    let Some(snapshot) = client.get_optional_bulk::<Snapshot>("/snapshot")? else {
        return Ok(None);
    };
    ensure!(
        snapshot.library_id == library.context.library_id,
        "remote library identity differs from replica"
    );
    let total = snapshot.assets.len() as u64;
    let state = library.state_dir.clone();
    let tx = library.conn.transaction()?;
    tx.execute_batch(
        "DELETE FROM search; DELETE FROM asset_tags; DELETE FROM memberships; UPDATE assets SET deleted=1;",
    )?;
    for (done, view) in snapshot.assets.iter().enumerate() {
        safe_id(&view.asset.id)?;
        store_view(&tx, &state, view)?;
        index(&tx, &view.asset)?;
        if done % 250 == 0 {
            progress("applying", done as u64, total);
        }
    }
    tx.execute(
        "DELETE FROM eagle_thumbnails WHERE asset_id IN (SELECT id FROM assets WHERE deleted=1)",
        [],
    )?;
    apply_collections(&tx, &snapshot.collections)?;
    apply_organization(&tx, Some(&snapshot.smart_folders), Some(&snapshot.tags))?;
    set_cursor(&tx, snapshot.cursor)?;
    tx.commit()?;
    progress("applying", total, total);
    Ok(Some(snapshot.cursor))
}

/// Current views for `ids`, one `POST /assets/batch` per chunk; absent ids were deleted.
/// Falls back to one `GET /assets/{id}` each against authorities without the batch route.
fn fetch_views(client: &Client, ids: &[&str]) -> Result<BTreeMap<String, AssetView>> {
    let mut views = BTreeMap::new();
    for chunk in ids.chunks(BATCH) {
        match client.post_json::<Vec<AssetView>>("/assets/batch", &json!({"ids": chunk})) {
            Ok(batch) => views.extend(batch.into_iter().map(|v| (v.asset.id.clone(), v))),
            Err(e) if format!("{e:#}").contains("remote error 4") => {
                for id in chunk {
                    if let Some(view) =
                        client.get_optional::<AssetView>(&format!("/assets/{}", safe_id(id)?))?
                    {
                        views.insert((*id).to_owned(), view);
                    }
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(views)
}

/// Pull authority changes after the stored cursor. A new (cursor 0) or far-behind replica
/// bootstraps from a snapshot, then replays the events after it; each event page fetches
/// every changed asset once, except assets whose only events are views carrying their time
/// (`media_viewed` with `payload.last_viewed_at`: applied in place, no round trip).
/// Thumbnails are only queued (see `download_thumbnails`). Returns `{applied, cursor,
/// snapshot, pending_thumbnails, kinds, asset_ids}`; `applied` counts events consumed
/// (snapshot assets for a bootstrap), `kinds` counts replayed events by kind and
/// `asset_ids` lists the assets they touched (null past [`CHANGED_IDS`]), so clients can
/// update what they show in place instead of reloading.
pub fn sync(library: &mut Library) -> Result<Value> {
    sync_with(library, &mut |_, _, _| {})
}

/// Also keeps the mirrored embeddings pack fresh (best effort; see `energy::replica`).
pub fn sync_with(library: &mut Library, progress: Progress) -> Result<Value> {
    let value = pull(library, progress, true, None)?;
    crate::energy::replica::refresh_after_sync(library, value["applied"].as_u64().unwrap_or(0), false);
    Ok(value)
}

/// `sync` that only replays events, never bootstrapping from a snapshot (the reference
/// a snapshot-bootstrapped mirror must match).
pub fn replay(library: &mut Library) -> Result<Value> {
    pull(library, &mut |_, _, _| {}, false, None)
}

/// `first`: events after cursor `.0` the caller already holds (a long-poll's answer),
/// replayed instead of asking `GET /events` for them again; ignored once the cursor moved.
fn pull(
    library: &mut Library,
    progress: Progress,
    bootstrap_allowed: bool,
    first: Option<(u64, Vec<Event>)>,
) -> Result<Value> {
    let client = library.remote_client()?;
    let mut after = cursor(&library.conn)?;
    let mut applied = 0u64;
    let mut snapshot = false;
    let page = |after: u64| -> Result<Vec<Event>> {
        client.get_json(
            "/events",
            &[("after", after.to_string()), ("limit", PAGE.to_string())],
        )
    };
    let mut events = match first {
        Some((from, events)) if from == after => events,
        _ => page(after)?,
    };
    let mut total = events.len() as u64;
    if bootstrap_allowed && (events.len() == PAGE || (after == 0 && !events.is_empty())) {
        let latest = client.hello()?["cursor"].as_u64();
        if let Some(latest) = latest {
            total = latest.saturating_sub(after);
        }
        if (after == 0 || total > SNAPSHOT_LAG)
            && let Some(cursor) = bootstrap(library, &client, &mut *progress)?
        {
            snapshot = true;
            applied = library.conn.query_row(
                "SELECT count(*) FROM assets WHERE deleted=0",
                [],
                |r| r.get::<_, i64>(0),
            )? as u64;
            after = cursor;
            events = page(after)?;
            total = events.len() as u64;
        }
    }
    let mut replayed = 0u64;
    let mut kinds: BTreeMap<String, u64> = BTreeMap::new();
    let mut changed: BTreeSet<String> = BTreeSet::new();
    while let Some(last) = events.last().map(|e| e.sequence) {
        ensure!(
            events
                .iter()
                .all(|e| e.library_id == library.context.library_id),
            "remote library identity differs from replica"
        );
        progress("events", replayed, total.max(replayed));
        // A view only moves `last_viewed_at`; its event carries the time (newer authorities),
        // so assets that were only viewed need no view fetch.
        let mut viewed: BTreeMap<&str, &str> = BTreeMap::new();
        let mut fetch: BTreeSet<&str> = BTreeSet::new();
        for event in &events {
            *kinds.entry(event.kind.clone()).or_default() += 1;
            let Some(id) = event.asset_id.as_deref() else {
                continue;
            };
            if changed.len() <= CHANGED_IDS {
                changed.insert(id.to_owned());
            }
            match event.payload["last_viewed_at"].as_str() {
                Some(at) if event.kind == "media_viewed" => {
                    viewed.insert(id, at);
                }
                _ => {
                    fetch.insert(id);
                }
            }
        }
        let ids: Vec<&str> = fetch.into_iter().collect();
        let views = fetch_views(&client, &ids)?;
        let collections: Option<Vec<Collection>> = events
            .iter()
            .any(|e| e.kind.starts_with("collection"))
            .then(|| client.get_json("/collections", &[]))
            .transpose()?;
        let smart: Option<Value> = events
            .iter()
            .any(|e| e.kind == "smart_folder_updated")
            .then(|| client.native(&json!({"operation":"smart_folders"})))
            .transpose()?;
        let tags: Option<Value> = events
            .iter()
            .any(|e| e.kind == "tag_groups_updated")
            .then(|| client.native(&json!({"operation":"tags"})))
            .transpose()?;
        let state = library.state_dir.clone();
        let tx = library.conn.transaction()?;
        for id in &ids {
            apply_view(&tx, &state, id, views.get(*id))?;
        }
        for (id, at) in viewed.iter().filter(|(id, _)| ids.binary_search(id).is_err()) {
            apply_viewed(&tx, id, at)?;
        }
        if let Some(collections) = &collections {
            apply_collections(&tx, collections)?;
        }
        apply_organization(&tx, smart.as_ref(), tags.as_ref())?;
        set_cursor(&tx, last)?;
        tx.commit()?;
        replayed += events.len() as u64;
        after = last;
        if events.len() < PAGE {
            break;
        }
        events = page(after)?;
    }
    if !snapshot {
        applied = replayed;
    }
    let added_by_mirrored = mirror_added_by(library, &client);
    progress("events", replayed, total.max(replayed));
    Ok(json!({
        "applied": applied,
        "cursor": after,
        "snapshot": snapshot,
        "pending_thumbnails": library.pending_eagle_thumbnails()?,
        "kinds": kinds,
        "asset_ids": (changed.len() <= CHANGED_IDS).then_some(changed),
        "added_by_mirrored": added_by_mirrored,
    }))
}

/// Replica state dirs that asked their authority for `added_by` in this process.
static ADDED_BY_ASKED: std::sync::LazyLock<std::sync::Mutex<BTreeSet<PathBuf>>> =
    std::sync::LazyLock::new(Default::default);

/// Mirrors cataloged before `added_by` existed: copies the authority's adders (native op
/// `added_by`) onto the assets that lack one, once (`added_by_mirrored` setting), instead
/// of re-downloading their documents. Best effort, asked at most once per process: an
/// authority predating the op fails it and the next launch asks again. Returns the
/// assets updated.
fn mirror_added_by(library: &mut Library, client: &Client) -> u64 {
    let pending = || -> Result<bool> {
        Ok(library.conn.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM settings WHERE key='added_by_mirrored') \
             AND EXISTS(SELECT 1 FROM assets INDEXED BY assets_added_by WHERE added_by IS NULL AND deleted=0)",
            [],
            |r| r.get(0),
        )?)
    };
    if !pending().unwrap_or(false)
        || !ADDED_BY_ASKED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(library.state_dir.clone())
    {
        return 0;
    }
    let mut apply = || -> Result<u64> {
        let value = client.native(&json!({"operation":"added_by"}))?;
        let actors: BTreeMap<String, Vec<String>> =
            serde_json::from_value(value["actors"].clone())?;
        let tx = library.conn.transaction()?;
        let mut updated = 0u64;
        {
            let mut stmt = tx.prepare(&format!(
                "UPDATE assets SET document=json_set(document,'$.metadata.extra.{ADDED_BY}',?2) WHERE id=?1 AND added_by IS NULL"
            ))?;
            for (actor, ids) in &actors {
                for id in ids {
                    updated += stmt.execute(params![id, actor])? as u64;
                }
            }
        }
        crate::catalog::set_setting(
            &tx,
            "added_by_mirrored",
            &json!({ "assets": updated }),
        )?;
        tx.commit()?;
        Ok(updated)
    };
    match apply() {
        Ok(updated) => updated,
        Err(error) => {
            if is_transient(&error) {
                ADDED_BY_ASKED
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&library.state_dir);
            }
            0
        }
    }
}

/// Most asset ids a sync result lists (beyond, clients reload instead of patching).
const CHANGED_IDS: usize = 1000;

/// Records an authority view (`media_viewed`) in the mirror: the document's
/// `last_viewed_at` only, as the authority's `touch` does (no reindex). Assets not
/// mirrored (yet) are skipped; their creation brings the whole view.
fn apply_viewed(tx: &Transaction, id: &str, at: &str) -> Result<()> {
    let Some(mut asset) = tx
        .query_row("SELECT document FROM assets WHERE id=?1", [id], |r| r.get::<_, String>(0))
        .optional()?
        .map(parse::<Asset>)
        .transpose()?
    else {
        return Ok(());
    };
    asset.last_viewed_at = Some(at.to_owned());
    tx.execute(
        "UPDATE assets SET document=?2 WHERE id=?1",
        params![id, json_text(&asset)?],
    )?;
    Ok(())
}

/// Downloads queued replica thumbnails, `THUMBNAIL_WORKERS` at a time, highest priority
/// (visible items, see `prioritize_thumbnails`) then newest first. The queue is durable,
/// so an interrupted run resumes; each landed thumbnail is logged as a local
/// `representation_ready` event for `thumbnail_updates`. Stops early at the first
/// transient failure (authority busy/unreachable), leaving those items' attempts
/// untouched for the caller to retry after a backoff. Returns
/// `{downloaded, failed, pending, transient}` (`transient`: that error, else null).
pub fn download_thumbnails(library: &mut Library, progress: &mut dyn FnMut(u64, u64)) -> Result<Value> {
    let client = library.remote_client()?;
    let (mut downloaded, mut failed) = (0u64, 0u64);
    let mut transient: Option<String> = None;
    while transient.is_none() {
        let batch: Vec<(String, String)> = library
            .conn
            .prepare(
                "SELECT asset_id, path FROM eagle_thumbnails WHERE kind='download' AND attempts<?1 ORDER BY priority DESC, order_key DESC LIMIT ?2",
            )?
            .query_map(params![THUMB_ATTEMPTS, (THUMBNAIL_WORKERS * PACK_CHUNK) as i64], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        if batch.is_empty() {
            break;
        }
        let state = &library.state_dir;
        // One pack request (grid JPEGs, length-prefixed) per chunk, chunks in parallel;
        // per-item downloads only when the authority predates packs.
        let pack_failure = std::sync::Mutex::new(None::<anyhow::Error>);
        let results: Vec<(usize, Result<()>)> = std::thread::scope(|scope| {
            let workers: Vec<_> = batch
                .chunks(PACK_CHUNK)
                .enumerate()
                .map(|(c, chunk)| {
                    let (client, pack_failure) = (&client, &pack_failure);
                    scope.spawn(move || {
                        let base = c * PACK_CHUNK;
                        let ids: Vec<String> = chunk.iter().map(|(id, _)| id.clone()).collect();
                        match crate::thumbs::fetch_pack(client, &ids, "grid") {
                            Ok(entries) => {
                                let found: std::collections::HashMap<String, Vec<u8>> =
                                    entries.into_iter().collect();
                                chunk
                                    .iter()
                                    .enumerate()
                                    .map(|(i, (id, path))| {
                                        let result = (|| {
                                            let data = found
                                                .get(id)
                                                .filter(|d| !d.is_empty())
                                                .context("authority has no thumbnail")?;
                                            let dest = local_representation(state, path)?;
                                            fs::create_dir_all(
                                                dest.parent().context("missing cache directory")?,
                                            )?;
                                            let tmp = dest.with_extension("part");
                                            fs::write(&tmp, data)?;
                                            fs::rename(&tmp, &dest)?;
                                            Ok(())
                                        })();
                                        (base + i, result)
                                    })
                                    .collect::<Vec<_>>()
                            }
                            Err(e) if is_transient(&e) => {
                                *pack_failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                                Vec::new()
                            }
                            // Authorities without packs: one request per thumbnail.
                            Err(_) => chunk
                                .iter()
                                .enumerate()
                                .map(|(i, (id, path))| {
                                    let result = (|| {
                                        let dest = local_representation(state, path)?;
                                        if !dest.is_file() {
                                            client.download(
                                                &format!("/assets/{}/thumbnail", safe_id(id)?),
                                                &dest,
                                                None,
                                            )?;
                                        }
                                        Ok(())
                                    })();
                                    (base + i, result)
                                })
                                .collect(),
                        }
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|w| w.join().unwrap_or_default())
                .collect()
        });
        if let Some(e) = pack_failure.into_inner().unwrap_or_else(|p| p.into_inner()) {
            transient = Some(format!("{e:#}"));
        }
        let tx = library.conn.transaction()?;
        for (i, result) in results {
            let (id, path) = &batch[i];
            match result {
                Ok(()) => {
                    downloaded += 1;
                    // Only if the queued path is still current (a sync may have replaced it).
                    if tx.execute(
                        "DELETE FROM eagle_thumbnails WHERE asset_id=?1 AND path=?2",
                        params![id, path],
                    )? > 0
                    {
                        log_thumbnail_ready(&tx, &library.context.library_id, id)?;
                    }
                }
                Err(e) if is_transient(&e) => {
                    tx.execute(
                        "UPDATE eagle_thumbnails SET error=?2 WHERE asset_id=?1",
                        params![id, format!("{e:#}")],
                    )?;
                    transient = Some(format!("{e:#}"));
                }
                Err(e) => {
                    failed += 1;
                    tx.execute(
                        "UPDATE eagle_thumbnails SET attempts=attempts+1, error=?2 WHERE asset_id=?1",
                        params![id, format!("{e:#}")],
                    )?;
                }
            }
        }
        tx.commit()?;
        progress(downloaded, library.pending_eagle_thumbnails()?);
    }
    Ok(
        json!({"downloaded":downloaded,"failed":failed,"pending":library.pending_eagle_thumbnails()?,"transient":transient}),
    )
}

/// Records a replica-local `representation_ready` (thumbnail) event. Replica events are a
/// local log read by `thumbnail_updates` only; they never reach the authority or jobs.
fn log_thumbnail_ready(tx: &Transaction, library_id: &str, asset_id: &str) -> Result<()> {
    let event = Event {
        sequence: 0,
        id: id(),
        library_id: library_id.to_owned(),
        kind: "representation_ready".into(),
        asset_id: Some(asset_id.to_owned()),
        revision: None,
        actor: "replica".into(),
        created_at: now(),
        payload: json!({"kind":"thumbnail"}),
    };
    tx.execute(
        "INSERT INTO events(id,document) VALUES(?1,?2)",
        params![event.id, json_text(&event)?],
    )?;
    Ok(())
}

/// Local path of a representation. Replicas download and cache it on first use.
pub fn representation(library: &Library, id: &str, kind: &str) -> Result<PathBuf> {
    representation_with(library, id, kind, &Transfer::default())
}

/// [`representation`] reporting download progress into `transfer`, abandoned when it is
/// cancelled there and yielding to foreground downloads when it is background. Replica
/// previews are the authority's preview tier (`/tier/preview`, WebP), falling back to its
/// older preview representation.
pub fn representation_with(
    library: &Library,
    id: &str,
    kind: &str,
    transfer: &Transfer,
) -> Result<PathBuf> {
    if library.remote().is_none() {
        return library.fetch(id, kind);
    }
    let asset = get_asset(&library.conn, id)?;
    match kind {
        "original" => original(library, &asset, transfer),
        "preview" => preview(library, &asset, transfer),
        "thumbnail" => {
            let dest = stored_representation(library, &asset, kind)?
                .with_context(|| format!("representation unavailable: {kind}"))?;
            if !dest.is_file() {
                library.remote_client()?.download_with(
                    &format!("/assets/{}/{kind}", safe_id(id)?),
                    &dest,
                    None,
                    transfer,
                    STALL_TIMEOUT,
                )?;
            }
            Ok(dest)
        }
        _ => anyhow::bail!("representation must be original, thumbnail or preview"),
    }
}

/// Cache path of the authority representation `kind` of the current original, if any.
fn stored_representation(library: &Library, asset: &Asset, kind: &str) -> Result<Option<PathBuf>> {
    library
        .conn
        .query_row(
            "SELECT document FROM representations WHERE asset_id=?1 AND kind=?2 AND source_hash=?3",
            params![asset.id, kind, asset.original.source_key()],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(|document| {
            let rep: Representation = parse(document)?;
            local_representation(&library.state_dir, &rep.path)
        })
        .transpose()
}

fn original(library: &Library, asset: &Asset, transfer: &Transfer) -> Result<PathBuf> {
    if let Some(path) = direct_original(library, &asset.original)? {
        return Ok(path);
    }
    let ext = crate::storage::original_extension(&asset.original);
    let key = asset.original.source_key().replace(':', "-");
    let dest = library.state_dir.join(format!("cache/original/{key}{ext}"));
    if !dest.is_file() {
        library.remote_client()?.download_with(
            &format!("/assets/{}/original", safe_id(&asset.id)?),
            &dest,
            // A pending authority fingerprint has no digest to verify against yet.
            (!asset.original.pending()).then_some(asset.original.blake3.as_str()),
            transfer,
            STALL_TIMEOUT,
        )?;
    }
    let policy = crate::cache::CachePolicy::load(&library.state_dir)?;
    crate::cache::used(library, crate::cache::ORIGINAL, &asset.id, &dest)?;
    if let Some(cap) = policy.originals_cap_bytes {
        crate::cache::evict(library, crate::cache::ORIGINAL, cap, &dest)?;
    }
    Ok(dest)
}

/// Cache path of an asset's preview tier: `cache/preview/<id>-<source8>.webp`.
pub fn preview_tier_path(library: &Library, asset: &Asset) -> PathBuf {
    let key = asset.original.source_key();
    let source8 = if key.len() >= 8 && key.bytes().all(|b| b.is_ascii_hexdigit()) {
        key[..8].to_owned()
    } else {
        blake3::hash(key.as_bytes()).to_hex()[..8].to_owned()
    };
    library
        .state_dir
        .join(format!("cache/preview/{}-{source8}.webp", asset.id))
}

fn preview(library: &Library, asset: &Asset, transfer: &Transfer) -> Result<PathBuf> {
    use crate::cache::{CachePolicy, PREVIEW, Previews, evict, used};
    let policy = CachePolicy::load(&library.state_dir)?;
    let tier = preview_tier_path(library, asset);
    let legacy = stored_representation(library, asset, "preview")?;
    let cached = std::iter::once(&tier)
        .chain(legacy.as_ref())
        .find(|p| p.is_file());
    if let Some(path) = cached {
        used(library, PREVIEW, &asset.id, path)?;
        return Ok(path.clone());
    }
    ensure!(policy.previews != Previews::Off, "previews disabled");
    let client = library.remote_client()?;
    let id = safe_id(&asset.id)?;
    let fetch = |path: &str, dest: &Path| client.download_with(path, dest, None, transfer, STALL_TIMEOUT);
    let path = match fetch(&format!("/assets/{id}/tier/preview"), &tier) {
        Ok(()) => tier,
        Err(e) if transfer.cancelled() => return Err(e),
        // Tier not generated yet (or an older authority): its preview representation.
        Err(tier_error) => {
            let legacy = legacy.ok_or(tier_error)?;
            fetch(&format!("/assets/{id}/preview"), &legacy)?;
            legacy
        }
    };
    used(library, PREVIEW, &asset.id, &path)?;
    if let Some(cap) = policy.preview_cap_bytes {
        evict(library, PREVIEW, cap, &path)?;
    }
    Ok(path)
}

/// Policy `all`: downloads up to `limit` missing preview tiers, newest first, until the
/// preview cap is reached. A preview the authority lacks is not asked for again for an
/// hour. Returns `(downloaded, more)`; transient failures are errors (retry later).
pub fn download_previews(library: &Library, limit: usize) -> Result<(u64, bool)> {
    use crate::cache::{CachePolicy, PREVIEW, Previews, mark_missing, missing_previews, total, used};
    let policy = CachePolicy::load(&library.state_dir)?;
    if policy.previews != Previews::All
        || policy
            .preview_cap_bytes
            .is_some_and(|cap| total(library, PREVIEW).is_ok_and(|t| t >= cap))
    {
        return Ok((0, false));
    }
    let ids = missing_previews(library, limit)?;
    if ids.is_empty() {
        return Ok((0, false));
    }
    let client = library.remote_client()?;
    // Bulk: yields to whatever the user is looking at.
    let transfer = Transfer::background();
    let mut downloaded = 0;
    for id in &ids {
        let asset = get_asset(&library.conn, id)?;
        let dest = preview_tier_path(library, &asset);
        let path = format!("/assets/{}/tier/preview", safe_id(id)?);
        match client.download_with(&path, &dest, None, &transfer, STALL_TIMEOUT) {
            Ok(()) => {
                used(library, PREVIEW, id, &dest)?;
                downloaded += 1;
            }
            Err(e) if is_transient(&e) => return Err(e),
            Err(_) => mark_missing(library, id)?,
        }
    }
    Ok((downloaded, true))
}

/// When a replica asks the authority again about a tile it could not upgrade: the grid tier
/// was missing (it is rendered in the background, e.g. just after intake), or it was no
/// sharper than the tile (re-rendered from a larger source later, if ever).
const GRID_RECHECK_MISSING: i64 = 600;
const GRID_RECHECK_SAME: i64 = 7 * 86_400;
/// Concurrent grid tier downloads per `fetch_grid`.
const GRID_WORKERS: usize = 4;
const GRID_CHECKS: &str = "CREATE TABLE IF NOT EXISTS replica_grid_checks(asset_id TEXT PRIMARY KEY, source TEXT NOT NULL, until INTEGER NOT NULL)";

/// Replica grid tier on demand: for each of `ids` (visible tiles) whose cached tile is
/// smaller than the grid tier could be (long edge below `GRID_EDGE` and below the asset's),
/// downloads the authority's grid tier (`/tier/grid`) into `cache/grid/` and records it in
/// `grid_thumbs`, so items list it as `grid_path`. Bulk downloads (`download_thumbnails`)
/// keep whatever the authority had when they ran: its thumbnail before the grid tier was
/// rendered, or a grid rendered before its hires source existed. Tiles that are already
/// final, upgraded, or recently checked cost one header read and no request. Stops at the
/// first cancellation or transient failure. Returns `{upgraded: [id], local: [id], checked,
/// bytes}`: `local` lists every asked id whose grid tier is here (upgraded earlier too).
pub fn fetch_grid(library: &Library, ids: &[String], transfer: &Transfer) -> Result<Value> {
    use crate::thumbs::GRID_EDGE;
    library.conn.execute_batch(GRID_CHECKS)?;
    let now = Utc::now().timestamp();
    let state = &library.state_dir;
    // (asset id, thumbnail source, tile edge, destination relative to the state dir)
    let mut candidates: Vec<(String, String, u32, String)> = Vec::new();
    let mut local = Vec::new();
    for id in ids {
        let Ok(asset) = get_asset(&library.conn, id) else {
            continue;
        };
        let Some(rep) = library
            .conn
            .prepare_cached("SELECT document FROM representations WHERE asset_id=?1 AND kind='thumbnail' AND source_hash=?2")?
            .query_row(params![asset.id, asset.original.source_key()], |r| r.get::<_, String>(0))
            .optional()?
            .map(parse::<Representation>)
            .transpose()?
        else {
            continue;
        };
        let tile = local_representation(state, &rep.path)?;
        let upgraded = library
            .conn
            .prepare_cached("SELECT path FROM grid_thumbs WHERE asset_id=?1 AND source=?2")?
            .query_row(params![asset.id, rep.source_hash], |r| r.get::<_, String>(0))
            .optional()?
            .is_some_and(|p| local_representation(state, &p).is_ok_and(|p| p.is_file()));
        let checked = library
            .conn
            .prepare_cached("SELECT 1 FROM replica_grid_checks WHERE asset_id=?1 AND source=?2 AND until>?3")?
            .exists(params![asset.id, rep.source_hash, now])?;
        if upgraded {
            local.push(asset.id);
            continue;
        }
        // No tile yet: the bulk queue fetches it (visible ones first).
        if checked || !tile.is_file() {
            continue;
        }
        let edge = image::ImageReader::open(&tile)
            .ok()
            .and_then(|r| r.with_guessed_format().ok())
            .and_then(|r| r.into_dimensions().ok())
            .map_or(0, |(w, h)| w.max(h));
        let wanted = match (asset.width, asset.height) {
            (Some(w), Some(h)) => w.max(h).min(GRID_EDGE),
            _ => GRID_EDGE,
        };
        if edge < wanted {
            let dest = cache_path("grid", &asset.id, &rep.source_hash, "image/webp");
            candidates.push((asset.id, rep.source_hash, edge, dest));
        }
    }
    let client = library.remote_client()?;
    let next = std::sync::atomic::AtomicUsize::new(0);
    let stop = std::sync::atomic::AtomicBool::new(false);
    // Per candidate: Some(Ok(new edge)) downloaded, Some(Err) failed, None not attempted.
    let outcomes: Vec<std::sync::Mutex<Option<Result<u32>>>> =
        candidates.iter().map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..GRID_WORKERS.min(candidates.len()) {
            scope.spawn(|| {
                use std::sync::atomic::Ordering::SeqCst;
                loop {
                    let index = next.fetch_add(1, SeqCst);
                    let Some((id, _, _, dest)) = candidates.get(index) else { break };
                    if stop.load(SeqCst) || transfer.cancelled() {
                        break;
                    }
                    let result = (|| -> Result<u32> {
                        let path = local_representation(state, dest)?;
                        client.download_with(&format!("/assets/{}/tier/grid", safe_id(id)?), &path, None, transfer, STALL_TIMEOUT)?;
                        let (w, h) = image::ImageReader::open(&path)?.with_guessed_format()?.into_dimensions()?;
                        Ok(w.max(h))
                    })();
                    if result.as_ref().is_err_and(|e| transfer.cancelled() || is_transient(e)) {
                        stop.store(true, SeqCst);
                    }
                    *outcomes[index].lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
                }
            });
        }
    });
    let mut upgraded = Vec::new();
    let mut failure = None;
    for ((id, source, edge, dest), outcome) in candidates.iter().zip(outcomes) {
        let recheck = |delay: i64| {
            library.conn.execute(
                "INSERT OR REPLACE INTO replica_grid_checks(asset_id,source,until) VALUES(?1,?2,?3)",
                params![id, source, now + delay],
            )
        };
        match outcome.into_inner().unwrap_or_else(|e| e.into_inner()) {
            None => {}
            Some(Ok(grid)) if grid > *edge => {
                let path = local_representation(state, dest)?;
                let old: Option<String> = library
                    .conn
                    .query_row("SELECT path FROM grid_thumbs WHERE asset_id=?1", [id], |r| r.get(0))
                    .optional()?;
                library.conn.execute(
                    "INSERT OR REPLACE INTO grid_thumbs(asset_id,source,path,byte_size,tier_version) VALUES(?1,?2,?3,?4,?5)",
                    params![id, source, dest, fs::metadata(&path)?.len() as i64, crate::thumbs::GRID_VERSION],
                )?;
                if let Some(old) = old.filter(|old| old != dest) {
                    let _ = local_representation(state, &old).map(fs::remove_file);
                }
                library.conn.execute("DELETE FROM replica_grid_checks WHERE asset_id=?1", [id])?;
                upgraded.push(id.clone());
            }
            // No sharper than the tile: not kept.
            Some(Ok(_)) => {
                let _ = local_representation(state, dest).map(fs::remove_file);
                recheck(GRID_RECHECK_SAME)?;
            }
            Some(Err(e)) if transfer.cancelled() || is_transient(&e) => {
                failure.get_or_insert(e);
            }
            // Not rendered yet (404 tier_missing) or unreadable: ask again later.
            Some(Err(_)) => {
                recheck(GRID_RECHECK_MISSING)?;
            }
        }
    }
    if let Some(e) = failure.filter(|_| upgraded.is_empty()) {
        return Err(e);
    }
    local.extend(upgraded.iter().cloned());
    Ok(json!({"upgraded": upgraded, "local": local, "checked": candidates.len(), "bytes": transfer.bytes()}))
}

/// Local paths where an authority original may already exist, most specific first:
/// the authority path itself (same machine or identical mount), the longest configured
/// `drive_roots` prefix (whole path components), and the same file under this user's
/// `~/Library/CloudStorage/<provider>/` when the authority path lies in one (e.g.
/// `GoogleDrive-alice@example.com`, synced to both machines by the same account).
pub fn direct_candidates(
    authority: &str,
    drive_roots: &BTreeMap<String, String>,
    user_home: Option<&Path>,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if authority.is_empty() {
        return out;
    }
    out.push(PathBuf::from(authority));
    let mapped = drive_roots
        .iter()
        .filter_map(|(prefix, local)| {
            let prefix = prefix.trim_end_matches('/');
            let rest = authority.strip_prefix(prefix)?;
            (rest.is_empty() || rest.starts_with('/')).then(|| {
                (
                    prefix.len(),
                    format!("{}{rest}", local.trim_end_matches('/')),
                )
            })
        })
        .max_by_key(|(len, _)| *len);
    if let Some((_, path)) = mapped {
        out.push(PathBuf::from(path));
    }
    const CLOUD: &str = "/Library/CloudStorage/";
    if let (Some(home), Some(at)) = (user_home, authority.find(CLOUD)) {
        let rest = &authority[at + CLOUD.len()..];
        if rest.split('/').next().is_some_and(|p| !p.is_empty()) {
            out.push(home.join("Library/CloudStorage").join(rest));
        }
    }
    out.dedup();
    out
}

/// An authority original readable on this machine with the cataloged size (and digest,
/// when hashed; a verified path/size/mtime is remembered so it is hashed once). Cloud
/// placeholders (dataless File Provider files) are skipped: reading one would block on
/// the cloud download, so the authority serves those bytes instead.
fn direct_original(library: &Library, original: &Original) -> Result<Option<PathBuf>> {
    let remote = library
        .remote()
        .context("context is not a remote replica")?;
    let user_home = std::env::var_os("HOME").map(PathBuf::from);
    for path in direct_candidates(&original.path, &remote.drive_roots, user_home.as_deref()) {
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() || meta.len() != original.byte_size || dataless(&meta) {
            continue;
        }
        if original.pending() || original.blake3.is_empty() {
            return Ok(Some(path));
        }
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        let stamp = format!("{}\n{}\n{mtime}", path.display(), meta.len());
        let marker = library
            .state_dir
            .join("cache/direct")
            .join(&original.blake3[..original.blake3.len().min(32)]);
        if fs::read_to_string(&marker).is_ok_and(|s| s == stamp) {
            return Ok(Some(path));
        }
        let mut hasher = blake3::Hasher::new();
        if hasher.update_reader(File::open(&path)?).is_err() {
            continue;
        }
        if hasher.finalize().to_hex().as_str() == original.blake3 {
            fs::create_dir_all(marker.parent().context("marker dir")?)?;
            fs::write(&marker, stamp)?;
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// A File Provider placeholder whose bytes are not on this disk (`SF_DATALESS`).
#[cfg(target_os = "macos")]
fn dataless(meta: &fs::Metadata) -> bool {
    use std::os::macos::fs::MetadataExt as _;
    const SF_DATALESS: u32 = 0x4000_0000;
    meta.st_flags() & SF_DATALESS != 0
}

#[cfg(not(target_os = "macos"))]
fn dataless(_: &fs::Metadata) -> bool {
    false
}

/// Block until the authority has events past this replica's cursor (or `wait` seconds
/// pass), then sync, replaying the events the long-poll returned (no second `GET /events`
/// round trip) and refreshing the embeddings pack in the background, so a push costs one
/// asset fetch on top of the long-poll. Returns the `sync` result plus `changed`.
pub fn sync_wait(library: &mut Library, wait: u64) -> Result<Value> {
    let after = cursor(&library.conn)?;
    let events = library.remote_client()?.wait_events(after, wait.min(60))?;
    let changed = !events.is_empty();
    let mut value = if changed {
        let value = pull(library, &mut |_, _, _| {}, true, Some((after, events)))?;
        crate::energy::replica::refresh_after_sync(library, value["applied"].as_u64().unwrap_or(0), true);
        value
    } else {
        json!({"applied":0,"cursor":after})
    };
    value["changed"] = json!(changed);
    Ok(value)
}

/// Semantic search for a replica: ranked by the authority, asset documents from the mirror
/// (ids not yet synced are skipped). `semantic` is false when the authority has no
/// embedder and answered with full-text matches.
pub fn semantic_search(
    library: &Library,
    text: &str,
    limit: usize,
    hybrid: bool,
    added_by: Option<&str>,
) -> Result<Value> {
    let (semantic, ranked) = library
        .remote_client()?
        .semantic_search(text, limit, hybrid, added_by)?;
    let mut results = Vec::with_capacity(ranked.len());
    for (id, score) in ranked {
        if let Ok(asset) = get_asset(&library.conn, &id) {
            results.push(json!({"asset":asset,"score":score}));
        }
    }
    Ok(json!({"semantic":semantic,"hybrid":hybrid,"results":results}))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DRIVE: &str = "/Users/alice/Library/CloudStorage/GoogleDrive-alice@example.com/My Drive/Refs.library/images/a.info/a.png";

    #[test]
    fn same_account_cloud_storage_maps_into_this_users_home() {
        let got = direct_candidates(DRIVE, &BTreeMap::new(), Some(Path::new("/Users/bob")));
        assert_eq!(
            got,
            [
                PathBuf::from(DRIVE),
                PathBuf::from(
                    "/Users/bob/Library/CloudStorage/GoogleDrive-alice@example.com/My Drive/Refs.library/images/a.info/a.png"
                ),
            ]
        );
    }

    #[test]
    fn longest_whole_component_drive_root_wins() {
        let roots = BTreeMap::from([
            ("/Users/alice/Library".to_owned(), "/x".to_owned()),
            (
                "/Users/alice/Library/CloudStorage/GoogleDrive-alice@example.com/My Drive/".to_owned(),
                "/mnt/gdrive/".to_owned(),
            ),
            // Not a component boundary of the authority path: never matches.
            ("/Users/alice/Lib".to_owned(), "/wrong".to_owned()),
        ]);
        let got = direct_candidates(DRIVE, &roots, None);
        assert_eq!(
            got,
            [
                PathBuf::from(DRIVE),
                PathBuf::from("/mnt/gdrive/Refs.library/images/a.info/a.png"),
            ]
        );
    }

    #[test]
    fn evicted_originals_have_no_candidates() {
        assert!(direct_candidates("", &BTreeMap::new(), Some(Path::new("/h"))).is_empty());
    }

    #[test]
    fn join_targets_accept_urls_hosts_and_ports() {
        assert_eq!(join_url("studio-mac"), "http://studio-mac:7777");
        assert_eq!(join_url("192.0.2.7:7790"), "http://192.0.2.7:7790");
        assert_eq!(join_url("http://studio:7777/"), "http://studio:7777");
        assert_eq!(join_url("fd7a::1"), "http://[fd7a::1]:7777");
        assert_eq!(join_url("[fd7a::1]:8000"), "http://[fd7a::1]:8000");
    }

    /// Serves every GET with `size` bytes trickled in 16 KB pieces every 20 ms.
    fn trickle_server(size: usize) -> String {
        use std::io::{BufRead as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                        line.clear();
                    }
                    let mut stream = stream;
                    let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n");
                    if stream.write_all(head.as_bytes()).is_err() {
                        return;
                    }
                    for _ in 0..size.div_ceil(16 * 1024) {
                        let piece = vec![7u8; 16 * 1024];
                        if stream.write_all(&piece[..]).is_err() {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                });
            }
        });
        url
    }

    #[test]
    fn background_downloads_wait_for_foreground_ones() {
        let size = 512 * 1024;
        let remote = Remote { url: trickle_server(size), token: String::new(), drive_roots: BTreeMap::new() };
        let client = Client::new(&remote).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let shown = Transfer::default();
        let prefetch = Transfer::background();
        std::thread::scope(|scope| {
            let front = scope.spawn(|| client.download_with("/shown", &dir.path().join("a"), None, &shown, STALL_TIMEOUT));
            // Starts after the foreground one: held until it ends.
            while shown.bytes() == 0 {
                std::thread::sleep(Duration::from_millis(10));
            }
            let back = scope.spawn(|| client.download_with("/next", &dir.path().join("b"), None, &prefetch, STALL_TIMEOUT));
            while !front.is_finished() {
                assert_eq!(prefetch.bytes(), 0, "background read while the shown item downloads");
                std::thread::sleep(Duration::from_millis(20));
            }
            front.join().unwrap().unwrap();
            back.join().unwrap().unwrap();
        });
        assert_eq!(prefetch.bytes(), size as u64);
        // Promotion mid-download counts it as foreground; demotion releases it again.
        let late = Transfer::background();
        late.begin();
        let waiting = Transfer::background();
        late.set_background(false);
        assert!(waiting.held());
        late.set_background(true);
        late.end();
    }
}
