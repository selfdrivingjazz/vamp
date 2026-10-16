//! Share publisher (contract `share`). A share is a frozen snapshot of a folder, an
//! exploration or a selection, published to a share host (`https://app.vamp.sh` unless
//! `VAMP_SHARE_HOST`, `--host` or `vamp share login --host` says otherwise):
//! - the scope is resolved here, never on the host; assets in the Trash and assets tagged
//!   [`NEVER_SHARE_TAG`] are always left out;
//! - metadata is scrubbed to the share's [`ShareFields`] before anything is sent (a hidden
//!   field never leaves this machine; original paths, `metadata.extra` and non-http source
//!   URLs never do);
//! - only derivatives that already exist are uploaded byte for byte (the 800 px grid WebP,
//!   the 2048 px hires WebP and the ~40 px micro JPEG; all re-encoded from pixels, so they
//!   carry no EXIF), content-addressed by SHA-256, so a re-run uploads only what the host
//!   lacks (idempotent and resumable);
//! - the host never connects back to a library.
//!
//! Account credentials live in `<home>/share.json` (0600) and are only ever sent to the
//! host they were saved for. Each share's local record (its scope and key secret) lives in
//! `<state_dir>/shares.json` (0600).
use super::*;
use crate::remote::{STALL_TIMEOUT, Transfer, is_transient};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{
    LazyLock, Mutex,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::SeqCst},
};

pub const DEFAULT_HOST: &str = "https://app.vamp.sh";
/// Assets with this tag (any case) are never published.
pub const NEVER_SHARE_TAG: &str = "private";
/// The `vamp-preview` precedent (`native::MAX_ASSETS`).
pub const MAX_SHARE_ASSETS: usize = 20_000;
/// Largest derivative the host accepts.
const MAX_BLOB: u64 = 32 * 1024 * 1024;
const UPLOAD_WORKERS: usize = 4;
const PLAN_CHUNK: usize = 1000;
/// Tries per host request on transient failures (connection, 429, 5xx).
const ATTEMPTS: u32 = 8;
const RECORDS: &str = "shares.json";
const CREDENTIALS: &str = "share.json";

/// Options of the share native ops and CLI.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShareOptions {
    pub scope: Option<ShareScope>,
    pub title: Option<String>,
    pub description: Option<String>,
    /// `1d`, `7d`, `30d`, `never`, or `<n>s|m|h|d` (at least 60 s).
    pub ttl: Option<String>,
    pub password: Option<String>,
    pub fields: Option<ShareFields>,
    pub host: Option<String>,
    /// The host's share id (`share_publish`, `share_revoke`).
    pub id: Option<String>,
}

/// Progress callback: `(phase, done, total)`; called from upload worker threads too.
pub type Progress<'a> = &'a (dyn Fn(&str, u64, u64) + Sync);

// ---------------------------------------------------------------------------------------
// Account

#[derive(Serialize, Deserialize)]
struct Credentials {
    host: String,
    token: String,
}

fn load_credentials(home: &Path) -> Result<Option<Credentials>> {
    match fs::read(home.join(CREDENTIALS)) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).context("invalid share.json")?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// `https://…`, or plain `http://` to a loopback address (a host under development).
fn normalize_host(host: &str) -> Result<String> {
    let host = host.trim().trim_end_matches('/');
    let loopback = ["http://127.0.0.1", "http://localhost", "http://[::1]"]
        .iter()
        .any(|p| host.strip_prefix(p).is_some_and(|rest| rest.is_empty() || rest.starts_with(':')));
    ensure!(
        (host.starts_with("https://") && host.len() > 8) || loopback,
        "share host must be an https:// URL (http:// only for localhost): {host}"
    );
    ensure!(
        !host[8..].contains(['/', '?', '#', '@', ' ']),
        "share host must be an origin without a path: {host}"
    );
    Ok(host.to_owned())
}

/// The host to talk to: `explicit`, else `VAMP_SHARE_HOST`, else the signed-in host, else
/// [`DEFAULT_HOST`].
pub fn host_for(home: &Path, explicit: Option<&str>) -> Result<String> {
    let env = std::env::var("VAMP_SHARE_HOST").ok().filter(|h| !h.trim().is_empty());
    match explicit.map(str::to_owned).or(env) {
        Some(host) => normalize_host(&host),
        None => match load_credentials(home)? {
            Some(c) => normalize_host(&c.host),
            None => Ok(DEFAULT_HOST.into()),
        },
    }
}

/// `VAMP_SHARE_TOKEN`, else the token saved for exactly this host.
fn token_for(home: &Path, host: &str) -> Result<Option<String>> {
    if let Some(token) = std::env::var("VAMP_SHARE_TOKEN").ok().filter(|t| !t.trim().is_empty()) {
        return Ok(Some(token.trim().to_owned()));
    }
    Ok(load_credentials(home)?
        .filter(|c| normalize_host(&c.host).is_ok_and(|h| h == host))
        .map(|c| c.token))
}

fn hint(token: &str) -> String {
    format!("{}…", token.chars().take(8).collect::<String>())
}

/// `{host, signed_in, token_hint}`.
pub fn account(home: &Path, host: Option<&str>) -> Result<Value> {
    let host = host_for(home, host)?;
    let token = token_for(home, &host)?;
    Ok(json!({"host":host,"signed_in":token.is_some(),"token_hint":token.as_deref().map(hint)}))
}

/// Checks `token` against the host, then saves it for that host (0600).
pub fn login(home: &Path, token: &str, host: Option<&str>) -> Result<Value> {
    let token = token.trim();
    ensure!(
        !token.is_empty()
            && token.len() <= 512
            && token.bytes().all(|b| b.is_ascii_graphic()),
        "invalid token"
    );
    let host = host_for(home, host)?;
    let client = HostClient::new(&host, token);
    let entitled = match client.json("GET", "/shares", None) {
        Ok(_) => true,
        Err(e) if format!("{e:#}").starts_with("share_required") => false,
        Err(e) => return Err(e),
    };
    let _lock = config_lock(home)?;
    atomic_json(
        &home.join(CREDENTIALS),
        &Credentials {
            host: host.clone(),
            token: token.into(),
        },
    )?;
    let mut value = account(home, Some(&host))?;
    value["entitled"] = json!(entitled);
    Ok(value)
}

pub fn logout(home: &Path) -> Result<Value> {
    match fs::remove_file(home.join(CREDENTIALS)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(json!({"signed_in":false}))
}

fn signed_in_client(home: &Path, host: Option<&str>) -> Result<HostClient> {
    let host = host_for(home, host)?;
    let token = token_for(home, &host)?.with_context(|| {
        format!("not_signed_in: no share token for {host}; run vamp share login --token TOKEN")
    })?;
    Ok(HostClient::new(&host, &token))
}

// ---------------------------------------------------------------------------------------
// Host API client

struct HostClient {
    agent: ureq::Agent,
    host: String,
    token: String,
}

enum Body<'a> {
    Json(&'a Value),
    Bytes(&'a [u8], &'a str),
}

impl HostClient {
    fn new(host: &str, token: &str) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .user_agent(concat!("vamp/", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(Duration::from_secs(20)))
            // Slow links: a 2 MB hires at 50 kB/s still fits.
            .timeout_send_body(Some(Duration::from_secs(600)))
            .timeout_recv_response(Some(Duration::from_secs(180)))
            .timeout_recv_body(Some(Duration::from_secs(180)))
            .build()
            .into();
        Self {
            agent,
            host: host.to_owned(),
            token: token.to_owned(),
        }
    }

    /// One request under `/api/v1`, retried with backoff on transient failures.
    /// Returns the status and JSON body (`null` when empty) for any final status.
    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<Body>,
        if_match: Option<u64>,
    ) -> Result<(u16, Value)> {
        let url = format!("{}/api/v1{path}", self.host);
        let auth = format!("Bearer {}", self.token);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let result = (|| -> Result<(u16, Value)> {
                let response = match method {
                    "GET" => self.agent.get(&url).header("Authorization", &auth).call(),
                    "DELETE" => self.agent.delete(&url).header("Authorization", &auth).call(),
                    "POST" | "PUT" | "PATCH" => {
                        let mut request = match method {
                            "POST" => self.agent.post(&url),
                            "PUT" => self.agent.put(&url),
                            _ => self.agent.patch(&url),
                        }
                        .header("Authorization", &auth);
                        if let Some(version) = if_match {
                            request = request.header("If-Match", version.to_string());
                        }
                        match body {
                            Some(Body::Json(value)) => request.send_json(value),
                            Some(Body::Bytes(bytes, mime)) => {
                                request.header("Content-Type", mime).send(bytes)
                            }
                            None => request.send_empty(),
                        }
                    }
                    other => anyhow::bail!("unsupported method {other}"),
                }?;
                let status = response.status().as_u16();
                let text = response
                    .into_body()
                    .with_config()
                    .limit(64 * 1024 * 1024)
                    .read_to_string()?;
                let value = if text.trim().is_empty() {
                    Value::Null
                } else {
                    serde_json::from_str(&text).unwrap_or_else(|_| json!({"error":text.trim()}))
                };
                Ok((status, value))
            })();
            let retry = match &result {
                Ok((status, _)) => matches!(status, 429 | 500 | 502 | 503 | 504),
                Err(e) => is_transient(e),
            };
            if !retry || attempt >= ATTEMPTS {
                return result.with_context(|| format!("{method} {url}"));
            }
            std::thread::sleep(Duration::from_secs((1u64 << attempt.min(5)).min(30)));
        }
    }

    /// [`Self::call`] that maps non-2xx statuses to errors (prefixed `not_signed_in:` /
    /// `share_required:` where the caller can act on them).
    fn json(&self, method: &str, path: &str, body: Option<Body>) -> Result<Value> {
        let (status, value) = self.call(method, path, body, None)?;
        check(status, value, &self.host)
    }
}

fn check(status: u16, value: Value, host: &str) -> Result<Value> {
    if (200..300).contains(&status) {
        return Ok(value);
    }
    let error = value["error"].as_str().unwrap_or("").to_owned();
    let message = value["message"].as_str().unwrap_or(&error).to_owned();
    match status {
        401 => anyhow::bail!(
            "not_signed_in: {host} rejected the share token ({message}); run vamp share login --token TOKEN"
        ),
        402 => anyhow::bail!(
            "share_required: sharing needs a Vampire Share subscription: {}",
            value["url"].as_str().unwrap_or(&format!("{host}/account/billing"))
        ),
        404 => anyhow::bail!("not_found: no such share on {host}"),
        _ => anyhow::bail!("share host {status}: {}", if message.is_empty() { "error" } else { &message }),
    }
}

// ---------------------------------------------------------------------------------------
// Scope

#[derive(Debug, Default, Clone, Serialize)]
pub struct Excluded {
    pub trash: u64,
    pub never_share: u64,
    pub missing_tier: u64,
}

struct Member {
    asset: Asset,
    /// Shared folders (collection ids) holding it, in tree order.
    folders: Vec<String>,
}

struct Resolved {
    kind: &'static str,
    title: String,
    description: Option<String>,
    /// Shared sub-folders (the root excluded), parents first, with their parent (None =
    /// directly below the root).
    folders: Vec<(Collection, Option<String>)>,
    members: Vec<Member>,
    excluded: Excluded,
}

fn never_share(asset: &Asset) -> bool {
    asset
        .metadata
        .tags
        .iter()
        .any(|t| t.trim().eq_ignore_ascii_case(NEVER_SHARE_TAG))
}

impl Resolved {
    fn label(&self) -> String {
        match self.kind {
            "selection" => format!("Selection ({})", self.members.len()),
            kind => format!(
                "{}{} · {} ({})",
                kind[..1].to_uppercase(),
                &kind[1..],
                self.title,
                self.members.len()
            ),
        }
    }
}

fn resolve(library: &Library, scope: &ShareScope) -> Result<Resolved> {
    let mut excluded = Excluded::default();
    let mut members: Vec<Member> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut seen_excluded: HashSet<String> = HashSet::new();
    let mut admit = |asset: Asset, trashed: bool, folder: Option<&str>, members: &mut Vec<Member>| {
        if trashed || never_share(&asset) {
            if seen_excluded.insert(asset.id.clone()) {
                if trashed {
                    excluded.trash += 1;
                } else {
                    excluded.never_share += 1;
                }
            }
            return;
        }
        let at = *index.entry(asset.id.clone()).or_insert_with(|| {
            members.push(Member {
                asset,
                folders: vec![],
            });
            members.len() - 1
        });
        if let Some(folder) = folder {
            members[at].folders.push(folder.to_owned());
        }
    };
    let (kind, root, tree) = match scope {
        ShareScope::Selection { asset_ids } => {
            ensure!(!asset_ids.is_empty(), "a selection share needs asset ids");
            for id in asset_ids {
                let (document, trashed): (String, bool) = library
                    .conn
                    .query_row(
                        "SELECT document,trashed FROM assets WHERE id=?1 AND deleted=0",
                        [id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?
                    .with_context(|| format!("no asset {id}"))?;
                admit(parse(document)?, trashed, None, &mut members);
            }
            drop(admit);
            return Ok(Resolved {
                kind: "selection",
                title: format!("{} items", members.len()),
                description: None,
                folders: vec![],
                members,
                excluded,
            });
        }
        ShareScope::Folder {
            collection_id,
            include_subfolders,
        } => {
            let tree = library.subtree(collection_id)?;
            let root = tree
                .first()
                .filter(|c| &c.id == collection_id)
                .cloned()
                .with_context(|| format!("no folder {collection_id}"))?;
            let tree = if *include_subfolders { tree } else { vec![root.clone()] };
            ("folder", root, tree)
        }
        ShareScope::Exploration { slug } => {
            let root = library.exploration(slug)?;
            let tree = library.subtree(&root.id)?;
            ("exploration", root, tree)
        }
    };
    let mut statement = library.conn.prepare(
        "SELECT a.document,a.trashed FROM memberships m JOIN assets a ON a.id=m.asset_id WHERE m.collection_id=?1 AND a.deleted=0 ORDER BY m.rowid",
    )?;
    for folder in &tree {
        let rows = statement
            .query_map([&folder.id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let filed = (folder.id != root.id).then_some(folder.id.as_str());
        for (document, trashed) in rows {
            admit(parse(document)?, trashed, filed, &mut members);
        }
    }
    drop(admit);
    let folders = tree
        .into_iter()
        .filter(|c| c.id != root.id)
        .map(|c| {
            let parent = c.parent_id.clone().filter(|p| *p != root.id);
            (c, parent)
        })
        .collect();
    Ok(Resolved {
        kind,
        title: root.name.clone(),
        description: root.description.clone().filter(|d| !d.trim().is_empty()),
        folders,
        members,
        excluded,
    })
}

// ---------------------------------------------------------------------------------------
// Derivatives

struct Tiers {
    grid: PathBuf,
    hires: Option<PathBuf>,
    micro: Option<Vec<u8>>,
}

/// `image/webp`, `image/jpeg` or `image/png` from magic bytes; anything else is refused.
fn sniff(head: &[u8]) -> Option<&'static str> {
    if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        Some("image/webp")
    } else if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else {
        None
    }
}

/// Local grid/hires files (`None` when absent here) and micro bytes, without network.
fn local_tiers(library: &mut Library, members: &[Member]) -> Result<Vec<(Option<PathBuf>, Option<PathBuf>, Option<Vec<u8>>)>> {
    let ids: Vec<String> = members.iter().map(|m| m.asset.id.clone()).collect();
    let mut micro = HashMap::new();
    for chunk in ids.chunks(crate::thumbs::MAX_PACK) {
        micro.extend(crate::thumbs::micro_for(library, chunk)?);
    }
    let mut out = Vec::with_capacity(members.len());
    for member in members {
        let id = &member.asset.id;
        let grid = crate::thumbs::tier_path(library, id, "grid")?;
        let mut hires = crate::thumbs::tier_path(library, id, "preview")?;
        if hires.is_none() && library.remote().is_some() {
            let cached = crate::remote::preview_tier_path(library, &member.asset);
            hires = cached.is_file().then_some(cached);
        }
        out.push((grid, hires, micro.remove(id).filter(|m| !m.is_empty())));
    }
    Ok(out)
}

fn share_cache(library: &Library) -> PathBuf {
    library.state_dir.join("cache/share")
}

/// Every member's tiers; on a replica, missing grid and hires tiers are downloaded from
/// the authority first. Members without a grid tier get `None` (excluded: missing_tier).
fn tiers(library: &mut Library, members: &[Member], progress: Progress) -> Result<Vec<Option<Tiers>>> {
    let local = local_tiers(library, members)?;
    let replica = library.remote().is_some();
    let total = members.len() as u64;
    let mut out = Vec::with_capacity(members.len());
    for (i, (member, (grid, hires, micro))) in members.iter().zip(local).enumerate() {
        progress("prepare", i as u64, total);
        let id = &member.asset.id;
        let (grid, hires) = if replica {
            let grid = match grid {
                Some(grid) => Some(grid),
                None => {
                    let dest = share_cache(library).join(format!("{id}-grid.webp"));
                    if !dest.is_file() {
                        let client = library.remote_client()?;
                        match client.download_with(
                            &format!("/assets/{id}/tier/grid"),
                            &dest,
                            None,
                            &Transfer::default(),
                            STALL_TIMEOUT,
                        ) {
                            Ok(()) => {}
                            Err(e) if is_transient(&e) => return Err(e),
                            Err(_) => {}
                        }
                    }
                    dest.is_file().then_some(dest)
                }
            };
            let hires = match hires {
                Some(hires) => Some(hires),
                None => match crate::remote::representation_with(library, id, "preview", &Transfer::default()) {
                    Ok(path) => Some(path),
                    Err(e) if is_transient(&e) => return Err(e),
                    Err(_) => None,
                },
            };
            (grid, hires)
        } else {
            (grid, hires)
        };
        out.push(grid.map(|grid| Tiers { grid, hires, micro }));
    }
    progress("prepare", total, total);
    Ok(out)
}

struct Blob {
    hash: String,
    bytes: u64,
    mime: &'static str,
    tier: &'static str,
    source: BlobSource,
}
enum BlobSource {
    File(PathBuf),
    Memory(Vec<u8>),
}
impl Blob {
    fn read(&self) -> Result<Vec<u8>> {
        Ok(match &self.source {
            BlobSource::File(path) => fs::read(path)?,
            BlobSource::Memory(bytes) => bytes.clone(),
        })
    }
}

fn blob(bytes: &[u8], tier: &'static str, source: BlobSource) -> Result<Blob> {
    ensure!(!bytes.is_empty() && bytes.len() as u64 <= MAX_BLOB, "{tier} derivative is empty or over 32 MiB");
    let mime = sniff(bytes).with_context(|| format!("{tier} derivative is not WebP, JPEG or PNG"))?;
    Ok(Blob {
        hash: blob_id(bytes),
        bytes: bytes.len() as u64,
        mime,
        tier,
        source,
    })
}

/// `sha256:<64 hex>` of `bytes`.
fn blob_id(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(71);
    out.push_str("sha256:");
    for b in digest.iter() {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn file_blob(path: &Path, tier: &'static str) -> Result<Blob> {
    ensure!(
        !fs::symlink_metadata(path)?.file_type().is_symlink(),
        "symlink derivative refused: {}",
        path.display()
    );
    let bytes = fs::read(path)?;
    blob(&bytes, tier, BlobSource::File(path.to_owned()))
}

// ---------------------------------------------------------------------------------------
// Manifest

fn key_bytes(secret: &str) -> Result<[u8; 32]> {
    ensure!(secret.len() == 64, "invalid share key secret");
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&secret[i * 2..i * 2 + 2], 16).context("invalid share key secret")?;
    }
    Ok(out)
}

fn new_secret() -> String {
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(Uuid::new_v4().as_bytes());
    bytes.extend_from_slice(Uuid::new_v4().as_bytes());
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `<prefix>_` + 20 hex digits of the keyed BLAKE3 of `kind:id`: stable per share, opaque,
/// and uncorrelated across shares.
fn derive_key(secret: &[u8; 32], prefix: &str, kind: &str, id: &str) -> String {
    let hash = blake3::keyed_hash(secret, format!("{kind}:{id}").as_bytes());
    format!("{prefix}_{}", &hash.to_hex()[..20])
}

fn web_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

fn kind_of(mime: &str) -> &'static str {
    match mime.split('/').next() {
        Some("image") => "image",
        Some("video") => "video",
        _ => "other",
    }
}

/// The share's assets and folders with the field filter applied, plus its blobs.
#[allow(clippy::too_many_arguments)]
fn build_manifest(
    resolved: &Resolved,
    tiers: &[Option<Tiers>],
    secret: &[u8; 32],
    fields: ShareFields,
    title: &str,
    description: Option<String>,
    excluded: &mut Excluded,
    progress: Progress,
) -> Result<(ShareManifest, Vec<Blob>)> {
    let mut blobs: BTreeMap<String, Blob> = BTreeMap::new();
    let mut assets = Vec::with_capacity(resolved.members.len());
    let mut keys: HashMap<&str, String> = HashMap::new();
    let total = resolved.members.len() as u64;
    for (i, (member, tiers)) in resolved.members.iter().zip(tiers).enumerate() {
        progress("hash", i as u64, total);
        let Some(tiers) = tiers else {
            excluded.missing_tier += 1;
            continue;
        };
        let asset = &member.asset;
        let grid = file_blob(&tiers.grid, "grid")?;
        let (mut width, mut height) = (asset.width.unwrap_or(0), asset.height.unwrap_or(0));
        if width == 0 || height == 0 {
            // Aspect is what the viewer lays out with; the grid tier has it.
            let (w, h) = image::ImageReader::open(&tiers.grid)?
                .with_guessed_format()?
                .into_dimensions()?;
            (width, height) = (w, h);
        }
        let hires = tiers
            .hires
            .as_deref()
            .map(|p| file_blob(p, "hires"))
            .transpose()?;
        let micro = tiers
            .micro
            .as_ref()
            .map(|m| blob(m, "micro", BlobSource::Memory(m.clone())))
            .transpose()
            .unwrap_or(None);
        let key = derive_key(secret, "k", "asset", &asset.id);
        keys.insert(asset.id.as_str(), key.clone());
        let m = &asset.metadata;
        let ext = storage::original_extension(&asset.original)
            .trim_start_matches('.')
            .to_ascii_lowercase();
        let text = |s: &Option<String>| s.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
        assets.push(ShareAsset {
            key,
            name: fields.name.then(|| asset.name.clone()),
            ext,
            kind: kind_of(&asset.original.mime).into(),
            width,
            height,
            placeholder: m.colors.first().map(|c| c.hex.clone()),
            micro: micro.as_ref().map(|b| b.hash.clone()),
            grid: grid.hash.clone(),
            hires: hires.as_ref().map(|b| b.hash.clone()),
            notes: if fields.notes { text(&m.description) } else { None },
            tags: (fields.tags && !m.tags.is_empty()).then(|| m.tags.clone()),
            source_url: if fields.source_url {
                m.source_url.clone().filter(|u| web_url(u))
            } else {
                None
            },
            rating: (fields.rating && m.rating > 0).then_some(m.rating),
            colors: (fields.colors && !m.colors.is_empty()).then(|| m.colors.clone()),
            imported_at: fields.dates.then(|| asset.created_at.clone()),
        });
        for b in [Some(grid), hires, micro].into_iter().flatten() {
            blobs.entry(b.hash.clone()).or_insert(b);
        }
    }
    progress("hash", total, total);
    let folder_keys: HashMap<&str, String> = resolved
        .folders
        .iter()
        .map(|(c, _)| (c.id.as_str(), derive_key(secret, "f", "folder", &c.id)))
        .collect();
    let folders = resolved
        .folders
        .iter()
        .map(|(c, parent)| ShareFolder {
            key: folder_keys[c.id.as_str()].clone(),
            name: c.name.clone(),
            parent: parent.as_deref().and_then(|p| folder_keys.get(p).cloned()),
            description: if fields.notes {
                c.description.clone().filter(|d| !d.trim().is_empty())
            } else {
                None
            },
            assets: resolved
                .members
                .iter()
                .filter(|m| m.folders.contains(&c.id))
                .filter_map(|m| keys.get(m.asset.id.as_str()).cloned())
                .collect(),
        })
        .collect();
    let manifest = ShareManifest {
        format: SHARE_FORMAT.into(),
        version: SHARE_VERSION,
        share: ShareInfo {
            title: title.to_owned(),
            description,
            layout: "waterfall".into(),
            sort: "manual".into(),
            fields,
            updated_at: now(),
        },
        folders,
        assets,
    };
    Ok((manifest, blobs.into_values().collect()))
}

// ---------------------------------------------------------------------------------------
// Records

fn load_records(library: &Library) -> Result<Vec<ShareRecord>> {
    match fs::read(library.state_dir.join(RECORDS)) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).context("invalid shares.json")?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(e.into()),
    }
}

fn save_record(library: &Library, record: &ShareRecord) -> Result<()> {
    let mut records = load_records(library)?;
    match records.iter_mut().find(|r| r.id == record.id && r.host == record.host) {
        Some(existing) => *existing = record.clone(),
        None => records.push(record.clone()),
    }
    atomic_json(&library.state_dir.join(RECORDS), &records)
}

// ---------------------------------------------------------------------------------------
// Operations

/// The shared folder's description: an exploration's brief and findings always, a plain
/// folder's only when notes are shared.
fn default_description(resolved: &Resolved, fields: ShareFields) -> Option<String> {
    (resolved.kind == "exploration" || fields.notes)
        .then(|| resolved.description.clone())
        .flatten()
}

/// Seconds from now, `None` for never.
pub fn parse_ttl(ttl: &str) -> Result<Option<i64>> {
    let ttl = ttl.trim();
    if ttl == "never" {
        return Ok(None);
    }
    let (number, unit) = ttl.split_at(ttl.len().saturating_sub(1));
    let n: i64 = number.parse().with_context(|| format!("invalid ttl {ttl:?} (1d, 7d, 30d, never, or <n>s|m|h|d)"))?;
    let seconds = n
        * match unit {
            "s" => 1,
            "m" => 60,
            "h" => 3600,
            "d" => 86_400,
            _ => anyhow::bail!("invalid ttl {ttl:?} (1d, 7d, 30d, never, or <n>s|m|h|d)"),
        };
    ensure!((60..=3650 * 86_400).contains(&seconds), "ttl must be between 60 s and 10 years");
    Ok(Some(seconds))
}

/// `share_plan`: what a share of `scope` would publish; never contacts the share host.
pub fn plan(library: &mut Library, scope: &ShareScope) -> Result<Value> {
    let resolved = resolve(library, scope)?;
    let local = local_tiers(library, &resolved.members)?;
    let replica = library.remote().is_some();
    let mut excluded = resolved.excluded.clone();
    let (mut grid, mut hires, mut micro, mut fetch) = (0u64, 0u64, 0u64, 0u64);
    let mut assets = 0u64;
    let size = |p: &Option<PathBuf>| p.as_ref().and_then(|p| fs::metadata(p).ok()).map_or(0, |m| m.len());
    for (g, h, m) in &local {
        match g {
            Some(_) => assets += 1,
            None if replica => {
                assets += 1;
                fetch += 1;
            }
            None => excluded.missing_tier += 1,
        }
        grid += size(g);
        hires += size(h);
        micro += m.as_ref().map_or(0, |m| m.len() as u64);
        if replica && g.is_some() && h.is_none() {
            fetch += 1;
        }
    }
    Ok(json!({
        "title": resolved.title,
        "scope_label": resolved.label(),
        "assets": assets,
        "folders": resolved.folders.len(),
        "excluded": excluded,
        "bytes": {"micro": micro, "grid": grid, "hires": hires, "total": micro + grid + hires},
        "to_fetch": fetch,
    }))
}

/// `share_create`: creates the share on the host (or resumes an unfinished one with the
/// same scope, title and fields), uploads what the host lacks and publishes the manifest.
pub fn create(home: &Path, library: &mut Library, options: &ShareOptions, progress: Progress) -> Result<Value> {
    let scope = options.scope.as_ref().context("scope is required")?;
    let fields = options.fields.unwrap_or_default();
    let resolved = resolve(library, scope)?;
    ensure!(!resolved.members.is_empty(), "nothing to share: the scope has no shareable items");
    ensure!(
        resolved.members.len() <= MAX_SHARE_ASSETS,
        "a share holds at most {MAX_SHARE_ASSETS} items"
    );
    let title = options
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or(&resolved.title)
        .to_owned();
    let ttl = parse_ttl(options.ttl.as_deref().unwrap_or("30d"))?;
    let expires_at = ttl.map(|s| Utc::now().timestamp() + s);
    let password = options.password.as_deref().filter(|p| !p.is_empty());
    if let Some(password) = password {
        ensure!((4..=256).contains(&password.chars().count()), "password must be 4–256 characters");
    }
    let description = options.description.clone().or_else(|| default_description(&resolved, fields));
    let client = signed_in_client(home, options.host.as_deref())?;
    let resumable = load_records(library)?.into_iter().find(|r| {
        r.host == client.host
            && r.published_version == 0
            && r.status == "uploading"
            && r.scope == *scope
            && r.title == title
            && r.fields == fields
            && r.protected == password.is_some()
    });
    let record = match resumable {
        Some(record) => {
            // The same snapshot again: keep its keys; apply this run's expiry/password.
            let mut patch = json!({"expires_at": expires_at});
            if let Some(password) = password {
                patch["password"] = json!(password);
            }
            match client.json("PATCH", &format!("/shares/{}", record.id), Some(Body::Json(&patch))) {
                Ok(_) => Some(record),
                // Revoked or expired meanwhile: start over.
                Err(e) if format!("{e:#}").starts_with("not_found") => None,
                Err(e) => return Err(e),
            }
        }
        None => None,
    };
    let mut record = match record {
        Some(mut record) => {
            record.expires_at = expires_at;
            record
        }
        None => {
            let actor = current_actor();
            let body = json!({
                "library_id": library.context.library_id,
                "title": title,
                "description": description,
                "scope_label": resolved.label(),
                "expires_at": expires_at,
                "password": password,
                "created_by": if actor == "local" { "owner".to_owned() } else { actor },
            });
            let created = client.json("POST", "/shares", Some(Body::Json(&body)))?;
            let record = ShareRecord {
                id: created["id"].as_str().context("host returned no share id")?.into(),
                host: client.host.clone(),
                url: created["url"].as_str().context("host returned no url")?.into(),
                slug: created["slug"].as_str().unwrap_or_default().into(),
                title: title.clone(),
                scope: scope.clone(),
                fields,
                key_secret: new_secret(),
                created_at: now(),
                expires_at,
                protected: password.is_some(),
                published_version: 0,
                asset_count: 0,
                bytes: 0,
                status: "uploading".into(),
            };
            save_record(library, &record)?;
            record
        }
    };
    publish_resolved(library, &client, &mut record, &resolved, description, progress)
}

/// `share_publish`: uploads the record's scope again as a new manifest version (resumes a
/// create that stopped before its manifest).
pub fn publish(home: &Path, library: &mut Library, id: &str, host: Option<&str>, progress: Progress) -> Result<Value> {
    let client = signed_in_client(home, host)?;
    let mut record = load_records(library)?
        .into_iter()
        .find(|r| r.id == id && r.host == client.host)
        .with_context(|| format!("no local record of share {id} in this library for {}", client.host))?;
    let resolved = resolve(library, &record.scope)?;
    let description = default_description(&resolved, record.fields);
    publish_resolved(library, &client, &mut record, &resolved, description, progress)
}

fn publish_resolved(
    library: &mut Library,
    client: &HostClient,
    record: &mut ShareRecord,
    resolved: &Resolved,
    description: Option<String>,
    progress: Progress,
) -> Result<Value> {
    let secret = key_bytes(&record.key_secret)?;
    let tiers = tiers(library, &resolved.members, progress)?;
    let mut excluded = resolved.excluded.clone();
    let (manifest, blobs) = build_manifest(
        resolved,
        &tiers,
        &secret,
        record.fields,
        &record.title,
        description,
        &mut excluded,
        progress,
    )?;
    ensure!(!manifest.assets.is_empty(), "nothing to share: no item has a grid thumbnail yet (vamp thumbs regenerate)");
    // Which blobs the host lacks (content-addressed: earlier runs' uploads count).
    let mut missing: HashSet<String> = HashSet::new();
    for chunk in blobs.chunks(PLAN_CHUNK) {
        let list: Vec<ShareBlob> = chunk
            .iter()
            .map(|b| ShareBlob {
                hash: b.hash.clone(),
                bytes: b.bytes,
                mime: b.mime.into(),
                tier: b.tier.into(),
            })
            .collect();
        let answer = client.json(
            "POST",
            &format!("/shares/{}/plan", record.id),
            Some(Body::Json(&json!({ "blobs": list }))),
        )?;
        for hash in answer["missing"].as_array().context("host plan has no missing list")? {
            missing.insert(hash.as_str().context("invalid missing hash")?.to_owned());
        }
    }
    let todo: Vec<&Blob> = blobs.iter().filter(|b| missing.contains(&b.hash)).collect();
    let total = todo.len() as u64;
    let done = AtomicU64::new(0);
    let sent = AtomicU64::new(0);
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let failure: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    progress("upload", 0, total);
    std::thread::scope(|scope| {
        for _ in 0..UPLOAD_WORKERS.min(todo.len()) {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, SeqCst);
                    let Some(blob) = todo.get(i) else { break };
                    if stop.load(SeqCst) {
                        break;
                    }
                    let result = blob.read().and_then(|bytes| {
                        ensure!(
                            blob_id(&bytes) == blob.hash,
                            "derivative changed while sharing; run the share again"
                        );
                        let (status, value) = client.call(
                            "PUT",
                            &format!("/blobs/{}", blob.hash),
                            Some(Body::Bytes(&bytes, blob.mime)),
                            None,
                        )?;
                        check(status, value, &client.host)?;
                        sent.fetch_add(bytes.len() as u64, SeqCst);
                        Ok(())
                    });
                    match result {
                        Ok(()) => progress("upload", done.fetch_add(1, SeqCst) + 1, total),
                        Err(e) => {
                            stop.store(true, SeqCst);
                            failure.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(e);
                            break;
                        }
                    }
                }
            });
        }
    });
    if let Some(e) = failure.into_inner().unwrap_or_else(|e| e.into_inner()) {
        return Err(e.context(format!(
            "upload stopped after {} of {total} new files; run the same share again (or vamp share publish {}) to resume",
            done.load(SeqCst),
            record.id
        )));
    }
    progress("manifest", 0, 1);
    let body = serde_json::to_value(&manifest)?;
    let mut version = record.published_version;
    let mut published = None;
    for _ in 0..2 {
        let (status, value) = client.call(
            "PUT",
            &format!("/shares/{}/manifest", record.id),
            Some(Body::Json(&body)),
            Some(version),
        )?;
        if status == 412 {
            // Another publisher (or an earlier run whose answer was lost) moved it on.
            let current = client.json("GET", &format!("/shares/{}", record.id), None)?;
            version = current["manifest_version"].as_u64().unwrap_or(version);
            continue;
        }
        published = Some(check(status, value, &client.host)?);
        break;
    }
    let published = published.context("the share's manifest kept changing on the host; run the share again")?;
    record.published_version = published["manifest_version"].as_u64().unwrap_or(version + 1);
    record.asset_count = manifest.assets.len() as u64;
    record.bytes = published["bytes"].as_u64().unwrap_or_else(|| blobs.iter().map(|b| b.bytes).sum());
    record.status = published["status"].as_str().unwrap_or("live").into();
    save_record(library, record)?;
    progress("done", 1, 1);
    Ok(json!({
        "id": record.id,
        "url": record.url,
        "slug": record.slug,
        "status": record.status,
        "title": record.title,
        "expires_at": record.expires_at,
        "protected": record.protected,
        "asset_count": record.asset_count,
        "bytes": record.bytes,
        "manifest_version": record.published_version,
        "excluded": excluded,
        "uploaded": {"blobs": total, "bytes": sent.load(SeqCst), "already_on_host": blobs.len() as u64 - total},
    }))
}

/// `share_list`: the account's shares on the host; with a library, those it published
/// carry `local: true` (their records are refreshed with the host's status).
pub fn list(home: &Path, library: Option<&Library>, host: Option<&str>) -> Result<Value> {
    let client = signed_in_client(home, host)?;
    let mut answer = client.json("GET", "/shares", None)?;
    if let (Some(library), Some(shares)) = (library, answer["shares"].as_array_mut()) {
        let records = load_records(library)?;
        for share in shares {
            let Some(id) = share["id"].as_str() else { continue };
            if let Some(record) = records.iter().find(|r| r.id == id && r.host == client.host) {
                share["local"] = json!(true);
                if let Some(status) = share["status"].as_str().filter(|s| *s != record.status) {
                    let mut record = record.clone();
                    record.status = status.to_owned();
                    save_record(library, &record)?;
                }
            }
        }
    }
    Ok(answer)
}

/// `share_revoke`: the host stops serving the share at once (media within its cache bound).
pub fn revoke(home: &Path, library: Option<&Library>, id: &str, host: Option<&str>) -> Result<Value> {
    let client = signed_in_client(home, host)?;
    let answer = client.json("DELETE", &format!("/shares/{id}"), None)?;
    if let Some(library) = library {
        if let Some(mut record) = load_records(library)?.into_iter().find(|r| r.id == id && r.host == client.host) {
            record.status = "revoked".into();
            save_record(library, &record)?;
        }
    }
    Ok(json!({"id": id, "status": answer["status"].as_str().unwrap_or("revoked")}))
}

// ---------------------------------------------------------------------------------------
// Progress of running creates (native op `share_progress`)

static PROGRESS: LazyLock<Mutex<HashMap<String, Value>>> = LazyLock::new(Default::default);

pub fn set_progress(transfer_id: &str, phase: &str, done: u64, total: u64) {
    PROGRESS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(transfer_id.to_owned(), json!({"phase":phase,"done":done,"total":total}));
}

pub fn progress(transfer_id: &str) -> Value {
    PROGRESS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(transfer_id)
        .cloned()
        .unwrap_or_else(|| json!({"phase":"unknown","done":0,"total":0}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    fn png(seed: u8) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(8, 6, image::Rgb([seed, 10, 20]));
        let mut out = std::io::Cursor::new(Vec::new());
        image.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    /// Gives `asset` a grid tier (and a hires tier when asked), as `vamp thumbs` would.
    fn tiers_for(library: &Library, asset: &Asset, seed: u8, hires: bool) {
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(80, 60, image::Rgb([seed, 1, 2])));
        let bytes = crate::thumbs::webp(&image, 80.0).unwrap();
        let dir = library.state_dir.join("representations/grid");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{}.webp", asset.id)), &bytes).unwrap();
        library
            .conn
            .execute(
                "INSERT INTO grid_thumbs(asset_id,source,path,byte_size,tier_version) VALUES(?1,?2,?3,?4,3)",
                params![asset.id, asset.original.source_key(), format!("representations/grid/{}.webp", asset.id), bytes.len() as i64],
            )
            .unwrap();
        if hires {
            let dir = library.state_dir.join("representations/hires");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(format!("{}.webp", asset.id)), &bytes).unwrap();
            library
                .conn
                .execute(
                    "INSERT INTO hires_thumbs(asset_id,source,path,byte_size,width,height) VALUES(?1,?2,?3,?4,80,60)",
                    params![asset.id, asset.original.source_key(), format!("representations/hires/{}.webp", asset.id), bytes.len() as i64],
                )
                .unwrap();
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        home: PathBuf,
        library: Library,
        root: Collection,
        sub: Collection,
        /// public, noted, private-tagged, trashed, in the sub-folder.
        assets: Vec<Asset>,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let mut library = Library::init(&home, "test", None, None).unwrap();
        let root = library.collection_create("Brutalist stairs", None).unwrap();
        let sub = library.collection_create("Ramps", Some(&root.id)).unwrap();
        let meta = |description: Option<&str>, tags: &[&str], url: Option<&str>| Metadata {
            description: description.map(str::to_owned),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            source_url: url.map(str::to_owned),
            colors: vec![ColorSwatch { hex: "#7a6f64".into(), ratio: 0.5 }],
            extra: BTreeMap::from([("original_error".to_owned(), json!("hidden"))]),
            ..Default::default()
        };
        let specs = [
            ("stair-01", meta(None, &["concrete"], Some("https://example.com/stairs"))),
            ("stair-02", meta(Some("secret note"), &["concrete"], Some("file:///Users/someone/stair.jpg"))),
            ("stair-03", meta(None, &["Private"], None)),
            ("stair-04", meta(None, &[], None)),
            ("ramp-01", meta(None, &[], None)),
        ];
        let mut assets = Vec::new();
        for (i, (name, metadata)) in specs.into_iter().enumerate() {
            let mut asset = library.ingest_bytes(&png(i as u8 * 40), &format!("{name}.png"), metadata).unwrap();
            asset = library
                .update(&asset.id, MetadataPatch { name: Some(name.into()), ..Default::default() }, None)
                .unwrap();
            let folder = if i == 4 { &sub } else { &root };
            library.collection_add(&folder.id, &asset.id).unwrap();
            tiers_for(&library, &asset, i as u8, i != 1);
            assets.push(asset);
        }
        library.trash(&[assets[3].id.clone()], None).unwrap();
        Fixture { _dir: dir, home, library, root, sub, assets }
    }

    fn folder_scope(f: &Fixture) -> ShareScope {
        ShareScope::Folder { collection_id: f.root.id.clone(), include_subfolders: true }
    }

    #[test]
    fn resolves_scopes_with_hard_exclusions() {
        let mut f = fixture();
        let resolved = resolve(&f.library, &folder_scope(&f)).unwrap();
        let names: Vec<_> = resolved.members.iter().map(|m| m.asset.name.as_str()).collect();
        assert_eq!(names, ["stair-01", "stair-02", "ramp-01"]);
        assert_eq!((resolved.excluded.trash, resolved.excluded.never_share), (1, 1));
        assert_eq!(resolved.label(), "Folder · Brutalist stairs (3)");
        assert_eq!(resolved.folders.len(), 1);
        let flat = resolve(&f.library, &ShareScope::Folder { collection_id: f.root.id.clone(), include_subfolders: false }).unwrap();
        assert_eq!(flat.members.len(), 2);
        let ids = vec![f.assets[2].id.clone(), f.assets[0].id.clone()];
        let selection = resolve(&f.library, &ShareScope::Selection { asset_ids: ids }).unwrap();
        assert_eq!(selection.members.len(), 1);
        assert_eq!(selection.excluded.never_share, 1);
        let scope = folder_scope(&f);
        let plan = plan(&mut f.library, &scope).unwrap();
        assert_eq!(plan["assets"], 3);
        assert_eq!(plan["excluded"], json!({"trash":1,"never_share":1,"missing_tier":0}));
        assert!(plan["bytes"]["grid"].as_u64().unwrap() > 0);
    }

    #[test]
    fn manifest_carries_only_shared_fields_and_opaque_keys() {
        let mut f = fixture();
        let scope = folder_scope(&f);
        let resolved = resolve(&f.library, &scope).unwrap();
        let tiers = tiers(&mut f.library, &resolved.members, &|_, _, _| {}).unwrap();
        let secret = key_bytes(&new_secret()).unwrap();
        let mut excluded = Excluded::default();
        let (manifest, blobs) = build_manifest(&resolved, &tiers, &secret, ShareFields::default(), "T", None, &mut excluded, &|_, _, _| {}).unwrap();
        let text = serde_json::to_string(&manifest).unwrap();
        for asset in &f.assets {
            assert!(!text.contains(&asset.id), "asset ids never leave the machine");
        }
        for hidden in [f.root.id.as_str(), f.sub.id.as_str(), "secret note", "file://", "original_error", "concrete", "/home", "inbox"] {
            assert!(!text.contains(hidden), "{hidden} leaked into {text}");
        }
        assert_eq!(manifest.assets[0].name.as_deref(), Some("stair-01"));
        assert_eq!(manifest.assets[0].placeholder.as_deref(), Some("#7a6f64"));
        assert!(manifest.assets[0].hires.is_some() && manifest.assets[1].hires.is_none());
        assert_eq!(manifest.folders.len(), 1);
        assert_eq!(manifest.folders[0].parent, None);
        assert_eq!(manifest.folders[0].assets, [manifest.assets[2].key.clone()]);
        assert!(blobs.iter().all(|b| b.hash.starts_with("sha256:") && b.mime == "image/webp"));

        let all = ShareFields { name: false, notes: true, tags: true, source_url: true, rating: true, colors: true, dates: true };
        let (manifest, _) = build_manifest(&resolved, &tiers, &secret, all, "T", None, &mut excluded, &|_, _, _| {}).unwrap();
        assert_eq!(manifest.assets[0].name, None);
        assert_eq!(manifest.assets[0].source_url.as_deref(), Some("https://example.com/stairs"));
        assert_eq!(manifest.assets[1].notes.as_deref(), Some("secret note"));
        assert_eq!(manifest.assets[1].source_url, None, "non-web links are never shared");
        assert_eq!(manifest.assets[0].tags.as_deref(), Some(&["concrete".to_owned()][..]));
    }

    /// A minimal share host: records requests, stores blobs, and fails the `fail_put`-th blob PUT once.
    struct MockHost {
        url: String,
        state: std::sync::Arc<Mutex<HostState>>,
    }
    #[derive(Default)]
    struct HostState {
        created: u32,
        blobs: HashSet<String>,
        puts: u32,
        fail_put: Option<u32>,
        version: u64,
        manifest: Option<Value>,
        auth: Vec<String>,
    }

    fn mock_host(fail_put: Option<u32>) -> MockHost {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let state = std::sync::Arc::new(Mutex::new(HostState { fail_put, ..Default::default() }));
        let shared = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let state = shared.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        let mut parts = line.split_whitespace();
                        let (method, path) = (parts.next().unwrap_or("").to_owned(), parts.next().unwrap_or("").to_owned());
                        let (mut length, mut auth, mut if_match) = (0usize, String::new(), None);
                        loop {
                            let mut header = String::new();
                            reader.read_line(&mut header).unwrap();
                            let header = header.trim_end();
                            if header.is_empty() {
                                break;
                            }
                            let (k, v) = header.split_once(':').unwrap();
                            match k.to_ascii_lowercase().as_str() {
                                "content-length" => length = v.trim().parse().unwrap(),
                                "authorization" => auth = v.trim().to_owned(),
                                "if-match" => if_match = Some(v.trim().parse::<u64>().unwrap()),
                                _ => {}
                            }
                        }
                        let mut body = vec![0u8; length];
                        reader.read_exact(&mut body).unwrap();
                        let mut s = state.lock().unwrap();
                        s.auth.push(auth);
                        let (status, answer) = match (method.as_str(), path.as_str()) {
                            ("POST", "/api/v1/shares") => {
                                s.created += 1;
                                (201, json!({"id":"shr_1","slug":"S","url":"https://host.test/s/S","status":"uploading"}))
                            }
                            ("PATCH", "/api/v1/shares/shr_1") => (200, json!({"id":"shr_1"})),
                            ("GET", "/api/v1/shares") => (200, json!({"shares":[{"id":"shr_1","status":"live"}]})),
                            ("GET", "/api/v1/shares/shr_1") => (200, json!({"id":"shr_1","manifest_version": s.version})),
                            ("POST", "/api/v1/shares/shr_1/plan") => {
                                let request: Value = serde_json::from_slice(&body).unwrap();
                                let missing: Vec<_> = request["blobs"]
                                    .as_array()
                                    .unwrap()
                                    .iter()
                                    .map(|b| b["hash"].as_str().unwrap().to_owned())
                                    .filter(|h| !s.blobs.contains(h))
                                    .collect();
                                (200, json!({ "missing": missing }))
                            }
                            ("PUT", p) if p.starts_with("/api/v1/blobs/") => {
                                s.puts += 1;
                                if s.fail_put == Some(s.puts) {
                                    s.fail_put = None;
                                    (400, json!({"error":"bad_request","message":"simulated"}))
                                } else {
                                    let hash = p.trim_start_matches("/api/v1/blobs/").to_owned();
                                    assert_eq!(blob_id(&body), hash, "uploaded bytes match their id");
                                    s.blobs.insert(hash);
                                    (201, json!({}))
                                }
                            }
                            ("PUT", "/api/v1/shares/shr_1/manifest") => {
                                if if_match != Some(s.version) {
                                    (412, json!({"error":"version_mismatch"}))
                                } else {
                                    s.version += 1;
                                    s.manifest = Some(serde_json::from_slice(&body).unwrap());
                                    (200, json!({"manifest_version": s.version, "status":"live", "bytes": 1}))
                                }
                            }
                            ("DELETE", "/api/v1/shares/shr_1") => (200, json!({"status":"revoked"})),
                            _ => (404, json!({"error":"not_found"})),
                        };
                        drop(s);
                        let text = answer.to_string();
                        let reply = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{text}", text.len());
                        if stream.write_all(reply.as_bytes()).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        MockHost { url, state }
    }

    #[test]
    fn create_resumes_after_an_interrupted_upload_and_revokes() {
        let mut f = fixture();
        let host = mock_host(Some(3));
        let home = f.home.clone();
        let account = login(&home, "vat_test_token", Some(&host.url)).unwrap();
        assert_eq!(account["signed_in"], true);
        assert_eq!(account["token_hint"], "vat_test…");
        let options = ShareOptions {
            scope: Some(folder_scope(&f)),
            ttl: Some("7d".into()),
            ..Default::default()
        };
        let error = create(&home, &mut f.library, &options, &|_, _, _| {}).unwrap_err();
        assert!(format!("{error:#}").contains("run the same share again"), "{error:#}");
        let uploaded_before = host.state.lock().unwrap().blobs.len();
        let result = create(&home, &mut f.library, &options, &|_, _, _| {}).unwrap();
        let state = host.state.lock().unwrap();
        assert_eq!(state.created, 1, "the interrupted share is resumed, not created again");
        assert_eq!(result["uploaded"]["already_on_host"].as_u64().unwrap() as usize, uploaded_before);
        assert_eq!(result["asset_count"], 3);
        assert_eq!(result["url"], "https://host.test/s/S");
        assert!(state.auth.iter().all(|a| a == "Bearer vat_test_token"));
        let manifest: ShareManifest = serde_json::from_value(state.manifest.clone().unwrap()).unwrap();
        assert_eq!(manifest.assets.len(), 3);
        drop(state);
        let records = load_records(&f.library).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!((records[0].published_version, records[0].status.as_str()), (1, "live"));
        // A later identical create is a new share (the first one was published).
        create(&home, &mut f.library, &options, &|_, _, _| {}).unwrap();
        assert_eq!(host.state.lock().unwrap().created, 2);
        let revoked = revoke(&home, Some(&f.library), "shr_1", None).unwrap();
        assert_eq!(revoked["status"], "revoked");
        assert_eq!(load_records(&f.library).unwrap()[0].status, "revoked");
        // The token is only ever sent to the host it was saved for.
        assert!(token_for(&home, "https://elsewhere.example").unwrap().is_none());
    }


    #[test]
    fn ttl_parsing() {
        assert_eq!(parse_ttl("never").unwrap(), None);
        assert_eq!(parse_ttl("1d").unwrap(), Some(86_400));
        assert_eq!(parse_ttl("30d").unwrap(), Some(30 * 86_400));
        assert_eq!(parse_ttl("2m").unwrap(), Some(120));
        assert!(parse_ttl("30s").is_err());
        assert!(parse_ttl("7x").is_err());
        assert!(parse_ttl("").is_err());
    }

    #[test]
    fn hosts_must_be_https_origins() {
        assert_eq!(normalize_host("https://app.vamp.sh/").unwrap(), "https://app.vamp.sh");
        assert_eq!(normalize_host("http://127.0.0.1:8787").unwrap(), "http://127.0.0.1:8787");
        assert!(normalize_host("http://example.com").is_err());
        assert!(normalize_host("https://example.com/x").is_err());
        assert!(normalize_host("http://localhost.evil.com").is_err());
    }

    #[test]
    fn keys_are_stable_per_share_and_uncorrelated_across_shares() {
        let a = key_bytes(&new_secret()).unwrap();
        let b = key_bytes(&new_secret()).unwrap();
        let id = "0b5c3d1e-0000-4000-8000-000000000000";
        let ka = derive_key(&a, "k", "asset", id);
        assert_eq!(ka, derive_key(&a, "k", "asset", id));
        assert_ne!(ka, derive_key(&b, "k", "asset", id));
        assert_ne!(ka, derive_key(&a, "f", "folder", id)[..]);
        assert!(ka.starts_with("k_") && ka.len() == 22 && !ka.contains(id));
    }

    #[test]
    fn sniffs_only_image_types() {
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert_eq!(sniff(b"<svg xmlns="), None);
        assert_eq!(sniff(b"<html>"), None);
    }
}
