use serde_json::{Value, json};
use std::{fs, path::Path, sync::Arc};
use vamp_core::{Library, native, remote};
use vamp_protocol::{Metadata, OriginalState};

fn op(home: &Path, mut value: Value) -> anyhow::Result<Value> {
    value["home"] = json!(home);
    native::request(serde_json::from_value(value)?)
}

#[test]
fn isolated_missing_moves_rename_ambiguity_sync_and_cached_replica() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("authority");
    let replica_home = tmp.path().join("replica");
    let root = tmp.path().join("originals");
    let a = root.join("a");
    let sibling = root.join("sibling");
    let other = tmp.path().join("other-root");
    for dir in [&a, &sibling, &other] {
        fs::create_dir_all(dir).unwrap();
    }
    let mut library = Library::init(&home, "test", Some(&root), None).unwrap();
    library.remember_import_root(&other).unwrap();
    let mut assets = Vec::new();
    for name in [
        "delete",
        "sibling",
        "other-root",
        "rename",
        "ambiguous",
        "cached",
    ] {
        let path = a.join(format!("{name}.jpg"));
        // Unique real JPEGs, so tier generation can also be exercised independently.
        let color = (assets.len() as u8 + 1) * 30;
        image::RgbImage::from_pixel(32, 24, image::Rgb([color, 80, 160]))
            .save(&path)
            .unwrap();
        assets.push(library.add(&path, Some(name), Metadata::default()).unwrap());
    }
    let token = vamp_server::token(&library.state_dir).unwrap();
    // Explicit isolated port; never uses the live authority's port/home.
    let server = Arc::new(tiny_http::Server::http("127.0.0.1:7796").unwrap());
    let url = "http://127.0.0.1:7796";
    let app = vamp_server::App::new(library, "test").unwrap();
    let serving = {
        let server = server.clone();
        std::thread::spawn(move || vamp_server::serve(server, app, 2))
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    remote::attach_remote(&replica_home, "replica", url, &token).unwrap();
    let replica = Library::open(&replica_home, None).unwrap();
    let cached = &assets[5];
    let cache = replica
        .state_dir
        .join(format!("cache/original/{}.jpg", cached.original.blake3));
    fs::create_dir_all(cache.parent().unwrap()).unwrap();
    fs::copy(&cached.original.path, &cache).unwrap();
    drop(replica);

    fs::remove_file(&assets[0].original.path).unwrap();
    fs::rename(&assets[1].original.path, sibling.join("moved.jpg")).unwrap();
    fs::rename(&assets[2].original.path, other.join("moved.jpg")).unwrap();
    fs::rename(&assets[3].original.path, a.join("renamed.jpg")).unwrap();
    fs::rename(&assets[4].original.path, sibling.join("first.jpg")).unwrap();
    fs::copy(sibling.join("first.jpg"), other.join("second.jpg")).unwrap();
    fs::remove_file(&cached.original.path).unwrap();

    // Access marks ENOENT immediately, without waiting for the sweep, and is typed.
    let mut response = agent
        .get(format!("{url}/v1/assets/{}/original", assets[0].id))
        .header("Authorization", format!("Bearer {token}"))
        .call()
        .unwrap();
    assert_eq!(response.status(), 404);
    let failure: Value = response.body_mut().read_json().unwrap();
    assert_eq!(failure["error"]["code"], "original_missing");
    assert_eq!(failure["error"]["asset_id"], assets[0].id);
    let mut authority = Library::open(&home, None).unwrap();
    assert_eq!(
        authority
            .inspect(&assets[0].id)
            .unwrap()
            .asset
            .original
            .availability
            .state,
        OriginalState::Missing
    );
    // No access to the other paths: the cheap server sweep detects their absence.
    std::thread::sleep(std::time::Duration::from_secs(32));
    let swept = authority.inspect(&cached.id).unwrap().asset;
    assert_eq!(swept.original.availability.state, OriginalState::Missing);
    assert!(swept.original.availability.last_checked_at.is_some());
    let cursor = authority.last_event().unwrap();
    let report = authority.doctor(false, &[]).unwrap();
    assert_eq!(
        authority.last_event().unwrap(),
        cursor,
        "default doctor must not write"
    );
    assert_eq!(report["counts"]["missing"], 6);
    assert_eq!(report["counts"]["relinkable"], 3);
    assert_eq!(report["counts"]["ambiguous"], 1);
    assert_eq!(report["counts"]["tier_gaps"], 6);
    let api_report: Value = agent
        .get(format!("{url}/v1/doctor"))
        .header("Authorization", format!("Bearer {token}"))
        .call()
        .unwrap()
        .body_mut()
        .read_json()
        .unwrap();
    assert_eq!(api_report["counts"], report["counts"]);
    let fixed = authority.doctor(true, &[]).unwrap();
    assert_eq!(fixed["counts"]["relinked"], 3);
    assert_eq!(fixed["counts"]["remaining_missing"], 3);
    for asset in &assets[1..4] {
        let repaired = authority.inspect(&asset.id).unwrap().asset;
        assert_eq!(repaired.original.availability.state, OriginalState::Present);
        assert!(repaired.original.availability.relinked_at.is_some());
        assert_eq!(repaired.revision, asset.revision);
        assert_eq!(repaired.original.blake3, asset.original.blake3);
    }
    op(&replica_home, json!({"operation":"sync"})).unwrap();
    let cell = op(
        &replica_home,
        json!({"operation":"items","asset_ids":[cached.id]}),
    )
    .unwrap();
    assert_eq!(
        cell["items"][0]["original_availability"]["state"],
        "missing"
    );
    assert_eq!(cell["items"][0]["available_locally"]["original"], true);
    let fetched = op(
        &replica_home,
        json!({"operation":"original","asset_id":cached.id}),
    )
    .unwrap();
    assert_eq!(fetched["path"], cache.to_str().unwrap());
    assert!(cache.is_file());
    let failure = op(
        &replica_home,
        json!({"operation":"original","asset_id":assets[0].id}),
    )
    .unwrap_err();
    assert!(vamp_core::OriginalMissing::from_error(&failure).is_some());
    assert_eq!(
        authority.list(None, 100, 0, None).unwrap().len(),
        6,
        "no catalog items removed"
    );
    // Explicit repair accepts a chosen ambiguous path only after exact hash verification.
    op(
        &replica_home,
        json!({"operation":"relink","asset_id":assets[4].id,"path":sibling.join("first.jpg")}),
    )
    .unwrap();
    let row = op(
        &replica_home,
        json!({"operation":"items","asset_ids":[assets[4].id]}),
    )
    .unwrap();
    assert_eq!(row["items"][0]["original_availability"]["state"], "present");
    let events = authority.events(0, 100).unwrap();
    assert!(
        events.iter().any(|e| e.kind == "original_relinked"
            && e.payload["previous_path"] == assets[4].original.path)
    );
    for _ in 0..2 {
        server.unblock();
    }
    serving.join().unwrap();
}
