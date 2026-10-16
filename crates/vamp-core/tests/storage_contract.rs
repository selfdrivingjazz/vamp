use serde_json::json;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use vamp_core::{Backend, Library, StorageConfig};
use vamp_protocol::{Metadata, Original};

/// Writes a 1-byte "jpeg" to the requested output path, standing in for vamp-media.
const FAKE_MEDIA: &str = r#"#!/bin/sh
out=$(sed -n 's/.*"output_path":"\([^"]*\)".*/\1/p')
printf x > "$out"
printf '{"representation":{"path":"%s","width":1,"height":1,"mime":"image/jpeg","byte_size":1}}' "$out"
"#;

struct Fixture {
    dir: TempDir,
    lib: Library,
    cloud: PathBuf,
    originals: PathBuf,
}
fn fixture(delete_local: bool) -> Fixture {
    let dir = TempDir::new().unwrap();
    let originals = dir.path().join("originals");
    let cloud = dir.path().join("cloud");
    fs::create_dir_all(&originals).unwrap();
    fs::create_dir_all(&cloud).unwrap();
    let mut lib = Library::init(&dir.path().join("home"), "test", Some(&originals), None).unwrap();
    set_folder(&mut lib, &cloud, delete_local, false);
    let helper = dir.path().join("fake-media");
    fs::write(&helper, FAKE_MEDIA).unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
    Fixture {
        originals: originals.canonicalize().unwrap(),
        dir,
        lib,
        cloud,
    }
}
fn set_folder(lib: &mut Library, root: &Path, delete: bool, anywhere: bool) {
    lib.storage_set(StorageConfig {
        backend: Backend::Folder {
            root: root.to_string_lossy().into(),
        },
        delete_local_after_upload: delete,
        delete_local_anywhere: anywhere,
    })
    .unwrap();
}
impl Fixture {
    fn add(&mut self, name: &str, bytes: &[u8]) -> (String, PathBuf) {
        let path = self.originals.join(name);
        fs::write(&path, bytes).unwrap();
        let asset = self.lib.add(&path, None, Metadata::default()).unwrap();
        (asset.id, path)
    }
    fn derivatives(&mut self, id: &str) {
        let helper = self.dir.path().join("fake-media");
        self.lib.generate(id, "thumbnail", &helper).unwrap();
        self.lib.generate(id, "preview", &helper).unwrap();
    }
    fn original(&self, id: &str) -> Original {
        self.lib.inspect(id).unwrap().asset.original
    }
    fn event_kinds(&self) -> Vec<String> {
        self.lib
            .events(0, 1000)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect()
    }
}

#[test]
fn folder_backend_full_cycle_evicts_only_verified_copy_and_rehydrates() {
    let mut f = fixture(true);
    let bytes = b"full resolution original bytes".repeat(1000);
    let (id, path) = f.add("photo.JPG", &bytes);
    assert_eq!(f.original(&id).storage, "uploading");

    let result = f.lib.run_jobs(10).unwrap();
    assert_eq!(result["uploads"][0]["status"], "completed", "{result}");
    let original = f.original(&id);
    assert_eq!(original.storage, "cloud+local");
    let locator = original.locator.clone().unwrap();
    let rel = locator.strip_prefix("folder://default/").unwrap();
    assert!(rel.ends_with(&format!("{}.jpg", original.blake3)));
    assert_eq!(fs::read(f.cloud.join(rel)).unwrap(), bytes);
    // No thumbnail+preview yet: the local original is kept.
    assert!(path.is_file());

    f.derivatives(&id);
    let result = f.lib.run_jobs(10).unwrap();
    assert_eq!(result["evicted"], json!([id]));
    assert!(!path.exists());
    let evicted = f.lib.inspect(&id).unwrap().asset;
    assert_eq!(evicted.original.storage, "cloud");
    assert!(evicted.original.path.is_empty());
    assert_eq!(evicted.original.locator.as_deref(), Some(locator.as_str()));
    let kinds = f.event_kinds();
    assert!(kinds.contains(&"original_stored".into()));
    assert!(kinds.contains(&"original_evicted".into()));
    assert!(!f.lib.original_available_locally(&evicted.original));

    let fetched = f.lib.fetch(&id, "original").unwrap();
    assert!(fetched.starts_with(&f.lib.state_dir));
    assert_eq!(fs::read(&fetched).unwrap(), bytes);
    assert!(f.lib.original_available_locally(&evicted.original));
    assert_eq!(f.lib.cache_prune().unwrap()["removed_files"], 1);
    assert_eq!(
        fs::read(f.lib.fetch(&id, "original").unwrap()).unwrap(),
        bytes
    );

    // A corrupted cloud copy is never served.
    f.lib.cache_prune().unwrap();
    fs::write(f.cloud.join(rel), b"tampered").unwrap();
    let error = f.lib.fetch(&id, "original").unwrap_err().to_string();
    assert!(error.contains("blake3"), "{error}");

    let error = f.lib.refresh(&id, None).unwrap_err().to_string();
    assert!(error.contains("only in cloud storage"), "{error}");
    assert!(f.lib.storage_unset().is_err());
}

#[test]
fn setting_off_never_deletes_and_is_the_default() {
    let mut f = fixture(false);
    let (id, path) = f.add("keep.png", b"keep me");
    f.derivatives(&id);
    for _ in 0..2 {
        f.lib.run_jobs(10).unwrap();
    }
    assert_eq!(fs::read(&path).unwrap(), b"keep me");
    assert_eq!(f.original(&id).storage, "cloud+local");
    let config = f.lib.storage_config().unwrap().unwrap();
    assert!(!config.delete_local_after_upload);
    let parsed: StorageConfig =
        serde_json::from_value(json!({"backend":"folder","root":"/x"})).unwrap();
    assert!(!parsed.delete_local_after_upload && !parsed.delete_local_anywhere);
}

#[test]
fn modified_local_file_is_not_deleted() {
    let mut f = fixture(false);
    let (id, path) = f.add("edit.png", b"version one");
    f.lib.run_jobs(10).unwrap();
    f.derivatives(&id);
    fs::write(&path, b"version TWO").unwrap();
    let cloud = f.cloud.clone();
    set_folder(&mut f.lib, &cloud, true, false);
    let result = f.lib.run_jobs(10).unwrap();
    assert_eq!(result["evicted"], json!([]));
    assert_eq!(fs::read(&path).unwrap(), b"version TWO");
    assert_eq!(f.original(&id).storage, "cloud+local");
}

#[test]
fn files_outside_library_roots_need_delete_local_anywhere() {
    let mut f = fixture(true);
    let outside = f.dir.path().join("elsewhere.png");
    fs::write(&outside, b"outside").unwrap();
    let id = f.lib.add(&outside, None, Metadata::default()).unwrap().id;
    f.derivatives(&id);
    f.lib.run_jobs(10).unwrap();
    assert!(outside.is_file());
    let cloud = f.cloud.clone();
    set_folder(&mut f.lib, &cloud, true, true);
    let result = f.lib.run_jobs(10).unwrap();
    assert_eq!(result["evicted"], json!([id]));
    assert!(!outside.exists());
}

#[test]
fn interrupted_upload_is_retried_from_scratch() {
    let mut f = fixture(false);
    let (id, _) = f.add("crash.png", b"crash test bytes");
    let hash = f.original(&id).blake3;
    // Simulate a worker killed mid-copy: row left running, partial temp object on disk.
    let conn = rusqlite::Connection::open(f.lib.state_dir.join("catalog.sqlite3")).unwrap();
    conn.execute("UPDATE uploads SET status='running'", [])
        .unwrap();
    let rel = format!("{}/originals/{}", f.lib.context.library_id, &hash[..2]);
    let dir = f.cloud.join(&rel);
    fs::create_dir_all(&dir).unwrap();
    let partial = dir.join(format!(".{hash}.png.vamp-tmp-deadbeef"));
    fs::write(&partial, b"crash").unwrap();

    let result = f.lib.run_jobs(10).unwrap();
    assert_eq!(result["uploads"][0]["status"], "completed", "{result}");
    assert_eq!(result["uploads"][0]["attempts"], 1);
    assert!(!partial.exists());
    assert_eq!(
        fs::read(dir.join(format!("{hash}.png"))).unwrap(),
        b"crash test bytes"
    );
    assert_eq!(f.original(&id).storage, "cloud+local");
}

#[test]
fn refresh_reuploads_changed_original_and_failed_upload_is_reported() {
    let mut f = fixture(false);
    let (id, path) = f.add("r.png", b"one");
    f.lib.run_jobs(10).unwrap();
    fs::write(&path, b"two!").unwrap();
    let refreshed = f.lib.refresh(&id, None).unwrap();
    assert_eq!(refreshed.original.storage, "uploading");
    assert!(refreshed.original.locator.is_none());
    // Local file changes again before the worker runs: upload refuses stale bytes.
    fs::write(&path, b"three").unwrap();
    let result = f.lib.run_jobs(10).unwrap();
    assert_eq!(result["failed"], 1);
    assert!(
        result["uploads"][0]["error"]
            .as_str()
            .unwrap()
            .contains("refresh")
    );
    f.lib.refresh(&id, None).unwrap();
    f.lib.run_jobs(10).unwrap();
    assert_eq!(f.original(&id).storage, "cloud+local");
}

#[test]
fn migrate_queues_existing_local_originals() {
    let dir = TempDir::new().unwrap();
    let source = dir.path().join("old.png");
    fs::write(&source, b"pre-cloud").unwrap();
    let cloud = dir.path().join("cloud");
    fs::create_dir(&cloud).unwrap();
    let mut lib = Library::init(&dir.path().join("home"), "t", None, None).unwrap();
    let id = lib.add(&source, None, Metadata::default()).unwrap().id;
    assert_eq!(lib.inspect(&id).unwrap().asset.original.storage, "local");
    assert!(lib.storage_migrate(10).is_err());
    set_folder(&mut lib, &cloud, false, false);
    assert_eq!(lib.storage_migrate(10).unwrap()["queued"], 1);
    lib.run_jobs(10).unwrap();
    assert_eq!(
        lib.inspect(&id).unwrap().asset.original.storage,
        "cloud+local"
    );
    assert_eq!(lib.storage_migrate(10).unwrap()["queued"], 0);
}

#[test]
fn catalogs_without_storage_fields_read_as_local() {
    let original: Original = serde_json::from_value(
        json!({"path":"/a.png","blake3":"00","byte_size":1,"mime":"image/png"}),
    )
    .unwrap();
    assert_eq!(original.storage, "local");
    assert!(original.locator.is_none());
}

#[cfg(feature = "native")]
#[test]
fn native_bridge_serves_evicted_originals_and_reports_storage() {
    use vamp_core::native;
    let mut f = fixture(true);
    let (id, path) = f.add("app.png", b"viewer bytes");
    f.derivatives(&id);
    f.lib.run_jobs(10).unwrap();
    assert!(!path.exists());
    let home = f.dir.path().join("home");
    let op = |value: serde_json::Value| {
        let mut value = value;
        value["home"] = json!(home);
        value["context"] = json!("test");
        native::request(serde_json::from_value(value).unwrap()).unwrap()
    };
    let item = &op(json!({"operation":"browse","offset":0,"limit":10}))["items"][0];
    assert_eq!(item["storage"], "cloud");
    assert_eq!(item["available_locally"]["original"], false);
    for request in [
        json!({"operation":"original","asset_id":id}),
        json!({"operation":"representation","asset_id":id,"kind":"original"}),
    ] {
        let served = op(request);
        assert_eq!(
            fs::read(served["path"].as_str().unwrap()).unwrap(),
            b"viewer bytes"
        );
    }
    let item = &op(json!({"operation":"browse","offset":0,"limit":10}))["items"][0];
    assert_eq!(item["available_locally"]["original"], true);
}
