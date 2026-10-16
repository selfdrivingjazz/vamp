//! VAMP's versioned, language-independent wire types. Paths are locators, not IDs.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const PROTOCOL_VERSION: &str = "0.1";

/// Original bytes. `path` is the authority-local copy; empty when evicted to cloud storage
/// (`storage == "cloud"`). `locator` names the durable cloud copy once uploaded.
///
/// Lazy fingerprint: references cataloged without reading bytes (Eagle import) have
/// `hash_state == "pending"`, an empty `blake3`, and `modified_ns` from `stat`. Their source
/// key (see [`Original::source_key`]) is `pending:<byte_size>:<modified_ns>` until the bytes
/// are first read, when the digest is computed and every source-keyed record is rekeyed.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Original {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub blake3: String,
    pub byte_size: u64,
    pub mime: String,
    /// `s3://bucket/key` or `folder://<backend>/<relpath>`; absent until stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locator: Option<String>,
    /// local, uploading, cloud, or cloud+local. Catalogs predating cloud storage read as local.
    #[serde(default = "local_storage")]
    pub storage: String,
    /// `hashed` (blake3 is the digest of the bytes) or `pending` (not yet read).
    #[serde(default = "hashed")]
    pub hash_state: String,
    /// Filesystem modification time (ns since epoch) recorded with the fingerprint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_ns: Option<i64>,
}
impl Original {
    pub fn pending(&self) -> bool {
        self.hash_state == "pending"
    }
    /// Identity of the source bytes for derived records (representations, observations,
    /// embeddings, jobs, uploads): the blake3 digest, or `pending:<size>:<mtime_ns>`.
    pub fn source_key(&self) -> String {
        if self.pending() {
            format!(
                "pending:{}:{}",
                self.byte_size,
                self.modified_ns.unwrap_or_default()
            )
        } else {
            self.blake3.clone()
        }
    }
}
fn local_storage() -> String {
    "local".into()
}
fn hashed() -> String {
    "hashed".into()
}
/// One JSON request on stdin to the `vamp-store` S3 helper; one `StoreResponse` on stdout.
/// Credentials come from the environment (AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY,
/// optional AWS_SESSION_TOKEN), never from the request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StoreRequest {
    /// put, get, head, delete, or presign.
    pub operation: String,
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub key: String,
    #[serde(default)]
    pub path_style: bool,
    /// Local file: source for put, destination for get.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Stored as object metadata `x-amz-meta-blake3` on put.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blake3: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_seconds: Option<u64>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct StoreResponse {
    /// head: whether the object exists.
    #[serde(default)]
    pub exists: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blake3: Option<String>,
    /// presign: time-limited GET URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}
/// One dominant color of an image and the fraction of pixels it covers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ColorSwatch {
    /// `#rrggbb`, lowercase.
    pub hex: String,
    /// 0–1.
    pub ratio: f64,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Metadata {
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub source_url: Option<String>,
    pub source_id: Option<String>,
    /// 0 (unrated) – 5.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub rating: u8,
    /// Dominant palette, largest ratio first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub colors: Vec<ColorSwatch>,
    #[serde(default)]
    pub extra: BTreeMap<String, Value>,
}
fn is_zero(value: &u8) -> bool {
    *value == 0
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Asset {
    pub id: String,
    pub revision: u64,
    pub name: String,
    pub original: Original,
    pub metadata: Metadata,
    /// When the asset entered this catalog ("Date Imported").
    pub created_at: String,
    pub updated_at: String,
    /// Pixel dimensions of the original when known (from the source catalog or a generated
    /// derivative); never obtained by reading the original during import.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// File creation / modification time of the original (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_modified_at: Option<String>,
    /// Set by native op `touch` (Recently Used).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_viewed_at: Option<String>,
    /// Set while the asset is in the trash; `restore` clears it, `empty_trash` tombstones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct MetadataPatch {
    pub name: Option<String>,
    pub description: Option<String>,
    /// Replaces all tags.
    pub tags: Option<Vec<String>>,
    pub source_url: Option<String>,
    /// 0–5.
    #[serde(default)]
    pub rating: Option<u8>,
    /// Added after `tags` is applied.
    #[serde(default)]
    pub add_tags: Option<Vec<String>>,
    /// Removed after `add_tags`.
    #[serde(default)]
    pub remove_tags: Option<Vec<String>>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Representation {
    pub kind: String,
    pub path: String,
    pub source_hash: String,
    pub width: u32,
    pub height: u32,
    pub mime: String,
    pub byte_size: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AssetView {
    pub asset: Asset,
    pub representations: Vec<Representation>,
    pub observations: Vec<Observation>,
    /// Model-suggested tags for the current original (from `tags` observations), excluding
    /// tags the user already has. Never merged into metadata.tags without accept_tags.
    #[serde(default)]
    pub suggested_tags: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Observation {
    pub id: String,
    pub asset_id: String,
    pub source_hash: String,
    pub processor: String,
    pub model: Option<String>,
    pub kind: String,
    pub value: Value,
    pub created_at: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Event {
    pub sequence: u64,
    pub id: String,
    pub library_id: String,
    pub kind: String,
    pub asset_id: Option<String>,
    pub revision: Option<u64>,
    pub actor: String,
    pub created_at: String,
    pub payload: Value,
}
/// A folder. Folders nest via `parent_id` and are ordered among siblings by `sort_order`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Collection {
    pub id: String,
    pub name: String,
    pub asset_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub sort_order: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Eagle `iconColor` (e.g. `red`) when imported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Foreign identity, e.g. `eagle:<folder id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
}
/// One smart-folder rule. Fields: tags, name, ext, rating, color, shape, width, height,
/// created, modified, imported, description, url, folder, size. See protocol/contract.json
/// `smart_folders` for operators.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SmartRule {
    pub field: String,
    pub op: String,
    #[serde(default)]
    pub value: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SmartRules {
    /// `all` or `any`.
    #[serde(rename = "match", default = "match_all")]
    pub match_: String,
    #[serde(default)]
    pub rules: Vec<SmartRule>,
}
fn match_all() -> String {
    "all".into()
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SmartFolder {
    pub id: String,
    pub name: String,
    pub rules: SmartRules,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub sort_order: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    /// Source rules VAMP could not translate (kept verbatim, ignored when matching).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unsupported: Vec<Value>,
}
/// Named tag group (Eagle `tagsGroups`). Tags remain plain strings on assets.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TagGroup {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Context {
    pub library_id: String,
    /// Catalog directory; config files may store a path relative to VAMP home.
    pub state_dir: String,
    pub original_root: Option<String>,
    /// Replica of a remote authority (vamp-server). Absent for local authority catalogs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<Remote>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Remote {
    pub url: String,
    /// Bearer token; empty when the authority admits this machine by Tailscale identity.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
    /// Authority path prefix -> local prefix where the same originals are reachable
    /// (e.g. a Google Drive mount); originals found there are read directly.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub drive_roots: BTreeMap<String, String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Config {
    pub current: Option<String>,
    #[serde(default)]
    pub contexts: BTreeMap<String, Context>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProcessorSpec {
    pub name: String,
    pub events: Vec<String>,
    /// Executable and arguments, invoked directly without a shell.
    pub command: Vec<String>,
    /// original, thumbnail, or preview. Missing inputs fail; no silent original fallback.
    pub input: String,
    /// observation, thumbnail, or preview.
    pub output: String,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    /// Who may execute jobs: `any` (default; local worker or a remote `vamp worker`),
    /// `local` (authority's own worker only) or `remote` (claimed via /v1/jobs/claim only).
    /// Remote placement requires `output: observation`.
    #[serde(default, skip_serializing_if = "Placement::is_any")]
    pub placement: Placement,
}
fn default_timeout() -> u64 {
    300
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Placement {
    #[default]
    Any,
    Local,
    Remote,
}
impl Placement {
    pub fn is_any(&self) -> bool {
        *self == Placement::Any
    }
}
/// A remote worker's time-bounded claim on a running job. Expired leases are reclaimed
/// (job back to pending, or failed after the attempt limit).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct JobLease {
    pub id: String,
    pub worker_id: String,
    pub expires_at: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Job {
    pub id: String,
    pub asset_id: String,
    pub source_hash: String,
    pub processor: ProcessorSpec,
    pub event_id: String,
    pub status: String,
    pub attempts: u32,
    pub error: Option<String>,
    pub created_at: String,
    /// Present while (and after) a remote worker holds the job; local runs never set it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<JobLease>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SheetItem {
    pub id: String,
    pub path: String,
}
/// One intake candidate checked by native `find_duplicates`: a file, raw bytes or a
/// precomputed digest, and the existing assets it duplicates.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DuplicateCandidate {
    /// Expanded file path (path inputs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// File name used for the `name_size` fallback (path inputs, or bytes with `filename`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// BLAKE3 of the candidate bytes; absent when they could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blake3: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_size: Option<u64>,
    /// Why the candidate could not be hashed (it is then reported without matches).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub matches: Vec<DuplicateMatch>,
}
/// An existing, non-tombstoned asset whose original matches a [`DuplicateCandidate`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DuplicateMatch {
    pub asset_id: String,
    /// `blake3`: identical bytes. `name_size`: the asset's fingerprint is still pending
    /// (no digest yet) and its byte size and file name equal the candidate's.
    pub basis: String,
    /// In the trash (restorable) rather than live.
    pub trashed: bool,
    /// The candidate path is this asset's original path (re-adding it returns this asset).
    pub same_path: bool,
    /// Folders containing the asset (empty = uncategorized).
    pub folder_ids: Vec<String>,
}
/// One JSON request on stdin, one JSON response on stdout; diagnostics use stderr.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ToolRequest {
    pub protocol_version: String,
    pub operation: String,
    pub input_path: Option<String>,
    pub output_path: Option<String>,
    pub max_edge: Option<u32>,
    #[serde(default)]
    pub items: Vec<SheetItem>,
    pub columns: Option<u32>,
    pub cell_size: Option<u32>,
    pub asset: Option<Asset>,
    pub library_id: Option<String>,
    pub event_id: Option<String>,
    /// Query text for operation `embed_text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}
impl Default for ToolRequest {
    fn default() -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION.into(),
            operation: "generate".into(),
            input_path: None,
            output_path: None,
            max_edge: None,
            items: vec![],
            columns: None,
            cell_size: None,
            asset: None,
            library_id: None,
            event_id: None,
            text: None,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RepresentationOutput {
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub mime: String,
    pub byte_size: u64,
    /// Decoded source dimensions (before downscaling), when the helper knows them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_height: Option<u32>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ObservationOutput {
    pub kind: String,
    pub model: Option<String>,
    pub value: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SheetTile {
    pub number: u32,
    pub asset_id: String,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ToolResponse {
    pub representation: Option<RepresentationOutput>,
    pub observation: Option<ObservationOutput>,
    pub tiles: Option<Vec<SheetTile>>,
    /// Operation `palette`: up to 8 dominant colors of `input_path`, largest first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub palette: Option<Vec<ColorSwatch>>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ErrorResponse {
    pub protocol_version: String,
    pub error: ErrorDetail,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
}

/// The format name of a [`ShareManifest`].
pub const SHARE_FORMAT: &str = "vamp-share";
/// The current [`ShareManifest`] version.
pub const SHARE_VERSION: u32 = 1;

/// Which metadata a share shows. A field that is off never leaves the publishing machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ShareFields {
    pub name: bool,
    /// The asset description.
    pub notes: bool,
    pub tags: bool,
    /// `http(s)` source URLs only.
    pub source_url: bool,
    pub rating: bool,
    /// Dominant palette.
    pub colors: bool,
    /// Date imported.
    pub dates: bool,
}
impl Default for ShareFields {
    fn default() -> Self {
        Self {
            name: true,
            notes: false,
            tags: false,
            source_url: false,
            rating: false,
            colors: true,
            dates: false,
        }
    }
}

/// What a share publishes, resolved on the publishing machine and never sent to the host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ShareScope {
    /// A folder and (by default) every folder below it.
    Folder {
        collection_id: String,
        #[serde(default = "default_true")]
        include_subfolders: bool,
    },
    /// An exploration folder (`vamp explore`) and its sub-folders.
    Exploration { slug: String },
    /// These assets, in this order.
    Selection { asset_ids: Vec<String> },
}
fn default_true() -> bool {
    true
}

/// `vamp-share` manifest v1: the public, scrubbed description of a published snapshot.
/// Library asset and folder ids never appear; keys are derived per share.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShareManifest {
    /// [`SHARE_FORMAT`].
    pub format: String,
    /// [`SHARE_VERSION`].
    pub version: u32,
    pub share: ShareInfo,
    /// Sub-folders of the shared folder (empty for selections and flat folders).
    pub folders: Vec<ShareFolder>,
    /// Every shared asset, in share order.
    pub assets: Vec<ShareAsset>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShareInfo {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `waterfall` | `justified` | `grid`.
    pub layout: String,
    /// `manual` (folder order) | `imported_desc` | `name`.
    pub sort: String,
    pub fields: ShareFields,
    /// RFC 3339.
    pub updated_at: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShareFolder {
    /// `f_` + 20 hex digits.
    pub key: String,
    pub name: String,
    /// Parent folder key; `None` directly below the shared folder.
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Keys of the shared assets filed in this folder, in folder order.
    pub assets: Vec<String>,
}
/// One shared asset. Blob ids are `sha256:<64 hex>` of the exact derivative bytes.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShareAsset {
    /// `k_` + 20 hex digits: keyed BLAKE3 of the asset id under the share's secret.
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Original extension without the dot, lowercase.
    pub ext: String,
    /// `image` | `video` | `other`.
    pub kind: String,
    pub width: u32,
    pub height: u32,
    /// `#rrggbb` (the dominant palette entry).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// ~40 px JPEG.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub micro: Option<String>,
    /// 800 px WebP.
    pub grid: String,
    /// 2048 px WebP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hires: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colors: Option<Vec<ColorSwatch>>,
    /// RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported_at: Option<String>,
}
/// One blob of a share plan (`POST /shares/{id}/plan`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ShareBlob {
    pub hash: String,
    pub bytes: u64,
    pub mime: String,
    /// `micro` | `grid` | `hires`.
    pub tier: String,
}
/// The publisher's local record of a share (`<state_dir>/shares.json`, 0600).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ShareRecord {
    /// The host's share id.
    pub id: String,
    pub host: String,
    pub url: String,
    pub slug: String,
    pub title: String,
    pub scope: ShareScope,
    pub fields: ShareFields,
    /// Hex 32 bytes deriving this share's asset and folder keys.
    pub key_secret: String,
    pub created_at: String,
    /// Unix seconds; `None` never expires.
    pub expires_at: Option<i64>,
    pub protected: bool,
    /// 0 until the first manifest is accepted.
    pub published_version: u64,
    pub asset_count: u64,
    pub bytes: u64,
    /// Last status the host reported.
    pub status: String,
}

pub fn schema() -> Value {
    serde_json::json!({
        "protocol_version": PROTOCOL_VERSION,
        "contract": serde_json::from_str::<Value>(include_str!("../protocol/contract.json")).expect("embedded protocol contract is valid JSON"),
        "asset": schemars::schema_for!(Asset),
        "asset_view": schemars::schema_for!(AssetView),
        "metadata_patch": schemars::schema_for!(MetadataPatch),
        "context": schemars::schema_for!(Context),
        "event": schemars::schema_for!(Event),
        "collection": schemars::schema_for!(Collection),
        "processor": schemars::schema_for!(ProcessorSpec),
        "job": schemars::schema_for!(Job),
        "smart_folder": schemars::schema_for!(SmartFolder),
        "tag_group": schemars::schema_for!(TagGroup),
        "tool_request": schemars::schema_for!(ToolRequest),
        "tool_response": schemars::schema_for!(ToolResponse),
        "store_request": schemars::schema_for!(StoreRequest),
        "store_response": schemars::schema_for!(StoreResponse),
        "duplicate_candidate": schemars::schema_for!(DuplicateCandidate),
        "share_manifest": schemars::schema_for!(ShareManifest),
        "share_scope": schemars::schema_for!(ShareScope),
        "share_record": schemars::schema_for!(ShareRecord),
        "error": schemars::schema_for!(ErrorResponse)
    })
}
