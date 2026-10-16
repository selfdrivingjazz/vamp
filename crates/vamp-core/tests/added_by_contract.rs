//! `added_by`: every asset records the actor of its `media_added` event (set on every add
//! path, backfilled once from events on older catalogs), browse filters on it, and the
//! sidebar counts the owner's assets against each agent's.
#![cfg(feature = "native")]
use base64::Engine as _;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;
use vamp_core::explore::{Content, Finding};
use vamp_core::native::{Request, request};
use vamp_core::{Library, added_by, is_agent, with_actor};
use vamp_protocol::Metadata;

fn call(home: &Path, mut value: Value) -> anyhow::Result<Value> {
    value["home"] = json!(home);
    let parsed: Request = serde_json::from_value(value)?;
    request(parsed)
}
fn ok(home: &Path, value: Value) -> Value {
    call(home, value.clone()).unwrap_or_else(|e| panic!("{value}: {e:#}"))
}
fn as_actor<R>(actor: &str, f: impl FnOnce() -> R) -> R {
    with_actor(Some(actor.into()), f)
}
fn png(seed: u8) -> Vec<u8> {
    let image = image::RgbImage::from_pixel(8, 8, image::Rgb([seed, seed / 2, 255 - seed]));
    let mut out = std::io::Cursor::new(Vec::new());
    image.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}
fn file(dir: &Path, name: &str, seed: u8) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, png(seed)).unwrap();
    path
}
fn ids(page: &Value) -> HashSet<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().to_owned())
        .collect()
}
fn set<const N: usize>(ids: [&str; N]) -> HashSet<String> {
    ids.into_iter().map(str::to_owned).collect()
}

struct Env {
    dir: TempDir,
    home: PathBuf,
}
impl Env {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let home = dir.path().join("home");
        Library::init(&home, "main", None, None).unwrap();
        Self { dir, home }
    }
    fn library(&self) -> Library {
        Library::open(&self.home, None).unwrap()
    }
    fn originals(&self) -> PathBuf {
        let path = self.dir.path().join("originals");
        fs::create_dir_all(&path).unwrap();
        path
    }
    fn catalog(&self) -> rusqlite::Connection {
        let state = self.library().state_dir.clone();
        rusqlite::Connection::open(state.join("catalog.sqlite3")).unwrap()
    }
}

#[test]
fn every_add_path_records_its_actor() {
    let env = Env::new();
    let originals = env.originals();
    let mut lib = env.library();

    // Library::add / ingest_bytes (CLI add/ingest, server ingest and clips).
    let owner = as_actor("local", || lib.add(&file(&originals, "a.png", 1), None, Metadata::default()))
        .unwrap();
    assert_eq!(added_by(&owner), Some("local"));
    let mut spoofed = Metadata::default();
    spoofed.extra.insert("added_by".into(), json!("local"));
    let agent = as_actor("agent:alpha", || lib.ingest_bytes(&png(2), "b.png", spoofed)).unwrap();
    assert_eq!(added_by(&agent), Some("agent:alpha"), "the caller's metadata cannot claim another adder");
    // Adding an original that is already cataloged keeps its first adder.
    let again = as_actor("agent:beta", || lib.add(Path::new(&owner.original.path), None, Metadata::default()))
        .unwrap();
    assert_eq!((again.id.as_str(), added_by(&again)), (owner.id.as_str(), Some("local")));

    // Native add/ingest (the Mac app; vamp-server attributes tailnet peers this way).
    let added = as_actor("tailnet:alice@example.com/laptop", || {
        ok(&env.home, json!({"operation":"add","paths":[file(&originals, "c.png", 3)]}))
    });
    let tailnet = added["assets"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(added["assets"][0]["metadata"]["extra"]["added_by"], "tailnet:alice@example.com/laptop");
    let encoded = base64::engine::general_purpose::STANDARD.encode(png(4));
    let ingested = as_actor("agent:beta", || {
        ok(&env.home, json!({"operation":"ingest","bytes_base64":encoded,"filename":"d.png"}))
    });
    assert_eq!(ingested["metadata"]["extra"]["added_by"], "agent:beta");

    // Explorations (agents only).
    let finding = as_actor("agent:gamma", || {
        lib.explore_ensure("study", Some("Study"), None).unwrap();
        lib.explore_add(
            "study",
            Finding {
                name: "Some studio".into(),
                note: None,
                tags: vec![],
                url: None,
                folder: None,
                content: Content::Card { kind: "studio".into() },
            },
            None,
        )
        .unwrap()
    });
    let card = finding["asset"]["id"].as_str().unwrap().to_owned();
    assert_eq!(added_by(&lib.inspect(&card).unwrap().asset), Some("agent:gamma"));

    // Cells and the inspector carry it.
    let items = ok(&env.home, json!({"operation":"items","asset_ids":[owner.id, card]}));
    assert_eq!(items["items"][0]["added_by"], "local");
    assert_eq!(items["items"][1]["added_by"], "agent:gamma");
    let inspected = ok(&env.home, json!({"operation":"inspect","asset_id":tailnet}));
    assert_eq!(inspected["asset"]["added_by"], "tailnet:alice@example.com/laptop");
    // Updates keep it.
    let renamed = lib
        .update(&agent.id, vamp_protocol::MetadataPatch { name: Some("renamed".into()), ..Default::default() }, None)
        .unwrap();
    assert_eq!(added_by(&renamed), Some("agent:alpha"));
}

#[test]
fn eagle_import_records_the_importer() {
    let env = Env::new();
    let library = env.dir.path().join("Fixture.library");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/eagle-fixture.py");
    let status = Command::new("python3")
        .arg(&script)
        .arg(&library)
        .args(["--items", "6", "--seed", "7"])
        .status()
        .expect("python3 is required for Eagle fixtures");
    assert!(status.success());
    as_actor("local", || ok(&env.home, json!({"operation":"import_eagle","path":library})));
    let page = ok(&env.home, json!({"operation":"browse","limit":100,"trash":false}));
    let items = page["items"].as_array().unwrap();
    assert!(!items.is_empty());
    assert!(items.iter().all(|i| i["added_by"] == "local"), "{page}");
    // A re-sync (an update) keeps the original importer even when run by someone else.
    as_actor("agent:sync", || ok(&env.home, json!({"operation":"import_eagle","path":library})));
    let page = ok(&env.home, json!({"operation":"browse","limit":100,"added_by":"owner"}));
    assert_eq!(page["total"], items.len());
}

/// Owner: `local` + a tailnet write; agents: two `alpha`, one `beta` (one more `beta`
/// trashed). Returns the ids `[local, tailnet, alpha1, alpha2, beta]`.
fn mixed(env: &Env) -> [String; 5] {
    let originals = env.originals();
    let mut lib = env.library();
    let mut add = |actor: &str, name: &str, seed: u8| {
        as_actor(actor, || lib.add(&file(&originals, name, seed), None, Metadata::default()))
            .unwrap()
            .id
    };
    let ids = [
        add("local", "sunset harbor.png", 10),
        add("tailnet:alice@example.com/laptop", "harbor crane.png", 11),
        add("agent:alpha", "harbor study.png", 12),
        add("agent:alpha", "type specimen.png", 13),
        add("agent:beta", "poster.png", 14),
    ];
    let trashed = add("agent:beta", "discarded.png", 15);
    lib.trash(&[trashed], None).unwrap();
    ids
}

#[test]
fn browse_filters_and_sidebar_counts_by_adder() {
    let env = Env::new();
    let [local, tailnet, alpha1, alpha2, beta] = mixed(&env);
    let browse = |extra: Value| {
        let mut body = json!({"operation":"browse","limit":100});
        body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        ok(&env.home, body)
    };
    assert_eq!(ids(&browse(json!({"added_by":"owner"}))), set([&local, &tailnet]));
    assert_eq!(ids(&browse(json!({"added_by":"agents"}))), set([&alpha1, &alpha2, &beta]));
    assert_eq!(ids(&browse(json!({"added_by":"actor:agent:alpha"}))), set([&alpha1, &alpha2]));
    assert_eq!(ids(&browse(json!({"added_by":"actor:local"}))), set([&local]));
    assert_eq!(browse(json!({}))["total"], 5);
    // Combined with full-text, folders and sorting; layout lists the same assets.
    assert_eq!(ids(&browse(json!({"query":"harbor","added_by":"owner"}))), set([&local, &tailnet]));
    assert_eq!(ids(&browse(json!({"query":"harbor","added_by":"agents"}))), set([&alpha1]));
    let mut lib = env.library();
    let folder = lib.collection_create("Study", None).unwrap();
    for id in [&local, &alpha1] {
        lib.collection_membership(&folder.id, id, true).unwrap();
    }
    // A folder scope is not filtered by adder unless asked.
    assert_eq!(ids(&browse(json!({"folder_ids":[folder.id]}))), set([&local, &alpha1]));
    assert_eq!(ids(&browse(json!({"folder_ids":[folder.id],"added_by":"owner"}))), set([&local]));
    let sorted = browse(json!({"added_by":"agents","sort":"name_asc"}));
    let names: Vec<&str> = sorted["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["harbor study.png", "poster.png", "type specimen.png"]);
    let layout = ok(&env.home, json!({"operation":"layout","added_by":"agents"}));
    assert_eq!(layout["ids"].as_array().unwrap().len(), 3);
    // Trash browses filter too.
    assert_eq!(browse(json!({"trash":true,"added_by":"actor:agent:beta"}))["total"], 1);
    for bad in ["me", "actor:", "agent:alpha"] {
        assert!(call(&env.home, json!({"operation":"browse","added_by":bad})).is_err(), "{bad}");
    }

    // Library::list (CLI media list/search --added-by).
    let listed: HashSet<String> = lib.list(None, 100, 0, Some("agents")).unwrap().into_iter().map(|a| a.id).collect();
    assert_eq!(listed, set([&alpha1, &alpha2, &beta]));
    let searched: Vec<String> = lib.list(Some("harbor"), 100, 0, Some("owner")).unwrap().into_iter().map(|a| a.id).collect();
    assert_eq!(searched.len(), 2);

    let sidebar = ok(&env.home, json!({"operation":"sidebar"}));
    assert_eq!(
        sidebar["added_by"],
        json!({"owner":2,"agents":3,"by_agent":[{"actor":"agent:alpha","count":2},{"actor":"agent:beta","count":1}]})
    );
    assert_eq!(sidebar["all"], 5);
    assert!(is_agent("agent:x") && !is_agent("local") && !is_agent("tailnet:agent:x/h"));
}

#[test]
fn older_catalogs_backfill_from_events_once() {
    let env = Env::new();
    let [local, tailnet, alpha1, _, beta] = mixed(&env);
    // Make it look like a catalog from before `added_by`: no field, no marker, no index.
    {
        let conn = env.catalog();
        conn.execute_batch(
            "UPDATE assets SET document=json_remove(document,'$.metadata.extra.added_by');
             DELETE FROM settings WHERE key='added_by_backfill';
             DROP INDEX assets_added_by;",
        )
        .unwrap();
        let missing: i64 = conn
            .query_row("SELECT count(*) FROM assets WHERE added_by IS NULL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(missing, 6);
    }
    let lib = env.library();
    for (id, actor) in [(&local, "local"), (&tailnet, "tailnet:alice@example.com/laptop"), (&alpha1, "agent:alpha"), (&beta, "agent:beta")] {
        assert_eq!(added_by(&lib.inspect(id).unwrap().asset), Some(actor));
    }
    drop(lib);
    let conn = env.catalog();
    let (missing, marker, indexed): (i64, String, bool) = conn
        .query_row(
            "SELECT (SELECT count(*) FROM assets WHERE added_by IS NULL), (SELECT document FROM settings WHERE key='added_by_backfill'), EXISTS(SELECT 1 FROM sqlite_master WHERE name='assets_added_by')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(missing, 0);
    assert_eq!(serde_json::from_str::<Value>(&marker).unwrap()["assets"], 6);
    assert!(indexed);
    let documents = |conn: &rusqlite::Connection| -> Vec<String> {
        let mut stmt = conn.prepare("SELECT document FROM assets ORDER BY id").unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
    };
    let before = documents(&conn);
    // Once: reopening does not run it again.
    drop(env.library());
    assert_eq!(documents(&conn), before);
    let sidebar = ok(&env.home, json!({"operation":"sidebar"}));
    assert_eq!(sidebar["added_by"]["owner"], 2);
    assert_eq!(sidebar["added_by"]["agents"], 3);
}
