use serde_json::{Value, json};
use std::sync::Arc;
use vamp_core::{Library, native, remote};
use vamp_protocol::Metadata;

fn op(home: &std::path::Path, value: Value) -> anyhow::Result<Value> {
    let mut value = value;
    value["home"] = json!(home);
    native::request(serde_json::from_value(value)?)
}

#[test]
fn replica_syncs_browses_offline_fetches_lazily_and_forwards_writes() {
    let authority_home = tempfile::tempdir().unwrap();
    let originals = tempfile::tempdir().unwrap();
    let replica_home = tempfile::tempdir().unwrap();
    let second_replica_home = tempfile::tempdir().unwrap();
    let bytes: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let file = originals.path().join("bat.png");
    std::fs::write(&file, &bytes).unwrap();

    let mut library = Library::init(authority_home.path(), "main", None, None).unwrap();
    let asset = library.add(&file, None, Metadata::default()).unwrap();
    let collection = library.collection_create("Night", None).unwrap();
    library
        .collection_membership(&collection.id, &asset.id, true)
        .unwrap();
    let token = vamp_server::token(&library.state_dir).unwrap();
    let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
    let url = format!("http://{}", server.server_addr());
    let app = vamp_server::App::new(library, "main").unwrap();
    let serving = {
        let server = server.clone();
        // Fewer workers than a replica `add` runs uploads in parallel: the uploads' new
        // connections must all be served (vendor/tiny_http task-pool fix).
        std::thread::spawn(move || vamp_server::serve(server, app, 2))
    };

    // Unauthorized requests are rejected before touching the catalog.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let status = |auth: &str| {
        agent
            .get(format!("{url}/v1/library"))
            .header("Authorization", auth)
            .call()
            .unwrap()
            .status()
            .as_u16()
    };
    assert_eq!(status("Bearer wrong"), 401);
    assert_eq!(status(&format!("Bearer {token}")), 200);
    assert!(remote::attach_remote(replica_home.path(), "bad", &url, "wrong").is_err());

    let home = replica_home.path();
    let attached = op(
        home,
        json!({"operation":"remote_attach","name":"lab","remote_url":url,"token":token}),
    )
    .unwrap();
    // Attach bootstraps the mirror from a snapshot before returning.
    assert_eq!(attached["sync"]["snapshot"], true);
    assert_eq!(attached["sync"]["applied"], 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(home.join("config.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "config holding tokens must be owner-only");
    }
    assert_eq!(op(home, json!({"operation":"sync"})).unwrap()["applied"], 0);
    let second_home = second_replica_home.path();
    let attached = op(
        second_home,
        json!({"operation":"remote_attach","name":"lab","remote_url":url,"token":token}),
    )
    .unwrap();
    assert_eq!(attached["sync"]["snapshot"], true);

    // The authority path is readable here (same machine) with the cataloged size and
    // digest, so the replica reads it directly instead of downloading a copy.
    let fetched = op(
        home,
        json!({"operation":"representation","asset_id":asset.id,"kind":"original"}),
    )
    .unwrap();
    let direct = std::path::PathBuf::from(fetched["path"].as_str().unwrap());
    assert_eq!(direct, std::path::PathBuf::from(&asset.original.path));
    assert_eq!(std::fs::read(&direct).unwrap(), bytes);

    // Two independent replicas editing the same revision must not both succeed.
    let barrier = std::sync::Barrier::new(2);
    let edit = |home: &std::path::Path| {
        barrier.wait();
        op(
            home,
            json!({"operation":"update","asset_id":asset.id,"name":"Vampire bat","expected_revision":1}),
        )
    };
    let outcomes = std::thread::scope(|scope| {
        let first = scope.spawn(|| edit(home));
        let second = edit(second_home);
        [first.join().unwrap(), second]
    });
    assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
    let conflict = outcomes.into_iter().find_map(Result::err).unwrap();
    assert!(format!("{conflict:#}").contains("revision conflict"));
    for replica in [home, second_home] {
        op(replica, json!({"operation":"sync"})).unwrap();
        let inspect = op(replica, json!({"operation":"inspect","asset_id":asset.id})).unwrap();
        assert_eq!(inspect["asset"]["name"], "Vampire bat");
        assert_eq!(inspect["asset"]["revision"], 2);
    }
    let view: Value = agent
        .get(format!("{url}/v1/assets/{}", asset.id))
        .header("Authorization", format!("Bearer {token}"))
        .call()
        .unwrap()
        .body_mut()
        .read_json()
        .unwrap();
    assert_eq!(view["asset"]["name"], "Vampire bat");
    assert_eq!(view["asset"]["revision"], 2);
    // Authority without an embedder: semantic browse degrades to mirror full-text.
    let semantic = op(
        home,
        json!({"operation":"browse","query":"vampire","semantic":true}),
    )
    .unwrap();
    assert_eq!(semantic["semantic"], false);
    assert_eq!(semantic["total"], 1);
    // Both the legacy single-asset route and the native list form enforce the revision.
    let stale = op(
        second_home,
        json!({"operation":"update","asset_ids":[asset.id],"name":"x","expected_revision":1}),
    )
    .unwrap_err();
    assert!(format!("{stale:#}").contains("revision conflict"));
    let native_conflict = agent
        .post(format!("{url}/v1/native"))
        .header("Authorization", format!("Bearer {token}"))
        .send_json(json!({"operation":"update","asset_ids":[asset.id],"name":"x","expected_revision":1}))
        .unwrap();
    assert_eq!(native_conflict.status().as_u16(), 409);
    let ambiguous = op(
        second_home,
        json!({"operation":"update","asset_ids":[asset.id, "missing"],"name":"x","expected_revision":2}),
    )
    .unwrap_err();
    assert!(format!("{ambiguous:#}").contains("expected_revision requires a single asset"));
    let failed_batch = op(
        second_home,
        json!({"operation":"update","asset_ids":[asset.id, "missing"],"name":"partial"}),
    )
    .unwrap_err();
    assert!(format!("{failed_batch:#}").contains("asset not found: missing"));
    let current = op(authority_home.path(), json!({"operation":"inspect","asset_id":asset.id})).unwrap();
    assert_eq!(current["asset"]["name"], "Vampire bat");
    assert_eq!(current["asset"]["revision"], 2);

    // Eagle-parity writes are forwarded (POST /v1/native) and mirrored by sync.
    let child = op(
        home,
        json!({"operation":"collection_create","name":"Bats","parent_id":collection.id}),
    )
    .unwrap();
    assert_eq!(child["parent_id"], collection.id.as_str());
    op(
        home,
        json!({"operation":"collection_add","collection_id":child["id"],"asset_ids":[asset.id]}),
    )
    .unwrap();
    let destination = op(
        second_home,
        json!({"operation":"collection_create","name":"Roost","parent_id":collection.id}),
    )
    .unwrap();
    let moved = op(
        second_home,
        json!({"operation":"collection_move","collection_id":child["id"],"parent_id":destination["id"],"index":0}),
    )
    .unwrap();
    assert_eq!(moved["parent_id"], destination["id"]);
    let smart = op(
        home,
        json!({"operation":"smart_folder_create","name":"Rated","rules":{"match":"all","rules":[{"field":"rating","op":"gte","value":4}]}}),
    )
    .unwrap();
    op(
        home,
        json!({"operation":"update","asset_ids":[asset.id],"metadata":{"rating":5,"add_tags":["night"]},"expected_revision":2}),
    )
    .unwrap();
    op(home, json!({"operation":"touch","asset_id":asset.id})).unwrap();
    let smart_list = op(home, json!({"operation":"smart_folders"})).unwrap();
    assert_eq!(smart_list[0]["id"], smart["id"]);
    assert_eq!(smart_list[0]["count"], 1);
    assert_eq!(
        op(home, json!({"operation":"remove","asset_ids":[asset.id]})).unwrap()["trashed"],
        1
    );
    assert_eq!(
        op(home, json!({"operation":"sidebar"})).unwrap()["trash"],
        1
    );
    op(home, json!({"operation":"restore","asset_ids":[asset.id]})).unwrap();
    let renamed_tag = op(
        home,
        json!({"operation":"tag_rename","from":"night","to":"nocturnal"}),
    )
    .unwrap();
    assert_eq!(renamed_tag["updated"], 1);
    let sidebar = op(home, json!({"operation":"sidebar"})).unwrap();
    assert_eq!(
        (
            sidebar["all"].clone(),
            sidebar["trash"].clone(),
            sidebar["recently_used"].clone()
        ),
        (json!(1), json!(0), json!(1))
    );
    let roost = &sidebar["folders"][0]["children"][0];
    assert_eq!(roost["id"], destination["id"]);
    assert_eq!(roost["children"][0]["id"], child["id"]);
    assert_eq!(roost["children"][0]["count"], 1);
    assert_eq!(sidebar["smart_folders"][0]["count"], 1);
    op(second_home, json!({"operation":"sync"})).unwrap();
    let second_sidebar = op(second_home, json!({"operation":"sidebar"})).unwrap();
    for key in ["all", "trash", "folders", "smart_folders"] {
        assert_eq!(second_sidebar[key], sidebar[key]);
    }
    assert_eq!(
        op(second_home, json!({"operation":"tags"})).unwrap()["tags"],
        json!([{"tag":"nocturnal","count":1}])
    );

    // Intake uploads files in parallel and mirrors the created assets at once (no sync round
    // trip before they show); the next sync replays their events.
    let paths: Vec<_> = (0..5)
        .map(|i| {
            let path = originals.path().join(format!("moth{i}.png"));
            std::fs::write(&path, format!("moth {i}")).unwrap();
            path
        })
        .collect();
    let added = op(home, json!({"operation":"add","paths":paths})).unwrap();
    let ids: Vec<&str> = added["assets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 5, "{added}");
    assert_eq!(added["errors"], json!([]));
    for (i, asset) in added["assets"].as_array().unwrap().iter().enumerate() {
        let original = asset["original"]["path"].as_str().unwrap();
        assert_eq!(std::fs::read(original).unwrap(), format!("moth {i}").as_bytes());
    }
    let items = op(home, json!({"operation":"items","asset_ids":ids})).unwrap();
    assert_eq!(items["items"].as_array().unwrap().len(), 5);
    assert_eq!(op(home, json!({"operation":"sidebar"})).unwrap()["all"], 6);
    assert!(op(home, json!({"operation":"sync"})).unwrap()["applied"].as_u64().unwrap() >= 5);
    assert_eq!(op(home, json!({"operation":"sidebar"})).unwrap()["all"], 6);

    // Failed batches must not change the authority or another replica.
    let updated = op(
        second_home,
        json!({"operation":"update","asset_ids":[ids[1],ids[0]],"metadata":{"rating":4}}),
    )
    .unwrap();
    assert_eq!(updated["assets"][0]["id"], ids[1]);
    assert_eq!(updated["assets"][1]["id"], ids[0]);
    assert_eq!(updated["assets"][0]["metadata"]["rating"], 4);
    let failed_trash = op(
        second_home,
        json!({"operation":"remove","asset_ids":[ids[0], "missing"]}),
    )
    .unwrap_err();
    assert!(format!("{failed_trash:#}").contains("asset not found: missing"));
    op(second_home, json!({"operation":"sync"})).unwrap();
    assert_eq!(op(second_home, json!({"operation":"sidebar"})).unwrap()["trash"], 0);
    let trashed = op(
        second_home,
        json!({"operation":"remove","asset_ids":[ids[0],ids[1],ids[1]]}),
    )
    .unwrap();
    assert_eq!(trashed["trashed"], 2);
    op(home, json!({"operation":"sync"})).unwrap();
    assert_eq!(op(home, json!({"operation":"sidebar"})).unwrap()["trash"], 2);
    let restored = op(
        second_home,
        json!({"operation":"restore","asset_ids":[ids[0],ids[1]]}),
    )
    .unwrap();
    assert_eq!(restored["restored"], 2);
    op(home, json!({"operation":"sync"})).unwrap();
    assert_eq!(op(home, json!({"operation":"sidebar"})).unwrap()["trash"], 0);

    for _ in 0..2 {
        server.unblock();
    }
    serving.join().unwrap();
    drop(server);

    // Offline: browse, inspect and collections come from the mirror; cached bytes remain.
    let browse = op(home, json!({"operation":"browse","query":"vampire"})).unwrap();
    assert_eq!(browse["total"], 1);
    assert_eq!(browse["items"][0]["id"], asset.id.as_str());
    let inspect = op(home, json!({"operation":"inspect","asset_id":asset.id})).unwrap();
    assert_eq!(inspect["asset"]["name"], "Vampire bat");
    let collections = op(home, json!({"operation":"collections"})).unwrap();
    assert_eq!(collections[0]["name"], "Night");
    assert_eq!(collections[0]["count"], 1);
    let again = op(home, json!({"operation":"original","asset_id":asset.id})).unwrap();
    assert_eq!(again["path"], fetched["path"]);
    assert!(op(home, json!({"operation":"sync"})).is_err());
}
