use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use vamp_core::Library;
use vamp_protocol::Metadata;

/// A helper that takes seconds (like hydrating a Drive placeholder) before rendering.
const SLOW_MEDIA: &str = r#"#!/bin/sh
out=$(sed -n 's/.*"output_path":"\([^"]*\)".*/\1/p')
sleep 3
printf x > "$out"
printf '{"representation":{"path":"%s","width":1,"height":1,"mime":"image/jpeg","byte_size":1}}' "$out"
"#;

/// Generating a missing preview on demand must not hold the library: other requests
/// answer meanwhile, and concurrent requests for the same preview share one render.
#[test]
fn on_demand_generation_does_not_block_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let helper = dir.path().join("slow-media");
    std::fs::write(&helper, SLOW_MEDIA).unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY: the only test in this binary; set before the server reads it.
    unsafe { std::env::set_var("VAMP_MEDIA_HELPER", &helper) };
    let originals = dir.path().join("originals");
    std::fs::create_dir_all(&originals).unwrap();
    let file = originals.join("slow.png");
    std::fs::write(&file, (0..50_000u32).map(|i| (i % 251) as u8).collect::<Vec<_>>()).unwrap();
    let mut library = Library::init(&dir.path().join("home"), "main", None, None).unwrap();
    let id = library.add(&file, None, Metadata::default()).unwrap().id;

    let token = vamp_server::token(&library.state_dir).unwrap();
    let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
    let url = format!("http://{}", server.server_addr());
    let app = vamp_server::App::new(library, "main").unwrap();
    let serving = {
        let server = server.clone();
        std::thread::spawn(move || vamp_server::serve(server, app, 4))
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .into();
    let auth = format!("Bearer {token}");
    let preview = || {
        let (agent, url, auth) = (agent.clone(), format!("{url}/v1/assets/{id}/preview"), auth.clone());
        std::thread::spawn(move || {
            let mut response = agent.get(url).header("Authorization", auth).call().unwrap();
            (response.status().as_u16(), response.body_mut().read_to_vec().unwrap())
        })
    };
    let started = Instant::now();
    let first = preview();
    let second = preview();
    std::thread::sleep(Duration::from_millis(500));
    let asked = Instant::now();
    let response = agent
        .get(format!("{url}/v1/library"))
        .header("Authorization", &auth)
        .call()
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert!(
        asked.elapsed() < Duration::from_secs(1),
        "library answered only after generation: {:?}",
        asked.elapsed()
    );
    assert!(!first.is_finished(), "generation should still be running");
    for request in [first, second] {
        assert_eq!(request.join().unwrap(), (200, b"x".to_vec()));
    }
    // Shared render: two concurrent requests took one helper run, not two in sequence.
    assert!(started.elapsed() < Duration::from_millis(5500));

    for _ in 0..4 {
        server.unblock();
    }
    serving.join().unwrap();
}
