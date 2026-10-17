//! Explorations: agent research missions filed in the library for the owner to review.
//!
//! An exploration is a folder under the top-level `Explorations` folder, found by its stable
//! slug (`source_id = exploration:<slug>`, so the owner may rename or move it), with optional
//! sub-folders (`exploration:<slug>/<path>`). Its description holds the brief and the running
//! findings summary. Findings are ordinary assets filed into it: downloaded images, copied
//! local files (screenshots) and, for text-only findings, rendered note cards
//! ([`crate::card`]). Each carries `metadata.extra.exploration` {slug, kind, note, actor,
//! image_url?}. [`Library::explore_changes`] reports what others changed in the exploration
//! since the reader's last read, as raw events and current item state; what those changes
//! mean is up to whoever reads them.
use super::*;
use std::collections::{BTreeMap, HashSet};

/// URL scheme the Vampire Mac app registers: `vampire://open?library=…&folder=…[&asset=…]`.
pub const SCHEME: &str = "vampire";
const ROOT_NAME: &str = "Explorations";
const ROOT_SOURCE: &str = "explorations";
/// Separates the brief from the findings summary in the folder description.
const FINDINGS: &str = "## Findings";

fn source(slug: &str) -> String {
    format!("exploration:{slug}")
}

/// `[a-z0-9][a-z0-9-]*`, at most 64 characters.
pub fn validate_slug(slug: &str) -> Result<()> {
    ensure!(
        !slug.is_empty()
            && slug.len() <= 64
            && slug
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !slug.starts_with('-'),
        "exploration slug must be lowercase letters, digits and '-' (at most 64): {slug:?}"
    );
    Ok(())
}

/// The agent writing explorations: an explicit `VAMP_ACTOR` (writes by anyone else, e.g. the
/// owner's apps, carry other actors, which is how `explore_changes` tells them apart).
pub fn agent_actor() -> Result<String> {
    let actor = current_actor();
    ensure!(
        actor != "local",
        "set VAMP_ACTOR to this agent's name (e.g. VAMP_ACTOR=agent:claude) before exploring: explore changes lists every change made by another actor"
    );
    Ok(actor)
}

/// Splits a folder description into (brief, findings summary).
fn split_description(description: Option<&str>) -> (String, String) {
    let text = description.unwrap_or("");
    match text.split_once(FINDINGS) {
        Some((brief, summary)) => (brief.trim().to_owned(), summary.trim().to_owned()),
        None => (text.trim().to_owned(), String::new()),
    }
}
fn join_description(brief: &str, summary: &str) -> String {
    match (brief.trim(), summary.trim()) {
        (brief, "") => brief.to_owned(),
        ("", summary) => format!("{FINDINGS}\n{summary}"),
        (brief, summary) => format!("{brief}\n\n{FINDINGS}\n{summary}"),
    }
}

/// The deeplink opening `folder` (and selecting `asset`) in the Vampire app.
pub fn link(library_id: &str, folder: &str, asset: Option<&str>) -> String {
    let mut url = format!("{SCHEME}://open?library={library_id}&folder={folder}");
    if let Some(asset) = asset {
        url.push_str("&asset=");
        url.push_str(asset);
    }
    url
}

/// What a finding brings into the library.
pub enum Content {
    /// Image or file bytes (downloaded or read by the caller). `image_url` is where a
    /// downloaded image came from; it identifies the finding on re-runs.
    Bytes {
        bytes: Vec<u8>,
        filename: String,
        image_url: Option<String>,
    },
    /// A text-only finding rendered as a note card; `kind` labels it (person, studio, link…).
    Card { kind: String },
}

pub struct Finding {
    pub name: String,
    /// Why it matters (becomes the asset description).
    pub note: Option<String>,
    pub tags: Vec<String>,
    /// Source page (asset `source_url`).
    pub url: Option<String>,
    /// Sub-folder path under the exploration, e.g. `people` or `studios/bakers`.
    pub folder: Option<String>,
    pub content: Content,
}

/// A card name as it identifies the card: lowercase, whitespace collapsed.
fn card_name(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Downloads an http(s) URL (at most 64 MiB, 30 s): bytes and a filename with an image
/// extension from the URL path or content type.
#[cfg(feature = "remote")]
pub fn download(url: &str) -> Result<(Vec<u8>, String)> {
    ensure!(
        url.starts_with("https://") || url.starts_with("http://"),
        "image URL must be http(s): {url}"
    );
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .max_redirects(5)
        // Some image hosts (e.g. Wikimedia) reject requests without an identifying agent.
        .user_agent(concat!("vamp/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let mut response = agent
        .get(url)
        .call()
        .with_context(|| format!("download failed: {url}"))?;
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_owned();
    let bytes = response
        .body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_to_vec()
        .with_context(|| format!("download failed or exceeds 64 MiB: {url}"))?;
    ensure!(!content_type.starts_with("text/html"), "{url} is a web page, not an image; pass the page as --url and an image (or a screenshot via --file) separately");
    let last = url.split(['?', '#']).next().unwrap_or("").rsplit('/').next().unwrap_or("");
    let has_ext = Path::new(last)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| (1..=5).contains(&e.len()) && e.chars().all(|c| c.is_ascii_alphanumeric()));
    let ext = match content_type.as_str() {
        "image/jpeg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        "image/avif" => Some("avif"),
        "image/svg+xml" => Some("svg"),
        "image/tiff" => Some("tiff"),
        _ => None,
    };
    let filename = match (has_ext, ext) {
        (true, _) => last.to_owned(),
        (false, Some(ext)) => format!("image.{ext}"),
        (false, None) => "image".to_owned(),
    };
    Ok((bytes, filename))
}

impl Library {
    fn folder_by_source(&self, source_id: &str) -> Result<Option<Collection>> {
        Ok(catalog::collection_rows(&self.conn)?
            .into_iter()
            .find(|c| c.source_id.as_deref() == Some(source_id)))
    }
    pub(crate) fn exploration(&self, slug: &str) -> Result<Collection> {
        validate_slug(slug)?;
        self.folder_by_source(&source(slug))?.with_context(|| {
            format!("no exploration {slug:?}; create it with vamp explore ensure {slug}")
        })
    }
    /// Folder with this `source_id`, created (last among `parent`'s children) when missing.
    fn ensure_folder(
        &mut self,
        source_id: &str,
        name: &str,
        parent: Option<&str>,
    ) -> Result<Collection> {
        // Looked up inside the (immediate) write transaction: concurrent agents never create
        // the same folder twice.
        let tx = self.conn.transaction()?;
        if let Some(existing) = catalog::collection_rows(&tx)?
            .into_iter()
            .find(|c| c.source_id.as_deref() == Some(source_id))
        {
            return Ok(existing);
        }
        let sort_order: i64 = tx.query_row(
            "SELECT coalesce(max(sort_order)+1,0) FROM collections WHERE parent_id IS ?1",
            [parent],
            |r| r.get(0),
        )?;
        let collection = Collection {
            id: id(),
            name: name.into(),
            asset_ids: vec![],
            parent_id: parent.map(str::to_owned),
            sort_order,
            description: None,
            color: None,
            icon: None,
            source_id: Some(source_id.into()),
        };
        catalog::insert_collection(&tx, &collection)?;
        emit(&tx, &self.context, "collection_created", None, json!(&collection))?;
        tx.commit()?;
        Ok(collection)
    }
    /// The exploration folder and every folder below it (the exploration first).
    pub(crate) fn subtree(&self, root: &str) -> Result<Vec<Collection>> {
        let rows = catalog::collection_rows(&self.conn)?;
        let mut ids: HashSet<String> = HashSet::from([root.to_owned()]);
        // Rows come parents first, so one pass collects every descendant.
        let mut out = Vec::new();
        for row in rows {
            if row.id == root {
                out.insert(0, row);
            } else if row.parent_id.as_ref().is_some_and(|p| ids.contains(p)) {
                ids.insert(row.id.clone());
                out.push(row);
            }
        }
        Ok(out)
    }

    /// Creates or finds exploration `slug` under `Explorations`. `title` names a new folder
    /// (default: the slug; an existing folder keeps the owner's name); `brief` replaces the
    /// brief part of its description.
    pub fn explore_ensure(
        &mut self,
        slug: &str,
        title: Option<&str>,
        brief: Option<&str>,
    ) -> Result<Value> {
        validate_slug(slug)?;
        let folder = match self.folder_by_source(&source(slug))? {
            Some(folder) => folder,
            None => {
                let root = self.ensure_folder(ROOT_SOURCE, ROOT_NAME, None)?;
                let name = title.map(str::trim).filter(|t| !t.is_empty()).unwrap_or(slug);
                self.ensure_folder(&source(slug), name, Some(&root.id))?
            }
        };
        if let Some(brief) = brief {
            let (old, summary) = split_description(folder.description.as_deref());
            if old != brief.trim() {
                self.collection_describe(&folder.id, Some(&join_description(brief, &summary)))?;
            }
        }
        self.explore_show(slug)
    }

    /// Replaces the running findings summary (the brief is kept).
    pub fn explore_summary(&mut self, slug: &str, summary: &str) -> Result<Value> {
        let folder = self.exploration(slug)?;
        let (brief, old) = split_description(folder.description.as_deref());
        if old != summary.trim() {
            self.collection_describe(&folder.id, Some(&join_description(&brief, summary)))?;
        }
        self.explore_show(slug)
    }

    /// Sub-folder `path` (`a/b`) of the exploration, created as needed.
    pub fn explore_folder(&mut self, slug: &str, path: &str) -> Result<Collection> {
        let mut folder = self.exploration(slug)?;
        let mut prefix = String::new();
        for part in path.split('/').map(str::trim).filter(|p| !p.is_empty()) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            folder = self.ensure_folder(&format!("{}/{prefix}", source(slug)), part, Some(&folder.id))?;
        }
        Ok(folder)
    }

    /// Links opening exploration `slug` (selecting `asset`) in the Mac app: `url`, the http
    /// bounce page `server` (a vamp-server base URL) serves at `/open` (clickable where only
    /// http(s) is linkified; null without a server), `app_url`, the `vampire://` deeplink
    /// itself, and `open`, a shell command opening it.
    pub fn explore_link(&self, slug: &str, asset: Option<&str>, server: Option<&str>) -> Result<Value> {
        if let Some(server) = server {
            ensure!(
                server.starts_with("https://") || server.starts_with("http://"),
                "server URL must be http(s): {server}"
            );
        }
        let folder = self.exploration(slug)?;
        if let Some(asset) = asset {
            get_asset(&self.conn, asset)?;
        }
        let app_url = link(&self.context.library_id, &folder.id, asset);
        let query = app_url.split_once('?').map_or("", |(_, q)| q);
        Ok(json!({
            "url": server.map(|server| format!("{}/open?{query}", server.trim_end_matches('/'))),
            "app_url": app_url,
            "open": format!("open '{app_url}'"),
            "library_id": self.context.library_id,
            "folder_id": folder.id,
            "asset_id": asset,
        }))
    }

    /// Every exploration: slug, folder, brief and item count.
    pub fn explore_list(&self) -> Result<Value> {
        let mut out = Vec::new();
        for folder in self.collections()? {
            let Some(slug) = folder.source_id.as_deref().and_then(|s| s.strip_prefix("exploration:")) else {
                continue;
            };
            if slug.contains('/') {
                continue;
            }
            let (brief, _) = split_description(folder.description.as_deref());
            let count: usize = self
                .subtree(&folder.id)?
                .iter()
                .map(|f| self.member_count(&f.id))
                .sum::<Result<usize>>()?;
            out.push(json!({"slug":slug,"folder_id":folder.id,"name":folder.name,"brief":brief,"items":count,
                "link":link(&self.context.library_id, &folder.id, None)}));
        }
        Ok(json!({"explorations":out}))
    }
    fn member_count(&self, folder: &str) -> Result<usize> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM memberships m JOIN assets a ON a.id=m.asset_id WHERE m.collection_id=?1 AND a.deleted=0 AND a.trashed=0",
            [folder],
            |r| r.get::<_, i64>(0),
        )? as usize)
    }

    /// The exploration: brief, summary, folders (path, count) and items with their current
    /// owner-facing state.
    pub fn explore_show(&self, slug: &str) -> Result<Value> {
        let folder = self.exploration(slug)?;
        let (brief, summary) = split_description(folder.description.as_deref());
        let tree = self.subtree(&folder.id)?;
        let paths = folder_paths(&tree);
        let mut folders = Vec::new();
        // One entry per asset, in the first folder holding it; `folders` lists them all.
        let mut items: Vec<Value> = Vec::new();
        let mut index: BTreeMap<String, usize> = BTreeMap::new();
        for f in &tree {
            folders.push(json!({"id":f.id,"name":f.name,"path":paths[&f.id],"items":self.member_count(&f.id)?}));
            for asset in self.members(&f.id)? {
                match index.get(&asset.id) {
                    Some(&i) => items[i]["folders"].as_array_mut().unwrap().push(json!(paths[&f.id])),
                    None => {
                        index.insert(asset.id.clone(), items.len());
                        let mut item = item_json(&asset, &paths[&f.id]);
                        item["folders"] = json!([paths[&f.id]]);
                        items.push(item);
                    }
                }
            }
        }
        Ok(json!({
            "slug": slug,
            "library_id": self.context.library_id,
            "folder_id": folder.id,
            "name": folder.name,
            "brief": brief,
            "summary": summary,
            "folders": folders,
            "items": items,
            "link": link(&self.context.library_id, &folder.id, None),
        }))
    }
    /// Live and trashed members, in insertion order.
    fn members(&self, folder: &str) -> Result<Vec<Asset>> {
        documents(
            &self.conn,
            "SELECT a.document FROM memberships m JOIN assets a ON a.id=m.asset_id WHERE m.collection_id=?1 AND a.deleted=0 ORDER BY m.rowid",
            [folder],
        )
    }

    /// Registers a finding in exploration `slug` (sub-folder `finding.folder`). Idempotent: the
    /// same image URL, the same bytes, or (cards) the same kind + name + page URL (no URL: the
    /// same kind + name in this exploration) finds the existing asset, which is filed and gains
    /// the new tags; its name and note are left as they are. Cards with different names or
    /// kinds about one page stay separate. An existing asset that is in the Trash is left
    /// untouched (not filed, not re-tagged) and reported with status `trashed`. `helper`
    /// (vamp-media) renders the thumbnail/preview replicas show; without it the asset is
    /// cataloged and `warnings` says so.
    pub fn explore_add(
        &mut self,
        slug: &str,
        finding: Finding,
        helper: Option<&Path>,
    ) -> Result<Value> {
        let actor = agent_actor()?;
        ensure!(!finding.name.trim().is_empty(), "a finding needs a name");
        if let Some(url) = &finding.url {
            ensure!(
                url.starts_with("https://") || url.starts_with("http://"),
                "--url must be an http(s) page URL: {url}"
            );
        }
        let folder = match finding.folder.as_deref().filter(|f| !f.trim().is_empty()) {
            Some(path) => self.explore_folder(slug, path)?,
            None => self.exploration(slug)?,
        };
        let mut tags = finding.tags.clone();
        normalize_tags(&mut tags);
        // Identity on re-runs: the image URL, else the bytes; cards: kind + name + page URL.
        let (kind, source_id, hash, bytes, filename, image_url) = match finding.content {
            Content::Bytes { bytes, filename, image_url } => {
                let kind = if image_url.is_some() { "image" } else { "file" };
                let source_id = image_url.as_ref().map(|u| format!("web:{u}"));
                let hash = blake3::hash(&bytes).to_hex().to_string();
                (kind.to_owned(), source_id, Some(hash), bytes, filename, image_url)
            }
            Content::Card { kind } => {
                let kind = match kind.trim().to_lowercase() {
                    kind if kind.is_empty() => "note".to_owned(),
                    kind => kind,
                };
                let png = card::render(&card::Card {
                    kind: &kind,
                    title: finding.name.trim(),
                    note: finding.note.as_deref(),
                    url: finding.url.as_deref(),
                    tags: &tags,
                })?;
                let scope = finding.url.as_deref().unwrap_or(slug);
                let key = json!([kind, card_name(&finding.name), scope]);
                (kind, Some(format!("card:{key}")), None, png, "card.png".to_owned(), None)
            }
        };
        let mut existing: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT id, document FROM assets WHERE deleted=0 AND ((?1 IS NOT NULL AND source_id=?1) OR (?2 IS NOT NULL AND hash=?2)) ORDER BY trashed, rowid LIMIT 1",
                params![source_id, hash],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if existing.is_none() && hash.is_none() {
            existing = self.legacy_card(slug, &kind, &finding.name, finding.url.as_deref())?;
        }
        let mut warnings = Vec::new();
        let (status, asset) = match existing {
            Some((_, document)) => {
                let asset: Asset = parse(document)?;
                if asset.deleted_at.is_some() {
                    return Ok(json!({"status":"trashed","asset":item_json(&asset, ""),"folder_id":folder.id,
                        "folder":folder.name,"warnings":["the existing asset is in the Trash; it was left unchanged"]}));
                }
                let missing: Vec<String> = tags.iter().filter(|t| !asset.metadata.tags.contains(t)).cloned().collect();
                let asset = if !missing.is_empty() || (asset.metadata.source_url.is_none() && finding.url.is_some()) {
                    self.update(
                        &asset.id,
                        MetadataPatch {
                            add_tags: (!missing.is_empty()).then_some(missing),
                            source_url: asset.metadata.source_url.is_none().then(|| finding.url.clone()).flatten(),
                            ..MetadataPatch::default()
                        },
                        Some(asset.revision),
                    )?
                } else {
                    asset
                };
                ("existing", asset)
            }
            None => {
                let mut metadata = Metadata {
                    description: finding.note.clone().filter(|n| !n.trim().is_empty()),
                    tags: tags.clone(),
                    source_url: finding.url.clone(),
                    source_id,
                    ..Metadata::default()
                };
                metadata.extra.insert(
                    "exploration".into(),
                    json!({"slug":slug,"kind":kind,"note":finding.note,"actor":actor,"image_url":image_url}),
                );
                let mut asset = self.ingest_bytes(&bytes, &filename, metadata)?;
                if asset.name != finding.name.trim() {
                    asset = self.update(
                        &asset.id,
                        MetadataPatch {
                            name: Some(finding.name.trim().into()),
                            ..MetadataPatch::default()
                        },
                        Some(asset.revision),
                    )?;
                }
                match helper {
                    Some(helper) if asset.original.mime.starts_with("image/") => {
                        for kind in ["thumbnail", "preview"] {
                            if let Err(e) = self.generate(&asset.id, kind, helper) {
                                warnings.push(format!("{kind}: {e:#}"));
                                break;
                            }
                        }
                    }
                    Some(_) => warnings.push(format!("{} is not an image; the grid shows a placeholder", asset.original.mime)),
                    None => warnings.push("vamp-media helper unavailable: no thumbnail, replicas show a placeholder (run with the vamp beside vamp-media or set VAMP_MEDIA_HELPER)".into()),
                }
                ("created", asset)
            }
        };
        self.collection_membership(&folder.id, &asset.id, true)?;
        Ok(json!({
            "status": status,
            "asset": item_json(&asset, ""),
            "folder_id": folder.id,
            "folder": folder.name,
            "link": link(&self.context.library_id, &folder.id, Some(&asset.id)),
            "warnings": warnings,
        }))
    }

    /// A card from before cards were keyed on kind + name + URL (source id `note:<url>` or
    /// `note:<slug>:<name>`) that has this kind and name, so re-runs of older missions match.
    fn legacy_card(&self, slug: &str, kind: &str, name: &str, url: Option<&str>) -> Result<Option<(String, String)>> {
        let key = match url {
            Some(url) => format!("note:{url}"),
            None => format!("note:{slug}:{}", name.trim().to_lowercase()),
        };
        let mut statement = self
            .conn
            .prepare("SELECT id, document FROM assets WHERE deleted=0 AND source_id=?1 ORDER BY trashed, rowid")?;
        let rows = statement.query_map([key], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (id, document) = row?;
            let asset: Asset = parse(document.clone())?;
            let same_kind = asset.metadata.extra.get("exploration").and_then(|e| e["kind"].as_str()) == Some(kind);
            if same_kind && card_name(&asset.name) == card_name(name) {
                return Ok(Some((id, document)));
            }
        }
        Ok(None)
    }

    /// What changed in exploration `slug` since this actor's last read: every event after the
    /// cursor made by an actor other than [`agent_actor`] that touches an item of the
    /// exploration (asset updates, trash/restore/removal, views, folder membership) or one of
    /// its folders. Items carry their events and current state; nothing is interpreted.
    /// `since` overrides the stored cursor; `advance` stores the new one.
    pub fn explore_changes(&mut self, slug: &str, since: Option<u64>, advance: bool) -> Result<Value> {
        let actor = agent_actor()?;
        let folder = self.exploration(slug)?;
        let has_reads: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='exploration_reads')",
            [], |r| r.get(0),
        )?;
        let stored: Option<i64> = if has_reads {
            self.conn.query_row(
                "SELECT cursor FROM exploration_reads WHERE slug=?1 AND actor=?2",
                params![slug, actor], |r| r.get(0),
            ).optional()?
        } else { None };
        let after = since.unwrap_or(stored.unwrap_or(0) as u64);
        let head = self.last_event()?;
        let tree = self.subtree(&folder.id)?;
        let paths = folder_paths(&tree);
        let folders: HashSet<&str> = tree.iter().map(|f| f.id.as_str()).collect();
        // Items: current members, findings stamped with this slug, plus any asset whose
        // membership in these folders changed (it may have been moved out since).
        let mut assets: HashSet<String> = HashSet::new();
        for f in &tree {
            for asset in self.members(&f.id)? {
                assets.insert(asset.id);
            }
        }
        for id in self
            .conn
            .prepare("SELECT id FROM assets WHERE json_extract(document,'$.metadata.extra.exploration.slug')=?1")?
            .query_map([slug], |r| r.get::<_, String>(0))?
        {
            assets.insert(id?);
        }
        let mut touched: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        let mut folder_events = Vec::new();
        let mut cursor = after;
        while cursor < head {
            let page = self.events(cursor, 1000)?;
            let Some(last) = page.last() else { break };
            cursor = last.sequence;
            for event in page {
                if event.actor == actor {
                    continue;
                }
                let raw = || json!({"sequence":event.sequence,"kind":event.kind,"actor":event.actor,"at":event.created_at,"payload":event.payload});
                let collection = event.payload["collection_id"].as_str();
                match event.kind.as_str() {
                    "collection_updated" if collection.is_some_and(|c| folders.contains(c)) => {
                        match event.payload["asset_id"].as_str() {
                            Some(asset) => {
                                assets.insert(asset.to_owned());
                                touched.entry(asset.to_owned()).or_default().push(raw());
                            }
                            None => folder_events.push(raw()),
                        }
                    }
                    "collection_deleted" if collection.is_some_and(|c| folders.contains(c)) => folder_events.push(raw()),
                    "collection_created" if event.payload["parent_id"].as_str().is_some_and(|p| folders.contains(p)) => {
                        folder_events.push(raw())
                    }
                    "media_updated" | "media_trashed" | "media_restored" | "media_removed" | "media_viewed" => {
                        if let Some(asset) = event.asset_id.as_deref().filter(|a| assets.contains(*a)) {
                            touched.entry(asset.to_owned()).or_default().push(raw());
                        }
                    }
                    _ => {}
                }
            }
        }
        // Current folders (paths within the exploration) of each touched item.
        let mut placed: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for f in &tree {
            for asset in self.members(&f.id)? {
                if touched.contains_key(&asset.id) {
                    placed.entry(asset.id).or_default().push(paths[&f.id].clone());
                }
            }
        }
        let mut items = Vec::new();
        for (id, events) in touched {
            let Some((document, deleted)): Option<(String, bool)> = self
                .conn
                .query_row("SELECT document, deleted FROM assets WHERE id=?1", [&id], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?
            else {
                continue;
            };
            let asset: Asset = parse(document)?;
            let folders = placed.remove(&id).unwrap_or_default();
            let mut item = item_json(&asset, folders.first().map_or("", String::as_str));
            item["folders"] = json!(folders);
            item["deleted"] = json!(deleted);
            item["events"] = json!(events);
            items.push(item);
        }
        if advance {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS exploration_reads(slug TEXT NOT NULL, actor TEXT NOT NULL, cursor INTEGER NOT NULL, PRIMARY KEY(slug, actor))",
            )?;
            self.conn.execute(
                "INSERT INTO exploration_reads(slug,actor,cursor) VALUES(?1,?2,?3) ON CONFLICT(slug,actor) DO UPDATE SET cursor=excluded.cursor",
                params![slug, actor, head as i64],
            )?;
        }
        Ok(json!({
            "slug": slug,
            "folder_id": folder.id,
            "reader": actor,
            "since": after,
            "cursor": head,
            "advanced": advance,
            "items": items,
            "folder_events": folder_events,
        }))
    }
}

/// Folder id → path relative to the exploration (`""` for the exploration itself).
fn folder_paths(tree: &[Collection]) -> BTreeMap<String, String> {
    let mut paths: BTreeMap<String, String> = BTreeMap::new();
    for (i, f) in tree.iter().enumerate() {
        let path = match f.parent_id.as_ref().and_then(|p| paths.get(p)).filter(|_| i > 0) {
            Some(parent) if parent.is_empty() => f.name.clone(),
            Some(parent) => format!("{parent}/{}", f.name),
            None => String::new(),
        };
        paths.insert(f.id.clone(), path);
    }
    paths
}

fn item_json(asset: &Asset, folder: &str) -> Value {
    let exploration = asset.metadata.extra.get("exploration");
    json!({
        "id": asset.id,
        "revision": asset.revision,
        "name": asset.name,
        "folder": folder,
        "kind": exploration.and_then(|e| e["kind"].as_str()),
        "source_url": asset.metadata.source_url,
        "image_url": exploration.and_then(|e| e["image_url"].as_str()),
        "description": asset.metadata.description,
        // The note the finding was added with (the description may have been edited since).
        "note": exploration.and_then(|e| e["note"].as_str()),
        "tags": asset.metadata.tags,
        "rating": asset.metadata.rating,
        "trashed": asset.deleted_at.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn library() -> (tempfile::TempDir, Library) {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::init(&dir.path().join("home"), "test", None, None).unwrap();
        (dir, library)
    }
    fn png(seed: u8) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(8, 8, image::Rgb([seed, 0, 0]));
        let mut out = std::io::Cursor::new(Vec::new());
        image.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }
    fn finding(name: &str, content: Content) -> Finding {
        Finding {
            name: name.into(),
            note: Some("why it matters".into()),
            tags: vec!["seed".into()],
            url: Some("https://bakers.studio/".into()),
            folder: None,
            content,
        }
    }
    fn bytes(seed: u8, image_url: Option<&str>) -> Content {
        Content::Bytes {
            bytes: png(seed),
            filename: "shot.png".into(),
            image_url: image_url.map(str::to_owned),
        }
    }

    #[test]
    fn description_round_trip() {
        assert_eq!(split_description(Some(&join_description("brief", "sum"))), ("brief".into(), "sum".into()));
        assert_eq!(split_description(Some(&join_description("", "sum"))), ("".into(), "sum".into()));
        assert_eq!(split_description(Some("only brief")), ("only brief".into(), "".into()));
        assert!(validate_slug("vamp-sh").is_ok());
        assert!(validate_slug("Vamp").is_err() && validate_slug("-x").is_err() && validate_slug("").is_err());
    }

    // One test owns VAMP_ACTOR (process-wide) by switching per-thread actors instead.
    #[test]
    fn findings_are_idempotent_and_changes_report_other_actors() {
        let (_dir, mut library) = library();
        with_actor(Some("local".into()), || assert!(library.explore_add("x", finding("n", Content::Card { kind: "note".into() }), None).is_err()));
        with_actor(Some("agent:test".into()), || {
            let shown = library.explore_ensure("vamp-sh", Some("Vamp.sh branding"), Some("brief")).unwrap();
            assert_eq!(shown["name"], "Vamp.sh branding");
            // Ensure again: same folder, title kept, brief replaced, one root.
            let again = library.explore_ensure("vamp-sh", Some("Other"), Some("brief 2")).unwrap();
            assert_eq!(again["folder_id"], shown["folder_id"]);
            assert_eq!((again["name"].as_str(), again["brief"].as_str()), (Some("Vamp.sh branding"), Some("brief 2")));
            library.explore_summary("vamp-sh", "first findings").unwrap();
            let shown = library.explore_show("vamp-sh").unwrap();
            assert_eq!((shown["brief"].as_str(), shown["summary"].as_str()), (Some("brief 2"), Some("first findings")));
            assert_eq!(library.collections().unwrap().iter().filter(|c| c.name == ROOT_NAME).count(), 1);

            let image = library.explore_add("vamp-sh", finding("Hero", bytes(1, Some("https://cdn.x/a.png"))), None).unwrap();
            assert_eq!(image["status"], "created");
            let image_id = image["asset"]["id"].as_str().unwrap().to_owned();
            // Same image URL (even with other bytes) and same bytes (from a file) dedupe.
            let mut repeat = finding("Hero again", bytes(2, Some("https://cdn.x/a.png")));
            repeat.tags = vec!["extra".into()];
            let repeat = library.explore_add("vamp-sh", repeat, None).unwrap();
            assert_eq!((repeat["status"].as_str(), repeat["asset"]["id"].as_str()), (Some("existing"), Some(image_id.as_str())));
            assert_eq!(repeat["asset"]["name"], "Hero");
            assert_eq!(repeat["asset"]["tags"], json!(["extra", "seed"]));
            let mut file = finding("Screenshot", bytes(1, None));
            file.folder = Some("studios/bakers".into());
            let file = library.explore_add("vamp-sh", file, None).unwrap();
            assert_eq!(file["asset"]["id"], image_id.as_str());
            let mut card = finding("Bakers Studio", Content::Card { kind: "studio".into() });
            card.folder = Some("studios".into());
            let card = library.explore_add("vamp-sh", card, None).unwrap();
            assert_eq!(card["status"], "created");
            let card_id = card["asset"]["id"].as_str().unwrap().to_owned();
            let mut card_again = finding("Bakers Studio", Content::Card { kind: "studio".into() });
            card_again.note = Some("different words".into());
            card_again.folder = Some("studios".into());
            assert_eq!(library.explore_add("vamp-sh", card_again, None).unwrap()["asset"]["id"], card_id.as_str());

            let shown = library.explore_show("vamp-sh").unwrap();
            let paths: Vec<&str> = shown["folders"].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap()).collect();
            assert_eq!(paths, ["", "studios", "studios/bakers"]);
            // The screenshot had the hero's bytes: one asset, filed in two folders.
            let items = shown["items"].as_array().unwrap();
            assert_eq!(items.len(), 2);
            assert_eq!(items[0]["folders"], json!(["", "studios/bakers"]));
            let linked = library.explore_link("vamp-sh", Some(&card_id), Some("http://authority.example:7777/")).unwrap();
            let query = format!("library={}&folder={}&asset={card_id}", library.context.library_id, shown["folder_id"].as_str().unwrap());
            assert_eq!(linked["url"], format!("http://authority.example:7777/open?{query}"));
            assert_eq!(linked["app_url"], format!("vampire://open?{query}"));
            assert_eq!(linked["open"], format!("open 'vampire://open?{query}'"));
            assert!(library.explore_link("vamp-sh", None, Some("vampire://x")).is_err());
            assert_eq!(library.explore_link("vamp-sh", None, None).unwrap()["url"], Value::Null);
            assert_eq!(library.explore_changes("vamp-sh", None, false).unwrap()["advanced"], false);
            let read_tables: i64 = library.conn.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='exploration_reads'", [], |r| r.get(0),
            ).unwrap();
            assert_eq!(read_tables, 0, "peeking must not create persistent cursor state");
            // Only this agent has written so far; the read advances the cursor.
            assert_eq!(library.explore_changes("vamp-sh", None, true).unwrap()["items"], json!([]));
        });
        let card_id = library.explore_show("vamp-sh").unwrap()["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["name"] == "Bakers Studio")
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let image_id = library.list(Some("Hero"), 1, 0, None).unwrap()[0].id.clone();
        with_actor(Some("tailnet:owner/laptop".into()), || {
            library
                .update(&card_id, MetadataPatch { rating: Some(5), add_tags: Some(vec!["any-tag".into()]), description: Some("edited".into()), ..Default::default() }, None)
                .unwrap();
            library.remove(&image_id, None).unwrap();
        });
        with_actor(Some("agent:test".into()), || {
            let changes = library.explore_changes("vamp-sh", None, false).unwrap();
            assert_eq!(changes["reader"], "agent:test");
            let items = changes["items"].as_array().unwrap();
            assert_eq!(items.len(), 2, "{changes:#}");
            // Raw state and events only: no derived interpretation.
            let card = items.iter().find(|i| i["id"] == card_id.as_str()).unwrap();
            assert_eq!((card["rating"].as_u64(), card["description"].as_str(), card["note"].as_str()), (Some(5), Some("edited"), Some("why it matters")));
            assert_eq!(card["tags"], json!(["any-tag", "seed"]));
            assert_eq!(card["folders"], json!(["studios"]));
            let kinds: Vec<&str> = card["events"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
            assert_eq!(kinds, ["media_updated"]);
            assert_eq!(card["events"][0]["actor"], "tailnet:owner/laptop");
            assert!(card.get("signals").is_none() && changes.get("counts").is_none());
            let image = items.iter().find(|i| i["id"] == image_id.as_str()).unwrap();
            assert_eq!((image["trashed"].as_bool(), image["deleted"].as_bool()), (Some(true), Some(false)));
            assert_eq!(image["events"][0]["kind"], "media_trashed");
            // --peek did not advance: same answer; advancing empties the next read.
            assert_eq!(library.explore_changes("vamp-sh", None, true).unwrap()["items"].as_array().unwrap().len(), 2);
            assert_eq!(library.explore_changes("vamp-sh", None, true).unwrap()["items"], json!([]));
            // An explicit cursor replays.
            assert_eq!(library.explore_changes("vamp-sh", Some(0), false).unwrap()["items"].as_array().unwrap().len(), 2);
            // A finding whose asset is in the Trash is reported and left untouched.
            let mut again = finding("Hero", bytes(1, Some("https://cdn.x/a.png")));
            again.tags = vec!["new".into()];
            let again = library.explore_add("vamp-sh", again, None).unwrap();
            assert_eq!(again["status"], "trashed");
            assert_eq!(again["asset"]["tags"], json!(["extra", "seed"]));
        });
    }

    #[test]
    fn cards_are_keyed_on_kind_name_and_url() {
        let (_dir, mut library) = library();
        with_actor(Some("agent:test".into()), || {
            library.explore_ensure("cards", None, None).unwrap();

            // Cards from before this key (source id `note:<url>`) still match by kind + name.
            let mut metadata = Metadata { source_url: Some("https://old.example/".into()), source_id: Some("note:https://old.example/".into()), ..Metadata::default() };
            metadata.extra.insert("exploration".into(), json!({"slug":"cards","kind":"person"}));
            let legacy = library.ingest_bytes(&png(9), "card.png", metadata).unwrap();
            let legacy = library.update(&legacy.id, MetadataPatch { name: Some("Old Friend".into()), ..Default::default() }, None).unwrap();
            let mut add = |name: &str, kind: &str, url: Option<&str>| {
                let mut card = finding(name, Content::Card { kind: kind.into() });
                card.url = url.map(str::to_owned);
                let added = library.explore_add("cards", card, None).unwrap();
                (added["status"].as_str().unwrap().to_owned(), added["asset"]["id"].as_str().unwrap().to_owned())
            };
            let page = Some("https://openpurpose.example/team");
            let (status, kaan) = add("Kaan Eryılmaz", "person", page);
            assert_eq!(status, "created");
            // Exact re-runs (case and spacing aside) stay idempotent.
            assert_eq!(add(" KAAN  Eryılmaz ", "Person", page), ("existing".into(), kaan.clone()));
            // Other cards about the same page do not merge into it.
            let (status, studio) = add("OpenPurpose", "studio", page);
            assert_eq!(status, "created");
            let (status, colleague) = add("Jane Doe", "person", page);
            assert_eq!(status, "created");
            let (status, as_link) = add("Kaan Eryılmaz", "link", page);
            assert_eq!(status, "created");
            let (status, elsewhere) = add("Kaan Eryılmaz", "person", Some("https://kaan.example/"));
            assert_eq!(status, "created");
            let ids = [&kaan, &studio, &colleague, &as_link, &elsewhere];
            assert_eq!(ids.iter().collect::<std::collections::HashSet<_>>().len(), ids.len());
            // Without a URL: kind + name within the exploration.
            let (status, theme) = add("Warm palettes", "theme", None);
            assert_eq!(status, "created");
            assert_eq!(add("warm palettes", "theme", None), ("existing".into(), theme));
            assert_eq!(add("Old Friend", "person", Some("https://old.example/")), ("existing".into(), legacy.id.clone()));
            assert_eq!(add("New Friend", "person", Some("https://old.example/")).0, "created");
        });
    }
}
