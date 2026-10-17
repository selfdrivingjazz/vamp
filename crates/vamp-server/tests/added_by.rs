//! `added_by` reaches replicas: with every mirrored view, and once, without re-downloading
//! documents, onto mirrors cataloged before the field existed.
use serde_json::{Value, json};
use std::sync::Arc;
use vamp_core::{Library, native, with_actor};
use vamp_protocol::Metadata;

fn op(home: &std::path::Path, value: Value) -> anyhow::Result<Value> {
    let mut value = value;
    value["home"] = json!(home);
    native::request(serde_json::from_value(value)?)
}

#[test]
fn replicas_mirror_added_by_and_backfill_older_mirrors_in_place() {
    let authority_home = tempfile::tempdir().unwrap();
    let originals = tempfile::tempdir().unwrap();
    let replica_home = tempfile::tempdir().unwrap();
    let file = |name: &str, seed: u8| {
        let path = originals.path().join(name);
        std::fs::write(&path, vec![seed; 4096]).unwrap();
        path
    };
    let mut library = Library::init(authority_home.path(), "main", None, None).unwrap();
    let mut add = |actor: &str, name: &str, seed: u8| {
        with_actor(Some(actor.into()), || library.add(&file(name, seed), None, Metadata::default()))
            .unwrap()
            .id
    };
    let mine = add("local", "mine.png", 1);
    let alpha = add("agent:alpha", "alpha.png", 2);
    let token = vamp_server::token(&library.state_dir).unwrap();
    let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
    let url = format!("http://{}", server.server_addr());
    let app = vamp_server::App::new(library, "main").unwrap();
    let serving = {
        let server = server.clone();
        std::thread::spawn(move || vamp_server::serve(server, app, 2))
    };

    let home = replica_home.path();
    let attached = op(
        home,
        json!({"operation":"remote_attach","name":"lab","remote_url":url,"token":token}),
    )
    .unwrap();
    assert_eq!(attached["sync"]["snapshot"], true);
    let counts = |home| op(home, json!({"operation":"sidebar"})).unwrap()["added_by"].clone();
    assert_eq!(
        counts(home),
        json!({"owner":1,"agents":1,"by_agent":[{"actor":"agent:alpha","count":1}]})
    );
    let agents = op(home, json!({"operation":"browse","added_by":"agents"})).unwrap();
    assert_eq!(agents["items"][0]["id"], alpha.as_str());
    assert_eq!(agents["items"][0]["added_by"], "agent:alpha");

    // So do the views an incremental sync replays (another writer on the authority).
    let beta = with_actor(Some("agent:beta".into()), || {
        Library::open(authority_home.path(), None)
            .unwrap()
            .add(&file("beta.png", 3), None, Metadata::default())
            .unwrap()
            .id
    });
    let synced = op(home, json!({"operation":"sync"})).unwrap();
    assert_eq!(synced["added_by_mirrored"], 0);
    let inspected = op(home, json!({"operation":"inspect","asset_id":beta})).unwrap();
    assert_eq!(inspected["asset"]["added_by"], "agent:beta");
    assert_eq!(counts(home)["agents"], 2);

    // A mirror from before `added_by`: one `added_by` request fills it in place.
    let state = Library::open(home, Some("lab")).unwrap().state_dir.clone();
    let catalog = rusqlite::Connection::open(state.join("catalog.sqlite3")).unwrap();
    catalog
        .execute_batch(
            "UPDATE assets SET document=json_remove(document,'$.metadata.extra.added_by');
             DELETE FROM settings WHERE key='added_by_mirrored';",
        )
        .unwrap();
    assert_eq!(counts(home)["owner"], 3);
    let cursor = |c: &rusqlite::Connection| -> i64 {
        c.query_row("SELECT value FROM replica_state WHERE key='cursor'", [], |r| r.get(0))
            .unwrap()
    };
    let before = cursor(&catalog);
    let synced = op(home, json!({"operation":"sync"})).unwrap();
    assert_eq!(synced["added_by_mirrored"], 3);
    assert_eq!(synced["snapshot"], false);
    assert_eq!(cursor(&catalog), before, "no events or snapshot were needed");
    assert_eq!(
        counts(home),
        json!({"owner":1,"agents":2,"by_agent":[{"actor":"agent:alpha","count":1},{"actor":"agent:beta","count":1}]})
    );
    let owner = op(home, json!({"operation":"browse","added_by":"owner"})).unwrap();
    assert_eq!(owner["items"][0]["id"], mine.as_str());
    // Done once.
    assert_eq!(op(home, json!({"operation":"sync"})).unwrap()["added_by_mirrored"], 0);

    for _ in 0..2 {
        server.unblock();
    }
    serving.join().unwrap();
}

#[test]
fn authenticated_replica_agent_provenance_and_explorations() -> anyhow::Result<()> {
    let authority = tempfile::tempdir()?;
    let replica = tempfile::tempdir()?;
    let library = Library::init(authority.path(), "authority", None, None)?;
    let token = vamp_server::token(&library.state_dir)?;
    let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
    let url = format!("http://{}", server.server_addr());
    let app = vamp_server::App::new(library, "authority")?;
    let serving = {
        let server = server.clone();
        std::thread::spawn(move || vamp_server::serve(server, app, 2))
    };
    let result = (|| -> anyhow::Result<()> {
        op(replica.path(), json!({"operation":"remote_attach","name":"replica","remote_url":url,"token":token}))?;
        with_actor(Some("agent:mcp-test".into()), || -> anyhow::Result<()> {
            let added = op(replica.path(), json!({"operation":"ingest","filename":"mcp.txt","bytes_base64":"aGVsbG8gbWNw"}))?;
            let id = added["id"].as_str().unwrap();
            let asset = op(replica.path(), json!({"operation":"inspect","asset_id":id}))?;
            assert_eq!(asset["asset"]["added_by"], "agent:mcp-test");
            let library = Library::open(replica.path(), None)?;
            let client = library.remote_client()?;
            let mission = client.native(&json!({"operation":"explore_ensure","slug":"mcp-test","title":"MCP mission","brief":"Review references"}))?;
            assert!(mission.is_object());
            client.native(&json!({"operation":"explore_add","slug":"mcp-test","name":"A note","note":"Why it matters","kind":"note","tags":["mcp"]}))?;
            client.native(&json!({"operation":"explore_summary","slug":"mcp-test","text":"One finding"}))?;
            let shown = client.native(&json!({"operation":"explore_show","slug":"mcp-test"}))?;
            assert!(shown.to_string().contains("A note"), "{shown}");
            let changes = client.native(&json!({"operation":"explore_changes","slug":"mcp-test","peek":true}))?;
            assert_eq!(changes["reader"], "agent:mcp-test");
            Ok(())
        })
    })();
    for _ in 0..2 { server.unblock(); }
    serving.join().unwrap();
    result
}
