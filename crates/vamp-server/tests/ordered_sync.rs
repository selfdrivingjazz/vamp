use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex, atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst}, mpsc};
use std::time::Duration;
use vamp_core::{Library, native};
use vamp_protocol::{Metadata, MetadataPatch};

const DEADLINE: Duration = Duration::from_secs(5);

fn op(home: &Path, mut request: Value) -> anyhow::Result<Value> {
    request["home"] = json!(home);
    native::request(serde_json::from_value(request)?)
}

struct Fixture {
    _dir: tempfile::TempDir,
    home: std::path::PathBuf,
    writer: Library,
    id: String,
    cursor: Value,
    authority: Arc<tiny_http::Server>,
    relay: Arc<tiny_http::Server>,
}

impl Fixture {
    fn new(fault: impl Fn(&str, u16, &mut Vec<u8>) -> u16 + Send + Sync + 'static) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("authority");
        let mut library = Library::init(&source, "main", None, None).unwrap();
        let id = library.ingest_bytes(b"fixture", "fixture.txt", Metadata::default()).unwrap().id;
        let token = vamp_server::token(&library.state_dir).unwrap();
        let authority = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let upstream = format!("http://{}", authority.server_addr());
        let app = vamp_server::App::new(library, "main").unwrap();
        let server = authority.clone();
        std::thread::spawn(move || vamp_server::serve(server, app, 2));

        let relay = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let url = format!("http://{}", relay.server_addr());
        let server = relay.clone();
        let fault = Arc::new(fault);
        std::thread::spawn(move || {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .http_status_as_error(false).timeout_global(Some(DEADLINE)).build().into();
            for mut req in server.incoming_requests() {
                let (agent, upstream, fault) = (agent.clone(), upstream.clone(), fault.clone());
                std::thread::spawn(move || {
                    let path = req.url().to_owned();
                    let mut forward = ureq::http::Request::builder()
                        .method(req.method().as_str()).uri(format!("{upstream}{path}"));
                    for header in req.headers().iter().filter(|h| {
                        h.field.equiv("Authorization") || h.field.equiv("Content-Type")
                    }) {
                        forward = forward.header(header.field.as_str().as_str(), header.value.as_str());
                    }
                    let mut body = Vec::new();
                    req.as_reader().read_to_end(&mut body).unwrap();
                    let mut response = agent.run(forward.body(body).unwrap()).unwrap();
                    let mut body = response.body_mut().read_to_vec().unwrap();
                    let status = fault(&path, response.status().as_u16(), &mut body);
                    let reply = tiny_http::Response::from_data(body).with_status_code(status)
                        .with_header(tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap());
                    req.respond(reply).unwrap();
                });
            }
        });

        let home = dir.path().join("replica");
        let attached = op(&home, json!({"operation":"remote_attach","name":"replica","remote_url":url,"token":token})).unwrap();
        Self { _dir: dir, home, writer: Library::open(&source, None).unwrap(), id,
            cursor: attached["sync"]["cursor"].clone(), authority, relay }
    }

    fn rename(&mut self, name: &str) {
        self.writer.update(&self.id, MetadataPatch { name: Some(name.into()), ..Default::default() }, None).unwrap();
    }

    fn mirror(&self) -> Value {
        op(&self.home, json!({"operation":"inspect","asset_id":self.id})).unwrap()["asset"].clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.relay.unblock();
        self.authority.unblock();
    }
}

#[test]
fn overlapping_pulls_cannot_roll_back_and_all_readers_get_changes() {
    let (batch_held, held) = mpsc::channel();
    let (release, batch_release) = mpsc::channel();
    let batch_release = Mutex::new(Some(batch_release));
    let batches = Arc::new(AtomicUsize::new(0));
    let mut f = Fixture::new({
        let batches = batches.clone();
        move |path, status, _| {
            if path == "/v1/assets/batch" && batches.fetch_add(1, SeqCst) == 0 {
                batch_held.send(()).unwrap();
                batch_release.lock().unwrap().take().unwrap().recv_timeout(DEADLINE).unwrap();
            }
            status
        }
    });
    let initial = f.mirror();
    f.rename("first");
    let first = {
        let home = f.home.clone();
        std::thread::spawn(move || op(&home, json!({"operation":"sync"})).unwrap())
    };
    held.recv_timeout(DEADLINE).unwrap();
    f.rename("second");
    let (fast_out, fast_in) = mpsc::channel();
    let second = {
        let home = f.home.clone();
        std::thread::spawn(move || fast_out.send(op(&home, json!({"operation":"sync"}))).unwrap())
    };
    // The second pull cannot fetch/apply a newer view while the first still owns its page.
    let early = fast_in.recv_timeout(Duration::from_millis(300));
    let during = f.mirror();
    release.send(()).unwrap();
    assert!(matches!(early, Err(mpsc::RecvTimeoutError::Timeout)));
    assert_eq!(during, initial);
    let first = first.join().unwrap();
    let second_value = fast_in.recv_timeout(DEADLINE).unwrap().unwrap();
    second.join().unwrap();
    assert_eq!(first["cursor"], f.cursor.as_u64().unwrap() + 1);
    assert_eq!(second_value["cursor"], f.writer.last_event().unwrap());
    assert_eq!(first["kinds"]["media_updated"], 1);
    assert_eq!(second_value["kinds"]["media_updated"], 2);
    assert_eq!(second_value["asset_ids"], json!([f.id]));
    assert_eq!(f.mirror()["name"], "second");
    assert_eq!(f.mirror()["revision"], initial["revision"].as_u64().unwrap() + 2);
    assert_eq!(batches.load(SeqCst), 2);

    // A watcher between polls receives the same committed notices without a refetch.
    let notice = op(&f.home, json!({"operation":"sync_wait","sync_since":f.cursor,"wait":0})).unwrap();
    assert_eq!(notice["cursor"], second_value["cursor"]);
    assert_eq!(notice["kinds"], second_value["kinds"]);
    assert_eq!(notice["asset_ids"], second_value["asset_ids"]);
    assert_eq!(batches.load(SeqCst), 2);
}

#[test]
fn confirmed_replica_write_returns_success_when_catch_up_fails() {
    let outage = Arc::new(AtomicBool::new(false));
    let committed = Arc::new(AtomicUsize::new(0));
    let f = Fixture::new({
        let (outage, committed) = (outage.clone(), committed.clone());
        move |path, status, body| {
            if path.starts_with("/v1/assets/") && path != "/v1/assets/batch" && status == 200 {
                committed.fetch_add(1, SeqCst);
                outage.store(true, SeqCst);
            }
            if path.starts_with("/v1/events?") && outage.load(SeqCst) {
                *body = br#"{"error":{"code":"unavailable","message":"post-write outage"}}"#.to_vec();
                return 503;
            }
            status
        }
    });
    let before = f.mirror();
    let response = op(&f.home, json!({"operation":"update","asset_id":f.id,
        "expected_revision":before["revision"],"name":"saved"})).unwrap();
    assert_eq!(response["name"], "saved");
    assert!(response["sync_pending"].as_str().unwrap().contains("post-write outage"));
    assert_eq!(f.writer.inspect(&f.id).unwrap().asset.name, "saved");
    assert_eq!(f.mirror(), before);
    outage.store(false, SeqCst);
    let sync = op(&f.home, json!({"operation":"sync"})).unwrap();
    assert_eq!(sync["kinds"]["media_updated"], 1);
    assert_eq!(f.mirror()["name"], "saved");
    assert_eq!(f.mirror()["revision"], before["revision"].as_u64().unwrap() + 1);
    assert_eq!(committed.load(SeqCst), 1);
}

#[test]
fn established_replica_rechecks_protocol_and_authority_identity() {
    let skew = Arc::new(AtomicUsize::new(0));
    let mut f = Fixture::new({
        let skew = skew.clone();
        move |path, status, body| {
            if path == "/v1/hello" {
                let mut hello: Value = serde_json::from_slice(body).unwrap();
                match skew.load(SeqCst) {
                    1 => hello["protocol_version"] = json!("0.2"),
                    2 => hello["library_id"] = json!("wrong-library"),
                    _ => return status,
                }
                *body = serde_json::to_vec(&hello).unwrap();
            }
            status
        }
    });
    let before = f.mirror();
    f.rename("never apply during skew");
    for (mode, message) in [(1, "unsupported remote protocol version"), (2, "identity differs")] {
        skew.store(mode, SeqCst);
        let error = op(&f.home, json!({"operation":"sync"})).unwrap_err();
        assert!(error.to_string().contains(message), "{error:#}");
        assert_eq!(f.mirror(), before);
    }
    skew.store(0, SeqCst);
    op(&f.home, json!({"operation":"sync"})).unwrap();
    assert_eq!(f.mirror()["name"], "never apply during skew");
}
