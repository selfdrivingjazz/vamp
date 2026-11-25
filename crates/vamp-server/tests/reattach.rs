use serde_json::json;
use std::{path::Path, sync::Arc};
use vamp_core::{Library, native, remote};

#[test]
fn replica_reattach_replaces_verified_bearer_without_replacing_mirror() {
    let authority_home = tempfile::tempdir().unwrap();
    let replica_home = tempfile::tempdir().unwrap();
    let library = Library::init(authority_home.path(), "main", None, None).unwrap();
    let token_file = library.state_dir.join("server-token");
    let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
    let url = format!("http://{}", server.server_addr());
    let start = |library| {
        let server = server.clone();
        let app = vamp_server::App::new(library, "main").unwrap();
        std::thread::spawn(move || vamp_server::serve(server, app, 1))
    };
    let old = vamp_server::token(&library.state_dir).unwrap();
    let serving = start(library);
    let home = replica_home.path();
    let mappings = [("/authority/refs".into(), "/replica/refs".into())].into();
    let (_, previous) = remote::join(home, &url, &old, Some("lab"), mappings).unwrap();
    let marker = Path::new(&previous.state_dir).join("keep");
    std::fs::write(&marker, b"existing replica data").unwrap();
    server.unblock();
    serving.join().unwrap();

    std::fs::remove_file(&token_file).unwrap();
    let library = Library::open(authority_home.path(), Some("main")).unwrap();
    let new = vamp_server::token(&library.state_dir).unwrap();
    assert_ne!(new, old);
    let serving = start(library);
    let attach = |token: &str| {
        native::request(serde_json::from_value(json!({
            "operation": "remote_attach", "home": home, "name": "lab", "remote_url": url, "token": token
        })).unwrap())
    };
    assert!(attach(&old).is_err(), "old bearer is rejected before changing the mirror");
    let attached = attach(&new);
    server.unblock();
    serving.join().unwrap();

    assert_eq!(attached.unwrap()["state_dir"], previous.state_dir);
    assert_eq!(std::fs::read(&marker).unwrap(), b"existing replica data");
    let config = vamp_core::load_config(home).unwrap();
    let saved = config.contexts["lab"].remote.as_ref().unwrap();
    assert_eq!(saved.token, new);
    assert_eq!(saved.drive_roots, previous.remote.unwrap().drive_roots);
}
