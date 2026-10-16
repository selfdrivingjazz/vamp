//! Originals that arrive after their asset. A clipped image is cataloged the moment the
//! request lands (name, source page, tags, folder, size when known), optionally with a
//! provisional thumbnail the clipper rendered from the page; its bytes follow from a
//! background download or the clipper's own upload.
//!
//! Until then the original is honest about having no bytes: `path` '', `blake3` '',
//! `byte_size` 0, `hash_state` `pending` (source key `pending:0:<ns>`, unique per asset),
//! `locator` the image URL and `storage` `fetching`. A download that fails turns `storage`
//! into `failed` with the reason in `metadata.extra.original_error`; the asset stays (an
//! upload can still complete it). Landing bytes replaces the original (`media_updated`
//! with `change: original`), like a lazy fingerprint that turned out different.
use super::*;

/// `original.storage` while the bytes are on their way.
pub const FETCHING: &str = "fetching";
/// `original.storage` once fetching failed (see [`ORIGINAL_ERROR`]).
pub const FETCH_FAILED: &str = "failed";
/// `metadata.extra` key holding why the original could not be fetched.
pub const ORIGINAL_ERROR: &str = "original_error";
/// Representations attached before the original exists live under this state-dir folder.
const PROVISIONAL: &str = "representations/provisional";
/// Longest edge kept from a client-rendered provisional thumbnail.
const PROVISIONAL_EDGE: u32 = 512;

/// Whether the asset's original has not landed yet (fetching or failed).
pub fn awaiting_original(original: &Original) -> bool {
    original.pending() && matches!(original.storage.as_str(), FETCHING | FETCH_FAILED)
}

impl Library {
    /// Catalogs an asset whose original is still at `locator` (see the module docs).
    /// `filename_hint` gives the provisional mime type; `size` the pixel size if known.
    pub fn add_fetching(
        &mut self,
        name: &str,
        locator: &str,
        filename_hint: &str,
        mut metadata: Metadata,
        size: Option<(u32, u32)>,
    ) -> Result<Asset> {
        ensure!(!name.trim().is_empty(), "name is required");
        normalize_tags(&mut metadata.tags);
        catalog::set_added_by(&mut metadata, &current_actor());
        let timestamp = now();
        let created_ns = Utc::now().timestamp_nanos_opt().unwrap_or_default();
        let asset = Asset {
            id: id(),
            revision: 1,
            name: name.trim().into(),
            original: Original {
                path: String::new(),
                blake3: String::new(),
                byte_size: 0,
                mime: mime_for(Path::new(filename_hint)).into(),
                locator: Some(locator.into()),
                storage: FETCHING.into(),
                hash_state: "pending".into(),
                modified_ns: Some(created_ns),
            },
            metadata,
            created_at: timestamp.clone(),
            updated_at: timestamp,
            width: size.map(|s| s.0).filter(|w| *w > 0),
            height: size.map(|s| s.1).filter(|h| *h > 0),
            original_created_at: None,
            original_modified_at: None,
            last_viewed_at: None,
            deleted_at: None,
        };
        let tx = self.conn.transaction()?;
        // assets.path is the unique reference key; there is no file yet.
        tx.execute(
            "INSERT INTO assets(id,path,hash,document) VALUES(?1,?2,?3,?4)",
            params![
                asset.id,
                format!("{FETCHING}:{}", asset.id),
                asset.original.source_key(),
                json_text(&asset)?
            ],
        )?;
        reindex(&tx, &asset)?;
        emit(
            &tx,
            &self.context,
            "media_added",
            Some(&asset),
            json!({"storage":FETCHING,"locator":locator}),
        )?;
        tx.commit()?;
        Ok(asset)
    }

    /// Records a client-rendered thumbnail (any decodable image; stored as JPEG, at most
    /// 512 px) for an asset whose original has not landed. Replaced by the real thumbnail
    /// once it has.
    pub fn attach_provisional(&mut self, id: &str, bytes: &[u8]) -> Result<Representation> {
        let asset = get_asset(&self.conn, id)?;
        ensure!(
            awaiting_original(&asset.original),
            "the original of {id} has landed; its thumbnail comes from it"
        );
        let image = image::load_from_memory(bytes).context("provisional thumbnail is not an image")?;
        let image = if image.width().max(image.height()) > PROVISIONAL_EDGE {
            image.thumbnail(PROVISIONAL_EDGE, PROVISIONAL_EDGE)
        } else {
            image
        };
        let mut jpeg = Vec::new();
        image
            .to_rgb8()
            .write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)?;
        let relative = format!("{PROVISIONAL}/{id}/thumbnail-{}.jpg", id::simple());
        let output = self.state_dir.join(&relative);
        fs::create_dir_all(output.parent().unwrap())?;
        fs::write(&output, &jpeg)?;
        let rep = Representation {
            kind: "thumbnail".into(),
            path: relative,
            source_hash: asset.original.source_key(),
            width: image.width(),
            height: image.height(),
            mime: "image/jpeg".into(),
            byte_size: jpeg.len() as u64,
        };
        let tx = self.conn.transaction()?;
        let current = get_asset(&tx, id)?;
        ensure!(
            current.original.source_key() == rep.source_hash,
            "the original of {id} changed"
        );
        attach_representation(&tx, &self.context, &current, &rep)?;
        tx.commit()?;
        Ok(rep)
    }

    /// Stores the bytes of an asset cataloged by [`Library::add_fetching`] (a background
    /// download or the client's upload, also after a failed fetch) and makes them its
    /// original. Errors when the original has already landed.
    pub fn land_original(&mut self, id: &str, bytes: &[u8], filename_hint: &str) -> Result<Asset> {
        ensure!(
            awaiting_original(&get_asset(&self.conn, id)?.original),
            "the original of {id} is already stored"
        );
        let (path, _) = self.write_inbox(bytes, filename_hint)?;
        let landed = (|| {
            let mut original = fingerprint(&path)?;
            let tx = self.conn.transaction()?;
            let mut asset = get_asset(&tx, id)?;
            ensure!(
                awaiting_original(&asset.original),
                "the original of {id} is already stored"
            );
            let cloud = storage::config(&tx)?.is_some();
            original.storage = if cloud { "uploading" } else { "local" }.into();
            let previous = asset.original.source_key();
            asset.original = original;
            asset.metadata.extra.remove(ORIGINAL_ERROR);
            bump(&mut asset);
            tx.execute(
                "UPDATE assets SET path=?2 WHERE id=?1",
                params![id, asset.original.path],
            )?;
            // Provisional thumbnails were keyed to the placeholder; the real ones follow.
            tx.execute(
                "DELETE FROM representations WHERE asset_id=?1 AND source_hash=?2",
                params![id, previous],
            )?;
            save_asset(&tx, &asset)?;
            if cloud {
                storage::enqueue_upload(&tx, &asset)?;
            }
            emit(
                &tx,
                &self.context,
                "media_updated",
                Some(&asset),
                json!({"change":"original","previous_hash":previous}),
            )?;
            tx.commit()?;
            Ok(asset)
        })();
        match landed {
            Ok(asset) => {
                let _ = fs::remove_dir_all(self.state_dir.join(PROVISIONAL).join(id));
                Ok(asset)
            }
            Err(error) => {
                let _ = fs::remove_file(&path);
                Err(error)
            }
        }
    }

    /// Marks a still-missing original as failed (`storage` `failed`, the reason in
    /// `metadata.extra.original_error`). No-op once the original has landed.
    pub fn fail_original(&mut self, id: &str, reason: &str) -> Result<Asset> {
        let tx = self.conn.transaction()?;
        let mut asset = get_asset(&tx, id)?;
        if !awaiting_original(&asset.original) {
            return Ok(asset);
        }
        asset.original.storage = FETCH_FAILED.into();
        asset
            .metadata
            .extra
            .insert(ORIGINAL_ERROR.into(), json!(reason));
        bump(&mut asset);
        save_asset(&tx, &asset)?;
        emit(
            &tx,
            &self.context,
            "media_updated",
            Some(&asset),
            json!({"change":"original_failed","error":reason}),
        )?;
        tx.commit()?;
        Ok(asset)
    }
}
