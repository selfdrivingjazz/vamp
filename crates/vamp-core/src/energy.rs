//! Compact similarity pack for in-memory "Same Energy" browsing: every live asset's current
//! image embedding quantized to int8 (per-row scale) plus a 64-bin CIELAB palette vector.
//! The authority builds it from the catalog and caches it under `energy/pack.bin`, rebuilding
//! when the catalog fingerprint changes; replicas mirror the file (`GET /v1/embeddings/pack`)
//! on sync. Clients load it once and rank by brute force, so a click never touches SQLite,
//! the server lock, or the network.
//!
//! Layout (little endian): `b"VEPK0001"`, `u32` header length, header JSON
//! (`PackHeader`), zero padding to 16 bytes, `f32 scales[count]`, `i8 vibe[count*dim]`,
//! `u8 palette[count*palette_dim]` (value/255 of an L2-normalized non-negative vector).
//! A vibe vector is `scale * q`.
use super::*;
use std::io::Read;

pub const MAGIC: &[u8; 8] = b"VEPK0001";
pub const PALETTE_DIM: usize = 64;
const PALETTE_SIGMA: f64 = 25.0;

#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq)]
pub struct PackHeader {
    pub format: u32,
    /// Fingerprint of (asset, source hash, model, palette) rows; changes with any of them.
    pub version: String,
    pub model: String,
    pub dim: usize,
    pub palette_dim: usize,
    pub count: usize,
    pub quant: String,
    pub ids: Vec<String>,
}

pub fn pack_path(state_dir: &Path) -> PathBuf {
    state_dir.join("energy").join("pack.bin")
}

/// 64-bin soft CIELAB histogram (4 L × 4 a × 4 b Gaussian bins weighted by swatch ratio),
/// L2-normalized; all zeros without swatches.
pub fn palette_vector(swatches: &[ColorSwatch]) -> [f32; PALETTE_DIM] {
    const L: [f64; 4] = [12.5, 37.5, 62.5, 87.5];
    const AB: [f64; 4] = [-60.0, -20.0, 20.0, 60.0];
    let mut out = [0f64; PALETTE_DIM];
    for swatch in swatches {
        let Some(lab) = hex_to_lab(&swatch.hex) else {
            continue;
        };
        let ratio = swatch.ratio.clamp(0.0, 1.0);
        for (i, l) in L.iter().enumerate() {
            for (j, a) in AB.iter().enumerate() {
                for (k, b) in AB.iter().enumerate() {
                    let d2 = (lab[0] - l).powi(2) + (lab[1] - a).powi(2) + (lab[2] - b).powi(2);
                    out[i * 16 + j * 4 + k] +=
                        ratio * (-d2 / (2.0 * PALETTE_SIGMA * PALETTE_SIGMA)).exp();
                }
            }
        }
    }
    let norm = out.iter().map(|v| v * v).sum::<f64>().sqrt();
    let mut vector = [0f32; PALETTE_DIM];
    if norm > 0.0 {
        for (o, v) in vector.iter_mut().zip(out) {
            *o = (v / norm) as f32;
        }
    }
    vector
}

/// Symmetric per-row int8 quantization: `v ≈ scale * q`.
pub fn quantize(vector: &[f32], out: &mut Vec<u8>) -> f32 {
    let max = vector.iter().fold(0f32, |m, v| m.max(v.abs()));
    let scale = if max > 0.0 { max / 127.0 } else { 1.0 };
    out.extend(
        vector
            .iter()
            .map(|v| ((v / scale).round().clamp(-127.0, 127.0) as i8) as u8),
    );
    scale
}

impl Library {
    /// Embedding model the pack uses: the configured embedder's, else the most common one.
    fn pack_model(&self) -> Result<Option<String>> {
        if let Some(spec) = self.embedder()? {
            return Ok(Some(spec.model));
        }
        Ok(self
            .conn
            .query_row(
                "SELECT model FROM embeddings GROUP BY model ORDER BY count(*) DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?)
    }

    fn pack_rows_sql(with_vector: bool) -> String {
        format!(
            "SELECT a.id,a.hash,a.colors{} FROM embeddings e JOIN assets a ON a.id=e.asset_id AND a.hash=e.source_hash AND a.deleted=0 AND a.trashed=0 WHERE e.model=?1 ORDER BY a.id",
            if with_vector { ",e.vector" } else { "" }
        )
    }

    /// Catalog fingerprint the pack is keyed by (cheap: no vectors read).
    fn pack_fingerprint(&self, model: &str) -> Result<String> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(model.as_bytes());
        let mut stmt = self.conn.prepare(&Self::pack_rows_sql(false))?;
        let mut rows = stmt.query([model])?;
        while let Some(row) = rows.next()? {
            for i in 0..3 {
                if let Some(text) = row.get_ref(i)?.as_str_or_null()? {
                    hasher.update(text.as_bytes());
                }
                hasher.update(&[0]);
            }
        }
        Ok(hasher.finalize().to_hex()[..16].to_owned())
    }

    /// Authority: the current pack (rebuilt when stale). `None` without any embeddings.
    pub fn energy_pack(&self) -> Result<Option<(PathBuf, PackHeader)>> {
        let Some(model) = self.pack_model()? else {
            return Ok(None);
        };
        let version = self.pack_fingerprint(&model)?;
        let path = pack_path(&self.state_dir);
        if let Ok(header) = read_header(&path)
            && header.version == version
        {
            return Ok(Some((path, header)));
        }
        let bytes = self.build_pack(&model, version)?;
        fs::create_dir_all(path.parent().context("pack directory")?)?;
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        fs::write(&tmp, &bytes)?;
        fs::rename(&tmp, &path)?;
        Ok(Some((path.clone(), read_header(&path)?)))
    }

    fn build_pack(&self, model: &str, version: String) -> Result<Vec<u8>> {
        let mut ids = Vec::new();
        let mut scales = Vec::new();
        let mut vibe = Vec::new();
        let mut palette = Vec::new();
        let mut dim = 0usize;
        let mut stmt = self.conn.prepare(&Self::pack_rows_sql(true))?;
        let mut rows = stmt.query([model])?;
        while let Some(row) = rows.next()? {
            let blob = row.get_ref(3)?.as_blob()?;
            let (chunks, _) = blob.as_chunks::<4>();
            let vector: Vec<f32> = chunks.iter().map(|c| f32::from_le_bytes(*c)).collect();
            if dim == 0 {
                dim = vector.len();
            }
            if vector.len() != dim || dim == 0 {
                continue;
            }
            ids.push(row.get::<_, String>(0)?);
            scales.push(quantize(&vector, &mut vibe));
            let swatches: Vec<ColorSwatch> = row
                .get::<_, Option<String>>(2)?
                .and_then(|c| serde_json::from_str(&c).ok())
                .unwrap_or_default();
            palette.extend(
                palette_vector(&swatches)
                    .iter()
                    .map(|v| (v * 255.0).round() as u8),
            );
        }
        let header = PackHeader {
            format: 1,
            version,
            model: model.to_owned(),
            dim,
            palette_dim: PALETTE_DIM,
            count: ids.len(),
            quant: "int8".into(),
            ids,
        };
        Ok(encode(&header, &scales, &vibe, &palette))
    }
}

pub fn encode(header: &PackHeader, scales: &[f32], vibe: &[u8], palette: &[u8]) -> Vec<u8> {
    let json = serde_json::to_vec(header).expect("header serializes");
    let mut out = Vec::with_capacity(32 + json.len() + scales.len() * 4 + vibe.len() + palette.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(&json);
    out.resize(out.len().next_multiple_of(16), 0);
    for s in scales {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out.extend_from_slice(vibe);
    out.extend_from_slice(palette);
    out
}

/// Parses and validates a pack's header (and its total length) without reading vectors.
pub fn read_header(path: &Path) -> Result<PackHeader> {
    let mut file = fs::File::open(path)?;
    let mut prefix = [0u8; 12];
    file.read_exact(&mut prefix)?;
    ensure!(&prefix[..8] == MAGIC, "not a vamp embeddings pack");
    let len = u32::from_le_bytes(prefix[8..12].try_into()?) as usize;
    ensure!(len < 1 << 30, "pack header too large");
    let mut json = vec![0u8; len];
    file.read_exact(&mut json)?;
    let header: PackHeader = serde_json::from_slice(&json)?;
    ensure!(header.ids.len() == header.count, "pack id count mismatch");
    let body = (12 + len).next_multiple_of(16)
        + header.count * (4 + header.dim + header.palette_dim);
    ensure!(
        file.metadata()?.len() as usize == body,
        "truncated embeddings pack"
    );
    Ok(header)
}

/// `{path, version, model, dim, palette_dim, count}` for clients; `ids` stay in the file.
pub fn describe(path: &Path, header: &PackHeader) -> Value {
    json!({
        "path": path,
        "version": header.version,
        "model": header.model,
        "dim": header.dim,
        "palette_dim": header.palette_dim,
        "count": header.count,
        "quant": header.quant,
        "bytes": fs::metadata(path).map(|m| m.len()).unwrap_or(0),
    })
}

#[cfg(feature = "remote")]
pub mod replica {
    use super::*;

    /// Mirrors the authority's pack when its version differs from the local copy.
    /// Returns whether a new pack was downloaded.
    pub fn refresh(library: &Library) -> Result<bool> {
        refresh_with(&library.remote_client()?, &library.state_dir)
    }

    fn refresh_with(client: &crate::remote::Client, state_dir: &Path) -> Result<bool> {
        let meta: Value = client.get_json("/embeddings/pack/meta", &[])?;
        let Some(version) = meta["version"].as_str() else {
            return Ok(false); // authority has no embeddings yet
        };
        let path = pack_path(state_dir);
        if read_header(&path).is_ok_and(|h| h.version == version) {
            return Ok(false);
        }
        client.download("/embeddings/pack", &path, None)?;
        read_header(&path).context("downloaded embeddings pack is invalid")?;
        Ok(true)
    }

    /// State dirs with a background refresh in flight.
    static REFRESHING: std::sync::Mutex<std::collections::BTreeSet<PathBuf>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());

    /// [`refresh`] on a background thread, at most one per replica at a time.
    pub fn refresh_in_background(library: &Library) {
        let client = match library.remote_client() {
            Ok(client) => client,
            Err(e) => {
                eprintln!("embeddings pack: {e:#}");
                return;
            }
        };
        let state_dir = library.state_dir.clone();
        let refreshing = || REFRESHING.lock().unwrap_or_else(|e| e.into_inner());
        if !refreshing().insert(state_dir.clone()) {
            return;
        }
        std::thread::spawn(move || {
            if let Err(e) = refresh_with(&client, &state_dir) {
                eprintln!("embeddings pack: {e:#}");
            }
            refreshing().remove(&state_dir);
        });
    }

    /// Sync hook: refresh after catalog changes or when no pack is mirrored yet. With
    /// `background` the sync returns at once (live pushes never wait on the pack).
    pub fn refresh_after_sync(library: &Library, applied: u64, background: bool) {
        if applied == 0 && read_header(&pack_path(&library.state_dir)).is_ok() {
            return;
        }
        if background {
            refresh_in_background(library);
        } else if let Err(e) = refresh(library) {
            eprintln!("embeddings pack: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_vector_prefers_matching_hues() {
        let swatch = |hex: &str| ColorSwatch {
            hex: hex.into(),
            ratio: 1.0,
        };
        let red = palette_vector(&[swatch("#e02020")]);
        let red2 = palette_vector(&[swatch("#c81818")]);
        let blue = palette_vector(&[swatch("#2040e0")]);
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        assert!((dot(&red, &red) - 1.0).abs() < 1e-5);
        assert!(dot(&red, &red2) > 0.9, "{}", dot(&red, &red2));
        assert!(dot(&red, &blue) < 0.3, "{}", dot(&red, &blue));
        assert_eq!(palette_vector(&[]), [0f32; PALETTE_DIM]);
    }

    #[test]
    fn quantization_preserves_cosine() {
        let a: Vec<f32> = (0..768).map(|i| ((i * 37 % 101) as f32 - 50.0) / 50.0).collect();
        let mut q = Vec::new();
        let scale = quantize(&a, &mut q);
        let back: Vec<f32> = q.iter().map(|v| *v as i8 as f32 * scale).collect();
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        let cos = dot(&a, &back) / (dot(&a, &a).sqrt() * dot(&back, &back).sqrt());
        assert!(cos > 0.999, "{cos}");
    }

    #[test]
    fn header_rejects_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pack.bin");
        let header = PackHeader {
            format: 1,
            version: "v".into(),
            model: "m".into(),
            dim: 2,
            palette_dim: PALETTE_DIM,
            count: 1,
            quant: "int8".into(),
            ids: vec!["a".into()],
        };
        let bytes = encode(&header, &[0.5], &[1, 2], &[0; PALETTE_DIM]);
        fs::write(&path, &bytes).unwrap();
        assert_eq!(read_header(&path).unwrap(), header);
        fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
        assert!(read_header(&path).is_err());
    }
}
