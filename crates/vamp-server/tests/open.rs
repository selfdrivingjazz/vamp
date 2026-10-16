use std::sync::Arc;
use vamp_core::Library;

fn get(url: &str) -> (u16, String, String) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into();
    let mut response = agent.get(url).call().unwrap();
    let status = response.status().as_u16();
    let kind = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    (status, kind, response.body_mut().read_to_string().unwrap())
}

/// `/open` bounces http(s)-only link handlers to the app's vampire:// deeplink without any
/// authentication (the API itself still demands it) and only for UUID-shaped ids.
#[test]
fn open_bounces_to_the_app_deeplink_without_auth() {
    let home = tempfile::tempdir().unwrap();
    let library = Library::init(home.path(), "main", None, None).unwrap();
    let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
    let base = format!("http://{}", server.server_addr());
    let app = vamp_server::App::new(library, "main").unwrap();
    std::thread::spawn({
        let server = server.clone();
        move || vamp_server::serve(server, app, 1)
    });

    let (library, folder, asset) = (
        "6f1c0e2a-3b4d-4e5f-8a9b-0c1d2e3f4a5b",
        "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d",
        "ffffffff-0000-4000-8000-123456789abc",
    );
    let (status, kind, page) = get(&format!("{base}/open?library={library}&folder={folder}&asset={asset}"));
    assert_eq!((status, kind.as_str()), (200, "text/html; charset=utf-8"));
    let target = format!("vampire://open?library={library}&amp;folder={folder}&amp;asset={asset}");
    assert!(page.contains(&format!("content=\"0; url={target}\"")), "{page}");
    assert!(page.contains(&format!("<a href=\"{target}\">")), "{page}");

    let (status, _, page) = get(&format!("{base}/open?library={library}&folder={folder}"));
    assert_eq!(status, 200);
    assert!(page.contains(&format!("vampire://open?library={library}&amp;folder={folder}\"")), "{page}");

    for bad in [
        format!("library={library}"),
        format!("library={library}&folder=x"),
        format!("library={library}&folder={folder}&asset=%22%3E%3Cscript%3E"),
        format!("library={library}&folder={folder}&asset=../../etc"),
        String::new(),
    ] {
        let (status, kind, _) = get(&format!("{base}/open?{bad}"));
        assert_eq!((status, kind.as_str()), (400, "application/json"), "{bad}");
    }
    assert_eq!(get(&format!("{base}/v1/library")).0, 401);
}
