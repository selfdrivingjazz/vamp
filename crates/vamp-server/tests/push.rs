use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use vamp_core::{Library, native};
use vamp_protocol::{Metadata, MetadataPatch};

fn op(home: &std::path::Path, value: Value) -> anyhow::Result<Value> {
    let mut value = value;
    value["home"] = json!(home);
    native::request(serde_json::from_value(value)?)
}

/// A replica long-polling `sync_wait` wakes as soon as another catalog connection (as a
/// separate CLI process would) writes, and an idle wait ends with `changed: false`.
#[test]
fn sync_wait_wakes_on_writes_from_other_connections_and_times_out_idle() {
    let authority_home = tempfile::tempdir().unwrap();
    let originals = tempfile::tempdir().unwrap();
    let replica_home = tempfile::tempdir().unwrap();
    let file = originals.path().join("a.bin");
    std::fs::write(&file, b"push").unwrap();
    let mut library = Library::init(authority_home.path(), "main", None, None).unwrap();
    let asset = library.add(&file, None, Metadata::default()).unwrap();
    let token = vamp_server::token(&library.state_dir).unwrap();
    let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
    let url = format!("http://{}", server.server_addr());
    // One worker: long-polls must not occupy it (they get their own threads).
    let app = vamp_server::App::new(library, "main").unwrap();
    std::thread::spawn({
        let server = server.clone();
        move || vamp_server::serve(server, app, 1)
    });
    let home = replica_home.path();
    op(
        home,
        json!({"operation":"remote_attach","name":"lab","remote_url":url,"token":token}),
    )
    .unwrap();
    op(home, json!({"operation":"sync"})).unwrap();

    let started = Instant::now();
    let idle = op(home, json!({"operation":"sync_wait","wait":1})).unwrap();
    assert_eq!(idle["changed"], false);
    assert!(started.elapsed() >= Duration::from_millis(900));

    let waiter = {
        let home = home.to_owned();
        std::thread::spawn(move || {
            let started = Instant::now();
            let value = op(&home, json!({"operation":"sync_wait","wait":20})).unwrap();
            (value, started.elapsed())
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    // The single worker still answers ordinary requests while the long-poll is open.
    let other = op(home, json!({"operation":"sync"})).unwrap();
    assert_eq!(other["applied"], 0);
    let mut writer = Library::open(authority_home.path(), Some("main")).unwrap();
    writer
        .update(
            &asset.id,
            MetadataPatch {
                tags: Some(vec!["live".into()]),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let (value, elapsed) = waiter.join().unwrap();
    assert_eq!(value["changed"], true);
    assert_eq!(value["applied"], 1);
    assert!(elapsed < Duration::from_secs(3), "woke after {elapsed:?}");
    let view = op(home, json!({"operation":"inspect","asset_id":asset.id})).unwrap();
    assert_eq!(view["asset"]["tags"], json!(["live"]));
}

/// Check the wire, not an HTTP client that transparently decodes gzip.
#[test]
fn event_pages_negotiate_gzip_without_changing_events() {
    use std::io::{Read, Write};
    let dir = tempfile::tempdir().unwrap();
    let mut library = Library::init(&dir.path().join("home"), "main", None, None).unwrap();
    library.ingest_bytes(b"example", "sample.txt", Metadata::default()).unwrap();
    let expected = serde_json::to_value(library.events(0, 10).unwrap()).unwrap();
    let token = vamp_server::token(&library.state_dir).unwrap();
    let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
    let address = server.server_addr().to_ip().unwrap();
    let app = vamp_server::App::new(library, "main").unwrap();
    let serving = {
        let server = server.clone();
        std::thread::spawn(move || vamp_server::serve(server, app, 1))
    };
    for route in ["events", "events/stream?wait=2"] {
        for (encoding, compressed) in [("br, GZIP;q=0.8", true), ("gzip;q=0", false), ("identity", false)] {
            let mut stream = std::net::TcpStream::connect(address).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            write!(stream, "GET /v1/{route} HTTP/1.0\r\nAuthorization: Bearer {token}\r\nAccept-Encoding: {encoding}\r\n\r\n").unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).unwrap();
            let at = response.windows(4).position(|s| s == b"\r\n\r\n").unwrap();
            let headers = std::str::from_utf8(&response[..at]).unwrap().to_ascii_lowercase();
            assert!(headers.contains(" 200 "));
            assert_eq!(headers.contains("content-encoding: gzip"), compressed);
            let body = &response[at + 4..];
            let events: Value = if compressed {
                serde_json::from_reader(flate2::read::GzDecoder::new(body)).unwrap()
            } else {
                serde_json::from_slice(body).unwrap()
            };
            assert_eq!(events, expected);
        }
    }
    server.unblock();
    serving.join().unwrap();
}
