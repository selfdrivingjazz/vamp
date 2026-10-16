//! Model-derived catalog data: suggested tags and image/text embeddings.
//!
//! Vectors live in their own table (never in the observation JSON search index) and are keyed
//! by the original digest they were computed from, so a refreshed original never matches stale
//! vectors. Ranking is brute-force cosine over L2-normalized f32 vectors; no ML code runs here.
use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

pub(crate) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS embeddings(asset_id TEXT NOT NULL REFERENCES assets(id), source_hash TEXT NOT NULL, model TEXT NOT NULL, dim INTEGER NOT NULL, vector BLOB NOT NULL, PRIMARY KEY(asset_id,source_hash,model));
CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY, document TEXT NOT NULL);";
const EMBEDDER_KEY: &str = "embedder";
const MAX_DIM: u64 = 8192;
/// Reciprocal-rank-fusion constant for hybrid search.
const RRF_K: f32 = 60.0;

/// Trusted local executable answering ToolRequest operation `embed_text` with an
/// `embedding` observation from the same model as the stored image embeddings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbedderSpec {
    pub command: Vec<String>,
    pub model: String,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    /// The command also accepts `--serve --socket PATH --idle-seconds N` and then answers
    /// one line-delimited ToolRequest per connection with one ToolResponse line.
    #[serde(default)]
    pub serve: bool,
}
fn default_timeout() -> u64 {
    120
}

/// Unix socket of a library's persistent embedder: `<state_dir>/embedder.sock`, or a
/// per-user temp path derived from it when that exceeds the platform's sun_path limit.
#[cfg(unix)]
fn socket_path(state_dir: &Path) -> PathBuf {
    let path = state_dir.join("embedder.sock");
    if path.as_os_str().len() < 100 {
        return path;
    }
    let digest = blake3::hash(state_dir.as_os_str().as_encoded_bytes()).to_hex();
    std::env::temp_dir().join(format!("vamp-embedder-{}.sock", &digest[..16]))
}

#[derive(Debug, Clone, Serialize)]
pub struct Scored {
    pub asset: Asset,
    pub score: f32,
}

/// Decode `{model, dim, vector_b64}` (f32 little-endian) into a unit vector.
pub(crate) fn decode(value: &Value) -> Result<(String, Vec<f32>)> {
    let model = value["model"]
        .as_str()
        .filter(|m| !m.trim().is_empty())
        .context("embedding model must be a non-empty string")?;
    let dim = value["dim"]
        .as_u64()
        .filter(|d| (1..=MAX_DIM).contains(d))
        .with_context(|| format!("embedding dim must be 1–{MAX_DIM}"))?;
    let bytes = STANDARD
        .decode(
            value["vector_b64"]
                .as_str()
                .context("embedding vector_b64 must be a string")?,
        )
        .context("embedding vector_b64 is not valid base64")?;
    ensure!(
        bytes.len() as u64 == dim * 4,
        "embedding has {} bytes; expected dim*4 = {}",
        bytes.len(),
        dim * 4
    );
    let mut vector = from_blob(&bytes);
    ensure!(
        vector.iter().all(|v| v.is_finite()),
        "embedding contains non-finite values"
    );
    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    ensure!(norm > 0.0, "embedding is a zero vector");
    for v in &mut vector {
        *v /= norm;
    }
    Ok((model.to_owned(), vector))
}
fn to_blob(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|v| v.to_le_bytes()).collect()
}
pub(crate) fn from_blob(bytes: &[u8]) -> Vec<f32> {
    let (chunks, _) = bytes.as_chunks::<4>();
    chunks.iter().map(|c| f32::from_le_bytes(*c)).collect()
}
fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Store an applied `embedding` observation's vector and strip it from the observation value.
pub(crate) fn store(
    tx: &Transaction,
    asset_id: &str,
    source_hash: &str,
    value: &mut Value,
) -> Result<()> {
    let (model, vector) = decode(value)?;
    tx.execute(
        "INSERT OR REPLACE INTO embeddings(asset_id,source_hash,model,dim,vector) VALUES(?1,?2,?3,?4,?5)",
        params![asset_id, source_hash, model, vector.len() as i64, to_blob(&vector)],
    )?;
    if let Some(object) = value.as_object_mut() {
        object.remove("vector_b64");
    }
    Ok(())
}

/// Lowercased, deduplicated tags from `tags` observations that the user does not already have.
pub(crate) fn suggested_tags(asset: &Asset, observations: &[Observation]) -> Vec<String> {
    let mut seen: Vec<String> = asset
        .metadata
        .tags
        .iter()
        .map(|t| t.to_lowercase())
        .collect();
    let mut suggested = Vec::new();
    for observation in observations.iter().filter(|o| o.kind == "tags") {
        for tag in observation.value["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let tag = tag.trim().to_lowercase();
            if !tag.is_empty() && !seen.contains(&tag) {
                seen.push(tag.clone());
                suggested.push(tag);
            }
        }
    }
    suggested
}

impl Library {
    pub fn embedder_set(&mut self, spec: EmbedderSpec) -> Result<()> {
        ensure!(
            !spec.command.is_empty() && !spec.command[0].is_empty(),
            "embedder command cannot be empty"
        );
        ensure!(
            !spec.model.trim().is_empty(),
            "embedder model cannot be empty"
        );
        ensure!(
            (1..=3600).contains(&spec.timeout_seconds),
            "embedder timeout must be 1–3600 seconds"
        );
        self.conn.execute(
            "INSERT INTO settings(key,document) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET document=excluded.document",
            params![EMBEDDER_KEY, json_text(&spec)?],
        )?;
        Ok(())
    }
    pub fn embedder(&self) -> Result<Option<EmbedderSpec>> {
        self.conn
            .query_row(
                "SELECT document FROM settings WHERE key=?1",
                [EMBEDDER_KEY],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .map(parse)
            .transpose()
    }
    pub fn embedder_remove(&mut self) -> Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM settings WHERE key=?1", [EMBEDDER_KEY])?
            > 0)
    }
    /// Persistent embedders are queried over their socket; when none is listening, one is
    /// started in the background and this query runs as a one-shot subprocess.
    pub fn embed_text(&self, text: &str) -> Result<(String, Vec<f32>)> {
        let spec = self
            .embedder()?
            .context("no text embedder configured; run vamp setup models --install")?;
        ensure!(!text.trim().is_empty(), "semantic query cannot be empty");
        ensure!(text.len() <= 4096, "semantic query exceeds 4096 bytes");
        let request = ToolRequest {
            operation: "embed_text".into(),
            text: Some(text.into()),
            library_id: Some(self.context.library_id.clone()),
            ..ToolRequest::default()
        };
        let response = self.embed_request(&spec, &request)?;
        let observation = response
            .observation
            .context("embedder returned no observation")?;
        ensure!(
            observation.kind == "embedding",
            "embedder returned {:?}, expected an embedding observation",
            observation.kind
        );
        let (model, vector) = decode(&observation.value["embedding"])?;
        ensure!(
            model == spec.model,
            "embedder returned model {model:?} but is configured for {:?}",
            spec.model
        );
        Ok((model, vector))
    }
    fn embed_request(&self, spec: &EmbedderSpec, request: &ToolRequest) -> Result<ToolResponse> {
        #[cfg(unix)]
        if spec.serve {
            let socket = socket_path(&self.state_dir);
            match tools::query_server(&socket, request, spec.timeout_seconds) {
                Ok(response) => return Ok(response),
                Err(_) => {
                    // A live server that failed this request keeps its socket; only start one
                    // when nothing is listening. Spawn failures surface via the one-shot below.
                    if std::os::unix::net::UnixStream::connect(&socket).is_err() {
                        let _ = tools::spawn_server(
                            &spec.command,
                            &socket,
                            &self.state_dir.join("embedder.log"),
                        );
                    }
                }
            }
        }
        run_tool(&spec.command, request, spec.timeout_seconds)
    }
    /// Current-revision embeddings of live assets for `model`, ranked by cosine similarity.
    /// `candidates` restricts ranking to those asset IDs.
    fn rank(
        &self,
        model: &str,
        query: &[f32],
        exclude: Option<&str>,
        candidates: Option<&HashSet<String>>,
    ) -> Result<Vec<(String, f32)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT e.asset_id,e.vector FROM embeddings e JOIN assets a ON a.id=e.asset_id AND a.hash=e.source_hash AND a.deleted=0 WHERE e.model=?1 AND e.dim=?2",
        )?;
        let mut ranked = Vec::new();
        let mut rows = stmt.query(params![model, query.len() as i64])?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            if exclude == Some(id.as_str()) || candidates.is_some_and(|c| !c.contains(&id)) {
                continue;
            }
            let blob = row.get_ref(1)?.as_blob()?;
            ranked.push((id, dot(query, &from_blob(blob))));
        }
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Ok(ranked)
    }
    /// Full semantic ranking of `candidates` (all live assets when `None`).
    pub(crate) fn semantic_ranked(
        &self,
        text: &str,
        candidates: Option<&HashSet<String>>,
    ) -> Result<Vec<(String, f32)>> {
        let (model, query) = self.embed_text(text)?;
        self.rank(&model, &query, None, candidates)
    }
    fn scored(&self, ranked: Vec<(String, f32)>, limit: usize) -> Result<Vec<Scored>> {
        ranked
            .into_iter()
            .take(limit)
            .map(|(id, score)| {
                Ok(Scored {
                    asset: get_asset(&self.conn, &id)?,
                    score,
                })
            })
            .collect()
    }
    /// Brute-force cosine search. `hybrid` fuses semantic and FTS ranks (reciprocal rank
    /// fusion). `added_by` restricts candidates like the browse filter.
    pub fn semantic_search(
        &self,
        text: &str,
        limit: usize,
        hybrid: bool,
        added_by: Option<&str>,
    ) -> Result<Vec<Scored>> {
        ensure!(
            (1..=1000).contains(&limit),
            "limit must be between 1 and 1000"
        );
        let candidates = added_by
            .map(|added_by| crate::catalog::added_by_ids(&self.conn, added_by))
            .transpose()?;
        let ranked = self.semantic_ranked(text, candidates.as_ref())?;
        if !hybrid {
            return self.scored(ranked, limit);
        }
        let mut fused: HashMap<String, f32> = HashMap::new();
        for (rank, (id, _)) in ranked.into_iter().enumerate() {
            *fused.entry(id).or_default() += 1.0 / (RRF_K + rank as f32 + 1.0);
        }
        for (rank, asset) in self.list(Some(text), 1000, 0, added_by)?.into_iter().enumerate() {
            *fused.entry(asset.id).or_default() += 1.0 / (RRF_K + rank as f32 + 1.0);
        }
        let mut fused: Vec<_> = fused.into_iter().collect();
        fused.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        self.scored(fused, limit)
    }
    /// Assets whose current-revision embedding is closest to this asset's current one.
    pub fn similar(&self, asset_id: &str, limit: usize) -> Result<Vec<Scored>> {
        ensure!(
            (1..=1000).contains(&limit),
            "limit must be between 1 and 1000"
        );
        let asset = get_asset(&self.conn, asset_id)?;
        let preferred = self.embedder()?.map(|spec| spec.model);
        let (model, blob): (String, Vec<u8>) = self
            .conn
            .query_row(
                "SELECT model,vector FROM embeddings WHERE asset_id=?1 AND source_hash=?2 ORDER BY model=?3 DESC,model LIMIT 1",
                params![asset_id, asset.original.source_key(), preferred],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .context("no embedding for the current original; run an embeddings processor")?;
        let ranked = self.rank(&model, &from_blob(&blob), Some(asset_id), None)?;
        self.scored(ranked, limit)
    }
    /// Copy suggested tags into user tags. Empty `tags` accepts every current suggestion.
    pub fn accept_tags(
        &mut self,
        asset_id: &str,
        tags: &[String],
        expected_revision: Option<u64>,
    ) -> Result<Asset> {
        let view = self.inspect(asset_id)?;
        let accepted = if tags.is_empty() {
            view.suggested_tags.clone()
        } else {
            tags.iter()
                .map(|tag| {
                    let tag = tag.trim().to_lowercase();
                    ensure!(
                        view.suggested_tags.contains(&tag),
                        "not a current suggested tag: {tag:?}"
                    );
                    Ok(tag)
                })
                .collect::<Result<Vec<_>>>()?
        };
        ensure!(!accepted.is_empty(), "no suggested tags to accept");
        let mut merged = view.asset.metadata.tags.clone();
        merged.extend(accepted);
        self.update(
            asset_id,
            MetadataPatch {
                tags: Some(merged),
                ..MetadataPatch::default()
            },
            expected_revision.or(Some(view.asset.revision)),
        )
    }
}

#[cfg(feature = "native")]
pub(crate) mod native {
    use super::*;
    use crate::native::{BrowseFilter, Request, browse, filter_conditions, media_item};

    fn item(library: &Library, asset: &Asset) -> Result<Value> {
        let reps = documents::<Representation>(
            &library.conn,
            "SELECT document FROM representations WHERE asset_id=?1 AND source_hash=?2",
            params![asset.id, asset.original.source_key()],
        )?;
        media_item(library, asset, &reps)
    }

    /// Semantic browse; falls back to FTS browse with `semantic:false` when no embedder is set.
    /// Every browse filter selects candidates before ranking; sort is ignored.
    pub(crate) fn semantic_browse(library: &Library, request: &Request) -> Result<Value> {
        let filter = BrowseFilter::from_request(request);
        let query = filter.query.unwrap_or("").trim();
        if query.is_empty() || library.embedder()?.is_none() {
            let mut value = browse(library, &filter)?;
            value["semantic"] = json!(false);
            return Ok(value);
        }
        ensure!(
            (1..=500).contains(&filter.limit),
            "invalid pagination; limit must be 1–500"
        );
        let (conditions, values) = filter_conditions(library, &filter)?;
        let candidates = {
            let mut stmt = library.conn.prepare(&format!(
                "SELECT a.id FROM assets a INDEXED BY assets_facets WHERE a.deleted=0{conditions}"
            ))?;
            Some(
                stmt.query_map(rusqlite::params_from_iter(&values), |r| {
                    r.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<HashSet<_>>>()?,
            )
        };
        let ranked = library.semantic_ranked(query, candidates.as_ref())?;
        let items = ranked
            .iter()
            .skip(filter.offset)
            .take(filter.limit)
            .map(|(id, score)| {
                let mut value = item(library, &get_asset(&library.conn, id)?)?;
                value["score"] = json!(score);
                Ok(value)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(
            json!({"has_more":filter.offset + items.len() < ranked.len(),"items":items,"total":ranked.len(),"offset":filter.offset,"semantic":true}),
        )
    }

    pub(crate) fn similar(library: &Library, request: &Request) -> Result<Value> {
        let results = library.similar(
            request
                .asset_id
                .as_deref()
                .context("asset_id is required")?,
            request.limit.unwrap_or(40),
        )?;
        let items = results
            .iter()
            .map(|s| {
                let mut value = item(library, &s.asset)?;
                value["score"] = json!(s.score);
                Ok(value)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(json!({ "items": items }))
    }

    pub(crate) fn accept_tags(library: &mut Library, request: &Request) -> Result<Value> {
        let asset = library.accept_tags(
            request
                .asset_id
                .as_deref()
                .context("asset_id is required")?,
            &request.tags,
            None,
        )?;
        Ok(serde_json::to_value(asset)?)
    }
}
