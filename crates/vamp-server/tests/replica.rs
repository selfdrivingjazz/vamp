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

    // Rename through the replica lands on the authority and back in the mirror.
    let renamed = op(home, json!({"operation":"update","asset_id":asset.id,"name":"Vampire bat","expected_revision":1})).unwrap();
    assert_eq!(renamed["name"], "Vampire bat");
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
    // Stale revisions are refused by the authority.
    let stale = op(
        home,
        json!({"operation":"update","asset_id":asset.id,"name":"x","expected_revision":1}),
    );
    assert!(format!("{:#}", stale.unwrap_err()).contains("revision conflict"));

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
    let smart = op(
        home,
        json!({"operation":"smart_folder_create","name":"Rated","rules":{"match":"all","rules":[{"field":"rating","op":"gte","value":4}]}}),
    )
    .unwrap();
    op(
        home,
        json!({"operation":"update","asset_ids":[asset.id],"metadata":{"rating":5,"add_tags":["night"]}}),
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
    assert_eq!(sidebar["folders"][0]["children"][0]["name"], "Bats");
    assert_eq!(sidebar["smart_folders"][0]["count"], 1);

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
