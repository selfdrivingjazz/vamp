use base64::Engine as _;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use vamp_core::{Library, native, remote};

const FAKE_MEDIA: &str = r#"#!/bin/sh
out=$(sed -n 's/.*"output_path":"\([^"]*\)".*/\1/p')
printf x > "$out"
printf '{"representation":{"path":"%s","width":1,"height":1,"mime":"image/jpeg","byte_size":1}}' "$out"
"#;

fn op(home: &Path, value: Value) -> Value {
    let mut value = value;
    value["home"] = json!(home);
    native::request(serde_json::from_value(value).unwrap()).unwrap()
}

/// Every mirrored table, row by row (not the thumbnail queue, drained in the background).
fn dump(home: &Path, name: &str) -> Vec<(String, Vec<String>)> {
    let library = Library::open(home, Some(name)).unwrap();
    let conn = rusqlite::Connection::open(library.state_dir.join("catalog.sqlite3")).unwrap();
    [
        "assets",
        "representations",
        "observations",
        "search",
        "asset_tags",
        "collections",
        "memberships",
        "smart_folders",
        "tag_groups",
        "settings",
        "replica_state",
    ]
    .into_iter()
    .map(|table| {
        let mut stmt = conn
            .prepare(&format!("SELECT * FROM {table} ORDER BY 1,2"))
            .unwrap();
        let columns = stmt.column_count();
        let mut rows: Vec<String> = stmt
            .query_map([], |r| {
                Ok((0..columns)
                    .map(|i| match r.get_ref(i).unwrap() {
                        rusqlite::types::ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join("|"))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        rows.sort();
        (table.to_owned(), rows)
    })
    .collect()
}

struct Authority {
    url: String,
    token: String,
    server: Arc<tiny_http::Server>,
    serving: Option<std::thread::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl Authority {
    fn start() -> Self {
        static HELPER: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        HELPER.get_or_init(|| {
            let dir = tempfile::tempdir().unwrap();
            let helper = dir.path().join("fake-media");
            std::fs::write(&helper, FAKE_MEDIA).unwrap();
            std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
            // SAFETY: set once, before any server of this binary reads it.
            unsafe { std::env::set_var("VAMP_MEDIA_HELPER", &helper) };
            dir
        });
        let dir = tempfile::tempdir().unwrap();
        let library = Library::init(&dir.path().join("home"), "main", None, None).unwrap();
        let token = vamp_server::token(&library.state_dir).unwrap();
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let url = format!("http://{}", server.server_addr());
        let app = vamp_server::App::new(library, "main").unwrap();
        let serving = {
            let server = server.clone();
            Some(std::thread::spawn(move || vamp_server::serve(server, app, 4)))
        };
        Self {
            url,
            token,
            server,
            serving,
            _dir: dir,
        }
    }
    fn attach(&self, home: &Path, name: &str) -> Value {
        op(
            home,
            json!({"operation":"remote_attach","name":name,"remote_url":self.url,"token":self.token}),
        )
    }
}

impl Drop for Authority {
    fn drop(&mut self) {
        for _ in 0..4 {
            self.server.unblock();
        }
        if let Some(serving) = self.serving.take() {
            serving.join().unwrap();
        }
    }
}

fn ingest(home: &Path, name: &str, seed: u32, tags: &[&str]) -> String {
    let bytes: Vec<u8> = (0..20_000u32).map(|i| (i * seed % 251) as u8).collect();
    op(
        home,
        json!({"operation":"ingest","filename":format!("{name}.png"),"name":name,
               "bytes_base64":base64::engine::general_purpose::STANDARD.encode(bytes),
               "metadata":{"tags":tags}}),
    )["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// A mirror bootstrapped from a snapshot and then synced incrementally (batched) equals one
/// that replayed every event; thumbnails download afterwards and are reported as updates.
#[test]
fn snapshot_then_incremental_equals_full_replay() {
    let authority = Authority::start();
    let writer = tempfile::tempdir().unwrap();
    authority.attach(writer.path(), "w");
    let reference = tempfile::tempdir().unwrap();
    let url = &authority.url;
    remote::attach_remote(reference.path(), "r", url, &authority.token).unwrap();
    let replay = |home: &Path| {
        let mut library = Library::open(home, Some("r")).unwrap();
        remote::replay(&mut library).unwrap();
    };

    let w = writer.path();
    let bat = ingest(w, "bat", 7, &["night"]);
    let moth = ingest(w, "moth", 11, &["night", "wings"]);
    let owl = ingest(w, "owl", 13, &[]);
    let folder = op(w, json!({"operation":"collection_create","name":"Night"}));
    op(
        w,
        json!({"operation":"collection_add","collection_id":folder["id"],"asset_ids":[bat, moth]}),
    );
    op(
        w,
        json!({"operation":"smart_folder_create","name":"Rated","rules":{"match":"all","rules":[{"field":"rating","op":"gte","value":4}]}}),
    );
    replay(reference.path());

    // Joins midway: snapshot of the current state, nothing replayed.
    let joined = tempfile::tempdir().unwrap();
    let attached = authority.attach(joined.path(), "r");
    assert_eq!(attached["sync"]["snapshot"], true);
    assert_eq!(attached["sync"]["applied"], 3);

    let wolf = ingest(w, "wolf", 17, &["fur"]);
    op(
        w,
        json!({"operation":"update","asset_ids":[owl, wolf],"metadata":{"rating":5,"add_tags":["eyes"]}}),
    );
    op(w, json!({"operation":"remove","asset_ids":[moth]}));
    op(w, json!({"operation":"empty_trash"}));
    op(w, json!({"operation":"remove","asset_ids":[bat]}));
    op(w, json!({"operation":"tag_rename","from":"night","to":"nocturnal"}));
    op(
        w,
        json!({"operation":"collection_add","collection_id":folder["id"],"asset_ids":[wolf]}),
    );

    replay(reference.path());
    let synced = op(joined.path(), json!({"operation":"sync"}));
    assert_eq!(synced["snapshot"], false);
    assert!(synced["applied"].as_u64().unwrap() > 0);
    for (joined, reference) in dump(joined.path(), "r").into_iter().zip(dump(reference.path(), "r")) {
        assert_eq!(joined, reference);
    }

    // Thumbnails (owl, wolf, trashed bat; the moth is gone) download in the background.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while op(joined.path(), json!({"operation":"sync_status","context":"r"}))["pending_thumbnails"] != 0 {
        assert!(std::time::Instant::now() < deadline, "thumbnails never landed");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let updates = op(
        joined.path(),
        json!({"operation":"thumbnail_updates","context":"r","since":0}),
    );
    // (The moth's may have landed before it was deleted.)
    let ids = updates["asset_ids"].as_array().unwrap();
    for id in [&bat, &owl, &wolf] {
        assert!(ids.contains(&json!(id)), "no thumbnail update for {id}");
    }
    let thumbnail = op(
        joined.path(),
        json!({"operation":"representation","context":"r","asset_id":owl,"kind":"thumbnail"}),
    );
    assert_eq!(std::fs::read(thumbnail["path"].as_str().unwrap()).unwrap(), b"x");
}

/// An attach interrupted before its catalog landed resumes on the next attach/sync, and a
/// stale mirror re-bootstraps to the authority's state.
#[test]
fn interrupted_attach_resumes() {
    let authority = Authority::start();
    let writer = tempfile::tempdir().unwrap();
    authority.attach(writer.path(), "w");
    let w = writer.path();
    let keep = ingest(w, "keep", 19, &["a"]);
    let gone = ingest(w, "gone", 23, &["b"]);

    // Context registered, catalog never synced (the app was quit mid-attach).
    let home = tempfile::tempdir().unwrap();
    remote::attach_remote(home.path(), "lib", &authority.url, &authority.token).unwrap();
    let resumed = authority.attach(home.path(), "lib");
    assert_eq!(resumed["sync"]["snapshot"], true);
    assert_eq!(
        op(home.path(), json!({"operation":"browse","context":"lib"}))["total"],
        2
    );
    // Same name, different authority: still refused.
    let other = Authority::start();
    let mut clash = json!({"operation":"remote_attach","name":"lib","remote_url":other.url,"token":other.token});
    clash["home"] = json!(home.path());
    assert!(native::request(serde_json::from_value(clash).unwrap()).is_err());

    // A mirror that lost its cursor (or lags far behind) is rebuilt from a snapshot,
    // dropping what the authority deleted meanwhile.
    op(w, json!({"operation":"remove","asset_ids":[gone]}));
    op(w, json!({"operation":"empty_trash"}));
    let library = Library::open(home.path(), Some("lib")).unwrap();
    let conn = rusqlite::Connection::open(library.state_dir.join("catalog.sqlite3")).unwrap();
    conn.execute("UPDATE replica_state SET value=0 WHERE key='cursor'", [])
        .unwrap();
    let synced = op(home.path(), json!({"operation":"sync","context":"lib"}));
    assert_eq!(synced["snapshot"], true);
    let browse = op(home.path(), json!({"operation":"browse","context":"lib"}));
    assert_eq!(browse["total"], 1);
    assert_eq!(browse["items"][0]["id"], keep.as_str());
    let deleted: i64 = conn
        .query_row("SELECT deleted FROM assets WHERE id=?1", [&gone], |r| r.get(0))
        .unwrap();
    assert_eq!(deleted, 1);
}
