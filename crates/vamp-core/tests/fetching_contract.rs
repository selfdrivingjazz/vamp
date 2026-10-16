//! Assets whose original arrives after them (`vamp_core::fetching`): cataloged pending with
//! no bytes, never picked up by the lazy hasher, failure recorded visibly, and completed
//! exactly once by landing bytes.
use serde_json::json;
use vamp_core::Library;
use vamp_core::fetching::{FETCH_FAILED, FETCHING, ORIGINAL_ERROR, awaiting_original};
use vamp_protocol::Metadata;

/// 1×1 PNG.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
    0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
    0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xfc,
    0xcf, 0xc0, 0xf0, 0x1f, 0x00, 0x05, 0x05, 0x02, 0x00, 0x5f, 0xc8, 0xf1, 0xd2, 0x00, 0x00,
    0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

#[test]
fn fetching_originals_are_pending_until_they_land_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut library = Library::init(&dir.path().join("home"), "main", None, None).unwrap();
    let metadata = Metadata {
        source_url: Some("https://example.com/page".into()),
        tags: vec!["b".into(), " a ".into()],
        ..Metadata::default()
    };
    let url = "https://img.example.com/cat.png";
    let first = library
        .add_fetching("Cat", url, "cat.png", metadata.clone(), Some((800, 600)))
        .unwrap();
    let second = library
        .add_fetching("Dog", "https://img.example.com/dog", "dog", metadata, None)
        .unwrap();
    for asset in [&first, &second] {
        assert!(awaiting_original(&asset.original));
        assert_eq!(asset.original.storage, FETCHING);
        assert_eq!(asset.original.path, "");
        assert_eq!(asset.original.byte_size, 0);
    }
    assert_eq!(first.original.mime, "image/png");
    assert_eq!(first.original.locator.as_deref(), Some(url));
    assert_eq!((first.width, first.height), (Some(800), Some(600)));
    assert_eq!(first.metadata.tags, ["a", "b"]);
    assert!(first.metadata.extra.contains_key("added_by"));
    // Distinct placeholder keys: replicas never mix the two up.
    assert_ne!(first.original.source_key(), second.original.source_key());
    assert!(first.original.source_key().starts_with("pending:0:"));

    // Nothing to hash yet: the lazy hasher neither counts nor touches them.
    assert_eq!(library.pending_hash_count().unwrap(), 0);
    let hashed = library.hash_pending(10).unwrap();
    assert_eq!(hashed["failed"], json!([]));

    // Provisional thumbnail, only while waiting; reading the original fails meanwhile.
    let rep = library.attach_provisional(&first.id, PNG).unwrap();
    assert_eq!(rep.source_hash, first.original.source_key());
    assert_eq!(rep.mime, "image/jpeg");
    assert!(library.fetch(&first.id, "thumbnail").unwrap().is_file());
    assert!(library.fetch(&first.id, "original").is_err());
    assert!(library.attach_provisional(&first.id, b"not an image").is_err());

    // Failure is recorded and visible; a later landing still completes the asset.
    let failed = library.fail_original(&second.id, "HTTP 403").unwrap();
    assert_eq!(failed.original.storage, FETCH_FAILED);
    assert_eq!(failed.metadata.extra[ORIGINAL_ERROR], json!("HTTP 403"));
    assert_eq!(failed.revision, 2);
    let landed = library.land_original(&second.id, PNG, "dog.png").unwrap();
    assert!(!awaiting_original(&landed.original));
    assert_eq!(landed.original.storage, "local");
    assert_eq!(landed.original.hash_state, "hashed");
    assert_eq!(landed.original.byte_size, PNG.len() as u64);
    assert!(!landed.metadata.extra.contains_key(ORIGINAL_ERROR));
    assert!(std::path::Path::new(&landed.original.path).is_file());

    let landed = library.land_original(&first.id, PNG, "cat.png").unwrap();
    assert_eq!(landed.revision, 2);
    assert_eq!(library.fetch(&first.id, "original").unwrap().to_str().unwrap(), landed.original.path);
    // The provisional thumbnail belonged to the placeholder.
    assert!(library.fetch(&first.id, "thumbnail").is_err());
    assert!(library.attach_provisional(&first.id, PNG).is_err());
    assert!(library.land_original(&first.id, PNG, "again.png").is_err());
    // Failing after landing is a no-op.
    assert_eq!(library.fail_original(&first.id, "late").unwrap().original.storage, "local");

    let kinds: Vec<_> = library
        .events(0, 100)
        .unwrap()
        .into_iter()
        .filter(|e| e.asset_id.as_deref() == Some(first.id.as_str()))
        .map(|e| e.kind)
        .collect();
    assert_eq!(kinds, ["media_added", "representation_ready", "media_updated"]);
}
