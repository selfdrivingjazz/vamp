use std::sync::Arc;
use vamp_core::Library;

fn get(url: &str) -> (u16, String) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let mut response = agent.get(url).call().unwrap();
    let status = response.status().as_u16();
    (status, response.body_mut().read_to_string().unwrap())
}

/// Installer and release files are public while the library API still demands auth; the
/// installer points back at the address the client used, and paths cannot escape the dist dir.
#[test]
fn dist_routes_are_public_and_confined_while_v1_requires_auth() {
    let home = tempfile::tempdir().unwrap();
    let dist = tempfile::tempdir().unwrap();
    std::fs::create_dir(dist.path().join("x86_64-unknown-linux-gnu")).unwrap();
    std::fs::write(
        dist.path().join("x86_64-unknown-linux-gnu/SHA256SUMS"),
        "abc  vamp\n",
    )
    .unwrap();
    std::fs::write(home.path().join("secret"), "nope").unwrap();
    let library = Library::init(home.path(), "main", None, None).unwrap();
    let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
    let base = format!("http://{}", server.server_addr());
    let app = vamp_server::App::new(library, "main")
        .unwrap()
        .with_dist(dist.path().to_owned());
    std::thread::spawn({
        let server = server.clone();
        move || vamp_server::serve(server, app, 1)
    });

    let (status, script) = get(&format!("{base}/install.sh"));
    assert_eq!(status, 200);
    assert!(script.contains(&format!("base='{base}'")), "{script}");
    assert!(!script.contains("@BASE@"));

    let (status, sums) = get(&format!("{base}/dist/x86_64-unknown-linux-gnu/SHA256SUMS"));
    assert_eq!((status, sums.as_str()), (200, "abc  vamp\n"));
    assert_eq!(get(&format!("{base}/dist/missing")).0, 404);
    assert_eq!(get(&format!("{base}/dist/%2e%2e/secret")).0, 404);
    assert_eq!(get(&format!("{base}/v1/library")).0, 401);
}
