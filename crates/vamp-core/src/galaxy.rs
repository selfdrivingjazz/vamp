//! Galaxy: a 3-D projection of the library's image embeddings, so similar images sit
//! together as constellations.
//!
//! The authority computes the layout (UMAP-style: cosine kNN graph, fuzzy simplicial
//! weights, SGD with negative sampling, PCA initialisation) and stores it in `galaxy`.
//! New embeddings are placed incrementally at the weighted mean of their nearest laid-out
//! neighbours; once incremental placements exceed a fifth of the layout (or the embedding
//! model changes) the next request recomputes it in full. Replicas fetch the authority's
//! layout over `POST /v1/native` and keep the last answer for offline use.
use super::*;
use crate::embeddings::from_blob;
use std::collections::HashMap;
use std::time::Instant;

pub(crate) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS galaxy(asset_id TEXT PRIMARY KEY, model TEXT NOT NULL, x REAL NOT NULL, y REAL NOT NULL, z REAL NOT NULL, incremental INTEGER NOT NULL DEFAULT 0);";

const NEIGHBORS: usize = 15;
const EPOCHS: usize = 300;
const NEGATIVE: usize = 5;
/// UMAP curve parameters for min_dist 0.1, spread 1.0.
const A: f32 = 1.577;
const B: f32 = 0.895;
/// Incremental placements beyond this share of the layout trigger a full recompute.
const REBUILD_SHARE: f64 = 0.2;

/// Current-revision embeddings of live, untrashed assets for the preferred model.
struct Embedded {
    model: String,
    dim: usize,
    ids: Vec<String>,
    vectors: Vec<f32>,
}

impl Embedded {
    fn row(&self, i: usize) -> &[f32] {
        &self.vectors[i * self.dim..(i + 1) * self.dim]
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    // Four accumulators let the compiler vectorise without -ffast-math.
    let mut acc = [0f32; 4];
    let (ca, ra) = a.as_chunks::<4>();
    let (cb, rb) = b.as_chunks::<4>();
    for (x, y) in ca.iter().zip(cb) {
        for k in 0..4 {
            acc[k] += x[k] * y[k];
        }
    }
    let tail: f32 = ra.iter().zip(rb).map(|(x, y)| x * y).sum();
    acc[0] + acc[1] + acc[2] + acc[3] + tail
}

/// Deterministic xorshift so a layout is reproducible for the same embeddings.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
}

fn threads() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

/// `k` nearest neighbours (by cosine) of each row, as (index, similarity), best first.
fn knn(data: &Embedded, k: usize) -> Vec<Vec<(usize, f32)>> {
    let n = data.ids.len();
    let k = k.min(n.saturating_sub(1));
    let mut out = vec![Vec::new(); n];
    let chunk = n.div_ceil(threads()).max(1);
    std::thread::scope(|scope| {
        for (c, slot) in out.chunks_mut(chunk).enumerate() {
            scope.spawn(move || {
                let mut sims = Vec::with_capacity(n);
                for (offset, neighbours) in slot.iter_mut().enumerate() {
                    let i = c * chunk + offset;
                    let row = data.row(i);
                    sims.clear();
                    sims.extend(
                        (0..n)
                            .filter(|&j| j != i)
                            .map(|j| (j, dot(row, data.row(j)))),
                    );
                    if k < sims.len() {
                        sims.select_nth_unstable_by(k, |a, b| b.1.total_cmp(&a.1));
                        sims.truncate(k);
                    }
                    sims.sort_by(|a, b| b.1.total_cmp(&a.1));
                    neighbours.extend_from_slice(&sims);
                }
            });
        }
    });
    out
}

/// Fuzzy-union edge weights (UMAP's smooth kNN distances, symmetrised).
fn edges(neighbours: &[Vec<(usize, f32)>]) -> Vec<(usize, usize, f32)> {
    let target = (neighbours.first().map_or(1, Vec::len).max(2) as f32).log2();
    let mut directed: HashMap<(usize, usize), f32> = HashMap::new();
    for (i, list) in neighbours.iter().enumerate() {
        let dists: Vec<f32> = list.iter().map(|&(_, s)| (1.0 - s).max(0.0)).collect();
        let Some(&rho) = dists.first() else { continue };
        let (mut lo, mut hi, mut sigma) = (0f32, f32::INFINITY, 1f32);
        for _ in 0..64 {
            let sum: f32 = dists
                .iter()
                .map(|d| (-(d - rho).max(0.0) / sigma).exp())
                .sum();
            if (sum - target).abs() < 1e-5 {
                break;
            }
            if sum > target {
                hi = sigma;
                sigma = (lo + hi) / 2.0;
            } else {
                lo = sigma;
                sigma = if hi.is_infinite() {
                    sigma * 2.0
                } else {
                    (lo + hi) / 2.0
                };
            }
        }
        let sigma = sigma
            .max(1e-3 * dists.iter().sum::<f32>() / dists.len() as f32)
            .max(1e-6);
        for (&(j, _), d) in list.iter().zip(&dists) {
            directed.insert((i, j), (-(d - rho).max(0.0) / sigma).exp());
        }
    }
    let mut out = Vec::with_capacity(directed.len());
    for (&(i, j), &w) in &directed {
        let back = directed.get(&(j, i)).copied().unwrap_or(0.0);
        if back > 0.0 && j < i {
            continue; // counted from the (j, i) side
        }
        out.push((i, j, w + back - w * back));
    }
    out.sort_by_key(|e| (e.0, e.1));
    out
}

/// Top-3 principal components (power iteration with deflation), scaled to ±10.
fn pca3(data: &Embedded) -> Vec<[f32; 3]> {
    let (n, d) = (data.ids.len(), data.dim);
    let mut mean = vec![0f32; d];
    for i in 0..n {
        for (m, v) in mean.iter_mut().zip(data.row(i)) {
            *m += v / n as f32;
        }
    }
    let centered: Vec<f32> = (0..n)
        .flat_map(|i| data.row(i).iter().zip(&mean).map(|(v, m)| v - m))
        .collect();
    let row = |i: usize| &centered[i * d..(i + 1) * d];
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut components: Vec<Vec<f32>> = Vec::new();
    for _ in 0..3 {
        let mut v: Vec<f32> = (0..d).map(|_| rng.unit() - 0.5).collect();
        for _ in 0..60 {
            let mut next = vec![0f32; d];
            for i in 0..n {
                let p = dot(row(i), &v);
                for (x, r) in next.iter_mut().zip(row(i)) {
                    *x += p * r;
                }
            }
            for c in &components {
                let p = dot(&next, c);
                for (x, cv) in next.iter_mut().zip(c) {
                    *x -= p * cv;
                }
            }
            let norm = dot(&next, &next).sqrt().max(1e-12);
            v = next.into_iter().map(|x| x / norm).collect();
        }
        components.push(v);
    }
    let mut out: Vec<[f32; 3]> = (0..n)
        .map(|i| std::array::from_fn(|c| dot(row(i), &components[c])))
        .collect();
    let extent = out
        .iter()
        .flat_map(|p| p.iter().map(|v| v.abs()))
        .fold(1e-6f32, f32::max);
    for p in &mut out {
        for v in p.iter_mut() {
            *v *= 10.0 / extent;
        }
    }
    out
}

/// UMAP-style optimisation of a 3-D embedding.
fn project(data: &Embedded) -> Vec<[f32; 3]> {
    let n = data.ids.len();
    if n < 3 {
        return (0..n).map(|i| [i as f32, 0.0, 0.0]).collect();
    }
    let graph = edges(&knn(data, NEIGHBORS));
    let mut y = pca3(data);
    let max_w = graph.iter().map(|e| e.2).fold(0f32, f32::max).max(1e-6);
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let clip = |v: f32| v.clamp(-4.0, 4.0);
    for epoch in 0..EPOCHS {
        let alpha = 1.0 - epoch as f32 / EPOCHS as f32;
        for &(i, j, w) in &graph {
            if rng.unit() > w / max_w {
                continue;
            }
            let (pi, pj) = (y[i], y[j]);
            let diff: [f32; 3] = std::array::from_fn(|c| pi[c] - pj[c]);
            let d2 = diff.iter().map(|v| v * v).sum::<f32>();
            if d2 > 0.0 {
                let g = -2.0 * A * B * d2.powf(B - 1.0) / (A * d2.powf(B) + 1.0);
                for c in 0..3 {
                    let step = clip(g * diff[c]) * alpha;
                    y[i][c] += step;
                    y[j][c] -= step;
                }
            }
            for _ in 0..NEGATIVE {
                let k = rng.below(n);
                if k == i {
                    continue;
                }
                let pi = y[i];
                let pk = y[k];
                let diff: [f32; 3] = std::array::from_fn(|c| pi[c] - pk[c]);
                let d2 = diff.iter().map(|v| v * v).sum::<f32>();
                let g = 2.0 * B / ((0.001 + d2) * (A * d2.powf(B) + 1.0));
                for c in 0..3 {
                    y[i][c] += if g > 0.0 { clip(g * diff[c]) } else { 4.0 } * alpha;
                }
            }
        }
    }
    normalize(&mut y);
    y
}

/// Centre on the origin and scale to unit RMS radius.
fn normalize(points: &mut [[f32; 3]]) {
    let n = points.len().max(1) as f32;
    let centre: [f32; 3] = std::array::from_fn(|c| points.iter().map(|p| p[c]).sum::<f32>() / n);
    let rms = (points
        .iter()
        .map(|p| (0..3).map(|c| (p[c] - centre[c]).powi(2)).sum::<f32>())
        .sum::<f32>()
        / n)
        .sqrt()
        .max(1e-6);
    for p in points {
        for c in 0..3 {
            p[c] = (p[c] - centre[c]) / rms;
        }
    }
}

/// Weighted mean of the nearest laid-out neighbours, with a little deterministic jitter.
fn place(vector: &[f32], data: &Embedded, placed: &[(usize, [f32; 3])], seed: u64) -> [f32; 3] {
    let mut sims: Vec<(f32, [f32; 3])> = placed
        .iter()
        .map(|&(i, p)| (dot(vector, data.row(i)), p))
        .collect();
    let k = 8.min(sims.len());
    if k == 0 {
        return [0.0; 3];
    }
    if k < sims.len() {
        sims.select_nth_unstable_by(k, |a, b| b.0.total_cmp(&a.0));
    }
    let top = &sims[..k];
    let best = top.iter().map(|s| s.0).fold(f32::MIN, f32::max);
    let mut sum = [0f32; 3];
    let mut total = 0f32;
    for (s, p) in top {
        let w = ((s - best) * 20.0).exp();
        total += w;
        for c in 0..3 {
            sum[c] += w * p[c];
        }
    }
    let mut rng = Rng(seed | 1);
    std::array::from_fn(|c| sum[c] / total + (rng.unit() - 0.5) * 0.04)
}

fn seed_of(id: &str) -> u64 {
    let hash = blake3::hash(id.as_bytes());
    u64::from_le_bytes(hash.as_bytes()[..8].try_into().unwrap_or_default())
}

impl Library {
    fn galaxy_embedded(&self) -> Result<Option<Embedded>> {
        let preferred = self.embedder()?.map(|spec| spec.model);
        let Some((model, dim)) = self
            .conn
            .query_row(
                "SELECT e.model,e.dim FROM embeddings e JOIN assets a ON a.id=e.asset_id AND a.hash=e.source_hash AND a.deleted=0 GROUP BY e.model,e.dim ORDER BY e.model=?1 DESC,count(*) DESC LIMIT 1",
                [preferred],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?
        else {
            return Ok(None);
        };
        let dim = dim as usize;
        let mut stmt = self.conn.prepare(
            "SELECT e.asset_id,e.vector FROM embeddings e JOIN assets a ON a.id=e.asset_id AND a.hash=e.source_hash AND a.deleted=0 AND a.trashed=0 WHERE e.model=?1 AND e.dim=?2 ORDER BY e.asset_id",
        )?;
        let mut rows = stmt.query(params![model, dim as i64])?;
        let (mut ids, mut vectors) = (Vec::new(), Vec::new());
        while let Some(row) = rows.next()? {
            ids.push(row.get::<_, String>(0)?);
            vectors.extend(from_blob(row.get_ref(1)?.as_blob()?));
        }
        Ok(Some(Embedded {
            model,
            dim,
            ids,
            vectors,
        }))
    }

    /// Bring the stored layout up to date: full projection when there is none, the model
    /// changed, `rebuild` is set, or incremental placements outgrew [`REBUILD_SHARE`];
    /// otherwise place only new embeddings. Returns `{model, count, full, added, removed, ms}`.
    pub fn galaxy_refresh(&mut self, rebuild: bool) -> Result<Value> {
        let started = Instant::now();
        let Some(data) = self.galaxy_embedded()? else {
            self.conn.execute("DELETE FROM galaxy", [])?;
            return Ok(json!({"model":null,"count":0,"full":false,"added":0,"removed":0,"ms":0}));
        };
        let mut stored: HashMap<String, ([f32; 3], bool)> = HashMap::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT asset_id,x,y,z,incremental FROM galaxy WHERE model=?1")?;
            let mut rows = stmt.query([&data.model])?;
            while let Some(r) = rows.next()? {
                stored.insert(
                    r.get(0)?,
                    (
                        [
                            r.get::<_, f64>(1)? as f32,
                            r.get::<_, f64>(2)? as f32,
                            r.get::<_, f64>(3)? as f32,
                        ],
                        r.get::<_, bool>(4)?,
                    ),
                );
            }
        }
        let index: HashMap<&str, usize> = data
            .ids
            .iter()
            .enumerate()
            .map(|(i, id)| (id.as_str(), i))
            .collect();
        let removed = stored
            .keys()
            .filter(|id| !index.contains_key(id.as_str()))
            .count();
        let missing: Vec<usize> = (0..data.ids.len())
            .filter(|&i| !stored.contains_key(&data.ids[i]))
            .collect();
        let incremental = stored.values().filter(|v| v.1).count() + missing.len();
        let full = rebuild
            || stored.len() - removed < 3
            || incremental as f64 > REBUILD_SHARE * data.ids.len() as f64;
        let tx = self.conn.transaction()?;
        if full {
            let points = project(&data);
            tx.execute("DELETE FROM galaxy", [])?;
            let mut insert = tx.prepare(
                "INSERT INTO galaxy(asset_id,model,x,y,z,incremental) VALUES(?1,?2,?3,?4,?5,0)",
            )?;
            for (id, p) in data.ids.iter().zip(&points) {
                insert.execute(params![id, data.model, p[0], p[1], p[2]])?;
            }
        } else if !missing.is_empty() || removed > 0 {
            tx.execute(
                "DELETE FROM galaxy WHERE model<>?1 OR asset_id NOT IN (SELECT e.asset_id FROM embeddings e JOIN assets a ON a.id=e.asset_id AND a.hash=e.source_hash AND a.deleted=0 AND a.trashed=0 WHERE e.model=?1)",
                [&data.model],
            )?;
            let placed: Vec<(usize, [f32; 3])> = stored
                .iter()
                .filter_map(|(id, (p, _))| index.get(id.as_str()).map(|&i| (i, *p)))
                .collect();
            let mut insert = tx.prepare(
                "INSERT OR REPLACE INTO galaxy(asset_id,model,x,y,z,incremental) VALUES(?1,?2,?3,?4,?5,1)",
            )?;
            for &i in &missing {
                let p = place(data.row(i), &data, &placed, seed_of(&data.ids[i]));
                insert.execute(params![data.ids[i], data.model, p[0], p[1], p[2]])?;
            }
        }
        tx.commit()?;
        Ok(json!({
            "model": data.model,
            "count": data.ids.len(),
            "full": full,
            "added": if full { data.ids.len() } else { missing.len() },
            "removed": removed,
            "ms": started.elapsed().as_millis() as u64,
        }))
    }

    /// The layout plus what the client needs to draw and lexically search it:
    /// `{model, ids, xyz (base64 f32 LE, 3 per id), names, tags, captions, tints (0xRRGGBB)}`.
    pub fn galaxy(&mut self, rebuild: bool) -> Result<Value> {
        let refresh = self.galaxy_refresh(rebuild)?;
        let mut stmt = self.conn.prepare(
            "SELECT g.asset_id,g.x,g.y,g.z,a.document,
               (SELECT json_extract(o.document,'$.value.text') FROM observations o WHERE o.asset_id=a.id AND o.source_hash=a.hash AND json_extract(o.document,'$.kind')='description' LIMIT 1),
               (SELECT json_extract(o.document,'$.value') FROM observations o WHERE o.asset_id=a.id AND o.source_hash=a.hash AND json_extract(o.document,'$.kind')='tags' LIMIT 1)
             FROM galaxy g JOIN assets a ON a.id=g.asset_id ORDER BY g.asset_id",
        )?;
        let mut rows = stmt.query([])?;
        let (mut ids, mut xyz, mut names, mut tags, mut captions, mut tints) =
            (vec![], vec![], vec![], vec![], vec![], vec![]);
        while let Some(r) = rows.next()? {
            ids.push(r.get::<_, String>(0)?);
            for c in 1..=3 {
                xyz.extend((r.get::<_, f64>(c)? as f32).to_le_bytes());
            }
            let asset: Asset = parse(r.get::<_, String>(4)?)?;
            let suggested: Value = r
                .get::<_, Option<String>>(6)?
                .map(|s| serde_json::from_str(&s))
                .transpose()?
                .unwrap_or(Value::Null);
            let mut all = asset.metadata.tags.clone();
            if let Some(list) = suggested["tags"].as_array() {
                all.extend(list.iter().filter_map(Value::as_str).map(str::to_owned));
            }
            all.dedup();
            tags.push(all.join(", "));
            let caption = r
                .get::<_, Option<String>>(5)?
                .or_else(|| suggested["caption"].as_str().map(str::to_owned))
                .unwrap_or_default();
            captions.push(caption);
            names.push(asset.name.clone());
            tints.push(tint(&asset));
        }
        let xyz = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(&xyz)
        };
        Ok(json!({
            "model": refresh["model"],
            "refresh": refresh,
            "ids": ids,
            "xyz": xyz,
            "names": names,
            "tags": tags,
            "captions": captions,
            "tints": tints,
        }))
    }

    /// Semantic ranking for the galaxy search bar: `{semantic, ids, scores, embed_ms, ms}`.
    pub fn galaxy_search(&self, query: &str, limit: usize) -> Result<Value> {
        let started = Instant::now();
        let (model, vector) = self.embed_text(query)?;
        let embed_ms = started.elapsed().as_secs_f64() * 1000.0;
        let ranked = self.galaxy_rank(&model, &vector, limit)?;
        Ok(json!({
            "semantic": true,
            "ids": ranked.iter().map(|r| &r.0).collect::<Vec<_>>(),
            "scores": ranked.iter().map(|r| r.1).collect::<Vec<_>>(),
            "embed_ms": embed_ms,
            "ms": started.elapsed().as_secs_f64() * 1000.0,
        }))
    }

    /// Top `limit` by cosine over an in-process copy of the embedding matrix, reloaded only
    /// when the embeddings table or asset liveness changes.
    fn galaxy_rank(&self, model: &str, query: &[f32], limit: usize) -> Result<Vec<(String, f32)>> {
        let stamp: (i64, i64, i64) = self.conn.query_row(
            "SELECT (SELECT count(*) FROM embeddings),(SELECT coalesce(max(rowid),0) FROM embeddings),(SELECT coalesce(sum(deleted+trashed),0) FROM assets)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let mut cache = MATRICES.lock().unwrap_or_else(|e| e.into_inner());
        let key = self.state_dir.clone();
        let fresh = cache.get(&key).is_some_and(|m| {
            m.stamp == stamp && m.data.model == model && m.data.dim == query.len()
        });
        if !fresh {
            let data = self
                .galaxy_embedded()?
                .filter(|d| d.model == model && d.dim == query.len())
                .context("no image embeddings for the embedder's model")?;
            cache.insert(key.clone(), Matrix { stamp, data });
        }
        let data = &cache[&key].data;
        let mut scored: Vec<(usize, f32)> = (0..data.ids.len())
            .map(|i| (i, dot(query, data.row(i))))
            .collect();
        let limit = limit.min(scored.len());
        if limit < scored.len() {
            scored.select_nth_unstable_by(limit, |a, b| b.1.total_cmp(&a.1));
            scored.truncate(limit);
        }
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        Ok(scored
            .into_iter()
            .map(|(i, s)| (data.ids[i].clone(), s))
            .collect())
    }
}

struct Matrix {
    stamp: (i64, i64, i64),
    data: Embedded,
}
static MATRICES: std::sync::LazyLock<std::sync::Mutex<HashMap<PathBuf, Matrix>>> =
    std::sync::LazyLock::new(Default::default);

/// The most vivid prominent palette colour as 0xRRGGBB (soft white when there is none).
fn tint(asset: &Asset) -> u32 {
    let mut best = (f64::MIN, 0xd8d8e8u32);
    for color in &asset.metadata.colors {
        let Some(rgb) = color
            .hex
            .strip_prefix('#')
            .and_then(|h| u32::from_str_radix(h, 16).ok())
        else {
            continue;
        };
        let [r, g, b] = [(rgb >> 16) & 255, (rgb >> 8) & 255, rgb & 255].map(|v| v as f64 / 255.0);
        let max = r.max(g).max(b);
        let chroma = max - r.min(g).min(b);
        let score = color.ratio.sqrt() * (0.25 + chroma) * (0.3 + max);
        if score > best.0 {
            best = (score, rgb);
        }
    }
    best.1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster_data() -> Embedded {
        // Three tight clusters on orthogonal axes in 16-D.
        let dim = 16;
        let mut rng = Rng(7);
        let mut ids = Vec::new();
        let mut vectors = Vec::new();
        for i in 0..90 {
            let axis = i % 3;
            let mut v: Vec<f32> = (0..dim).map(|_| (rng.unit() - 0.5) * 0.2).collect();
            v[axis] += 1.0;
            let norm = dot(&v, &v).sqrt();
            vectors.extend(v.iter().map(|x| x / norm));
            ids.push(format!("{axis}-{i:03}"));
        }
        Embedded {
            model: "m".into(),
            dim,
            ids,
            vectors,
        }
    }

    #[test]
    fn projection_keeps_clusters_apart() {
        let data = cluster_data();
        let points = project(&data);
        let dist =
            |a: [f32; 3], b: [f32; 3]| (0..3).map(|c| (a[c] - b[c]).powi(2)).sum::<f32>().sqrt();
        let (mut within, mut across, mut nw, mut na) = (0f32, 0f32, 0, 0);
        for i in 0..points.len() {
            for j in i + 1..points.len() {
                if i % 3 == j % 3 {
                    within += dist(points[i], points[j]);
                    nw += 1;
                } else {
                    across += dist(points[i], points[j]);
                    na += 1;
                }
            }
        }
        let (within, across) = (within / nw as f32, across / na as f32);
        assert!(across > 3.0 * within, "within {within} across {across}");
        assert!(points.iter().all(|p| p.iter().all(|v| v.is_finite())));
    }

    #[test]
    fn placement_lands_near_its_cluster() {
        let data = cluster_data();
        let points = project(&data);
        let placed: Vec<(usize, [f32; 3])> = points.iter().copied().enumerate().skip(1).collect();
        let p = place(data.row(0), &data, &placed, 1);
        let nearest = placed
            .iter()
            .min_by(|a, b| {
                let d = |q: [f32; 3]| (0..3).map(|c| (q[c] - p[c]).powi(2)).sum::<f32>();
                d(a.1).total_cmp(&d(b.1))
            })
            .unwrap();
        assert_eq!(nearest.0 % 3, 0);
    }
}
