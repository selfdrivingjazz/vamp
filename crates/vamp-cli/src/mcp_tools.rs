//! MCP adapters over the core engine's native operations. No catalog logic lives here.
//!
//! Reads use the selected context; native writes use the engine's replica forwarding.
//! Attribution is thread scoped with `with_actor`, never a process environment mutation.
//! Images and imported files are bounded before being encoded into MCP/native messages.
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use vamp_core::{Library, native};
use vamp_protocol::MetadataPatch;

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;
const DEFAULT_IMAGE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_IMPORT_BYTES: u64 = 64 * 1024 * 1024;

fn text(description: &str) -> Value {
    json!({"type":"string","description":description,"maxLength":65536})
}
fn id() -> Value {
    json!({"type":"string","minLength":1,"maxLength":1024})
}
fn strings(description: &str) -> Value {
    json!({"type":"array","description":description,"maxItems":256,"items":{"type":"string","minLength":1,"maxLength":1024}})
}
fn boolean(description: &str) -> Value {
    json!({"type":"boolean","description":description})
}
fn limit() -> Value {
    json!({"type":"integer","minimum":1,"maximum":MAX_LIMIT,"default":DEFAULT_LIMIT})
}
fn offset() -> Value {
    json!({"type":"integer","minimum":0,"maximum":i64::MAX})
}
fn revision() -> Value {
    json!({"type":"integer","minimum":1,"description":"Optional optimistic concurrency guard from inspect."})
}
fn slug() -> Value {
    json!({"type":"string","pattern":"^[a-z0-9][a-z0-9-]*$","minLength":1,"maxLength":64})
}
fn added_by() -> Value {
    text("Filter to owner, agents, or actor:<exact actor>.")
}
fn schema(name: &str, description: &str, properties: Value, required: &[&str], read: bool) -> Value {
    json!({
        "name":name,"description":description,
        "inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},
        "annotations":{"readOnlyHint":read,"destructiveHint":false,"openWorldHint":true}
    })
}

/// The complete tool catalog. Read-only servers never advertise mutation tools.
pub fn schemas(read_only: bool) -> Vec<Value> {
    let mut tools = vec![
        schema("vamp_search", "Search library metadata, or use semantic/hybrid ranking when a text embedder and image embeddings are available. Semantic errors are returned, not hidden by a metadata fallback.", json!({
            "query":{"type":"string","minLength":1,"maxLength":4096},
            "mode":{"type":"string","enum":["metadata","semantic","hybrid"],"default":"metadata"},
            "limit":limit(),"offset":offset(),"added_by":added_by()
        }), &["query"], true),
        schema("vamp_browse", "Browse items with an optional query restricted to folders, a smart folder, tags, and other core browse filters. Semantic ranking uses the same scope when configured; metadata search is the default.", json!({
            "query":{"type":"string","maxLength":4096},"collection_id":id(),"folder_ids":strings("Folder IDs to include."),
            "include_subfolders":boolean("Include descendants of selected folders."),"smart_folder_id":id(),
            "tags":strings("Require every tag."),"tags_any":strings("Require at least one tag."),
            "untagged":boolean("Only untagged items."),"uncategorized":boolean("Only items without a folder."),
            "kind":text("MIME top-level type, such as image or video."),"exts":strings("File extensions."),
            "shape":{"type":"string","enum":["landscape","portrait","square","panorama"]},
            "rating_min":{"type":"integer","minimum":0,"maximum":5},
            "colors":strings("Hex colors, such as #ff8800."),"color_tolerance":{"type":"number","minimum":0},
            "sort":{"type":"string","enum":["added_desc","added_asc","name_asc","name_desc","rating_desc","size_desc","dimensions_desc","modified_desc","created_desc","last_viewed_desc","random","color"]},
            "seed":{"type":"integer"},"date_from":text("Inclusive date or RFC 3339 timestamp."),"date_to":text("Inclusive date or RFC 3339 timestamp."),
            "date_field":{"type":"string","enum":["imported","created","modified"]},
            "size_min":{"type":"integer","minimum":0},"size_max":{"type":"integer","minimum":0},
            "recently_used":boolean("Only previously viewed items."),"trash":boolean("Browse trash instead of live items."),
            "semantic":boolean("Rank the scoped query semantically when an embedder is configured."),
            "added_by":added_by(),"limit":limit(),"offset":offset()
        }), &[], true),
        schema("vamp_folders", "List library folders with IDs, hierarchy, descriptions and item counts.", json!({}), &[], true),
        schema("vamp_smart_folders", "List saved smart-folder rules and current item counts; pass an ID to vamp_browse.smart_folder_id.", json!({}), &[], true),
        schema("vamp_tags", "List tags and counts, tag groups, and starred tags.", json!({}), &[], true),
        schema("vamp_inspect", "Inspect an item, including metadata, provenance, observations, suggested tags, folders and available representations.", json!({"asset_id":id()}), &["asset_id"], true),
        schema("vamp_fetch_image", "Return MCP image content, defaulting to the thumbnail; preview is opt-in and original requires explicit tier=original. Never falls back to the original. Only PNG, JPEG, GIF and WebP are accepted. Encoded image bytes are capped before reading/encoding; originals are rejected before fetching when catalog size exceeds the cap.", json!({
            "asset_id":id(),"tier":{"type":"string","enum":["thumbnail","preview","original"],"default":"thumbnail"},
            "max_bytes":{"type":"integer","minimum":1,"maximum":MAX_IMAGE_BYTES,"default":DEFAULT_IMAGE_BYTES}
        }), &["asset_id"], true),
        schema("vamp_explore_show", "Show an exploration's brief, findings summary, folders, items and app link.", json!({"slug":slug()}), &["slug"], true),
        schema("vamp_explore_list", "List explorations with briefs, folder IDs and item counts.", json!({}), &[], true),
        schema("vamp_explore_link", "Get the existing Vampire app/deeplink and optional server link for an exploration or one of its items.", json!({
            "slug":slug(),"asset_id":id(),"server_url":text("Optional http(s) authority URL; otherwise use the context's configured URL.")
        }), &["slug"], true),
        schema("vamp_explore_changes", "Read other actors' changes since a cursor or this actor's last read. peek=true leaves the cursor unchanged. Read-only servers always peek and reject peek=false.", json!({
            "slug":slug(),"since":{"type":"integer","minimum":0,"maximum":i64::MAX},
            "peek":if read_only { json!({"type":"boolean","enum":[true],"default":true}) } else { json!({"type":"boolean","default":false}) }
        }), &["slug"], read_only),
    ];
    if !read_only {
        tools.extend([
            schema("vamp_add", "Import one local file or downloaded http(s) media URL (exactly one), at most 64 MiB. The engine attributes added_by to this MCP actor. Bytes are imported into the library; a URL is media, not a web page.", json!({
                "file":text("Path to a local regular file, resolved on the MCP host."),"url":text("Direct http(s) media URL."),
                "description":text("Human description."),"tags":strings("Initial user tags."),"source_url":text("Source page URL; defaults to the downloaded media URL.")
            }), &[], false),
            schema("vamp_tag", "Add/remove user tags, or explicitly replace all tags. Uses the core MetadataPatch rules (replace, then add, then remove).", json!({
                "asset_id":id(),"tags":strings("Replace all user tags, including [] to clear."),
                "add_tags":strings("Tags to add."),"remove_tags":strings("Tags to remove."),"expected_revision":revision()
            }), &["asset_id"], false),
            schema("vamp_rate", "Set an item's rating from 0 (unrated) to 5 using MetadataPatch.", json!({
                "asset_id":id(),"rating":{"type":"integer","minimum":0,"maximum":5},"expected_revision":revision()
            }), &["asset_id","rating"], false),
            schema("vamp_annotate", "Set the human description using MetadataPatch; an empty string clears it. Model observations are left unchanged.", json!({
                "asset_id":id(),"description":text("Replacement human annotation."),"expected_revision":revision()
            }), &["asset_id","description"], false),
            schema("vamp_create_folder", "Create a folder at the root or beneath an existing parent folder.", json!({"name":id(),"parent_id":id()}), &["name"], false),
            schema("vamp_explore_ensure", "Idempotently create/find an exploration under Explorations/. A supplied brief replaces its brief while preserving its findings summary.", json!({
                "slug":slug(),"title":text("Display title used when creating the exploration."),"brief":text("Exploration brief.")
            }), &["slug"], false),
            schema("vamp_explore_add", "Add a finding with provenance, idempotently using existing core exploration logic. Supply a direct image_url, a local file, or neither for a rendered note/link card. At most 64 MiB of imported media. URL is the source page, not the downloaded image.", json!({
                "slug":slug(),"name":id(),"note":text("Why the finding matters."),"tags":strings("User tags."),
                "url":text("Source page http(s) URL."),"folder":text("Subfolder path within the exploration, e.g. studios/posters."),
                "image_url":text("Direct http(s) media URL; mutually exclusive with file."),"file":text("Local regular file on the MCP host; mutually exclusive with image_url."),
                "kind":text("Card label when no file/image_url is supplied; defaults to link or note.")
            }), &["slug","name"], false),
            schema("vamp_explore_summary", "Replace an exploration's running findings summary while keeping its brief.", json!({"slug":slug(),"text":text("Findings summary.")}), &["slug","text"], false),
        ]);
    }
    tools
}

/// Test availability without allocating or materializing the tool schemas.
pub fn available(name: &str, read_only: bool) -> bool {
    Tool::parse(name).is_ok_and(|tool| !read_only || !tool.mutates())
}

#[derive(Clone, Copy)]
enum Tool {
    Search, Browse, Folders, SmartFolders, Tags, Inspect, FetchImage, Add, Tag, Rate,
    Annotate, CreateFolder, ExploreEnsure, ExploreAdd, ExploreSummary, ExploreShow,
    ExploreList, ExploreLink, ExploreChanges,
}
impl Tool {
    fn parse(name: &str) -> Result<Self> {
        Ok(match name {
            "vamp_search" => Self::Search, "vamp_browse" => Self::Browse,
            "vamp_folders" => Self::Folders, "vamp_smart_folders" => Self::SmartFolders,
            "vamp_tags" => Self::Tags, "vamp_inspect" => Self::Inspect,
            "vamp_fetch_image" => Self::FetchImage, "vamp_add" => Self::Add,
            "vamp_tag" => Self::Tag, "vamp_rate" => Self::Rate,
            "vamp_annotate" => Self::Annotate, "vamp_create_folder" => Self::CreateFolder,
            "vamp_explore_ensure" => Self::ExploreEnsure, "vamp_explore_add" => Self::ExploreAdd,
            "vamp_explore_summary" => Self::ExploreSummary, "vamp_explore_show" => Self::ExploreShow,
            "vamp_explore_list" => Self::ExploreList, "vamp_explore_link" => Self::ExploreLink,
            "vamp_explore_changes" => Self::ExploreChanges,
            _ => bail!("unknown MCP tool: {name}"),
        })
    }
    fn mutates(self) -> bool {
        matches!(self, Self::Add | Self::Tag | Self::Rate | Self::Annotate | Self::CreateFolder
            | Self::ExploreEnsure | Self::ExploreAdd | Self::ExploreSummary)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Item { asset_id: String }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    query: String,
    #[serde(default)] mode: SearchMode,
    limit: Option<usize>, offset: Option<usize>, added_by: Option<String>,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SearchMode { #[default] Metadata, Semantic, Hybrid }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchImage { asset_id: String, tier: Option<String>, max_bytes: Option<u64> }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Add {
    file: Option<PathBuf>, url: Option<String>, description: Option<String>,
    #[serde(default)] tags: Vec<String>, source_url: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tag {
    asset_id: String, tags: Option<Vec<String>>, add_tags: Option<Vec<String>>,
    remove_tags: Option<Vec<String>>, expected_revision: Option<u64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rate { asset_id: String, rating: u8, expected_revision: Option<u64> }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Annotate { asset_id: String, description: String, expected_revision: Option<u64> }
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Folder { name: String, #[serde(skip_serializing_if = "Option::is_none")] parent_id: Option<String> }
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ExploreEnsure { slug: String, title: Option<String>, brief: Option<String> }
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ExploreSummary { slug: String, text: String }
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ExploreShow { slug: String }
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ExploreLink { slug: String, asset_id: Option<String>, server_url: Option<String> }
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ExploreChanges { slug: String, since: Option<u64>, peek: Option<bool> }
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ExploreAdd {
    slug: String, name: String, note: Option<String>,
    #[serde(default)] tags: Vec<String>, url: Option<String>, folder: Option<String>,
    image_url: Option<String>, #[serde(skip_serializing)] file: Option<PathBuf>, kind: Option<String>,
}

fn decode<T: DeserializeOwned>(args: Value) -> Result<T> {
    serde_json::from_value(args).context("invalid MCP tool arguments")
}
fn nonempty(value: &str, field: &str, max: usize) -> Result<()> {
    ensure!(!value.trim().is_empty() && value.len() <= max, "{field} must be nonempty and at most {max} bytes");
    Ok(())
}
fn tags(values: &[String]) -> Result<()> {
    ensure!(values.len() <= 256, "at most 256 tags");
    for value in values { nonempty(value, "tag", 1024)?; }
    Ok(())
}
fn page(limit: Option<usize>, offset: Option<usize>) -> Result<(usize, usize)> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT);
    let offset = offset.unwrap_or(0);
    ensure!((1..=MAX_LIMIT).contains(&limit), "limit must be 1–{MAX_LIMIT}");
    ensure!(offset <= i64::MAX as usize, "offset exceeds the core pagination bound");
    Ok((limit, offset))
}
fn object(args: Value) -> Result<Map<String, Value>> {
    match args { Value::Object(args) => Ok(args), _ => bail!("MCP arguments must be an object") }
}
fn native_call(home: &Path, context: Option<&str>, operation: &str, args: Value) -> Result<Value> {
    let mut args = object(args)?;
    args.insert("operation".into(), json!(operation));
    args.insert("home".into(), json!(home));
    args.insert("context".into(), json!(context));
    let request: native::Request = serde_json::from_value(Value::Object(args))?;
    native::request(request)
}
fn patch(home: &Path, context: Option<&str>, asset_id: String, metadata: MetadataPatch, expected_revision: Option<u64>) -> Result<Value> {
    nonempty(&asset_id, "asset_id", 1024)?;
    ensure!(expected_revision != Some(0), "expected_revision must be positive");
    native_call(home, context, "update", json!({"asset_id":asset_id,"metadata":metadata,"expected_revision":expected_revision}))
}

/// Invoke one tool. Errors deliberately propagate to the transport's MCP error wrapper.
pub fn call(home: &Path, context: Option<&str>, actor: &str, read_only: bool, name: &str, args: Value) -> Result<Value> {
    let tool = Tool::parse(name)?;
    ensure!(!read_only || !tool.mutates(), "MCP server is read-only; {name} is a mutation tool");
    nonempty(actor, "actor", 256)?;
    ensure!(args.is_object(), "MCP arguments must be an object");
    // The transport also bounds incoming messages; library callers get the same bound
    // for strings/arrays before any file/network side effect, without serializing twice.
    argument_bounds(&args)?;
    vamp_core::with_actor(Some(actor.to_owned()), || {
        let invoke = |operation: &str, args| native_call(home, context, operation, args);
        let value = match tool {
            Tool::Search => {
                let args: Search = decode(args)?;
                nonempty(&args.query, "query", 4096)?;
                let (limit, offset) = page(args.limit, args.offset)?;
                match args.mode {
                    SearchMode::Metadata => invoke("browse", json!({"query":args.query,"limit":limit,"offset":offset,"added_by":args.added_by}))?,
                    mode => {
                        ensure!(offset == 0, "semantic/hybrid search does not support offset; increase limit");
                        let library = Library::open(home, context)?;
                        let hybrid = matches!(mode, SearchMode::Hybrid);
                        #[cfg(feature = "remote")]
                        if library.context.remote.is_some() {
                            return text_result(vamp_core::remote::semantic_search(&library, &args.query, limit, hybrid, args.added_by.as_deref())?);
                        }
                        json!({"semantic":true,"hybrid":hybrid,"results":library.semantic_search(&args.query, limit, hybrid, args.added_by.as_deref())?})
                    }
                }
            }
            Tool::Browse => browse(home, context, args)?,
            Tool::Folders | Tool::SmartFolders | Tool::Tags | Tool::ExploreList => {
                let _: Empty = decode(args)?;
                invoke(match tool { Tool::Folders => "collections", Tool::SmartFolders => "smart_folders", Tool::Tags => "tags", _ => "explore_list" }, json!({}))?
            }
            Tool::Inspect => {
                let args: Item = decode(args)?;
                nonempty(&args.asset_id, "asset_id", 1024)?;
                invoke("inspect", json!({"asset_id":args.asset_id}))?
            }
            Tool::FetchImage => return fetch_image(home, context, decode(args)?),
            Tool::Add => {
                let args: Add = decode(args)?;
                tags(&args.tags)?;
                let (bytes, filename) = import_source(args.file.as_deref(), args.url.as_deref())?;
                invoke("ingest", json!({
                    "bytes_base64":STANDARD.encode(bytes),"filename":filename,
                    "metadata":{"description":args.description,"tags":args.tags,"source_url":args.source_url.or(args.url)}
                }))?
            }
            Tool::Tag => {
                let args: Tag = decode(args)?;
                ensure!(args.tags.is_some() || args.add_tags.is_some() || args.remove_tags.is_some(), "provide tags, add_tags, or remove_tags");
                for values in [&args.tags, &args.add_tags, &args.remove_tags].into_iter().flatten() { tags(values)?; }
                patch(home, context, args.asset_id, MetadataPatch { tags:args.tags, add_tags:args.add_tags, remove_tags:args.remove_tags, ..MetadataPatch::default() }, args.expected_revision)?
            }
            Tool::Rate => {
                let args: Rate = decode(args)?;
                ensure!(args.rating <= 5, "rating must be 0–5");
                patch(home, context, args.asset_id, MetadataPatch { rating:Some(args.rating), ..MetadataPatch::default() }, args.expected_revision)?
            }
            Tool::Annotate => {
                let args: Annotate = decode(args)?;
                patch(home, context, args.asset_id, MetadataPatch { description:Some(args.description), ..MetadataPatch::default() }, args.expected_revision)?
            }
            Tool::CreateFolder => {
                let args: Folder = decode(args)?;
                nonempty(&args.name, "name", 1024)?;
                if let Some(parent) = &args.parent_id { nonempty(parent, "parent_id", 1024)?; }
                invoke("collection_create", serde_json::to_value(args)?)?
            }
            Tool::ExploreEnsure => {
                let args: ExploreEnsure = decode(args)?;
                vamp_core::explore::validate_slug(&args.slug)?;
                invoke("explore_ensure", serde_json::to_value(args)?)?
            }
            Tool::ExploreSummary => {
                let args: ExploreSummary = decode(args)?;
                vamp_core::explore::validate_slug(&args.slug)?;
                invoke("explore_summary", serde_json::to_value(args)?)?
            }
            Tool::ExploreShow => {
                let args: ExploreShow = decode(args)?;
                vamp_core::explore::validate_slug(&args.slug)?;
                invoke("explore_show", serde_json::to_value(args)?)?
            }
            Tool::ExploreLink => {
                let args: ExploreLink = decode(args)?;
                vamp_core::explore::validate_slug(&args.slug)?;
                invoke("explore_link", serde_json::to_value(args)?)?
            }
            Tool::ExploreChanges => {
                let mut args: ExploreChanges = decode(args)?;
                vamp_core::explore::validate_slug(&args.slug)?;
                ensure!(args.since.is_none_or(|since| since <= i64::MAX as u64), "since exceeds the core event cursor bound");
                ensure!(!read_only || args.peek != Some(false), "MCP server is read-only; explore changes must use peek=true");
                args.peek = Some(read_only || args.peek.unwrap_or(false));
                invoke("explore_changes", serde_json::to_value(args)?)?
            }
            Tool::ExploreAdd => {
                let args: ExploreAdd = decode(args)?;
                vamp_core::explore::validate_slug(&args.slug)?;
                nonempty(&args.name, "name", 1024)?;
                tags(&args.tags)?;
                ensure!(args.file.is_none() || args.image_url.is_none(), "file and image_url are mutually exclusive");
                let content = if args.file.is_some() || args.image_url.is_some() {
                    Some(import_source(args.file.as_deref(), args.image_url.as_deref())?)
                } else { None };
                let mut body = object(serde_json::to_value(args)?)?;
                if let Some((bytes, filename)) = content {
                    body.insert("bytes_base64".into(), json!(STANDARD.encode(bytes)));
                    body.insert("filename".into(), json!(filename));
                }
                invoke("explore_add", Value::Object(body))?
            }
        };
        text_result(value)
    })
}

fn text_result(value: Value) -> Result<Value> {
    Ok(json!({"content":[{"type":"text","text":serde_json::to_string(&value)?}],"isError":false}))
}
fn argument_bounds(value: &Value) -> Result<()> {
    match value {
        Value::String(value) => ensure!(value.len() <= 65536, "argument string exceeds 64 KiB"),
        Value::Array(values) => {
            ensure!(values.len() <= 256, "argument array exceeds 256 entries");
            for value in values { argument_bounds(value)?; }
        }
        // All accepted argument objects are flat (apart from arrays of strings). Reject
        // nested structures rather than recursively walking arbitrary untrusted JSON.
        Value::Object(values) => {
            ensure!(values.len() <= 40, "too many argument fields");
            for value in values.values() {
                ensure!(!value.is_object(), "nested argument objects are not supported");
                if let Value::Array(items) = value {
                    ensure!(items.iter().all(Value::is_string), "argument arrays must contain strings");
                }
                argument_bounds(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

const BROWSE_FIELDS: &[&str] = &[
    "query", "collection_id", "folder_ids", "include_subfolders", "smart_folder_id", "tags", "tags_any",
    "untagged", "uncategorized", "kind", "exts", "shape", "rating_min", "colors", "color_tolerance",
    "sort", "seed", "date_from", "date_to", "date_field", "size_min", "size_max", "recently_used",
    "trash", "semantic", "added_by", "limit", "offset",
];
fn browse(home: &Path, context: Option<&str>, args: Value) -> Result<Value> {
    let mut args = object(args)?;
    for key in args.keys() { ensure!(BROWSE_FIELDS.contains(&key.as_str()), "unknown browse argument: {key}"); }
    if let Some(Value::String(query)) = args.get("query") { ensure!(query.len() <= 4096, "query exceeds 4096 bytes"); }
    // Consume arguments into the engine request rather than copying query/filter lists.
    args.insert("operation".into(), json!("browse"));
    let mut request: native::Request = serde_json::from_value(Value::Object(args))?;
    let (limit, _) = page(request.limit, Some(request.offset))?;
    ensure!(request.rating_min.is_none_or(|rating| rating <= 5), "rating_min must be 0–5");
    for values in [&request.tags, &request.tags_any, &request.exts, &request.colors, &request.folder_ids] { tags(values)?; }
    request.home = home.to_owned();
    request.context = context.map(str::to_owned);
    request.limit = Some(limit);
    native::request(request)
}

fn bounded_read(path: &Path, cap: u64) -> Result<Vec<u8>> {
    let file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "source must be a regular file");
    ensure!(metadata.len() <= cap, "file exceeds {cap} byte limit");
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(cap + 1).read_to_end(&mut bytes)?;
    ensure!(!bytes.is_empty(), "file is empty");
    ensure!(bytes.len() as u64 <= cap, "file exceeds {cap} byte limit");
    Ok(bytes)
}
fn import_source(file: Option<&Path>, url: Option<&str>) -> Result<(Vec<u8>, String)> {
    ensure!(file.is_some() != url.is_some(), "provide exactly one of file or media URL");
    if let Some(file) = file {
        let filename = file.file_name().and_then(|name| name.to_str()).context("file requires a UTF-8 filename")?.to_owned();
        return Ok((bounded_read(file, MAX_IMPORT_BYTES)?, filename));
    }
    let url = url.context("media URL is required")?;
    nonempty(url, "media URL", 65536)?;
    #[cfg(feature = "remote")]
    { vamp_core::explore::download(url) }
    #[cfg(not(feature = "remote"))]
    { bail!("media URL imports require a vamp build with the remote feature; download the file and use file instead") }
}
fn image_mime(bytes: &[u8]) -> Result<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") { Ok("image/png") }
    else if bytes.starts_with(b"\xff\xd8\xff") { Ok("image/jpeg") }
    else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") { Ok("image/gif") }
    else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" { Ok("image/webp") }
    else { bail!("unsupported image MIME: only PNG, JPEG, GIF and WebP can be returned as MCP images") }
}
fn fetch_image(home: &Path, context: Option<&str>, args: FetchImage) -> Result<Value> {
    nonempty(&args.asset_id, "asset_id", 1024)?;
    let tier = args.tier.as_deref().unwrap_or("thumbnail");
    ensure!(matches!(tier, "thumbnail" | "preview" | "original"), "tier must be thumbnail, preview or original");
    let cap = args.max_bytes.unwrap_or(DEFAULT_IMAGE_BYTES);
    ensure!((1..=MAX_IMAGE_BYTES).contains(&cap), "max_bytes must be 1–{MAX_IMAGE_BYTES}");
    let library = Library::open(home, context)?;
    let view = library.inspect(&args.asset_id)?;
    if tier == "original" {
        ensure!(matches!(view.asset.original.mime.as_str(), "image/png" | "image/jpeg" | "image/gif" | "image/webp"), "unsupported original image MIME: {}", view.asset.original.mime);
        ensure!(view.asset.original.byte_size <= cap, "original exceeds {cap} byte limit; request thumbnail or preview");
    } else if let Some(rep) = view.representations.iter().find(|rep| rep.kind == tier) {
        // A replica preview may use the newer WebP tier instead of the legacy rep;
        // its actual bounded read below remains authoritative for that tier.
        if tier == "thumbnail" || library.context.remote.is_none() {
            ensure!(rep.byte_size <= cap, "{tier} exceeds {cap} byte limit");
            ensure!(matches!(rep.mime.as_str(), "image/png" | "image/jpeg" | "image/gif" | "image/webp"), "unsupported {tier} image MIME: {}", rep.mime);
        }
    }
    #[cfg(feature = "remote")]
    let path = vamp_core::remote::representation(&library, &args.asset_id, tier)?;
    #[cfg(not(feature = "remote"))]
    let path = library.fetch(&args.asset_id, tier)?;
    let bytes = bounded_read(&path, cap)?;
    let mime = image_mime(&bytes)?;
    Ok(json!({"content":[{"type":"image","data":STANDARD.encode(bytes),"mimeType":mime}],"isError":false}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let home = std::env::temp_dir().join(format!("vamp-mcp-tools-{}-{nonce}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            Library::init(&home, "scratch", None, None).unwrap();
            Self(home)
        }
        fn call(&self, name: &str, args: Value) -> Value {
            let result = call(&self.0, None, "agent:mcp-test", false, name, args).unwrap();
            assert_eq!(result["isError"], false);
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
        }
    }
    impl Drop for Scratch { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }

    #[test]
    fn golden_schema_surface() {
        // Full public snapshot: descriptions, input schemas, defaults and annotations.
        let golden: Value = serde_json::from_str(include_str!("../tests/fixtures/mcp_tools.schema.json")).unwrap();
        assert_eq!(json!(schemas(false)), golden);
        let mut read_only = golden.as_array().unwrap().iter().filter(|tool| {
            tool["annotations"]["readOnlyHint"] == true || tool["name"] == "vamp_explore_changes"
        }).cloned().collect::<Vec<_>>();
        let changes = read_only.iter_mut().find(|tool| tool["name"] == "vamp_explore_changes").unwrap();
        changes["annotations"]["readOnlyHint"] = json!(true);
        changes["inputSchema"]["properties"]["peek"] = json!({"type":"boolean","enum":[true],"default":true});
        assert_eq!(json!(schemas(true)), Value::Array(read_only));
        for tool in schemas(false) {
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
            for field in tool["inputSchema"]["required"].as_array().unwrap() {
                assert!(tool["inputSchema"]["properties"].get(field.as_str().unwrap()).is_some());
            }
            let name = tool["name"].as_str().unwrap();
            assert!(available(name, false));
            assert_eq!(available(name, true), !Tool::parse(name).unwrap().mutates());
        }
        let fetch = schemas(true).into_iter().find(|tool| tool["name"] == "vamp_fetch_image").unwrap();
        assert_eq!(fetch["inputSchema"]["properties"]["max_bytes"]["maximum"], MAX_IMAGE_BYTES);
        assert_eq!(fetch["inputSchema"]["properties"]["tier"]["default"], "thumbnail");
        let changes = schemas(true).into_iter().find(|tool| tool["name"] == "vamp_explore_changes").unwrap();
        assert_eq!(changes["inputSchema"]["properties"]["peek"]["enum"], json!([true]));
    }

    #[test]
    fn read_only_and_invalid_arguments_fail_before_opening_library() {
        let absent = Path::new("/nonexistent-vamp-mcp-test-home");
        for tool in schemas(false).into_iter().filter(|tool| Tool::parse(tool["name"].as_str().unwrap()).unwrap().mutates()) {
            assert!(call(absent, None, "agent:test", true, tool["name"].as_str().unwrap(), json!({})).unwrap_err().to_string().contains("read-only"));
        }
        assert!(call(absent, None, "agent:test", true, "vamp_explore_changes", json!({"slug":"study","peek":false})).unwrap_err().to_string().contains("read-only"));
        assert!(call(absent, None, "agent:test", false, "vamp_search", json!({"query":"x","limit":501})).is_err());
        assert!(call(absent, None, "agent:test", false, "vamp_browse", json!({"operation":"remove"})).is_err());
        assert!(call(absent, None, "agent:test", false, "vamp_add", json!({"file":"x","url":"https://example.test/x"})).is_err());
        assert!(call(absent, None, "agent:test", false, "vamp_fetch_image", json!({"asset_id":"x","tier":"automatic"})).is_err());
        assert!(call(absent, None, "agent:test", false, "vamp_fetch_image", json!({"asset_id":"x","max_bytes":MAX_IMAGE_BYTES+1})).is_err());
        assert!(call(absent, None, "agent:test", false, "vamp_rate", json!({"asset_id":"x","rating":6})).is_err());
        assert!(call(absent, None, "agent:test", false, "vamp_folders", json!({"unexpected":true})).is_err());
    }

    #[test]
    fn imports_metadata_scoped_browse_and_actor_are_core_operations() {
        let scratch = Scratch::new();
        let source = scratch.0.join("sample.txt");
        std::fs::write(&source, "sample media").unwrap();
        let asset = scratch.call("vamp_add", json!({"file":source,"tags":["reference"],"description":"initial"}));
        let asset_id = asset["id"].as_str().unwrap();
        assert_eq!(asset["metadata"]["extra"]["added_by"], "agent:mcp-test");
        let folder = scratch.call("vamp_create_folder", json!({"name":"References"}));
        let folder_id = folder["id"].as_str().unwrap();
        let mut library = Library::open(&scratch.0, None).unwrap();
        library.collection_membership(folder_id, asset_id, true).unwrap();
        let tagged = scratch.call("vamp_tag", json!({"asset_id":asset_id,"add_tags":["selected"],"remove_tags":["reference"],"expected_revision":asset["revision"]}));
        assert_eq!(tagged["metadata"]["tags"], json!(["selected"]));
        let rated = scratch.call("vamp_rate", json!({"asset_id":asset_id,"rating":4}));
        assert_eq!(rated["metadata"]["rating"], 4);
        let annotated = scratch.call("vamp_annotate", json!({"asset_id":asset_id,"description":"useful finding"}));
        assert_eq!(annotated["metadata"]["description"], "useful finding");
        assert!(call(&scratch.0, None, "agent:mcp-test", false, "vamp_rate", json!({"asset_id":asset_id,"rating":3,"expected_revision":asset["revision"]})).is_err());
        let found = scratch.call("vamp_browse", json!({"collection_id":folder_id,"query":"useful","tags":["selected"],"rating_min":4}));
        assert_eq!(found["total"], 1);
        let found = scratch.call("vamp_search", json!({"query":"useful"}));
        assert_eq!(found["total"], 1);
        for mode in ["semantic", "hybrid"] {
            // Explicit semantic modes propagate missing-model errors; they must not
            // quietly return metadata results when embeddings are unavailable.
            assert!(call(&scratch.0, None, "agent:mcp-test", false, "vamp_search", json!({"query":"useful","mode":mode})).is_err());
        }
        let inspected = scratch.call("vamp_inspect", json!({"asset_id":asset_id}));
        assert_eq!(inspected["human_description"], "useful finding");
        assert_eq!(scratch.call("vamp_folders", json!({})).as_array().unwrap().len(), 1);
        let smart = library.smart_folder_create("Selected", serde_json::from_value(json!({"match":"all","rules":[{"field":"tags","op":"contains","value":"selected"}]})).unwrap(), None).unwrap();
        assert_eq!(scratch.call("vamp_smart_folders", json!({})).as_array().unwrap().len(), 1);
        assert_eq!(scratch.call("vamp_browse", json!({"smart_folder_id":smart.id,"query":"useful"}))["total"], 1);
        assert_eq!(scratch.call("vamp_tags", json!({}))["tags"][0]["tag"], "selected");
        let view = library.inspect(asset_id).unwrap();
        assert!(library.events(0, 100).unwrap().iter().filter(|event| event.kind == "media_updated").all(|event| event.actor == "agent:mcp-test"));
        assert_eq!(view.asset.metadata.rating, 4);
    }

    #[test]
    fn exploration_operations_preserve_provenance_and_read_only_peeks() {
        let scratch = Scratch::new();
        scratch.call("vamp_explore_ensure", json!({"slug":"study","title":"Study","brief":"A brief"}));
        let source = scratch.0.join("finding.txt");
        std::fs::write(&source, "finding").unwrap();
        let finding = scratch.call("vamp_explore_add", json!({"slug":"study","name":"Finding","file":source,"note":"Evidence","folder":"references"}));
        let shown = scratch.call("vamp_explore_show", json!({"slug":"study"}));
        assert_eq!(shown["items"].as_array().unwrap().len(), 1);
        let asset_id = shown["items"][0]["id"].as_str().unwrap();
        assert!(finding.get("asset").is_some());
        scratch.call("vamp_explore_summary", json!({"slug":"study","text":"Useful references"}));
        assert_eq!(scratch.call("vamp_explore_show", json!({"slug":"study"}))["summary"], "Useful references");
        assert_eq!(scratch.call("vamp_explore_list", json!({}))["explorations"].as_array().unwrap().len(), 1);
        let link = scratch.call("vamp_explore_link", json!({"slug":"study","asset_id":asset_id}));
        assert!(link.to_string().contains("vampire://"));
        vamp_core::with_actor(Some("owner:test".into()), || {
            Library::open(&scratch.0, None).unwrap().update(asset_id, MetadataPatch { rating:Some(5), ..MetadataPatch::default() }, None).unwrap();
        });
        let peek = call(&scratch.0, None, "agent:mcp-test", true, "vamp_explore_changes", json!({"slug":"study"})).unwrap();
        let peek: Value = serde_json::from_str(peek["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(peek["advanced"], false);
        assert_eq!(peek["items"].as_array().unwrap().len(), 1);
        let advanced = scratch.call("vamp_explore_changes", json!({"slug":"study"}));
        assert_eq!(advanced["advanced"], true);
        assert!(scratch.call("vamp_explore_changes", json!({"slug":"study","peek":true}))["items"].as_array().unwrap().is_empty());
    }

    #[test]
    fn image_content_requires_explicit_original_and_obeys_byte_cap() {
        let scratch = Scratch::new();
        let png = STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=").unwrap();
        let asset = Library::open(&scratch.0, None).unwrap().ingest_bytes(&png, "pixel.png", Default::default()).unwrap();
        // There is no derivative: the default request must not silently read the original.
        assert!(call(&scratch.0, None, "agent:test", true, "vamp_fetch_image", json!({"asset_id":asset.id})).is_err());
        let result = call(&scratch.0, None, "agent:test", true, "vamp_fetch_image", json!({"asset_id":asset.id,"tier":"original"})).unwrap();
        assert_eq!(result["content"][0]["type"], "image");
        assert_eq!(result["content"][0]["mimeType"], "image/png");
        assert_eq!(STANDARD.decode(result["content"][0]["data"].as_str().unwrap()).unwrap(), png);
        assert!(call(&scratch.0, None, "agent:test", true, "vamp_fetch_image", json!({"asset_id":asset.id,"tier":"original","max_bytes":1})).is_err());
        assert!(image_mime(b"<svg></svg>").is_err());
        assert!(image_mime(b"%PDF-1.7").is_err());
        assert_eq!(image_mime(b"GIF89a").unwrap(), "image/gif");
    }
}
