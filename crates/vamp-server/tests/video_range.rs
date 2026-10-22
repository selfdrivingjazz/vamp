//! Progressive MP4 clients require byte-exact Range responses, including suffix reads.
use std::sync::Arc;
use std::time::Duration;
use vamp_core::Library;
use vamp_protocol::Metadata;

#[test]
#[cfg(unix)]
fn video_original_supports_progressive_byte_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let mut library = Library::init(&dir.path().join("home"), "main", None, None).unwrap();
    let bytes: Vec<u8> = (0..65536).map(|i| (i % 251) as u8).collect();
    let input = dir.path().join("movie.mp4");
    std::fs::write(&input, &bytes).unwrap();
    let id = library.add(&input, None, Metadata::default()).unwrap().id;
    use std::os::unix::fs::PermissionsExt;
    let helper = dir.path().join("media");
    std::fs::write(&helper, r#"#!/bin/sh
out=$(sed -n 's/.*"output_path":"\([^"]*\)".*/\1/p')
printf abc > "$out"
printf '{"representation":{"path":"%s","width":480,"height":320,"mime":"video/mp4","byte_size":3}}' "$out"
"#).unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let rep = library.generate(&id, "loop", &helper).unwrap();
    assert!(rep.path.ends_with(".mp4"));
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
    let loop_url = url.replace("/original", "/tier/loop");
    let mut response = agent.get(&loop_url).header("Authorization", &auth).header("Range", "bytes=0-1").call().unwrap();
    assert_eq!(response.status().as_u16(), 206);
    assert_eq!(response.headers()["content-type"], "video/mp4");
    assert_eq!(response.headers()["content-length"], "2");
    assert_eq!(response.headers()["content-range"], "bytes 0-1/3");
    assert_eq!(response.headers()["accept-ranges"], "bytes");
    assert_eq!(response.body_mut().read_to_vec().unwrap(), b"ab");
    let mut missing = agent.get(loop_url.replace(&id, "missing")).header("Authorization", &auth).call().unwrap();
    assert_eq!(missing.status().as_u16(), 404);
    let missing: serde_json::Value = missing.body_mut().read_json().unwrap();
    assert_eq!(missing["error"]["code"], "tier_missing");
    for _ in 0..2 { server.unblock(); }
    serving.join().unwrap();
}
