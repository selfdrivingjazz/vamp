use std::fs;
use tempfile::TempDir;
use vamp_core::Library;
use vamp_protocol::{Metadata, MetadataPatch, ProcessorSpec};

fn library() -> (TempDir, Library, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let source = dir.path().join("original.txt");
    fs::write(&source, b"first revision").unwrap();
    let lib = Library::init(&dir.path().join("state"), "test", None, None).unwrap();
    (dir, lib, source)
}

#[test]
fn rejected_state_location_never_creates_files_inside_original_storage() {
    let dir = TempDir::new().unwrap();
    let originals = dir.path().join("originals");
    fs::create_dir(&originals).unwrap();
    let forbidden = originals.join("new-local-state");
    assert!(
        Library::init(
            &dir.path().join("home"),
            "test",
            Some(&originals),
            Some(&forbidden),
        )
        .is_err()
    );
    assert!(!forbidden.exists());
    assert_eq!(fs::read_dir(&originals).unwrap().count(), 0);
}

#[test]
fn ingested_bytes_become_inbox_originals_outside_catalog_state() {
    let (_dir, mut lib, _) = library();
    let asset = lib
        .ingest_bytes(b"clip bytes", "Screen Shot.PNG", Metadata::default())
        .unwrap();
    let path = std::path::Path::new(&asset.original.path);
    assert_eq!(asset.name, "Screen Shot.png");
    assert_eq!(asset.original.mime, "image/png");
    assert!(path.starts_with(lib.inbox_dir().canonicalize().unwrap()));
    assert!(!path.starts_with(&lib.state_dir));
    assert_eq!(fs::read(path).unwrap(), b"clip bytes");
    // Identical bytes ingested twice are separate clips, not a silent dedupe to one path.
    let again = lib
        .ingest_bytes(b"clip bytes", "../../evil", Metadata::default())
        .unwrap();
    assert_ne!(again.id, asset.id);
    assert!(
        std::path::Path::new(&again.original.path)
            .starts_with(lib.inbox_dir().canonicalize().unwrap())
    );
    assert!(lib.ingest_bytes(b"", "x.png", Metadata::default()).is_err());
}

#[test]
fn offline_original_does_not_erase_search_or_membership() {
    let (_dir, mut lib, source) = library();
    let asset = lib
        .add(&source, Some("amber reference"), Metadata::default())
        .unwrap();
    let collection = lib.collection_create("References", None).unwrap();
    lib.collection_membership(&collection.id, &asset.id, true)
        .unwrap();
    fs::remove_file(&source).unwrap();
    assert_eq!(lib.list(Some("amber"), 10, 0, None).unwrap()[0].id, asset.id);
    assert_eq!(
        lib.inspect(&asset.id).unwrap().asset.original.blake3,
        asset.original.blake3
    );
    assert_eq!(
        lib.collections().unwrap()[0].asset_ids,
        vec![asset.id.clone()]
    );
    assert!(lib.fetch(&asset.id, "original").is_err());
    assert!(lib.refresh(&asset.id, None).is_err());
    assert_eq!(lib.inspect(&asset.id).unwrap().asset.revision, 1);
}

#[test]
fn stale_revision_cannot_overwrite_metadata_or_emit_event() {
    let (_dir, mut lib, source) = library();
    let asset = lib.add(&source, None, Metadata::default()).unwrap();
    let updated = lib
        .update(
            &asset.id,
            MetadataPatch {
                name: Some("human choice".into()),
                ..Default::default()
            },
            Some(1),
        )
        .unwrap();
    let events = lib.events(0, 100).unwrap().len();
    assert!(
        lib.update(
            &asset.id,
            MetadataPatch {
                name: Some("stale agent choice".into()),
                ..Default::default()
            },
            Some(1)
        )
        .is_err()
    );
    assert_eq!(lib.inspect(&asset.id).unwrap().asset.name, "human choice");
    assert_eq!(
        lib.inspect(&asset.id).unwrap().asset.revision,
        updated.revision
    );
    assert_eq!(lib.events(0, 100).unwrap().len(), events);
}

#[test]
fn removing_membership_and_asset_never_deletes_original_bytes() {
    let (_dir, mut lib, source) = library();
    let asset = lib.add(&source, None, Metadata::default()).unwrap();
    let first = lib.collection_create("First", None).unwrap();
    let second = lib.collection_create("Second", None).unwrap();
    lib.collection_membership(&first.id, &asset.id, true)
        .unwrap();
    lib.collection_membership(&second.id, &asset.id, true)
        .unwrap();
    lib.collection_membership(&first.id, &asset.id, false)
        .unwrap();
    assert_eq!(
        lib.collections()
            .unwrap()
            .into_iter()
            .find(|c| c.id == second.id)
            .unwrap()
            .asset_ids,
        vec![asset.id.clone()]
    );
    lib.remove(&asset.id, Some(1)).unwrap();
    assert_eq!(fs::read(&source).unwrap(), b"first revision");
    assert!(lib.list(None, 10, 0, None).unwrap().is_empty());
    // Trash keeps folder memberships so a restore puts the asset back where it was.
    assert_eq!(lib.restore(std::slice::from_ref(&asset.id)).unwrap(), 1);
    assert_eq!(lib.list(None, 10, 0, None).unwrap().len(), 1);
    lib.remove(&asset.id, None).unwrap();
    assert_eq!(lib.empty_trash(None).unwrap(), 1);
    assert_eq!(fs::read(&source).unwrap(), b"first revision");
    assert!(
        lib.collections()
            .unwrap()
            .iter()
            .all(|c| c.asset_ids.is_empty())
    );
    assert_eq!(lib.export().unwrap()["tombstones"][0]["id"], asset.id);
}

#[test]
fn queued_old_revision_cannot_annotate_replaced_original() {
    let (dir, mut lib, source) = library();
    lib.processor_add(ProcessorSpec {
        name: "caption".into(),
        events: vec!["media_added".into()],
        command: vec!["unreachable-command-must-not-run".into()],
        input: "original".into(),
        output: "observation".into(),
        timeout_seconds: 1,
        placement: Default::default(),
    })
    .unwrap();
    let asset = lib.add(&source, None, Metadata::default()).unwrap();
    drop(lib);
    let mut lib = Library::open(&dir.path().join("state"), None).unwrap();
    assert_eq!(lib.jobs(Some("pending"), 10).unwrap()[0].asset_id, asset.id);
    fs::write(&source, b"second revision with new content").unwrap();
    lib.refresh(&asset.id, Some(1)).unwrap();
    let result = lib.run_jobs(10).unwrap();
    assert_eq!(result["failed"], 1);
    let failed = lib.jobs(Some("failed"), 10).unwrap().remove(0);
    assert!(failed.error.unwrap().contains("stale job"));
    assert!(lib.retry_job(&failed.id).is_err());
    assert!(lib.inspect(&asset.id).unwrap().observations.is_empty());
}

#[test]
fn sidebar_folder_counts_include_sub_folders() {
    let (_dir, mut lib, _) = library();
    let asset = |lib: &mut Library, bytes: &[u8]| {
        lib.ingest_bytes(bytes, "x.png", Metadata::default())
            .unwrap()
            .id
    };
    let [a, b, c, d, trashed] = [b"a", b"b", b"c", b"d", b"e"].map(|bytes| asset(&mut lib, bytes));
    // Explorations (empty) > Mission {a} > people {b, c, trashed}, studios {a, d}.
    let root = lib.collection_create("Explorations", None).unwrap();
    let mission = lib.collection_create("Mission", Some(&root.id)).unwrap();
    let people = lib.collection_create("people", Some(&mission.id)).unwrap();
    let studios = lib.collection_create("studios", Some(&mission.id)).unwrap();
    for (folder, id) in [(&mission, &a), (&people, &b), (&people, &c), (&people, &trashed), (&studios, &a), (&studios, &d)] {
        lib.collection_membership(&folder.id, id, true).unwrap();
    }
    lib.remove(&trashed, None).unwrap();
    let sidebar = lib.sidebar().unwrap();
    let root_node = &sidebar["folders"][0];
    let mission_node = &root_node["children"][0];
    let count = |node: &serde_json::Value| (node["name"].as_str().unwrap().to_owned(), node["count"].as_i64().unwrap());
    // Distinct live assets of each subtree: `a` counts once, the trashed one not at all.
    assert_eq!(count(root_node), ("Explorations".into(), 4));
    assert_eq!(count(mission_node), ("Mission".into(), 4));
    assert_eq!(count(&mission_node["children"][0]), ("people".into(), 2));
    assert_eq!(count(&mission_node["children"][1]), ("studios".into(), 2));
}

#[test]
fn server_url_is_recorded_per_authority_library_and_reopened() {
    let (dir, lib, _) = library();
    assert_eq!(lib.server_url().unwrap(), None);
    assert!(lib.set_server_url(Some("authority.example:7777")).is_err());
    assert!(lib.set_server_url(Some("http://")).is_err());
    assert_eq!(
        lib.set_server_url(Some(" http://authority.example:7777/ ")).unwrap().as_deref(),
        Some("http://authority.example:7777")
    );
    let reopened = Library::open(&dir.path().join("state"), Some("test")).unwrap();
    assert_eq!(reopened.server_url().unwrap().as_deref(), Some("http://authority.example:7777"));
    assert_eq!(reopened.set_server_url(None).unwrap(), None);
    assert_eq!(lib.server_url().unwrap(), None);
    assert_eq!(reopened.set_server_url(None).unwrap(), None);
}
