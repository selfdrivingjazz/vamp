//! Sparse updates reuse the persisted quantized rows, never the embedding model.
use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::collections::{BTreeMap, HashSet};
use std::io::{BufWriter, Write};

#[derive(Debug, Serialize, serde::Deserialize)]
pub struct PackDelta {
    pub format: u32,
    pub base_version: String,
    pub version: String,
    pub model: String,
    pub dim: usize,
    pub palette_dim: usize,
    pub count: usize,
    pub quant: String,
    pub cursor: u64,
    pub upserts: Vec<DeltaRow>,
    pub removed: Vec<String>,
}
#[derive(Debug, Serialize, serde::Deserialize)]
pub struct DeltaRow {
    pub id: String,
    /// Exact scale bytes + int8 vibe bytes + quantized palette bytes.
    pub data: String,
}

pub fn header_from_bytes(bytes: &[u8]) -> Result<PackHeader> {
    ensure!(bytes.len() >= 12 && &bytes[..8] == MAGIC, "not a vamp embeddings pack");
    let len = u32::from_le_bytes(bytes[8..12].try_into()?) as usize;
    ensure!(len <= bytes.len() - 12, "truncated pack header");
    let header = serde_json::from_slice(&bytes[12..12 + len])?;
    validate_header(&header, len, bytes.len())?;
    Ok(header)
}
fn body_offset(bytes: &[u8]) -> usize {
    (12 + u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize).next_multiple_of(16)
}

/// Changes through the pack's own snapshot checkpoint. Unrelated view/scope events
/// never force vector transfers. Source, palette, visibility and observation changes do.
pub fn delta_for(library: &Library, header: &PackHeader, bytes: &[u8], after: u64, base: &str) -> Result<PackDelta> {
    ensure!(after > 0 && after <= header.cursor && header.cursor <= i64::MAX as u64, "invalid pack checkpoint");
    let _snapshot = read_snapshot(&library.conn)?;
    let mut statement = library.conn.prepare(
        "SELECT DISTINCT json_extract(document,'$.asset_id') FROM events WHERE sequence>?1 AND sequence<=?2 AND json_extract(document,'$.asset_id') IS NOT NULL AND json_extract(document,'$.kind') IN ('media_added','media_updated','media_trashed','media_restored','media_removed','observation_added','representation_ready')"
    )?;
    let mut changed = statement.query_map(params![after as i64, header.cursor as i64], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    let offset = body_offset(bytes);
    let vibes = offset + header.count * 4;
    let palettes = vibes + header.count * header.dim;
    let mut row = Vec::with_capacity(4 + header.dim + header.palette_dim);
    let mut upserts = Vec::new();
    for (i, id) in header.ids.iter().enumerate() {
        if !changed.remove(id) { continue; }
        row.clear();
        row.extend_from_slice(&bytes[offset + i*4..offset + (i+1)*4]);
        row.extend_from_slice(&bytes[vibes + i*header.dim..vibes + (i+1)*header.dim]);
        row.extend_from_slice(&bytes[palettes + i*header.palette_dim..palettes + (i+1)*header.palette_dim]);
        upserts.push(DeltaRow { id: id.clone(), data: STANDARD.encode(&row) });
    }
    let mut removed: Vec<_> = changed.into_iter().collect();
    removed.sort_unstable();
    Ok(PackDelta { format: 1, base_version: base.into(), version: header.version.clone(), model: header.model.clone(), dim: header.dim, palette_dim: header.palette_dim, count: header.count, quant: header.quant.clone(), cursor: header.cursor, upserts, removed })
}

#[derive(Clone, Copy)]
enum Source { Old(usize), Changed(usize) }

/// Validate before publishing. A failed/interrupted transfer or invalid delta never
/// replaces the last usable pack. The staging file belongs to the caller.
pub fn apply_download(path: &Path, staging: &Path) -> Result<()> {
    let mut prefix = [0; 8];
    fs::File::open(staging)?.read_exact(&mut prefix)?;
    if &prefix == MAGIC {
        read_header(staging)?;
        fs::rename(staging, path)?;
        return Ok(());
    }
    let bytes = fs::read(staging)?;
    let delta: PackDelta = serde_json::from_slice(&bytes).context("invalid embeddings delta")?;
    let old = fs::read(path).context("embeddings delta needs its cached base")?;
    let previous = header_from_bytes(&old)?;
    ensure!(delta.format == 1 && delta.base_version == previous.version, "embeddings delta base mismatch");
    ensure!(delta.model == previous.model && delta.dim == previous.dim && delta.palette_dim == previous.palette_dim && delta.quant == previous.quant, "embeddings delta model/dimensions changed");
    ensure!(delta.cursor >= previous.cursor, "embeddings delta checkpoint regressed");
    let mut rows: BTreeMap<String, Source> = previous.ids.into_iter().enumerate().map(|(i,id)| (id, Source::Old(i))).collect();
    for id in delta.removed { rows.remove(&id); }
    let row_size = 4 + delta.dim + delta.palette_dim;
    let mut changed = Vec::new();
    let mut seen = HashSet::new();
    for row in delta.upserts {
        ensure!(seen.insert(row.id.clone()), "duplicate embeddings delta row");
        let start = changed.len();
        STANDARD.decode_vec(row.data, &mut changed).context("invalid embeddings delta row bytes")?;
        ensure!(changed.len() - start == row_size, "embeddings delta row size mismatch");
        rows.insert(row.id, Source::Changed(start));
    }
    ensure!(rows.len() == delta.count, "embeddings delta row count mismatch");
    let (ids, sources): (Vec<_>, Vec<_>) = rows.into_iter().unzip();
    let header = PackHeader { format: 1, version: delta.version, model: delta.model, dim: delta.dim, palette_dim: delta.palette_dim, count: ids.len(), quant: delta.quant, ids, cursor: delta.cursor };
    let old_offset = body_offset(&old);
    let old_vibes = old_offset + previous.count * 4;
    let old_palettes = old_vibes + previous.count * previous.dim;
    let temp = path.with_extension(format!("apply-{}.tmp", id::simple()));
    let result = (|| -> Result<()> {
        let file = OpenOptions::new().write(true).create_new(true).open(&temp)?;
        let mut out = BufWriter::new(file);
        let json = serde_json::to_vec(&header)?;
        out.write_all(MAGIC)?; out.write_all(&(json.len() as u32).to_le_bytes())?; out.write_all(&json)?;
        let padding = (12 + json.len()).next_multiple_of(16) - (12 + json.len());
        out.write_all(&[0;16][..padding])?;
        for column in 0..3 {
            for source in &sources {
                let slice = match (*source, column) {
                    (Source::Old(i), 0) => &old[old_offset+i*4..old_offset+(i+1)*4],
                    (Source::Old(i), 1) => &old[old_vibes+i*header.dim..old_vibes+(i+1)*header.dim],
                    (Source::Old(i), _) => &old[old_palettes+i*header.palette_dim..old_palettes+(i+1)*header.palette_dim],
                    (Source::Changed(i), 0) => &changed[i..i+4],
                    (Source::Changed(i), 1) => &changed[i+4..i+4+header.dim],
                    (Source::Changed(i), _) => &changed[i+4+header.dim..i+row_size],
                };
                out.write_all(slice)?;
            }
        }
        out.flush()?; out.get_ref().sync_all()?;
        read_header(&temp)?;
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() { let _ = fs::remove_file(&temp); }
    result
}

#[cfg(feature = "remote")]
pub fn refresh_download(client: &crate::remote::Client, state_dir: &Path) -> Result<bool> {
    let path = pack_path(state_dir);
    let cached = read_header(&path).ok();
    let mut url = "/embeddings/pack".to_owned();
    if let Some(h) = cached.as_ref().filter(|h| h.cursor > 0) {
        let query = form_urlencoded::Serializer::new(String::new())
            .append_pair("after", &h.cursor.to_string()).append_pair("version", &h.version)
            .append_pair("model", &h.model).append_pair("dim", &h.dim.to_string())
            .append_pair("palette_dim", &h.palette_dim.to_string()).finish();
        url.push('?'); url.push_str(&query);
    }
    let staging = path.with_extension(format!("download-{}.tmp", id::simple()));
    fs::create_dir_all(path.parent().context("pack directory")?)?;
    let result = (|| -> Result<bool> {
        if !client.download_if_changed(&url, &staging, cached.as_ref().map(|h| h.version.as_str()), &crate::remote::Transfer::background())? { return Ok(false); }
        apply_download(&path, &staging)?;
        Ok(true)
    })();
    let _ = fs::remove_file(&staging);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn blob(vector: &[f32]) -> Vec<u8> { vector.iter().flat_map(|v| v.to_le_bytes()).collect() }
    fn fixture() -> (tempfile::TempDir, Library, Vec<Asset>) {
        let dir = tempfile::tempdir().unwrap();
        let mut library = Library::init(&dir.path().join("home"), "main", None, None).unwrap();
        let mut assets = Vec::new();
        for n in 0..20 {
            let file = dir.path().join(format!("{n}.jpg")); fs::write(&file,[n]).unwrap();
            let asset = library.add(&file, None, Metadata::default()).unwrap();
            library.conn.execute("INSERT INTO embeddings(asset_id,source_hash,model,dim,vector) VALUES(?1,?2,'m',2,?3)", params![asset.id, asset.original.blake3, blob(&[0.5,0.5])]).unwrap();
            assets.push(asset);
        }
        (dir, library, assets)
    }
    #[test]
    fn sparse_update_and_remove_reconstruct_exact_pack_without_touching_base_on_failure() {
        let (dir, mut library, assets) = fixture();
        let (authority, base) = library.energy_pack().unwrap().unwrap();
        let replica = dir.path().join("replica.bin"); fs::copy(&authority,&replica).unwrap();
        library.conn.execute("INSERT OR REPLACE INTO embeddings(asset_id,source_hash,model,dim,vector) VALUES(?1,?2,'m',2,?3)", params![assets[0].id, assets[0].original.blake3, blob(&[0.8,0.2])]).unwrap();
        library.update(&assets[0].id, MetadataPatch { description: Some("changed".into()), ..MetadataPatch::default() }, None).unwrap();
        library.remove(&assets[1].id, None).unwrap();
        let (authority,target) = library.energy_pack().unwrap().unwrap();
        assert_ne!(base.version,target.version);
        let expected = fs::read(authority).unwrap();
        let delta = delta_for(&library,&target,&expected,base.cursor,&base.version).unwrap();
        assert_eq!(delta.upserts.len(),1); assert_eq!(delta.removed,vec![assets[1].id.clone()]);
        let staging = dir.path().join("delta.json"); fs::write(&staging,serde_json::to_vec(&delta).unwrap()).unwrap();
        apply_download(&replica,&staging).unwrap(); assert_eq!(fs::read(&replica).unwrap(),expected);
        let before = fs::read(&replica).unwrap();
        assert!(apply_download(&replica,&staging).is_err()); assert_eq!(fs::read(replica).unwrap(),before);
    }
    #[test]
    fn ordinary_view_does_not_invalidate_pack_and_corruption_does_not_replace_cache() {
        let (dir, mut library, assets) = fixture();
        let (path,first) = library.energy_pack().unwrap().unwrap();
        library.touch(&assets[0].id).unwrap();
        assert_eq!(library.energy_pack().unwrap().unwrap().1.version,first.version);
        let before = fs::read(&path).unwrap();
        let staging = dir.path().join("broken.bin"); fs::write(&staging,b"VEPK0001").unwrap();
        assert!(apply_download(&path,&staging).is_err()); assert_eq!(fs::read(&path).unwrap(),before);
        let mut bad = before; bad[8..12].copy_from_slice(&u32::MAX.to_le_bytes()); fs::write(&staging,bad).unwrap();
        assert!(read_header(&staging).is_err());
    }
}
