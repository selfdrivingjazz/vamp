//! Progressive MP4 clients require byte-exact Range responses, including suffix reads.
use std::sync::Arc;
use std::time::Duration;
use vamp_core::Library;
use vamp_protocol::Metadata;

#[test]
fn video_original_supports_progressive_byte_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let mut library = Library::init(&dir.path().join("home"), "main", None, None).unwrap();
    let bytes: Vec<u8> = (0..65536).map(|i| (i % 251) as u8).collect();
    let input = dir.path().join("movie.mp4");
    std::fs::write(&input, &bytes).unwrap();
    let id = library.add(&input, None, Metadata::default()).unwrap().id;
    let token = vamp_server::token(&library.state_dir).unwrap();
    let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
    let url = format!("http://{}/v1/assets/{id}/original", server.server_addr());
    let app = vamp_server::App::new(library, "main").unwrap();
    let serving = {
        let server = server.clone();
        std::thread::spawn(move || vamp_server::serve(server, app, 2))
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false).timeout_global(Some(Duration::from_secs(5)))
        .build().into();
    let auth = format!("Bearer {token}");
    for (range, expected, content_range) in [
        ("bytes=0-1", &bytes[..2], "bytes 0-1/65536"),
        ("bytes=1024-", &bytes[1024..], "bytes 1024-65535/65536"),
        ("bytes=-16", &bytes[65520..], "bytes 65520-65535/65536"),
    ] {
        let mut response = agent.get(&url).header("Authorization", &auth).header("Range", range).call().unwrap();
        assert_eq!(response.status().as_u16(), 206);
        assert_eq!(response.headers()["content-type"], "video/mp4");
        assert_eq!(response.headers()["accept-ranges"], "bytes");
        assert_eq!(response.headers()["content-range"], content_range);
        assert_eq!(response.headers()["content-length"], expected.len().to_string());
        assert_eq!(response.body_mut().read_to_vec().unwrap(), expected);
    }
    let mut response = agent.get(&url).header("Authorization", &auth).call().unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.headers()["content-length"], "65536");
    assert_eq!(response.body_mut().read_to_vec().unwrap(), bytes);
    let response = agent.get(&url).header("Authorization", &auth).header("Range", "bytes=65536-").call().unwrap();
    assert_eq!(response.status().as_u16(), 416);
    assert_eq!(response.headers()["content-range"], "bytes */65536");
    for _ in 0..2 { server.unblock(); }
    serving.join().unwrap();
}
