//! A replica keeps working through an authority that stalls: thumbnail packs answering
//! 503 for a while still all land without another sync, and a stalled original or preview
//! download is abandoned at the stall timeout or when its request is cancelled.
use serde_json::{Value, json};
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use vamp_core::Library;
use vamp_protocol::Metadata;

const FAKE_MEDIA: &str = r#"#!/bin/sh
out=$(sed -n 's/.*"output_path":"\([^"]*\)".*/\1/p')
printf x > "$out"
printf '{"representation":{"path":"%s","width":1,"height":1,"mime":"image/jpeg","byte_size":1}}' "$out"
"#;

const ASSETS: u32 = 40;
/// How long the authority answers thumbnail requests with 503 after the first one.
const OUTAGE: Duration = Duration::from_secs(5);

fn op(home: &Path, mut request: Value) -> anyhow::Result<Value> {
    request["home"] = json!(home);
    vamp_core::native::request(serde_json::from_value(request)?)
}

/// Fault-injecting HTTP proxy in front of the authority (one request per connection).
struct Proxy {
    url: String,
    /// When the first grid thumbnail request arrived.
    outage_start: Mutex<Option<Instant>>,
    unavailable: AtomicUsize,
    /// Originals and preview tiers: send headers, then never the body.
    stall_originals: AtomicBool,
    stop: AtomicBool,
}

impl Proxy {
    fn start(upstream: String) -> Arc<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy = Arc::new(Self {
            url: format!("http://{}", listener.local_addr().unwrap()),
            outage_start: Mutex::new(None),
            unavailable: AtomicUsize::new(0),
            stall_originals: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });
        let shared = proxy.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (proxy, upstream) = (shared.clone(), upstream.clone());
                std::thread::spawn(move || proxy.handle(stream, &upstream));
            }
        });
        proxy
    }

    fn handle(&self, mut client: TcpStream, upstream: &str) {
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let mut head = Vec::new();
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            if line == "\r\n" {
                break;
            }
            let lower = line.to_ascii_lowercase();
            if let Some(value) = lower.strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
            if !lower.starts_with("connection:") {
                head.push(line);
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        let request_line = head[0].clone();
        let thumbnails = (request_line.starts_with("POST /v1/thumbnails/pack")
            && String::from_utf8_lossy(&body).contains("\"grid\""))
            || (request_line.starts_with("GET /v1/assets/") && request_line.contains("/thumbnail "));
        if thumbnails {
            let start = *self
                .outage_start
                .lock()
                .unwrap()
                .get_or_insert_with(Instant::now);
            if start.elapsed() < OUTAGE {
                self.unavailable.fetch_add(1, Ordering::SeqCst);
                let body = r#"{"error":{"code":"unavailable","message":"authority busy"}}"#;
                let _ = write!(
                    client,
                    "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                return;
            }
        }
        if request_line.starts_with("GET /v1/assets/")
            && (request_line.contains("/original ") || request_line.contains("/tier/preview "))
            && self.stall_originals.load(Ordering::SeqCst)
        {
            let _ = client.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n",
            );
            while !self.stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(50));
            }
            return;
        }
        let mut server = TcpStream::connect(upstream).unwrap();
        for line in &head {
            server.write_all(line.as_bytes()).unwrap();
        }
        server.write_all(b"Connection: close\r\n\r\n").unwrap();
        server.write_all(&body).unwrap();
        let _ = std::io::copy(&mut server, &mut client);
    }
}

#[test]
fn replica_survives_authority_outage_and_stalled_originals() {
    let dir = tempfile::tempdir().unwrap();
    let helper = dir.path().join("fake-media");
    std::fs::write(&helper, FAKE_MEDIA).unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let originals = dir.path().join("originals");
    std::fs::create_dir_all(&originals).unwrap();
    let mut library = Library::init(&dir.path().join("authority"), "main", None, None).unwrap();
    let mut ids = Vec::new();
    for n in 0..ASSETS {
        let file = originals.join(format!("{n}.png"));
        std::fs::write(&file, (0..5000u32).map(|i| (i * (n + 3) % 251) as u8).collect::<Vec<_>>())
            .unwrap();
        let id = library.add(&file, None, Metadata::default()).unwrap().id;
        library.generate(&id, "thumbnail", &helper).unwrap();
        ids.push(id);
    }
    // Not readable on the replica's machine: originals must come over HTTP.
    std::fs::remove_dir_all(&originals).unwrap();

    let token = vamp_server::token(&library.state_dir).unwrap();
    let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
    let upstream = server.server_addr().to_string();
    let app = vamp_server::App::new(library, "main").unwrap();
    let serving = {
        let server = server.clone();
        std::thread::spawn(move || vamp_server::serve(server, app, 8))
    };
    let proxy = Proxy::start(upstream);

    // Thumbnails 503 for a while: the downloader backs off and retries on its own (more
    // failures than the per-item attempt cap), with no further sync.
    let home = dir.path().join("replica");
    op(
        &home,
        json!({"operation":"remote_attach","name":"r","remote_url":proxy.url,"token":token}),
    )
    .unwrap();
    let started = Instant::now();
    loop {
        let status = op(&home, json!({"operation":"sync_status","context":"r"})).unwrap();
        if status["pending_thumbnails"] == 0 && status["downloading"] == false {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(60), "stuck: {status}");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        proxy.unavailable.load(Ordering::SeqCst) >= 2,
        "outage not exercised"
    );
    for id in &ids {
        let path = op(
            &home,
            json!({"operation":"representation","context":"r","asset_id":id,"kind":"thumbnail"}),
        )
        .unwrap()["path"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(std::fs::read(path).unwrap(), b"x");
    }

    // A stalled original: progress is visible and cancel ends the request promptly.
    proxy.stall_originals.store(true, Ordering::SeqCst);
    let fetch = {
        let (home, id) = (home.clone(), ids[0].clone());
        std::thread::spawn(move || {
            let started = Instant::now();
            let result = op(
                &home,
                json!({"operation":"representation","context":"r","asset_id":id,"kind":"original","transfer_id":"viewer-1"}),
            );
            (result.map_err(|e| format!("{e:#}")), started.elapsed())
        })
    };
    let progress = loop {
        let progress = op(
            &home,
            json!({"operation":"download_progress","context":"r","transfer_id":"viewer-1"}),
        )
        .unwrap();
        if progress["total"] == 100000 {
            break progress;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(progress, json!({"bytes":0,"total":100000,"active":true,"background":false}));
    std::thread::sleep(Duration::from_millis(500));
    let cancelled = op(
        &home,
        json!({"operation":"cancel_download","context":"r","transfer_id":"viewer-1"}),
    )
    .unwrap();
    assert_eq!(cancelled["cancelled"], true);
    let (result, elapsed) = fetch.join().unwrap();
    assert!(result.unwrap_err().contains("cancelled"));
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    let progress = op(
        &home,
        json!({"operation":"download_progress","context":"r","transfer_id":"viewer-1"}),
    )
    .unwrap();
    assert_eq!(progress["active"], false);

    // A preview prefetch is cancelled by its request id, in flight or before it reached the
    // engine, and leaves nothing in the cache.
    for (transfer, early) in [("prefetch-2", false), ("prefetch-3", true)] {
        if early {
            op(&home, json!({"operation":"cancel_download","context":"r","transfer_id":transfer})).unwrap();
        }
        let fetch = {
            let (home, id) = (home.clone(), ids[2].clone());
            std::thread::spawn(move || {
                let started = Instant::now();
                let result = op(
                    &home,
                    json!({"operation":"representation","context":"r","asset_id":id,"kind":"preview","transfer_id":transfer,"background":true}),
                );
                (result.map_err(|e| format!("{e:#}")), started.elapsed())
            })
        };
        if !early {
            while op(&home, json!({"operation":"download_progress","context":"r","transfer_id":transfer})).unwrap()["active"] != true {
                std::thread::sleep(Duration::from_millis(20));
            }
            std::thread::sleep(Duration::from_millis(300));
            op(&home, json!({"operation":"cancel_download","context":"r","transfer_id":transfer})).unwrap();
        }
        let (result, elapsed) = fetch.join().unwrap();
        assert!(result.unwrap_err().contains("cancelled"), "{transfer}");
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    }
    let previews = home.join("replicas");
    let leftovers: Vec<_> = walk(&previews).into_iter().filter(|p| p.to_string_lossy().contains("/cache/preview/")).collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // Nobody cancels: the stall timeout ends it.
    let replica = Library::open(&home, Some("r")).unwrap();
    let dest = dir.path().join("stalled.png");
    let started = Instant::now();
    let error = replica
        .remote_client()
        .unwrap()
        .download_with(
            &format!("/assets/{}/original", ids[1]),
            &dest,
            None,
            &vamp_core::remote::Transfer::default(),
            Duration::from_secs(1),
        )
        .unwrap_err();
    assert!(error.is::<vamp_core::remote::Stalled>(), "{error:#}");
    assert!(vamp_core::remote::is_transient(&error));
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    assert!(!dest.exists());

    proxy.stop.store(true, Ordering::SeqCst);
    for _ in 0..8 {
        server.unblock();
    }
    serving.join().unwrap();
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}
