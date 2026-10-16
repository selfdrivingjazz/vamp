use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use vamp_core::Library;
use vamp_protocol::Metadata;

const FAKE_MEDIA: &str = r#"#!/bin/sh
out=$(sed -n 's/.*"output_path":"\([^"]*\)".*/\1/p')
printf x > "$out"
printf '{"representation":{"path":"%s","width":1,"height":1,"mime":"image/jpeg","byte_size":1}}' "$out"
"#;

const STUCK: usize = 6;

/// An original whose `open()` never returns (a FIFO without a writer, like a stalled Google
/// Drive File Provider): catalog endpoints stay fast while requests for it pile up, those
/// answer 503 `original_unavailable` after the deadline, and once the stall clears the
/// original is served again.
#[test]
fn stalled_original_storage_does_not_block_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let helper = dir.path().join("fake-media");
    std::fs::write(&helper, FAKE_MEDIA).unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let originals = dir.path().join("originals");
    std::fs::create_dir_all(&originals).unwrap();
    let file = originals.join("drive.png");
    let bytes: Vec<u8> = (0..100_000u32).map(|i| (i * 7 % 251) as u8).collect();
    std::fs::write(&file, &bytes).unwrap();
    let mut library = Library::init(&dir.path().join("home"), "main", None, None).unwrap();
    let id = library.add(&file, None, Metadata::default()).unwrap().id;
    library.generate(&id, "thumbnail", &helper).unwrap();
    // The cloud provider stalls: opening the original blocks until a writer appears.
    std::fs::remove_file(&file).unwrap();
    let status = std::process::Command::new("mkfifo")
        .arg(&file)
        .status()
        .unwrap();
    assert!(status.success());

    let token = vamp_server::token(&library.state_dir).unwrap();
    let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
    let url = format!("http://{}", server.server_addr());
    let deadline = Duration::from_secs(2);
    let app = vamp_server::App::new(library, "main")
        .unwrap()
        .with_io_deadline(deadline);
    // Fewer HTTP workers than stuck requests: they must not hold workers.
    let workers = 4;
    let serving = {
        let server = server.clone();
        std::thread::spawn(move || vamp_server::serve(server, app, workers))
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .into();
    let auth = format!("Bearer {token}");
    let original = |agent: ureq::Agent| {
        let (url, auth) = (format!("{url}/v1/assets/{id}/original"), auth.clone());
        std::thread::spawn(move || {
            let started = Instant::now();
            let mut response = agent.get(url).header("Authorization", auth).call().unwrap();
            let retry = response
                .headers()
                .get("Retry-After")
                .map(|v| v.to_str().unwrap().to_owned());
            let body = response.body_mut().read_to_vec().unwrap();
            (response.status().as_u16(), retry, body, started.elapsed())
        })
    };
    let stuck: Vec<_> = (0..STUCK).map(|_| original(agent.clone())).collect();
    std::thread::sleep(Duration::from_millis(400));

    let timed = |name: &str, request: ureq::RequestBuilder<ureq::typestate::WithoutBody>| {
        let started = Instant::now();
        let mut response = request.header("Authorization", &auth).call().unwrap();
        let body = response.body_mut().read_to_vec().unwrap();
        assert_eq!(response.status().as_u16(), 200, "{name}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{name} took {:?} while originals are stuck",
            started.elapsed()
        );
        body
    };
    timed("hello", agent.get(format!("{url}/v1/hello")));
    timed("snapshot", agent.get(format!("{url}/v1/snapshot")));
    let started = Instant::now();
    let mut pack = agent
        .post(format!("{url}/v1/thumbnails/pack"))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({"ids":[id],"kind":"grid"}))
        .unwrap();
    assert_eq!(pack.status().as_u16(), 200);
    assert!(!pack.body_mut().read_to_vec().unwrap().is_empty());
    assert!(started.elapsed() < Duration::from_secs(1), "pack: {:?}", started.elapsed());
    timed("health", agent.get(format!("{url}/v1/health")));

    for request in stuck {
        let (status, retry, body, elapsed) = request.join().unwrap();
        assert_eq!(status, 503);
        assert_eq!(retry.as_deref(), Some("30"));
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "original_unavailable", "{body}");
        assert_eq!(body["error"]["retry_after"], 30);
        assert!(elapsed >= deadline && elapsed < deadline * 3, "{elapsed:?}");
    }
    // The opens stay parked on the I/O pool, past their deadline.
    let health: Value =
        serde_json::from_slice(&timed("health", agent.get(format!("{url}/v1/health")))).unwrap();
    assert_eq!(health["status"], "degraded", "{health}");
    assert_eq!(health["io"]["requests"]["busy"], STUCK, "{health}");
    assert_eq!(health["io"]["requests"]["stalled"], STUCK, "{health}");
    assert_eq!(health["subsystems"]["media"]["busy"], 0, "{health}");
    timed("hello", agent.get(format!("{url}/v1/hello")));

    // The stall clears: parked opens return (to callers that gave up) and the file is back.
    drop(
        std::fs::OpenOptions::new()
            .write(true)
            .open(&file)
            .unwrap(),
    );
    let clear = Instant::now();
    loop {
        let health: Value = serde_json::from_slice(&timed(
            "health",
            agent.get(format!("{url}/v1/health")),
        ))
        .unwrap();
        if health["io"]["requests"]["busy"] == 0 {
            break;
        }
        assert!(clear.elapsed() < Duration::from_secs(5), "{health}");
        std::thread::sleep(Duration::from_millis(50));
    }
    std::fs::remove_file(&file).unwrap();
    std::fs::write(&file, &bytes).unwrap();
    let (status, _, body, _) = original(agent.clone()).join().unwrap();
    assert_eq!(status, 200);
    assert_eq!(body, bytes);
    let health: Value =
        serde_json::from_slice(&timed("health", agent.get(format!("{url}/v1/health")))).unwrap();
    assert_eq!(health["status"], "ok", "{health}");

    for _ in 0..workers {
        server.unblock();
    }
    serving.join().unwrap();
}
