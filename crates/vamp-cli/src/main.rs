mod setup;
#[cfg(feature = "remote")]
mod self_update;
#[cfg(feature = "remote")]
mod sync_service;
#[cfg(feature = "remote")]
mod worker;

use anyhow::{Context as _, Result, bail, ensure};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::ExitCode,
};
use vamp_core::{
    Backend, EmbedderSpec, Library, StorageConfig, attach_context, default_home, load_config,
    select_context,
};
use vamp_protocol::{Metadata, MetadataPatch, ProcessorSpec, SheetItem, ToolRequest};

#[derive(Parser)]
#[command(
    name = "vamp",
    version,
    about = "Reference-only media catalog for agents",
    long_about = "Reference-only media catalog for agents. JSON output is the default. Originals are referenced in place and never modified or deleted (ingested bytes such as clips and exploration findings are copied into the library inbox). Search covers metadata and observations; --semantic ranks by embeddings. Set VAMP_ACTOR to identify event authors. Optional media/ML helpers are installed only by explicit setup --install. `vamp guide` prints the agent manual."
)]
struct Cli {
    #[arg(
        long,
        global = true,
        env = "VAMP_HOME",
        help = "Configuration and optional tool home"
    )]
    home: Option<PathBuf>,
    #[arg(
        long,
        global = true,
        env = "VAMP_CONTEXT",
        help = "Named catalog context"
    )]
    context: Option<String>,
    #[arg(
        long,
        global = true,
        help = "Request JSON output (already the default)"
    )]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the agent manual (plain text): discovery, search, safety rules.
    Guide,
    /// Print protocol JSON schemas.
    Schema,
    /// Print the machine-readable command and argument tree.
    Commands,
    /// Create an empty reference-only catalog.
    Init {
        #[arg(long)]
        name: String,
        #[arg(long)]
        original_root: Option<PathBuf>,
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Manage named catalog contexts.
    Contexts {
        #[command(subcommand)]
        command: ContextCommand,
    },
    /// Catalog references, metadata, and explicit local derivatives.
    Media {
        #[command(subcommand)]
        command: MediaCommand,
    },
    /// Import references without modifying the source library.
    Import {
        #[command(subcommand)]
        command: ImportCommand,
    },
    /// Manage catalog collections (nested folders).
    Collections {
        #[command(subcommand)]
        command: CollectionCommand,
    },
    /// Agent research missions: an exploration folder (brief, findings summary, sub-folders)
    /// under Explorations/, findings with provenance, the app deeplink and a read of others' changes.
    Explore {
        #[command(subcommand)]
        command: ExploreCommand,
    },
    /// Smart folders: saved rule sets evaluated on browse.
    Smart {
        #[command(subcommand)]
        command: SmartCommand,
    },
    /// Tag usage, groups, rename/merge and delete.
    Tags {
        #[command(subcommand)]
        command: TagCommand,
    },
    /// Sidebar counts (all, uncategorized, untagged, recently used, trash, folders, smart folders).
    Sidebar,
    /// Read ordered catalog events.
    Events {
        #[arg(long, default_value_t = 0)]
        after: u64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Export metadata and locators, NOT media bytes or a self-contained archive.
    Export,
    /// Register trusted local executables and enqueue work. No shell is used.
    Processors {
        #[command(subcommand)]
        command: ProcessorCommand,
    },
    /// Inspect and explicitly run durable processor jobs.
    Jobs {
        #[command(subcommand)]
        command: JobCommand,
    },
    /// Inspect optional capabilities; installation requires --install.
    Setup {
        #[arg(value_enum)]
        capability: Option<Capability>,
        #[arg(long)]
        install: bool,
        #[arg(
            long,
            help = "VAMP source workspace for optional media helper installation"
        )]
        source: Option<PathBuf>,
        #[arg(
            long,
            value_parser = ["auto", "cpu", "cuda", "mps"],
            help = "Model device (models): default cuda on Linux with nvidia-smi, else cpu"
        )]
        device: Option<String>,
    },
    /// Pull the authority's changes into the current replica context's mirror.
    #[cfg(feature = "remote")]
    Sync {
        /// Keep syncing: long-poll the authority and print one JSON line per change.
        #[arg(long)]
        watch: bool,
        #[command(subcommand)]
        command: Option<SyncCommand>,
    },
    /// Attach a replica of a tailnet vamp-server (no token: the server admits this
    /// machine by its Tailscale identity) and sync its catalog + thumbnails.
    #[cfg(feature = "remote")]
    Join {
        /// `host`, `host:port` (default port 7777) or a full `http://host:port` URL.
        target: String,
        /// Context name (default: the authority's context name).
        #[arg(long)]
        name: Option<String>,
        /// `AUTHORITY_PREFIX=LOCAL_PREFIX`: read originals under the authority prefix from
        /// this local path (e.g. a Drive mount) instead of downloading them. Repeatable.
        #[arg(long = "drive-root", value_name = "AUTHORITY=LOCAL")]
        drive_roots: Vec<String>,
    },
    /// Configure the text embedder used by semantic search (ToolRequest embed_text).
    Embedder {
        #[command(subcommand)]
        command: EmbedderCommand,
    },
    /// Cloud storage for originals (authority only). Uploads drain in `jobs run/watch`.
    Storage {
        #[command(subcommand)]
        command: StorageCommand,
    },
    /// Rehydrated-original cache (pinned until pruned); replica preview/original cache policy.
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
    /// Authority thumbnail tiers (micro, 800 px grid WebP, 2048 px preview WebP).
    Thumbs {
        #[command(subcommand)]
        command: ThumbsCommand,
    },
    /// Galaxy view: 3-D embedding projection (authority) and its search ranking.
    Galaxy {
        #[command(subcommand)]
        command: GalaxyCommand,
    },
    /// Run this machine's model processors for a vamp-server authority's jobs (GPU worker).
    #[cfg(feature = "remote")]
    #[command(args_conflicts_with_subcommands = true)]
    Worker {
        #[command(subcommand)]
        command: Option<WorkerCommand>,
        #[command(flatten)]
        args: WorkerArgs,
    },
    /// Update this release install (vamp + vamp-media) to the latest release,
    /// checksum-verified, replacing the binaries in place.
    #[cfg(feature = "remote")]
    SelfUpdate {
        /// Only report the current and latest versions.
        #[arg(long)]
        check: bool,
    },
}
#[cfg(feature = "remote")]
#[derive(Args)]
struct WorkerConnection {
    /// Authority URL, e.g. http://authority-host:7777
    #[arg(long, env = "VAMP_SERVER_URL")]
    server: Option<String>,
    /// Bearer token (the authority's <state_dir>/server-token).
    #[arg(long, env = "VAMP_SERVER_TOKEN", hide_env_values = true)]
    token: Option<String>,
    #[arg(long)]
    token_file: Option<PathBuf>,
    /// Offer only these locally installed processors (repeatable; default: all installed).
    #[arg(long)]
    processors: Vec<String>,
    #[arg(long, default_value_t = 1)]
    concurrency: usize,
}
#[cfg(feature = "remote")]
#[derive(Args)]
struct WorkerArgs {
    #[command(flatten)]
    connection: WorkerConnection,
    /// Defaults to <hostname>:<pid>.
    #[arg(long)]
    worker_id: Option<String>,
    #[arg(long, default_value_t = 120)]
    lease_seconds: u64,
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..))]
    poll_seconds: u64,
    /// Exit once a claim finds no work (default: run forever).
    #[arg(long)]
    exit_when_idle: bool,
}
#[cfg(feature = "remote")]
#[derive(Subcommand)]
enum WorkerCommand {
    /// Write a systemd --user unit for this worker and print the enable command.
    InstallService {
        #[command(flatten)]
        connection: WorkerConnection,
    },
}
#[cfg(feature = "remote")]
#[derive(Subcommand)]
enum SyncCommand {
    /// Keep `vamp sync --watch` running for this replica context: writes a systemd --user
    /// unit (Linux) or a launchd agent (macOS) and prints the command that starts it.
    InstallService,
}
#[derive(Subcommand)]
enum StorageCommand {
    /// Configure the library's backend. Credentials never enter the catalog.
    Set {
        #[command(subcommand)]
        backend: StorageBackend,
    },
    Show,
    /// Remove the backend; refused while any original exists only in cloud storage.
    Unset,
    /// Queue uploads for existing local originals (and re-queue failed ones).
    Migrate {
        #[arg(long, default_value_t = 1000)]
        limit: usize,
    },
    /// List durable upload records.
    Uploads {
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
}
#[derive(Subcommand)]
enum StorageBackend {
    /// S3-compatible bucket via the vamp-store helper. Credentials: AWS_ACCESS_KEY_ID /
    /// AWS_SECRET_ACCESS_KEY, or a 0600 file saved with --save-credentials.
    S3 {
        #[arg(long)]
        bucket: String,
        #[arg(long, default_value = "")]
        prefix: String,
        #[arg(long)]
        endpoint: String,
        #[arg(long, default_value = "us-east-1")]
        region: String,
        #[arg(long)]
        path_style: bool,
        /// Persist the current AWS_* environment credentials to the state dir (mode 0600).
        #[arg(long)]
        save_credentials: bool,
        #[command(flatten)]
        eviction: Eviction,
    },
    /// Synced folder (iCloud Drive, Google Drive, Dropbox, NAS mount...).
    Folder {
        #[arg(long)]
        root: PathBuf,
        #[command(flatten)]
        eviction: Eviction,
    },
}
#[derive(clap::Args)]
struct Eviction {
    /// DELETES the local original after a verified upload once thumbnail+preview exist.
    /// Default off; the only way VAMP ever deletes an original.
    #[arg(long)]
    delete_local_after_upload: bool,
    /// Allow that deletion outside original_root and the inbox.
    #[arg(long, requires = "delete_local_after_upload")]
    delete_local_anywhere: bool,
}
#[derive(Subcommand)]
enum CacheCommand {
    /// Delete rehydrated originals; they are downloaded again on demand.
    Prune,
    /// Show a replica's cache policy, or change it with flags.
    #[cfg(feature = "remote")]
    Policy {
        #[arg(long, value_enum)]
        previews: Option<PreviewPolicy>,
        /// Preview cache bound, e.g. 5GB, 500MB (`none`: unbounded).
        #[arg(long, value_parser = parse_cap)]
        preview_cap: Option<Cap>,
        /// Downloaded-originals cache bound (default 10GB; `none`: unbounded).
        #[arg(long, value_parser = parse_cap)]
        originals_cap: Option<Cap>,
    },
    /// A replica's cache usage (grid thumbnails, previews, originals) and authority tier stats.
    #[cfg(feature = "remote")]
    Stats,
}
#[cfg(feature = "remote")]
#[derive(Clone, ValueEnum)]
enum PreviewPolicy {
    /// Only previews already cached.
    Off,
    /// Downloaded when viewed.
    OnDemand,
    /// Every preview, in the background after the grid thumbnails.
    All,
    /// On demand, least recently used evicted beyond --preview-cap.
    Cap,
}
/// A cache bound in bytes; `None` is unbounded.
#[cfg(feature = "remote")]
#[derive(Clone)]
struct Cap(Option<u64>);
/// `none`, or bytes with an optional KB/MB/GB/TB (or KiB…TiB) suffix.
#[cfg(feature = "remote")]
fn parse_cap(text: &str) -> Result<Cap, String> {
    let text = text.trim().to_ascii_lowercase();
    if text == "none" {
        return Ok(Cap(None));
    }
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: f64 = number
        .parse()
        .map_err(|_| format!("invalid size: {text}"))?;
    let scale: u64 = match unit.trim() {
        "" | "b" => 1,
        "k" | "kb" => 1_000,
        "m" | "mb" => 1_000_000,
        "g" | "gb" => 1_000_000_000,
        "t" | "tb" => 1_000_000_000_000,
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        "tib" => 1 << 40,
        other => return Err(format!("unknown size unit: {other}")),
    };
    Ok(Cap(Some((number * scale as f64) as u64)))
}
#[derive(Subcommand)]
enum EmbedderCommand {
    /// Register from a JSON {command, model, timeout_seconds?} file.
    Set {
        #[arg(long)]
        spec: PathBuf,
    },
    Show,
    Remove,
}
#[derive(Subcommand)]
enum ContextCommand {
    List,
    Use {
        name: String,
    },
    Add {
        name: String,
        #[arg(long)]
        state_dir: PathBuf,
    },
    /// Attach a replica of a vamp-server library and sync its catalog + thumbnails.
    #[cfg(feature = "remote")]
    Remote {
        name: String,
        #[arg(long)]
        url: String,
        #[arg(long, env = "VAMP_REMOTE_TOKEN")]
        token: String,
    },
    /// Where vamp-server serves this local authority library (http base URL; `explore link`
    /// bounces through it): prints it, sets it to URL, or forgets it with --clear.
    Url {
        name: String,
        url: Option<String>,
        #[arg(long, conflicts_with = "url")]
        clear: bool,
    },
}
#[derive(Args)]
struct Page {
    #[arg(long, default_value_t = 100)]
    limit: usize,
    #[arg(long, default_value_t = 0)]
    offset: usize,
}
#[derive(Args)]
struct Revision {
    #[arg(long, help = "Reject changes if the asset revision differs")]
    expected_revision: Option<u64>,
}
#[derive(Subcommand)]
enum ThumbsCommand {
    /// Render missing/outdated tiers now (the server's jobs thread also does this in the
    /// background). Preview tiers read originals with a deadline; stalled ones are retried
    /// on a later run.
    Regenerate {
        #[arg(long, value_enum, default_value = "grid")]
        tier: TierArg,
    },
    /// Tier counts, bytes and pending work (same as `GET /v1/tiers/stats`).
    Stats,
}
#[derive(Subcommand)]
enum GalaxyCommand {
    /// Bring the layout up to date: place new embeddings near their neighbours, or recompute
    /// the projection in full (`--rebuild`, a changed model, or >20% placed incrementally).
    /// Run nightly (cron/launchd) to keep the constellations faithful as the library grows.
    Layout {
        #[arg(long)]
        rebuild: bool,
    },
    /// Rank images for a text query the way the galaxy search bar does (timings included).
    Search {
        text: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}
#[derive(Clone, Copy, ValueEnum)]
enum TierArg {
    Grid,
    Preview,
    All,
}
const ADDED_BY_HELP: &str = "Who added the assets: me (the owner: every actor but agent:*), agents, or an exact actor (e.g. agent:claude, local)";
/// `--added-by` as the browse filter value: `owner`, `agents` or `actor:<actor>`.
fn added_by_filter(value: &str) -> Result<String, String> {
    let value = value.trim();
    Ok(match value {
        "" => return Err("expected me, agents or an actor".into()),
        "me" | "owner" => "owner".into(),
        "agents" => "agents".into(),
        actor => format!("actor:{}", actor.strip_prefix("actor:").unwrap_or(actor)),
    })
}
#[derive(Subcommand)]
enum MediaCommand {
    /// Explicitly read and hash an original, storing only its reference.
    Add {
        path: PathBuf,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, help = "JSON Metadata file")]
        metadata: Option<PathBuf>,
    },
    /// List metadata only; never hydrate originals.
    List {
        #[arg(long)]
        query: Option<String>,
        #[arg(long, value_name = "WHO", value_parser = added_by_filter, help = ADDED_BY_HELP)]
        added_by: Option<String>,
        #[command(flatten)]
        page: Page,
    },
    /// Search indexed metadata and observations; --semantic ranks by embedding cosine.
    Search {
        query: String,
        #[arg(
            long,
            help = "Rank by text-embedding cosine via the configured embedder"
        )]
        semantic: bool,
        #[arg(
            long,
            requires = "semantic",
            help = "Fuse semantic and full-text ranks"
        )]
        hybrid: bool,
        #[arg(long, value_name = "WHO", value_parser = added_by_filter, help = ADDED_BY_HELP)]
        added_by: Option<String>,
        #[command(flatten)]
        page: Page,
    },
    /// Rank other assets by embedding similarity to this one.
    Similar {
        id: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Copy model-suggested tags into user tags (all suggestions when none are named).
    AcceptTags {
        id: String,
        tags: Vec<String>,
        #[command(flatten)]
        revision: Revision,
    },
    Inspect {
        id: String,
    },
    /// Apply a JSON metadata patch file.
    Update {
        id: String,
        #[arg(long)]
        patch: PathBuf,
        #[command(flatten)]
        revision: Revision,
    },
    /// Explicitly rehash the original and invalidate stale derivatives.
    Refresh {
        id: String,
        #[command(flatten)]
        revision: Revision,
    },
    /// Move to the trash (restorable); never deletes original files.
    Remove {
        id: String,
        #[command(flatten)]
        revision: Revision,
    },
    /// Restore trashed assets.
    Restore {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Tombstone trashed assets (all trashed when no ids); never deletes original files.
    EmptyTrash {
        ids: Vec<String>,
    },
    /// Compute pending (lazy) fingerprints; reads, and so hydrates, originals.
    Hash {
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=10000))]
        limit: u32,
    },
    /// Copy originals into an existing directory (hydrates cloud placeholders).
    Export {
        #[arg(required = true)]
        ids: Vec<String>,
        #[arg(long)]
        to: PathBuf,
    },
    /// Resolve a representation path. Never fall back to the original.
    Fetch {
        id: String,
        #[arg(long, value_enum, default_value = "original")]
        kind: Kind,
    },
    /// Explicitly generate a local thumbnail using the optional media helper.
    Thumbnail {
        id: String,
    },
    /// Explicitly generate a local preview using the optional media helper.
    Preview {
        id: String,
    },
    /// Build a numbered sheet from existing local thumbnails only.
    Sheet {
        #[arg(long, conflicts_with = "ids")]
        query: Option<String>,
        #[arg(long, value_delimiter = ',', num_args = 1.., conflicts_with = "query")]
        ids: Vec<String>,
        #[arg(long, default_value_t = 16, value_parser = clap::value_parser!(u32).range(1..=256))]
        limit: u32,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u32).range(1..=16))]
        columns: u32,
        #[arg(long, default_value_t = 256, value_parser = clap::value_parser!(u32).range(32..=1024))]
        cell_size: u32,
    },
}
#[derive(Clone, ValueEnum)]
enum Kind {
    Original,
    Thumbnail,
    Preview,
}
impl Kind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::Thumbnail => "thumbnail",
            Self::Preview => "preview",
        }
    }
}
#[derive(Clone, ValueEnum)]
enum Capability {
    Media,
    Florence,
    Models,
}
impl Capability {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Media => "media",
            Self::Florence => "florence",
            Self::Models => "models",
        }
    }
}
#[derive(Subcommand)]
enum ImportCommand {
    /// Import or incrementally sync an Eagle library without reading originals.
    Eagle {
        path: PathBuf,
        /// Accepted for clarity: every run is an incremental sync by Eagle item id.
        #[arg(long)]
        sync: bool,
        /// lazy/none: never read originals; full: hash every original afterwards.
        #[arg(long, default_value = "lazy", value_parser = ["lazy", "none", "full"])]
        hash: String,
    },
}
#[derive(Subcommand)]
enum CollectionCommand {
    Create {
        name: String,
        #[arg(long)]
        parent: Option<String>,
    },
    List,
    Add {
        collection_id: String,
        asset_id: String,
    },
    Remove {
        collection_id: String,
        asset_id: String,
    },
    Rename {
        collection_id: String,
        name: String,
    },
    /// Set the folder description (omit TEXT to clear it).
    Describe {
        collection_id: String,
        text: Option<String>,
    },
    /// Delete a collection and its memberships; member assets are untouched.
    Delete {
        collection_id: String,
    },
    /// Re-parent (no --parent = root) and/or reorder among siblings.
    Move {
        collection_id: String,
        #[arg(long)]
        parent: Option<String>,
        #[arg(long)]
        index: Option<usize>,
    },
}
#[derive(Subcommand)]
enum ExploreCommand {
    /// Create or find exploration SLUG (folder Explorations/<title>); idempotent. --brief
    /// replaces the brief kept in the folder description.
    Ensure {
        slug: String,
        /// Folder name for a new exploration (default: the slug); an existing one keeps its name.
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        brief: Option<String>,
    },
    /// Register a finding: --image-url downloads an image, --file copies a local file (e.g. a
    /// screenshot) into the library, neither renders a note card. Re-runs with the same image
    /// URL, bytes or (cards) --kind + --name + --url return the existing asset (status
    /// existing); an existing asset in the Trash is left unchanged (status trashed). Needs VAMP_ACTOR.
    Add {
        slug: String,
        #[arg(long)]
        name: String,
        /// Why it matters (the asset description).
        #[arg(long)]
        note: Option<String>,
        #[arg(long, value_delimiter = ',')]
        tags: Vec<String>,
        /// Source page (asset source_url).
        #[arg(long)]
        url: Option<String>,
        /// Sub-folder under the exploration, e.g. people or studios/bakers (created as needed).
        #[arg(long)]
        folder: Option<String>,
        #[arg(long, conflicts_with = "file")]
        image_url: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        /// Note-card label (person, studio, link, theme, note); default link with --url, else note.
        #[arg(long, conflicts_with_all = ["image_url", "file"])]
        kind: Option<String>,
    },
    /// Replace the running findings summary (the brief is kept).
    Summary { slug: String, text: String },
    /// Brief, summary, folders and items with their current ratings/tags/notes.
    Show { slug: String },
    List,
    /// Links opening the exploration (selecting --asset) in the Mac app: url (http bounce
    /// through vamp-server's /open, clickable in any terminal), app_url (the vampire://
    /// deeplink) and open (a quoted shell command for it).
    Link {
        slug: String,
        #[arg(long)]
        asset: Option<String>,
        /// vamp-server base URL for the bounce [default: a replica's authority URL, else the
        /// context's server URL (`vamp contexts url`); none: url is null]
        #[arg(long, env = "VAMP_SERVER_URL")]
        server_url: Option<String>,
    },
    /// Changes by other actors since this VAMP_ACTOR's last read: raw events plus the current
    /// state of each touched item and folder events (then advances the cursor).
    Changes {
        slug: String,
        /// Read from this event sequence instead of the stored cursor.
        #[arg(long)]
        since: Option<u64>,
        /// Do not advance the stored cursor.
        #[arg(long)]
        peek: bool,
    },
}
#[derive(Subcommand)]
enum SmartCommand {
    List,
    /// RULES: JSON {"match":"all"|"any","rules":[{"field","op","value"}]}.
    Create {
        name: String,
        rules: String,
        #[arg(long)]
        parent: Option<String>,
    },
    Update {
        id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        rules: Option<String>,
    },
    Delete {
        id: String,
    },
}
#[derive(Subcommand)]
enum TagCommand {
    List,
    /// Rename on every asset, merging into an existing tag.
    Rename {
        from: String,
        to: String,
    },
    Delete {
        tag: String,
    },
}
#[derive(Subcommand)]
enum ProcessorCommand {
    /// Register a trusted executable from a ProcessorSpec JSON file. Runs with your privileges.
    Add {
        #[arg(long)]
        spec: PathBuf,
    },
    List,
    Remove {
        name: String,
    },
    /// Who runs this processor's jobs: any (default), local (authority only), remote (vamp worker only).
    Placement {
        name: String,
        #[arg(value_parser = ["any", "local", "remote"])]
        placement: String,
    },
    Enqueue {
        name: String,
        asset_id: String,
    },
    /// Queue this processor for every live asset (skips assets with a pending/running job).
    EnqueueAll {
        name: String,
        /// Also skip assets whose current original already has a completed job.
        #[arg(long)]
        missing_only: bool,
    },
}
#[derive(Subcommand)]
enum JobCommand {
    List {
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Execute a bounded batch. Worker failures result in a nonzero exit status.
    Run {
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Continuously drain queued jobs; emit JSON lines for nonempty batches and continue after job failures.
    Watch {
        #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u64).range(1..))]
        interval_seconds: u64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    Retry {
        id: String,
    },
    /// Delete queued jobs (never running ones); prints how many were removed.
    Cancel {
        #[arg(long, default_value = "pending", value_parser = ["pending", "failed"])]
        status: String,
        #[arg(long)]
        processor: Option<String>,
    },
}

fn json_file<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("cannot open JSON file {}", path.display()))?;
    serde_json::from_reader(file).with_context(|| format!("invalid JSON in {}", path.display()))
}

fn executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}
fn resolve_helper(home: &Path) -> Result<PathBuf> {
    if let Some(value) = std::env::var_os("VAMP_MEDIA_HELPER") {
        let path = PathBuf::from(value);
        ensure!(
            executable(&path),
            "VAMP_MEDIA_HELPER is not an executable file: {}. Set it to a working vamp-media binary or run vamp setup media --install --source PATH",
            path.display()
        );
        return Ok(path);
    }
    let binary = format!("vamp-media{}", std::env::consts::EXE_SUFFIX);
    let mut candidates = vec![home.join("tools/bin").join(&binary)];
    if let Ok(current) = std::env::current_exe()
        && let Some(parent) = current.parent()
    {
        candidates.push(parent.join(&binary));
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|dir| dir.join(&binary)));
    }
    candidates.into_iter().find(|path| executable(path)).context("optional vamp-media helper is unavailable. Core catalog commands do not need it. Install explicitly with vamp setup media --install --source PATH, or set VAMP_MEDIA_HELPER to an executable")
}

fn command_tree(command: &clap::Command) -> Value {
    let args: Vec<Value> = command.get_arguments().map(|arg| {
        let parser = arg.get_value_parser();
        let kind = if matches!(arg.get_action(), clap::ArgAction::SetTrue | clap::ArgAction::SetFalse | clap::ArgAction::Help | clap::ArgAction::HelpShort | clap::ArgAction::HelpLong | clap::ArgAction::Version) { "boolean" }
            else { match arg.get_id().as_str() {
                "limit" | "offset" | "after" | "expected_revision" | "columns" | "cell_size" | "interval_seconds" => "integer",
                "home" | "original_root" | "state_dir" | "source" | "path" | "metadata" | "patch" | "output" | "spec" => "path",
                _ => "string",
            }};
        let choices = parser.possible_values().map(|values| values.filter(|v| !v.is_hide_set()).map(|v| v.get_name().to_owned()).collect::<Vec<_>>());
        let defaults: Vec<_> = arg.get_default_values().iter().map(|s| s.to_string_lossy().into_owned()).collect();
        let cardinality = arg.get_num_args().map(|n| json!({"min":n.min_values(), "max":n.max_values()}));
        json!({"name":arg.get_id().as_str(), "long":arg.get_long(), "short":arg.get_short(), "position":arg.get_index(), "type":kind,
            "required":arg.is_required_set(), "global":arg.is_global_set(), "defaults":defaults, "choices":choices,
            "num_args":cardinality, "action":format!("{:?}", arg.get_action()), "env":arg.get_env().map(|s| s.to_string_lossy().into_owned()),
            "help":arg.get_help().map(ToString::to_string)})
    }).collect();
    json!({"name":command.get_name(), "about":command.get_about().map(ToString::to_string), "arguments":args,
        "subcommands":command.get_subcommands().map(command_tree).collect::<Vec<_>>()})
}

/// Config for display: remote bearer tokens are never printed.
fn redacted(mut config: vamp_protocol::Config) -> Result<Value> {
    for context in config.contexts.values_mut() {
        if let Some(remote) = &mut context.remote
            && !remote.token.is_empty()
        {
            remote.token = "<redacted>".into();
        }
    }
    Ok(serde_json::to_value(config)?)
}
/// Commands that change a catalog; replica mirrors are written only by sync.
fn mutates(command: &Command) -> bool {
    match command {
        Command::Media { command } => !matches!(
            command,
            MediaCommand::List { .. }
                | MediaCommand::Search { .. }
                | MediaCommand::Similar { .. }
                | MediaCommand::Inspect { .. }
                | MediaCommand::Fetch { .. }
                | MediaCommand::Sheet { .. }
        ),
        Command::Collections { command } => !matches!(command, CollectionCommand::List),
        Command::Explore { command } => !matches!(
            command,
            ExploreCommand::Show { .. } | ExploreCommand::List | ExploreCommand::Link { .. }
        ),
        Command::Smart { command } => !matches!(command, SmartCommand::List),
        Command::Tags { command } => !matches!(command, TagCommand::List),
        Command::Embedder { command } => !matches!(command, EmbedderCommand::Show),
        Command::Import { .. }
        | Command::Processors { .. }
        | Command::Jobs { .. }
        | Command::Storage { .. }
        | Command::Thumbs { .. }
        | Command::Galaxy {
            command: GalaxyCommand::Layout { .. },
        } => true,
        _ => false,
    }
}
fn execute(cli: Cli) -> Result<Value> {
    let home = cli.home.unwrap_or_else(default_home);
    match cli.command {
        Command::Guide => unreachable!("guide prints text in main"),
        Command::Schema => Ok(vamp_protocol::schema()),
        Command::Commands => {
            let mut command = Cli::command();
            command.build();
            Ok(
                json!({"protocol_version":vamp_protocol::PROTOCOL_VERSION,"command":command_tree(&command)}),
            )
        }
        Command::Init {
            name,
            original_root,
            state_dir,
        } => {
            let library =
                Library::init(&home, &name, original_root.as_deref(), state_dir.as_deref())?;
            Ok(json!({"name":name,"context":library.context}))
        }
        Command::Contexts { command } => match command {
            ContextCommand::List => redacted(load_config(&home)?),
            ContextCommand::Use { name } => redacted(select_context(&home, &name)?),
            ContextCommand::Add { name, state_dir } => Ok(serde_json::to_value(attach_context(
                &home, &name, &state_dir,
            )?)?),
            ContextCommand::Url { name, url, clear } => {
                let library = Library::open(&home, Some(&name))?;
                let url = match (url, clear) {
                    (None, false) => library.server_url()?,
                    (url, _) => library.set_server_url(url.as_deref())?,
                };
                Ok(json!({"name":name,"library_id":library.context.library_id,"server_url":url}))
            }
            #[cfg(feature = "remote")]
            ContextCommand::Remote { name, url, token } => {
                let context = vamp_core::remote::attach_remote(&home, &name, &url, &token)?;
                let mut library = Library::open(&home, Some(&name))?;
                let sync = vamp_core::remote::sync(&mut library)?;
                Ok(
                    json!({"name":name,"library_id":context.library_id,"state_dir":context.state_dir,"remote_url":url,"sync":sync}),
                )
            }
        },
        #[cfg(feature = "remote")]
        Command::Join {
            target,
            name,
            drive_roots,
        } => {
            let roots = drive_roots
                .iter()
                .map(|pair| {
                    pair.split_once('=')
                        .map(|(a, l)| (a.to_owned(), l.to_owned()))
                        .with_context(|| format!("--drive-root needs AUTHORITY=LOCAL: {pair}"))
                })
                .collect::<Result<_>>()?;
            let (name, context) = vamp_core::remote::join(&home, &target, name.as_deref(), roots)?;
            let mut library = Library::open(&home, Some(&name))?;
            let sync = vamp_core::remote::sync(&mut library)?;
            Ok(
                json!({"name":name,"library_id":context.library_id,"state_dir":context.state_dir,"remote_url":context.remote.map(|r| r.url),"sync":sync}),
            )
        }
        Command::Setup {
            capability,
            install,
            source,
            device,
        } => setup::run(
            &home,
            cli.context.as_deref(),
            capability.as_ref().map(Capability::as_str),
            install,
            source.as_deref(),
            device.as_deref(),
        ),
        #[cfg(feature = "remote")]
        Command::Sync {
            command: Some(SyncCommand::InstallService),
            ..
        } => sync_service::install(&home, cli.context.as_deref()),
        #[cfg(feature = "remote")]
        Command::SelfUpdate { check } => self_update::run(check),
        #[cfg(feature = "remote")]
        Command::Worker { command, args } => match command {
            None => {
                let WorkerConnection {
                    server,
                    token,
                    token_file,
                    processors,
                    concurrency,
                } = args.connection;
                let options = worker::Options {
                    server: server.context("--server (or VAMP_SERVER_URL) is required")?,
                    token: worker::token(token, token_file.as_deref())?,
                    worker_id: args.worker_id.unwrap_or_else(worker::default_worker_id),
                    processors,
                    concurrency,
                    lease_seconds: args.lease_seconds,
                    poll_seconds: args.poll_seconds,
                    exit_when_idle: args.exit_when_idle,
                };
                worker::run(&home, setup::model_processors(&home)?, options)
            }
            Some(WorkerCommand::InstallService { connection }) => {
                let server = connection
                    .server
                    .context("--server (or VAMP_SERVER_URL) is required")?;
                let home = if home.is_absolute() {
                    home
                } else {
                    std::env::current_dir()?.join(home)
                };
                // Verify locally installed processors and the token before writing anything.
                setup::model_processors(&home)?;
                let token_file = match connection.token_file {
                    Some(file) => {
                        worker::token(None, Some(&file))?;
                        Some(file)
                    }
                    None => match worker::token(connection.token, None)? {
                        // Tailscale identity auth: the unit carries no credential.
                        token if token.is_empty() => None,
                        token => {
                            let file = home.join("worker-token");
                            std::fs::create_dir_all(&home)?;
                            let mut options = std::fs::OpenOptions::new();
                            options.write(true).create(true).truncate(true);
                            #[cfg(unix)]
                            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
                            std::io::Write::write_all(&mut options.open(&file)?, token.as_bytes())?;
                            Some(file)
                        }
                    },
                };
                let program = std::env::current_exe()?;
                let (path, unit) = worker::service_unit(
                    &home,
                    &program,
                    &server,
                    token_file.as_deref(),
                    &connection.processors,
                    connection.concurrency,
                )?;
                std::fs::create_dir_all(path.parent().unwrap())?;
                std::fs::write(&path, &unit)?;
                Ok(
                    json!({"unit":path,"token_file":token_file,"exec":unit.lines().find(|l| l.starts_with("ExecStart=")),
                    "enable":"systemctl --user daemon-reload && systemctl --user enable --now vamp-worker.service",
                    "notes":["Run `loginctl enable-linger $USER` once so the worker runs without a login session.",
                        "Logs: journalctl --user -u vamp-worker -f"]}),
                )
            }
        },
        command => {
            let mut library = Library::open(&home, cli.context.as_deref())?;
            ensure!(
                library.context.remote.is_none() || !mutates(&command),
                "this context is a read-only replica; change the authority library (vamp-server) or use the Vampire app, then vamp sync"
            );
            match command {
                Command::Media { command } => match command {
                    MediaCommand::Add {
                        path,
                        name,
                        metadata,
                    } => {
                        let metadata: Metadata = metadata
                            .as_deref()
                            .map(json_file)
                            .transpose()?
                            .unwrap_or_default();
                        Ok(serde_json::to_value(library.add(
                            &path,
                            name.as_deref(),
                            metadata,
                        )?)?)
                    }
                    MediaCommand::List {
                        query,
                        added_by,
                        page,
                    } => Ok(serde_json::to_value(library.list(
                        query.as_deref(),
                        page.limit,
                        page.offset,
                        added_by.as_deref(),
                    )?)?),
                    MediaCommand::Search {
                        query,
                        semantic,
                        hybrid,
                        added_by,
                        page,
                    } => {
                        if semantic {
                            ensure!(
                                page.offset == 0,
                                "--offset is not supported with --semantic; raise --limit"
                            );
                            let started = std::time::Instant::now();
                            #[cfg(feature = "remote")]
                            if library.context.remote.is_some() {
                                let mut value = vamp_core::remote::semantic_search(
                                    &library,
                                    &query,
                                    page.limit,
                                    hybrid,
                                    added_by.as_deref(),
                                )?;
                                value["elapsed_ms"] = json!(started.elapsed().as_millis() as u64);
                                return Ok(value);
                            }
                            let results = library.semantic_search(
                                &query,
                                page.limit,
                                hybrid,
                                added_by.as_deref(),
                            )?;
                            Ok(
                                json!({"semantic":true,"hybrid":hybrid,"elapsed_ms":started.elapsed().as_millis() as u64,"results":results}),
                            )
                        } else {
                            Ok(serde_json::to_value(library.list(
                                Some(&query),
                                page.limit,
                                page.offset,
                                added_by.as_deref(),
                            )?)?)
                        }
                    }
                    MediaCommand::Similar { id, limit } => {
                        Ok(json!({"results":library.similar(&id, limit)?}))
                    }
                    MediaCommand::AcceptTags { id, tags, revision } => Ok(serde_json::to_value(
                        library.accept_tags(&id, &tags, revision.expected_revision)?,
                    )?),
                    MediaCommand::Inspect { id } => {
                        Ok(serde_json::to_value(library.inspect(&id)?)?)
                    }
                    MediaCommand::Update {
                        id,
                        patch,
                        revision,
                    } => {
                        let patch: MetadataPatch = json_file(&patch)?;
                        Ok(serde_json::to_value(library.update(
                            &id,
                            patch,
                            revision.expected_revision,
                        )?)?)
                    }
                    MediaCommand::Refresh { id, revision } => Ok(serde_json::to_value(
                        library.refresh(&id, revision.expected_revision)?,
                    )?),
                    MediaCommand::Remove { id, revision } => {
                        library.remove(&id, revision.expected_revision)?;
                        Ok(json!({"id":id,"trashed":true,"original_deleted":false}))
                    }
                    MediaCommand::Restore { ids } => Ok(json!({"restored":library.restore(&ids)?})),
                    MediaCommand::EmptyTrash { ids } => Ok(json!({
                        "deleted": library.empty_trash((!ids.is_empty()).then_some(ids.as_slice()))?
                    })),
                    MediaCommand::Hash { limit } => library.hash_pending(limit as usize),
                    MediaCommand::Export { ids, to } => library.export_originals(&ids, &to),
                    MediaCommand::Fetch { id, kind } => {
                        #[cfg(feature = "remote")]
                        let path = vamp_core::remote::representation(&library, &id, kind.as_str())?;
                        #[cfg(not(feature = "remote"))]
                        let path = library.fetch(&id, kind.as_str())?;
                        Ok(json!({"id":id,"kind":kind.as_str(),"path":path}))
                    }
                    MediaCommand::Thumbnail { id } => Ok(serde_json::to_value(library.generate(
                        &id,
                        "thumbnail",
                        &resolve_helper(&home)?,
                    )?)?),
                    MediaCommand::Preview { id } => Ok(serde_json::to_value(library.generate(
                        &id,
                        "preview",
                        &resolve_helper(&home)?,
                    )?)?),
                    MediaCommand::Sheet {
                        query,
                        ids,
                        limit,
                        output,
                        columns,
                        cell_size,
                    } => {
                        ensure!(
                            ids.len() <= 256,
                            "a sheet accepts at most 256 explicit asset IDs"
                        );
                        let ids = if ids.is_empty() {
                            library
                                .list(query.as_deref(), limit as usize, 0, None)?
                                .into_iter()
                                .map(|asset| asset.id)
                                .collect()
                        } else {
                            ids
                        };
                        ensure!(!ids.is_empty(), "no assets selected for sheet");
                        let mut seen = std::collections::HashSet::new();
                        let mut items = Vec::with_capacity(ids.len());
                        for id in ids {
                            ensure!(seen.insert(id.clone()), "duplicate sheet asset ID: {id}");
                            let path = library.fetch(&id, "thumbnail").with_context(|| format!("sheet requires an existing local thumbnail for {id}; explicitly run vamp media thumbnail {id}"))?;
                            items.push(SheetItem {
                                id,
                                path: path.to_string_lossy().into_owned(),
                            });
                        }
                        let helper = resolve_helper(&home)?;
                        let request = ToolRequest {
                            operation: "sheet".into(),
                            output_path: Some(output.to_string_lossy().into_owned()),
                            items,
                            columns: Some(columns),
                            cell_size: Some(cell_size),
                            library_id: Some(library.context.library_id.clone()),
                            ..Default::default()
                        };
                        let response = vamp_core::run_tool(
                            &[helper.to_string_lossy().into_owned()],
                            &request,
                            300,
                        )?;
                        let representation = response
                            .representation
                            .context("media helper returned no sheet representation")?;
                        let tiles = response
                            .tiles
                            .context("media helper returned no stable asset-ID tile mapping")?;
                        ensure!(
                            tiles.len() == request.items.len()
                                && tiles
                                    .iter()
                                    .zip(&request.items)
                                    .all(|(tile, item)| tile.asset_id == item.id),
                            "media helper returned an invalid sheet asset-ID mapping"
                        );
                        Ok(
                            json!({"output":representation.path,"representation":representation,"tiles":tiles}),
                        )
                    }
                },
                Command::Import {
                    command: ImportCommand::Eagle { path, hash, .. },
                } => library.import_eagle_with(&path, hash == "full", &mut |p| {
                    // Human progress on stderr; stdout stays one JSON document.
                    eprintln!(
                        "eagle import: {} {}/{} (created {}, updated {}, trashed {})",
                        p.phase, p.done, p.total, p.created, p.updated, p.trashed
                    );
                }),
                Command::Sidebar => library.sidebar(),
                Command::Thumbs {
                    command: ThumbsCommand::Regenerate { tier },
                } => {
                    let mut out = json!({});
                    if matches!(tier, TierArg::Grid | TierArg::All) {
                        let started = std::time::Instant::now();
                        let mut grid = vamp_core::thumbs::generate_all_with(&mut library, &mut |done, total| {
                            eprintln!("grid tiers: {done}/{total}");
                        })?;
                        grid["seconds"] = json!(started.elapsed().as_secs_f64());
                        out["grid"] = grid;
                    }
                    if matches!(tier, TierArg::Preview | TierArg::All) {
                        let started = std::time::Instant::now();
                        let mut preview = vamp_core::thumbs::generate_hires_with(&mut library, &mut |done, total| {
                            eprintln!("preview tiers: {done}/{total}");
                        })?;
                        preview["seconds"] = json!(started.elapsed().as_secs_f64());
                        out["preview"] = preview;
                    }
                    out["stats"] = vamp_core::thumbs::tier_stats(&library)?;
                    Ok(out)
                }
                Command::Thumbs {
                    command: ThumbsCommand::Stats,
                } => vamp_core::thumbs::tier_stats(&library),
                Command::Galaxy { command } => match command {
                    GalaxyCommand::Layout { rebuild } => library.galaxy_refresh(rebuild),
                    GalaxyCommand::Search { text, limit } => library.galaxy_search(&text, limit),
                },
                Command::Smart { command } => match command {
                    SmartCommand::List => {
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
                    SmartCommand::Create {
                        name,
                        rules,
                        parent,
                    } => Ok(serde_json::to_value(library.smart_folder_create(
                        &name,
                        serde_json::from_str(&rules).context("invalid rules JSON")?,
                        parent.as_deref(),
                    )?)?),
                    SmartCommand::Update { id, name, rules } => {
                        let rules = rules
                            .map(|r| serde_json::from_str(&r))
                            .transpose()
                            .context("invalid rules JSON")?;
                        Ok(serde_json::to_value(library.smart_folder_update(
                            &id,
                            name.as_deref(),
                            rules,
                        )?)?)
                    }
                    SmartCommand::Delete { id } => {
                        library.smart_folder_delete(&id)?;
                        Ok(json!({"deleted":id}))
                    }
                },
                Command::Tags { command } => match command {
                    TagCommand::List => Ok(json!({
                        "tags": library.tag_counts()?.into_iter().map(|(tag, count)| json!({"tag":tag,"count":count})).collect::<Vec<_>>(),
                        "groups": library.tag_groups()?,
                        "starred": library.starred_tags()?,
                    })),
                    TagCommand::Rename { from, to } => {
                        Ok(json!({"updated":library.tag_rename(&from, &to)?}))
                    }
                    TagCommand::Delete { tag } => Ok(json!({"updated":library.tag_delete(&tag)?})),
                },
                Command::Collections { command } => match command {
                    CollectionCommand::Create { name, parent } => Ok(serde_json::to_value(
                        library.collection_create(&name, parent.as_deref())?,
                    )?),
                    CollectionCommand::Move {
                        collection_id,
                        parent,
                        index,
                    } => Ok(serde_json::to_value(library.collection_move(
                        &collection_id,
                        parent.as_deref(),
                        index,
                    )?)?),
                    CollectionCommand::List => Ok(serde_json::to_value(library.collections()?)?),
                    CollectionCommand::Add {
                        collection_id,
                        asset_id,
                    } => Ok(serde_json::to_value(library.collection_membership(
                        &collection_id,
                        &asset_id,
                        true,
                    )?)?),
                    CollectionCommand::Remove {
                        collection_id,
                        asset_id,
                    } => Ok(serde_json::to_value(library.collection_membership(
                        &collection_id,
                        &asset_id,
                        false,
                    )?)?),
                    CollectionCommand::Rename {
                        collection_id,
                        name,
                    } => Ok(serde_json::to_value(
                        library.collection_rename(&collection_id, &name)?,
                    )?),
                    CollectionCommand::Describe {
                        collection_id,
                        text,
                    } => Ok(serde_json::to_value(
                        library.collection_describe(&collection_id, text.as_deref())?,
                    )?),
                    CollectionCommand::Delete { collection_id } => {
                        library.collection_delete(&collection_id)?;
                        Ok(serde_json::json!({"deleted":collection_id}))
                    }
                },
                Command::Explore { command } => match command {
                    ExploreCommand::Ensure { slug, title, brief } => {
                        vamp_core::explore::agent_actor()?;
                        library.explore_ensure(&slug, title.as_deref(), brief.as_deref())
                    }
                    ExploreCommand::Add {
                        slug,
                        name,
                        note,
                        tags,
                        url,
                        folder,
                        image_url,
                        file,
                        kind,
                    } => {
                        use vamp_core::explore::{Content, Finding};
                        vamp_core::explore::agent_actor()?;
                        let content = match (image_url, file) {
                            (Some(image_url), _) => {
                                #[cfg(not(feature = "remote"))]
                                bail!("--image-url {image_url} needs a vamp built with the remote feature; download it and pass --file");
                                #[cfg(feature = "remote")]
                                {
                                    let (bytes, filename) = vamp_core::explore::download(&image_url)?;
                                    Content::Bytes { bytes, filename, image_url: Some(image_url) }
                                }
                            }
                            (None, Some(file)) => Content::Bytes {
                                bytes: std::fs::read(&file)
                                    .with_context(|| format!("cannot read {}", file.display()))?,
                                filename: file
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| "file".into()),
                                image_url: None,
                            },
                            (None, None) => Content::Card {
                                kind: kind.unwrap_or_else(|| {
                                    if url.is_some() { "link" } else { "note" }.into()
                                }),
                            },
                        };
                        let helper = resolve_helper(&home).ok();
                        library.explore_add(
                            &slug,
                            Finding { name, note, tags, url, folder, content },
                            helper.as_deref(),
                        )
                    }
                    ExploreCommand::Summary { slug, text } => {
                        vamp_core::explore::agent_actor()?;
                        library.explore_summary(&slug, &text)
                    }
                    ExploreCommand::Show { slug } => library.explore_show(&slug),
                    ExploreCommand::List => library.explore_list(),
                    ExploreCommand::Link { slug, asset, server_url } => {
                        let server = match server_url
                            .or_else(|| library.context.remote.as_ref().map(|r| r.url.clone()))
                        {
                            Some(url) => Some(url),
                            None => library.server_url()?,
                        };
                        library.explore_link(&slug, asset.as_deref(), server.as_deref())
                    }
                    ExploreCommand::Changes { slug, since, peek } => {
                        library.explore_changes(&slug, since, !peek)
                    }
                },
                Command::Events { after, limit } => {
                    Ok(serde_json::to_value(library.events(after, limit)?)?)
                }
                Command::Export => library.export(),
                Command::Processors { command } => match command {
                    ProcessorCommand::Add { spec } => {
                        let spec: ProcessorSpec = json_file(&spec)?;
                        let name = spec.name.clone();
                        library.processor_add(spec)?;
                        Ok(json!({"name":name,"registered":true}))
                    }
                    ProcessorCommand::List => Ok(serde_json::to_value(library.processors()?)?),
                    ProcessorCommand::Remove { name } => {
                        library.processor_remove(&name)?;
                        Ok(json!({"name":name,"removed":true}))
                    }
                    ProcessorCommand::Placement { name, placement } => {
                        let placement = serde_json::from_value(json!(placement))?;
                        Ok(serde_json::to_value(
                            library.processor_placement(&name, placement)?,
                        )?)
                    }
                    ProcessorCommand::Enqueue { name, asset_id } => {
                        Ok(serde_json::to_value(library.enqueue(&name, &asset_id)?)?)
                    }
                    ProcessorCommand::EnqueueAll { name, missing_only } => {
                        library.enqueue_all(&name, missing_only)
                    }
                },
                Command::Jobs { command } => match command {
                    JobCommand::List { status, limit } => Ok(serde_json::to_value(
                        library.jobs(status.as_deref(), limit)?,
                    )?),
                    JobCommand::Run { limit } => {
                        ensure!(limit > 0, "job run limit must be positive");
                        let result = library.run_jobs(limit)?;
                        if result.get("failed").and_then(Value::as_u64).unwrap_or(0) > 0 {
                            bail!("one or more jobs or uploads failed: {result}");
                        }
                        Ok(result)
                    }
                    JobCommand::Watch {
                        interval_seconds,
                        limit,
                    } => {
                        use std::io::Write;
                        ensure!(limit > 0, "job watch limit must be positive");
                        let stdout = std::io::stdout();
                        let mut out = stdout.lock();
                        loop {
                            let result = library.run_jobs(limit)?;
                            let completed =
                                result.get("completed").and_then(Value::as_u64).unwrap_or(0);
                            let failed = result.get("failed").and_then(Value::as_u64).unwrap_or(0);
                            if completed > 0 || failed > 0 {
                                serde_json::to_writer(&mut out, &result)?;
                                writeln!(out)?;
                                out.flush()?;
                            } else {
                                std::thread::sleep(std::time::Duration::from_secs(
                                    interval_seconds,
                                ));
                            }
                        }
                    }
                    JobCommand::Retry { id } => Ok(serde_json::to_value(library.retry_job(&id)?)?),
                    JobCommand::Cancel { status, processor } => Ok(
                        json!({"cancelled": library.cancel_jobs(&status, processor.as_deref())?}),
                    ),
                },
                #[cfg(feature = "remote")]
                Command::Sync { watch: false, .. } => {
                    // Metadata first (committed), then the thumbnail queue, 8 at a time.
                    let mut value = vamp_core::remote::sync(&mut library)?;
                    value["thumbnails"] =
                        vamp_core::remote::download_thumbnails(&mut library, &mut |_, _| {})?;
                    Ok(value)
                }
                #[cfg(feature = "remote")]
                Command::Sync { watch: true, .. } => {
                    use std::io::Write;
                    let mut out = std::io::stdout().lock();
                    let mut line = |value: &Value| -> Result<()> {
                        serde_json::to_writer(&mut out, value)?;
                        writeln!(out)?;
                        Ok(out.flush()?)
                    };
                    let now_ms = || {
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |d| d.as_millis() as u64)
                    };
                    let mut value = vamp_core::remote::sync(&mut library)?;
                    value["at_ms"] = json!(now_ms());
                    line(&value)?;
                    // Thumbnails never hold up the live event stream.
                    let (home, context) = (home.clone(), cli.context.clone());
                    std::thread::spawn(move || loop {
                        if let Err(error) = Library::open(&home, context.as_deref()).and_then(
                            |mut library| {
                                vamp_core::remote::download_thumbnails(&mut library, &mut |_, _| {})
                            },
                        ) {
                            eprintln!("thumbnails: {error:#}");
                        }
                        std::thread::sleep(std::time::Duration::from_secs(5));
                    });
                    loop {
                        match vamp_core::remote::sync_wait(&mut library, 25) {
                            Ok(mut value) => {
                                if value["changed"] == true {
                                    value["at_ms"] = json!(now_ms());
                                    line(&value)?;
                                }
                            }
                            // Authority restarting or unreachable: report and retry.
                            Err(error) => {
                                line(&json!({"error":format!("{error:#}"),"at_ms":now_ms()}))?;
                                std::thread::sleep(std::time::Duration::from_secs(5));
                            }
                        }
                    }
                }
                Command::Embedder { command } => match command {
                    EmbedderCommand::Set { spec } => {
                        let spec: EmbedderSpec = json_file(&spec)?;
                        library.embedder_set(spec.clone())?;
                        Ok(json!({"registered":true,"embedder":spec}))
                    }
                    EmbedderCommand::Show => Ok(json!({"embedder":library.embedder()?})),
                    EmbedderCommand::Remove => Ok(json!({"removed":library.embedder_remove()?})),
                },
                Command::Storage { command } => match command {
                    StorageCommand::Set { backend } => {
                        let (backend, eviction, save) = match backend {
                            StorageBackend::S3 {
                                bucket,
                                prefix,
                                endpoint,
                                region,
                                path_style,
                                save_credentials,
                                eviction,
                            } => (
                                Backend::S3 {
                                    bucket,
                                    prefix,
                                    endpoint,
                                    region,
                                    path_style,
                                },
                                eviction,
                                save_credentials,
                            ),
                            StorageBackend::Folder { root, eviction } => (
                                Backend::Folder {
                                    root: root.to_string_lossy().into_owned(),
                                },
                                eviction,
                                false,
                            ),
                        };
                        let credentials = if save {
                            let key = std::env::var("AWS_ACCESS_KEY_ID")
                                .context("--save-credentials needs AWS_ACCESS_KEY_ID")?;
                            let secret = std::env::var("AWS_SECRET_ACCESS_KEY")
                                .context("--save-credentials needs AWS_SECRET_ACCESS_KEY")?;
                            let token = std::env::var("AWS_SESSION_TOKEN").ok();
                            Some(library.storage_save_credentials(
                                &key,
                                &secret,
                                token.as_deref(),
                            )?)
                        } else {
                            None
                        };
                        let storage = library.storage_set(StorageConfig {
                            backend,
                            delete_local_after_upload: eviction.delete_local_after_upload,
                            delete_local_anywhere: eviction.delete_local_anywhere,
                        })?;
                        Ok(json!({"storage":storage,"credentials_file":credentials}))
                    }
                    StorageCommand::Show => Ok(json!({
                        "storage": library.storage_config()?,
                        "uploads": library.upload_counts()?,
                    })),
                    StorageCommand::Unset => {
                        library.storage_unset()?;
                        Ok(json!({"storage":null}))
                    }
                    StorageCommand::Migrate { limit } => library.storage_migrate(limit),
                    StorageCommand::Uploads { status, limit } => Ok(serde_json::to_value(
                        library.uploads(status.as_deref(), limit)?,
                    )?),
                },
                Command::Cache { command } => match command {
                    CacheCommand::Prune => library.cache_prune(),
                    #[cfg(feature = "remote")]
                    CacheCommand::Policy {
                        previews,
                        preview_cap,
                        originals_cap,
                    } => {
                        use vamp_core::cache::{CachePolicy, Previews};
                        let mut policy = CachePolicy::load(&library.state_dir)?;
                        if previews.is_none() && preview_cap.is_none() && originals_cap.is_none() {
                            return Ok(serde_json::to_value(policy)?);
                        }
                        if let Some(previews) = previews {
                            policy.previews = match previews {
                                PreviewPolicy::Off => Previews::Off,
                                PreviewPolicy::OnDemand => Previews::OnDemand,
                                PreviewPolicy::All => Previews::All,
                                PreviewPolicy::Cap => Previews::Cap,
                            };
                        }
                        if let Some(Cap(cap)) = preview_cap {
                            policy.preview_cap_bytes = cap;
                        }
                        if let Some(Cap(cap)) = originals_cap {
                            policy.originals_cap_bytes = cap;
                        }
                        vamp_core::cache::set_policy(&library, &policy)?;
                        Ok(serde_json::to_value(policy)?)
                    }
                    #[cfg(feature = "remote")]
                    CacheCommand::Stats => vamp_core::cache::stats(&library),
                },
                _ => unreachable!("non-library commands handled before opening a catalog"),
            }
        }
    }
}

fn print_error(code: &str, message: String) {
    eprintln!(
        "{}",
        json!({"protocol_version":vamp_protocol::PROTOCOL_VERSION,"error":{"code":code,"message":message}})
    );
}
/// docs/agents/SKILL.md (also installed as a Claude skill) without its YAML front matter
/// (build.rs).
fn guide() -> &'static str {
    include_str!(concat!(env!("OUT_DIR"), "/guide.md"))
}
fn main() -> ExitCode {
    vamp_core::allow_cloud_placeholder_reads();
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                return match error.print() {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(_) => ExitCode::FAILURE,
                };
            }
            print_error("invalid_arguments", error.to_string());
            return ExitCode::from(2);
        }
    };
    if matches!(cli.command, Command::Guide) {
        print!("{}", guide());
        return ExitCode::SUCCESS;
    }
    match execute(cli) {
        Ok(value) => {
            use std::io::Write;
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            if let Err(error) = serde_json::to_writer(&mut out, &value)
                .map_err(std::io::Error::other)
                .and_then(|()| writeln!(out))
            {
                print_error("output_failed", error.to_string());
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            print_error("operation_failed", format!("{error:#}"));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shell-like split: quotes group (and are removed); `quoted` marks tokens that had any.
    fn words(line: &str) -> Vec<(String, bool)> {
        let mut out = Vec::new();
        let mut current: Option<(String, bool)> = None;
        let mut quote = None;
        for c in line.chars() {
            match (quote, c) {
                (Some(q), c) if c == q => quote = None,
                (Some(_), c) => current.get_or_insert_default().0.push(c),
                (None, '\'' | '"') => {
                    quote = Some(c);
                    current.get_or_insert_default().1 = true;
                }
                (None, c) if c.is_whitespace() => out.extend(current.take()),
                (None, c) => current.get_or_insert_default().0.push(c),
            }
        }
        out.extend(current);
        out
    }

    /// Every `vamp …` command in the manual (inline code and code-block lines) parses with
    /// the CLI definition. Manual notation: `[x]` optional (included here), `a|b` choices
    /// (the first is used), `<id>`/NAME placeholders, `N` a number, `# …` a comment.
    #[test]
    fn manual_commands_parse() {
        let mut commands: Vec<&str> = Vec::new();
        let mut block = false;
        for line in guide().lines() {
            if line.trim_start().starts_with("```") {
                block = !block;
            } else if block {
                commands.push(line.split(" # ").next().unwrap().trim());
            } else {
                commands.extend(line.split('`').skip(1).step_by(2));
            }
        }
        let commands: Vec<&str> = commands.into_iter().filter(|c| c.starts_with("vamp ")).collect();
        assert!(commands.len() > 30, "found only {} commands", commands.len());
        let mut failures = Vec::new();
        for command in commands {
            let args: Vec<String> = words(command)
                .into_iter()
                .map(|(word, quoted)| {
                    if quoted {
                        return word;
                    }
                    let word = word.trim_start_matches('[').trim_end_matches(']');
                    match word.split('|').next().unwrap() {
                        "N" => "1".to_owned(),
                        word => word.to_owned(),
                    }
                })
                .collect();
            if !cfg!(feature = "remote") && matches!(args[1].as_str(), "join" | "sync" | "worker" | "self-update") {
                continue;
            }
            if let Err(error) = Cli::try_parse_from(&args) {
                failures.push(format!("{command}\n  {}", error.render().to_string().lines().next().unwrap_or("")));
            }
        }
        assert!(failures.is_empty(), "manual commands that do not parse:\n{}", failures.join("\n"));
    }
}
