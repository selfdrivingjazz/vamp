//! Library import/sync over a generated source library (tools/library-fixture.py). Originals are
//! made unreadable (chmod 000) to stand in for Google Drive placeholders: any attempt to read
//! one during import fails the import.
#![cfg(feature = "native")]
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;
use vamp_core::Library;
use vamp_core::native::{Request, request};

fn fixture(dir: &Path, items: usize, unreadable: bool) -> PathBuf {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/library-fixture.py");
    let library = dir.join("Fixture.library");
    let mut command = Command::new("python3");
    command
        .arg(&script)
        .arg(&library)
        .args(["--items", &items.to_string(), "--seed", "7"]);
    if unreadable {
        command.arg("--unreadable");
    }
    let status = command
        .status()
        .expect("python3 is required for library fixtures");
    assert!(status.success());
    library
}
fn mutate(library: &Path) -> Value {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/library-fixture.py");
    let output = Command::new("python3")
        .arg(&script)
        .arg(library)
        .args(["--mutate", "--seed", "3", "--count", "12"])
        .output()
        .unwrap();
    assert!(output.status.success());
    serde_json::from_slice(&output.stdout).unwrap()
}
/// Eagle item metadata as written by the fixture (only metadata.json files are read).
fn items(library: &Path) -> Vec<Value> {
    let mut items: Vec<Value> = fs::read_dir(library.join("images"))
        .unwrap()
        .map(|e| {
            serde_json::from_slice(&fs::read(e.unwrap().path().join("metadata.json")).unwrap())
                .unwrap()
        })
        .collect();
    items.sort_by_key(|i| i["id"].as_str().unwrap().to_owned());
    items
}

struct Env {
    _dir: TempDir,
    home: PathBuf,
    library: PathBuf,
}
impl Env {
    fn new(items: usize, unreadable: bool) -> Self {
        let dir = TempDir::new().unwrap();
        let library = fixture(dir.path(), items, unreadable);
        let home = dir.path().join("home");
        Library::init(&home, "main", None, None).unwrap();
        Self {
            _dir: dir,
            home,
            library,
        }
    }
    fn call(&self, mut value: Value) -> Value {
        value["home"] = json!(self.home);
        let parsed: Request = serde_json::from_value(value.clone()).unwrap();
        request(parsed).unwrap_or_else(|e| panic!("{value}: {e:#}"))
    }
    /// Phase A only (catalog rows; thumbnails queued).
    fn import_metadata(&self) -> Value {
        self.call(json!({"operation":"import_eagle","path":self.library}))
    }
    /// Phase A, then phase B until the thumbnail queue is empty.
    fn import(&self) -> Value {
        let report = self.import_metadata();
        let copied = self.call(json!({"operation":"copy_thumbnails"}));
        assert_eq!(copied["remaining"], 0, "{copied}");
        report
    }
    fn browse(&self, mut filter: Value) -> Value {
        filter["operation"] = json!("browse");
        if filter.get("limit").is_none() {
            filter["limit"] = json!(500);
        }
        self.call(filter)
    }
    fn names(&self, filter: Value) -> HashSet<String> {
        self.browse(filter)["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["name"].as_str().unwrap().to_owned())
            .collect()
    }
    /// The asset with this name, trashed or not.
    fn find(&self, name: &str) -> Value {
        [false, true]
            .into_iter()
            .flat_map(|trash| {
                self.browse(json!({"query":name,"trash":trash}))["items"]
                    .as_array()
                    .unwrap()
                    .clone()
            })
            .find(|i| i["name"] == name)
            .unwrap_or_else(|| panic!("no asset named {name}"))
    }
}
fn live(items: &[Value]) -> Vec<&Value> {
    items.iter().filter(|i| i["isDeleted"] != true).collect()
}
fn expected(items: &[Value], keep: impl Fn(&Value) -> bool) -> HashSet<String> {
    live(items)
        .into_iter()
        .filter(|i| keep(i))
        .map(|i| i["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn import_never_reads_originals_and_maps_eagle_metadata() {
    let env = Env::new(80, true);
    let source = items(&env.library);
    let red = source.iter().find(|i| i["name"] == "red-square").unwrap();
    let red_dir = env
        .library
        .join(format!("images/{}.info", red["id"].as_str().unwrap()));
    // The stand-in placeholders really are unreadable (not running as root).
    assert!(fs::File::open(red_dir.join("red-square.png")).is_err());

    let report = env.import();
    assert_eq!(report["created"], 80, "{report}");
    assert_eq!(report["skipped"], json!([]));
    assert_eq!(report["read_originals"], 0);
    let thumbnails = source
        .iter()
        .filter(|i| {
            env.library
                .join(format!(
                    "images/{}.info/{}_thumbnail.png",
                    i["id"].as_str().unwrap(),
                    i["name"].as_str().unwrap()
                ))
                .exists()
        })
        .count();
    assert_eq!(report["queued_thumbnails"], thumbnails);
    assert_eq!(report["missing_thumbnails"], 80 - thumbnails);

    let item = env.find("red-square");
    assert_eq!(item["ext"], "png");
    assert_eq!(item["rating"], 5);
    assert_eq!(item["tags"], json!(["primary", "red"]));
    assert_eq!(item["colors"][0]["hex"], "#ff0000");
    assert_eq!(item["colors"][0]["ratio"], 0.9);
    assert_eq!(
        (item["width"].clone(), item["height"].clone()),
        (json!(512), json!(512))
    );
    assert_eq!(item["hash_state"], "pending");
    assert_eq!(item["available_locally"]["thumbnail"], true);
    assert!(
        Path::new(item["thumbnail_path"].as_str().unwrap())
            .starts_with(env.home.canonicalize().unwrap())
    );
    assert_eq!(
        item["original_created_at"],
        chrono_ms(red["btime"].as_i64().unwrap())
    );
    // Folders: memberships and nesting mirror Eagle's tree.
    let inspected = env.call(json!({"operation":"inspect","asset_id":item["id"]}));
    assert_eq!(
        inspected["folder_ids"].as_array().unwrap().len(),
        red["folders"].as_array().unwrap().len()
    );
    assert_eq!(inspected["hash_state"], "pending");
    assert_eq!(inspected["blake3"], Value::Null);
    let sidebar = env.call(json!({"operation":"sidebar"}));
    fn depth(nodes: &Value) -> usize {
        nodes
            .as_array()
            .unwrap()
            .iter()
            .map(|n| 1 + depth(&n["children"]))
            .max()
            .unwrap_or(0)
    }
    assert!(depth(&sidebar["folders"]) >= 3, "{sidebar}");

    // Sidebar counts agree with the Eagle source.
    let alive = live(&source);
    assert_eq!(sidebar["all"], alive.len());
    assert_eq!(sidebar["trash"], source.len() - alive.len());
    assert!(sidebar["trash"].as_u64().unwrap() > 0);
    assert_eq!(
        sidebar["untagged"],
        alive.iter().filter(|i| i["tags"] == json!([])).count()
    );
    assert_eq!(
        sidebar["uncategorized"],
        alive.iter().filter(|i| i["folders"] == json!([])).count()
    );
    let tags: HashSet<&str> = alive
        .iter()
        .flat_map(|i| i["tags"].as_array().unwrap())
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(sidebar["tags_total"], tags.len());
    assert_eq!(sidebar["recently_used"], 0);

    // Smart folders: translated rules evaluate; untranslatable ones are kept aside.
    let smart = env.call(json!({"operation":"smart_folders"}));
    let smart = smart.as_array().unwrap();
    assert_eq!(
        smart.len(),
        report["smart_folders"].as_u64().unwrap() as usize
    );
    assert!(
        smart
            .iter()
            .any(|f| !f["unsupported"].as_array().is_none_or(Vec::is_empty))
    );
    let top = smart.iter().find(|f| f["name"] == "Top PNGs").unwrap();
    let want = expected(&source, |i| {
        i["star"].as_i64().unwrap_or(0) >= 4 && i["ext"] == "png"
    });
    assert_eq!(top["count"], want.len());
    assert_eq!(env.names(json!({"smart_folder_id":top["id"]})), want);

    // Tag groups and starred tags.
    let tags = env.call(json!({"operation":"tags"}));
    assert_eq!(
        tags["groups"].as_array().unwrap().len(),
        report["tag_groups"].as_u64().unwrap() as usize
    );

    // Hashing must read originals: unreadable placeholders fail and stay pending.
    let hashed = env.call(json!({"operation":"hash","limit":5}));
    assert_eq!(hashed["hashed"], 0);
    assert_eq!(hashed["failed"].as_array().unwrap().len(), 5);
    assert_eq!(env.find("red-square")["hash_state"], "pending");
}

fn chrono_ms(ms: i64) -> Value {
    // RFC 3339 UTC as the catalog stores it (seconds precision when whole). Computed here, not
    // with `date -r SECONDS`, which is BSD-only (GNU date reads a file's mtime instead).
    json!(chrono::DateTime::from_timestamp(ms / 1000, 0).unwrap().to_rfc3339())
}

#[test]
fn browse_filters_match_eagle_source() {
    let env = Env::new(120, true);
    env.import();
    let source = items(&env.library);
    let dims = |i: &Value| (i["width"].as_f64().unwrap(), i["height"].as_f64().unwrap());
    let shape = |i: &Value| {
        let (w, h) = dims(i);
        if w >= 2.0 * h || h >= 2.0 * w {
            "panorama"
        } else if (w - h).abs() <= 0.05 * w.max(h) {
            "square"
        } else if w > h {
            "landscape"
        } else {
            "portrait"
        }
    };
    for name in ["landscape", "portrait", "square", "panorama"] {
        let want = expected(&source, |i| shape(i) == name);
        assert!(!want.is_empty(), "fixture lacks {name}");
        assert_eq!(env.names(json!({"shape":name})), want, "shape {name}");
    }
    let want = expected(&source, |i| i["star"].as_i64().unwrap_or(0) >= 3);
    assert_eq!(env.names(json!({"rating_min":3})), want);
    let want = expected(&source, |i| i["ext"] == "pdf" || i["ext"] == "mp4");
    assert!(!want.is_empty());
    assert_eq!(env.names(json!({"exts":["PDF","mp4"]})), want);
    // Dates: original_created_at from Eagle btime; a bound in the middle splits the set.
    let mut times: Vec<i64> = live(&source)
        .iter()
        .map(|i| i["btime"].as_i64().unwrap())
        .collect();
    times.sort();
    let pivot = times[times.len() / 2];
    let from = chrono_ms(pivot);
    let want = expected(&source, |i| {
        i["btime"].as_i64().unwrap() / 1000 >= pivot / 1000
    });
    assert_eq!(
        env.names(json!({"date_from":from,"date_field":"created"})),
        want
    );
    // Tags any vs all, and composition with other filters.
    let any = expected(&source, |i| {
        let t = i["tags"].as_array().unwrap();
        t.contains(&json!("red")) || t.contains(&json!("blue"))
    });
    assert_eq!(env.names(json!({"tags_any":["red","blue"]})), any);
    let both = expected(&source, |i| {
        let t = i["tags"].as_array().unwrap();
        t.contains(&json!("red")) && t.contains(&json!("primary")) && i["ext"] == "png"
    });
    assert_eq!(
        env.names(json!({"tags":["red","primary"],"exts":["png"]})),
        both
    );

    // Color: #ff0000 ranks red-dominant items first, red-square at distance ~0.
    let red = env.browse(json!({"colors":["#ff0000"],"limit":50}));
    let found = red["items"].as_array().unwrap();
    assert!(!found.is_empty());
    let distances: Vec<f64> = found
        .iter()
        .map(|i| i["color_distance"].as_f64().unwrap())
        .collect();
    assert!(distances.windows(2).all(|w| w[0] <= w[1]), "{distances:?}");
    assert!(distances.iter().all(|d| *d <= 25.0));
    let square = found.iter().find(|i| i["name"] == "red-square").unwrap();
    assert!(square["color_distance"].as_f64().unwrap() < 3.0);
    assert_eq!(found[0]["colors"][0]["hex"], "#ff0000");
    // A blue query does not return the red square.
    assert!(
        !env.names(json!({"colors":["#0000ff"]}))
            .contains("red-square")
    );

    // Random sort is a stable permutation per seed.
    let ids = |seed: i64| -> Vec<Value> {
        env.browse(json!({"sort":"random","seed":seed,"limit":30}))["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].clone())
            .collect()
    };
    assert_eq!(ids(1), ids(1));
    assert_ne!(ids(1), ids(2));
    // Size bounds are inclusive on the Eagle-reported file size (stat of the reference).
    let mut sizes: Vec<u64> = live(&source)
        .iter()
        .map(|i| i["size"].as_u64().unwrap())
        .collect();
    sizes.sort();
    let (low, high) = (sizes[sizes.len() / 4], sizes[sizes.len() * 3 / 4]);
    assert!(low < high);
    let want = expected(&source, |i| {
        (low..=high).contains(&i["size"].as_u64().unwrap())
    });
    assert_eq!(env.names(json!({"size_min":low,"size_max":high})), want);
    assert_eq!(
        env.names(json!({"size_min":high + 1})),
        expected(&source, |i| i["size"].as_u64().unwrap() > high)
    );
    let ratings: Vec<i64> = env.browse(json!({"sort":"rating_desc"}))["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["rating"].as_i64().unwrap())
        .collect();
    assert!(ratings.windows(2).all(|w| w[0] >= w[1]));
}

#[test]
fn background_import_reports_progress_until_finished() {
    let env = Env::new(1200, true);
    let started = env.call(json!({"operation":"import_eagle_start","path":env.library}));
    let job = started["job_id"].clone();
    let status = loop {
        let status = env.call(json!({"operation":"import_status","job_id":job}));
        assert!(
            status["done"].as_u64() <= status["total"].as_u64(),
            "{status}"
        );
        if status["finished"] == true {
            break status;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    assert_eq!(status["error"], Value::Null, "{status}");
    assert_eq!(status["phase"], "done");
    assert_eq!(status["created"], 1200);
    assert_eq!(
        (status["done"].clone(), status["total"].clone()),
        (json!(1200), json!(1200))
    );
    assert_eq!(status["result"]["created"], 1200);
    assert_eq!(status["result"]["read_originals"], 0);
    assert_eq!(
        env.call(json!({"operation":"sidebar"}))["all"],
        live(&items(&env.library)).len()
    );
    // An unknown id is an error, not an empty status.
    let missing: Request = serde_json::from_value(
        json!({"operation":"import_status","job_id":"nope","home":env.home}),
    )
    .unwrap();
    assert!(request(missing).is_err());
    // Import failures surface through the status, not the start call.
    let bad =
        env.call(json!({"operation":"import_eagle_start","path":env.home.join("absent.library")}));
    let failed = loop {
        let status = env.call(json!({"operation":"import_status","job_id":bad["job_id"]}));
        if status["finished"] == true {
            break status;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    assert!(
        failed["error"]
            .as_str()
            .unwrap()
            .contains("import library unavailable")
    );
}

#[test]
fn trash_restore_touch_and_folder_moves() {
    let env = Env::new(30, true);
    env.import();
    let square = env.find("red-square");
    let id = square["id"].clone();
    let before = env.call(json!({"operation":"sidebar"}));
    assert_eq!(
        env.call(json!({"operation":"remove","asset_ids":[id]}))["trashed"],
        1
    );
    assert!(!env.names(json!({})).contains("red-square"));
    assert!(env.names(json!({"trash":true})).contains("red-square"));
    let trashed = env.call(json!({"operation":"sidebar"}));
    assert_eq!(trashed["trash"], before["trash"].as_u64().unwrap() + 1);
    assert_eq!(trashed["all"], before["all"].as_u64().unwrap() - 1);
    assert_eq!(
        env.call(json!({"operation":"restore","asset_ids":[id]}))["restored"],
        1
    );
    assert_eq!(env.call(json!({"operation":"sidebar"})), before);

    env.call(json!({"operation":"touch","asset_id":id}));
    assert_eq!(
        env.names(json!({"recently_used":true})),
        HashSet::from(["red-square".to_owned()])
    );
    let viewed = env.browse(json!({"sort":"last_viewed_desc","limit":1}));
    assert_eq!(viewed["items"][0]["id"], id);

    // Folder move: nest a root folder under another; subfolder browse includes it.
    let folders = env.call(json!({"operation":"collections"}));
    let roots: Vec<&Value> = folders
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["parent_id"].is_null())
        .collect();
    let (a, b) = (roots[0]["id"].clone(), roots[1]["id"].clone());
    let in_b = env.browse(json!({"collection_id":b}))["total"]
        .as_u64()
        .unwrap();
    let a_tree = env.browse(json!({"collection_id":a,"include_subfolders":true}))["total"]
        .as_u64()
        .unwrap();
    env.call(json!({"operation":"collection_move","collection_id":b,"parent_id":a,"index":0}));
    let moved = env.call(json!({"operation":"collections"}));
    let b_row = moved
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == b)
        .unwrap();
    assert_eq!(b_row["parent_id"], a);
    assert_eq!(b_row["sort_order"], 0);
    let widened = env.browse(json!({"collection_id":a,"include_subfolders":true}))["total"]
        .as_u64()
        .unwrap();
    assert!(widened >= a_tree.max(in_b));
    // Cycles are rejected.
    let parsed: Request = serde_json::from_value(
        json!({"operation":"collection_move","collection_id":a,"parent_id":b,"home":env.home}),
    )
    .unwrap();
    assert!(request(parsed).is_err());

    // Tag rename merges; delete removes everywhere.
    let renamed = env.call(json!({"operation":"tag_rename","from":"red","to":"primary"}));
    assert!(renamed["updated"].as_u64().unwrap() >= 1);
    assert_eq!(env.find("red-square")["tags"], json!(["primary"]));
    env.call(json!({"operation":"tag_delete","tag":"primary"}));
    assert_eq!(env.find("red-square")["tags"], json!([]));

    // Empty trash tombstones only trashed assets.
    let trash = env.call(json!({"operation":"sidebar"}))["trash"].clone();
    assert_eq!(
        env.call(json!({"operation":"empty_trash"}))["deleted"],
        trash
    );
    assert_eq!(env.call(json!({"operation":"sidebar"}))["trash"], 0);
}

#[test]
fn resync_applies_eagle_edits_idempotently() {
    let env = Env::new(60, true);
    env.import();
    let folders_before = env.call(json!({"operation":"collections"}));
    let changes = mutate(&env.library);
    let changed = changes["changed"].as_array().unwrap();
    let deleted = changes["deleted"].as_array().unwrap();
    let added = changes["added"].as_array().unwrap();
    let removed_folders = changes["removed_folders"].as_array().unwrap();
    assert!(!changed.is_empty() && !deleted.is_empty() && !added.is_empty());

    let report = env.import();
    assert_eq!(report["created"], added.len(), "{report}");
    assert_eq!(report["updated"], changed.len() + deleted.len(), "{report}");
    assert_eq!(report["read_originals"], 0);
    assert_eq!(report["folders"]["removed"], removed_folders.len());
    let source = items(&env.library);
    // Edited metadata (tags/star/annotation) now matches Eagle.
    for eid in changed {
        let item = source.iter().find(|i| &i["id"] == eid).unwrap();
        let asset = env.find(item["name"].as_str().unwrap());
        let mut tags: Vec<String> = item["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_owned())
            .collect();
        tags.sort();
        tags.dedup();
        assert_eq!(asset["tags"], json!(tags), "{eid}");
        assert_eq!(asset["rating"], item["star"].as_i64().unwrap_or(0));
        assert_eq!(asset["revision"], 2);
    }
    // Eagle deletions are in the trash.
    let trash = env.names(json!({"trash":true}));
    for eid in deleted {
        let item = source.iter().find(|i| &i["id"] == eid).unwrap();
        assert!(trash.contains(item["name"].as_str().unwrap()));
    }
    // Removed Eagle folders are gone; the rest keep their VAMP ids.
    let after = env.call(json!({"operation":"collections"}));
    assert_eq!(
        after.as_array().unwrap().len(),
        folders_before.as_array().unwrap().len() - removed_folders.len()
    );
    for folder in removed_folders {
        let source_id = format!("eagle:{}", folder.as_str().unwrap());
        assert!(
            after
                .as_array()
                .unwrap()
                .iter()
                .all(|f| f["source_id"] != json!(source_id))
        );
    }
    // Idempotent: nothing changes on a second sync.
    let events = Library::open(&env.home, None)
        .unwrap()
        .events(0, 1000)
        .unwrap()
        .len();
    let again = env.import();
    assert_eq!(
        (
            again["created"].clone(),
            again["updated"].clone(),
            again["trashed"].clone()
        ),
        (json!(0), json!(0), json!(0)),
        "{again}"
    );
    assert_eq!(again["unchanged"], source.len());
    assert_eq!(
        Library::open(&env.home, None)
            .unwrap()
            .events(0, 1000)
            .unwrap()
            .len(),
        events
    );
    // Restoring in Eagle restores in VAMP.
    let eid = deleted[0].as_str().unwrap();
    let meta = env.library.join(format!("images/{eid}.info/metadata.json"));
    let mut raw: Value = serde_json::from_slice(&fs::read(&meta).unwrap()).unwrap();
    raw["isDeleted"] = json!(false);
    raw["lastModified"] = json!(raw["lastModified"].as_i64().unwrap() + 1);
    fs::write(&meta, serde_json::to_vec(&raw).unwrap()).unwrap();
    // Eagle mirrors lastModified into mtime.json.
    let index = env.library.join("mtime.json");
    let mut mtimes: Value = serde_json::from_slice(&fs::read(&index).unwrap()).unwrap();
    mtimes[eid] = raw["lastModified"].clone();
    fs::write(&index, serde_json::to_vec(&mtimes).unwrap()).unwrap();
    assert_eq!(env.import()["restored"], 1);
    assert!(env.names(json!({})).contains(raw["name"].as_str().unwrap()));
}

#[test]
fn pending_hash_resolves_on_first_read_and_stays_sound() {
    let env = Env::new(20, false);
    env.import();
    let item = env.find("red-square");
    let id = item["id"].clone();
    let thumbnail = item["thumbnail_path"].clone();
    assert_eq!(item["hash_state"], "pending");
    let inspected = env.call(json!({"operation":"inspect","asset_id":id}));
    let original = PathBuf::from(inspected["original_path"].as_str().unwrap());
    // First read (viewer opening the original) computes the digest...
    let opened = env.call(json!({"operation":"original","asset_id":id}));
    assert_eq!(opened["path"], json!(original));
    let inspected = env.call(json!({"operation":"inspect","asset_id":id}));
    assert_eq!(inspected["hash_state"], "hashed");
    assert_eq!(
        inspected["blake3"],
        blake3::hash(&fs::read(&original).unwrap())
            .to_hex()
            .as_str()
    );
    // ...without a revision bump, and the Eagle thumbnail stays current (rekeyed).
    let item = env.find("red-square");
    assert_eq!(item["revision"], 1);
    assert_eq!(item["thumbnail_path"], thumbnail);
    assert_eq!(item["available_locally"]["thumbnail"], true);
    // Dedupe: adding the same referenced file returns the same asset.
    let mut library = Library::open(&env.home, None).unwrap();
    let again = library.add(&original, None, Default::default()).unwrap();
    assert_eq!(json!(again.id), id);
    // A pending asset whose file changed since import is a refresh, not a silent rekey.
    let with_thumbnails: Vec<Value> = env.browse(json!({"exts":["png"]}))["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["name"] != "red-square" && i["available_locally"]["thumbnail"] == true)
        .cloned()
        .collect();
    let other = with_thumbnails[0].clone();
    let other_id = other["id"].as_str().unwrap().to_owned();
    let path = env.call(json!({"operation":"inspect","asset_id":other_id}))["original_path"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::write(&path, b"different bytes, different size").unwrap();
    let resolved = library.resolve_hash(&other_id).unwrap();
    assert_eq!(resolved.revision, 2);
    assert_eq!(resolved.original.hash_state, "hashed");
    assert!(
        library
            .inspect(&other_id)
            .unwrap()
            .representations
            .is_empty(),
        "stale Eagle thumbnail must not survive a content change"
    );
    // Adding a pending file resolves it in place rather than duplicating.
    let third = with_thumbnails[1].clone();
    let third_path =
        env.call(json!({"operation":"inspect","asset_id":third["id"]}))["original_path"]
            .as_str()
            .unwrap()
            .to_owned();
    let added = library
        .add(Path::new(&third_path), None, Default::default())
        .unwrap();
    assert_eq!(json!(added.id), third["id"]);
    assert_eq!(added.original.hash_state, "hashed");
    assert_eq!(added.revision, 1);
}

/// asset id → Eagle modificationTime, for every imported asset (live and trashed).
fn modification_times(env: &Env) -> std::collections::HashMap<String, i64> {
    let source = items(&env.library);
    [false, true]
        .into_iter()
        .flat_map(|trash| {
            env.browse(json!({"trash":trash}))["items"]
                .as_array()
                .unwrap()
                .clone()
        })
        .map(|asset| {
            let item = source.iter().find(|i| i["name"] == asset["name"]).unwrap();
            (
                asset["id"].as_str().unwrap().to_owned(),
                item["modificationTime"].as_i64().unwrap(),
            )
        })
        .collect()
}
fn thumbnail_flags(env: &Env) -> Vec<bool> {
    env.browse(json!({}))["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["available_locally"]["thumbnail"] == true)
        .collect()
}

#[test]
fn phase_a_queues_thumbnails_and_phase_b_fills_them_resumably() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new(60, true);
    let report = env.import_metadata();
    let queued = report["queued_thumbnails"].as_u64().unwrap();
    assert!(queued > 10, "{report}");
    // Items without an Eagle thumbnail are queued for generation from the original.
    assert!(report["queued_generate"].as_u64().unwrap() > 0, "{report}");
    let pending = queued + report["queued_generate"].as_u64().unwrap();
    assert_eq!(report["pending_thumbnails"], pending);
    // Items are browsable right away, all without thumbnails; nothing copied into state.
    let flags = thumbnail_flags(&env);
    assert!(!flags.is_empty() && flags.iter().all(|f| !f));
    let rep_dir = PathBuf::from(
        env.call(json!({"operation":"contexts"}))["contexts"][0]["state_dir"]
            .as_str()
            .unwrap(),
    )
    .join("representations/eagle");
    assert_eq!(fs::read_dir(&rep_dir).map_or(0, |d| d.count()), 0);
    assert_eq!(
        env.call(json!({"operation":"sidebar"}))["pending_thumbnails"],
        pending
    );
    let start = env.call(json!({"operation":"thumbnail_updates"}));
    assert_eq!(start["asset_ids"], json!([]));

    // One thumbnail is unreadable: it fails without blocking the rest.
    let broken = items(&env.library)
        .into_iter()
        .map(|i| {
            env.library.join(format!(
                "images/{}.info/{}_thumbnail.png",
                i["id"].as_str().unwrap(),
                i["name"].as_str().unwrap()
            ))
        })
        .find(|p| p.exists())
        .unwrap();
    fs::set_permissions(&broken, fs::Permissions::from_mode(0o000)).unwrap();

    // An interrupted phase B (limit) leaves a durable remainder another process resumes.
    let first = env.call(json!({"operation":"copy_thumbnails","limit":10}));
    assert_eq!(
        first["copied"].as_u64().unwrap() + first["failed"].as_u64().unwrap(),
        10
    );
    assert!(first["remaining"].as_u64().unwrap() >= queued - 10);
    let mut library = Library::open(&env.home, None).unwrap();
    let rest = library
        .copy_eagle_thumbnails(usize::MAX, &mut |_| {})
        .unwrap();
    assert_eq!(rest["remaining"], 0, "{rest}");
    let copied = first["copied"].as_u64().unwrap() + rest["copied"].as_u64().unwrap();
    assert_eq!(copied, queued - 1);
    // Every copied thumbnail is announced to pollers exactly once.
    let updates = env.call(json!({"operation":"thumbnail_updates","since":start["cursor"]}));
    assert_eq!(
        updates["asset_ids"].as_array().unwrap().len() as u64,
        copied
    );
    assert_eq!(updates["pending"], 0);
    let again = env.call(json!({"operation":"thumbnail_updates","since":updates["cursor"]}));
    assert_eq!(again["asset_ids"], json!([]));
    // Eagle's WebP-in-.png thumbnails are stored as WebP.
    let webp = env.browse(json!({}))["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| {
            i["thumbnail_path"]
                .as_str()
                .is_some_and(|p| p.ends_with(".webp"))
        })
        .count();
    assert!(webp > 0);
    assert_eq!(fs::read_dir(&rep_dir).unwrap().count() as u64, copied);
    let announced: HashSet<&str> = updates["asset_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    for item in env.browse(json!({}))["items"].as_array().unwrap() {
        assert_eq!(
            item["available_locally"]["thumbnail"] == true,
            announced.contains(item["id"].as_str().unwrap()),
            "{item}"
        );
    }
    // Resync of an unchanged library re-queues nothing.
    let resync = env.import_metadata();
    assert_eq!(resync["unchanged"], 60, "{resync}");
    assert_eq!(resync["queued_thumbnails"], 0);
}

#[test]
fn prioritized_thumbnails_copy_first_then_newest() {
    let env = Env::new(40, true);
    env.import_metadata();
    let times = modification_times(&env);
    let start = env.call(json!({"operation":"thumbnail_updates"}));
    let mut by_age: Vec<(&String, &i64)> = times.iter().collect();
    by_age.sort_by_key(|(_, t)| **t);
    // The three oldest assets that have an Eagle thumbnail to copy.
    let with_thumb: HashSet<String> = env.browse(json!({}))["items"]
        .as_array()
        .unwrap()
        .iter()
        .chain(
            env.browse(json!({"trash":true}))["items"]
                .as_array()
                .unwrap(),
        )
        .filter(|a| {
            let item = items(&env.library)
                .into_iter()
                .find(|i| i["name"] == a["name"])
                .unwrap();
            env.library
                .join(format!(
                    "images/{}.info/{}_thumbnail.png",
                    item["id"].as_str().unwrap(),
                    item["name"].as_str().unwrap()
                ))
                .exists()
        })
        .map(|a| a["id"].as_str().unwrap().to_owned())
        .collect();
    let wanted: Vec<String> = by_age
        .iter()
        .map(|(id, _)| (*id).clone())
        .filter(|id| with_thumb.contains(id))
        .take(3)
        .collect();
    let answer = env.call(json!({"operation":"prioritize_thumbnails","asset_ids":wanted}));
    let boosted = answer["queued"].as_u64().unwrap() as usize;
    assert!(boosted >= 1, "{answer}");
    // The bridge resumes phase B on its own after prioritize; wait for it to drain.
    let mut library = Library::open(&env.home, None).unwrap();
    loop {
        let r = library
            .copy_eagle_thumbnails(usize::MAX, &mut |_| {})
            .unwrap();
        if r["busy"] != true && r["remaining"] == 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let order: Vec<String> = env
        .call(json!({"operation":"thumbnail_updates","since":start["cursor"]}))["asset_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    let head: HashSet<&String> = order[..boosted].iter().collect();
    assert!(head.iter().all(|id| wanted.contains(id)), "{order:?}");
    let rest: Vec<i64> = order[boosted..].iter().map(|id| times[id]).collect();
    assert!(
        rest.windows(2).all(|w| w[0] >= w[1]),
        "newest first: {rest:?}"
    );
}

#[test]
fn resync_skips_items_unchanged_in_mtime_index() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new(30, true);
    env.import();
    // Unchanged per mtime.json: their metadata.json is never opened.
    let source = items(&env.library);
    let metas: Vec<PathBuf> = source
        .iter()
        .map(|i| {
            env.library.join(format!(
                "images/{}.info/metadata.json",
                i["id"].as_str().unwrap()
            ))
        })
        .collect();
    for meta in &metas[1..] {
        fs::set_permissions(meta, fs::Permissions::from_mode(0o000)).unwrap();
    }
    // ...while an item whose mtime.json entry moved is re-read and updated.
    let eid = source[0]["id"].as_str().unwrap();
    let mut raw = source[0].clone();
    raw["star"] = json!(1);
    raw["lastModified"] = json!(raw["lastModified"].as_i64().unwrap() + 5);
    fs::write(&metas[0], serde_json::to_vec(&raw).unwrap()).unwrap();
    let index = env.library.join("mtime.json");
    let mut mtimes: Value = serde_json::from_slice(&fs::read(&index).unwrap()).unwrap();
    mtimes[eid] = raw["lastModified"].clone();
    fs::write(&index, serde_json::to_vec(&mtimes).unwrap()).unwrap();
    let report = env.import_metadata();
    for meta in &metas[1..] {
        fs::set_permissions(meta, fs::Permissions::from_mode(0o644)).unwrap();
    }
    assert_eq!(report["skipped"], json!([]), "{report}");
    assert_eq!(report["unchanged"], 29, "{report}");
    assert_eq!(report["updated"], 1, "{report}");
    assert_eq!(report["trashed"], 0, "{report}");
    assert_eq!(env.find(raw["name"].as_str().unwrap())["rating"], 1);
}
