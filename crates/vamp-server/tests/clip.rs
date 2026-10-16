//! Browser-clipper routes: image clips cataloged at once with their originals fetched in
//! the background (provisional thumbnail, failure state, client upload), folder filing
//! (`collection_id`), `POST /v1/clip/batch`, and the page sent as `Referer`.
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use vamp_core::Library;

/// A media helper that takes two seconds per render.
const SLOW_MEDIA: &str = r#"#!/bin/sh
out=$(sed -n 's/.*"output_path":"\([^"]*\)".*/\1/p')
sleep 2
printf x > "$out"
printf '{"representation":{"path":"%s","width":1,"height":1,"mime":"image/jpeg","byte_size":1}}' "$out"
"#;

/// 1×1 PNG.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
    0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
    0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xfc,
    0xcf, 0xc0, 0xf0, 0x1f, 0x00, 0x05, 0x05, 0x02, 0x00, 0x5f, 0xc8, 0xf1, 0xd2, 0x00, 0x00,
    0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

struct Fixture {
    url: String,
    auth: String,
    agent: ureq::Agent,
    images: String,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let helper = dir.path().join("slow-media");
        std::fs::write(&helper, SLOW_MEDIA).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        // SAFETY: the only test in this binary; set before the server reads it.
        unsafe { std::env::set_var("VAMP_MEDIA_HELPER", &helper) };
        let library = Library::init(&dir.path().join("home"), "main", None, None).unwrap();
        let token = vamp_server::token(&library.state_dir).unwrap();
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let url = format!("http://{}", server.server_addr());
        let app = vamp_server::App::new(library, "main").unwrap();
        std::thread::spawn(move || vamp_server::serve(server, app, 4));
        Self {
            url,
            auth: format!("Bearer {token}"),
            agent: ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_global(Some(Duration::from_secs(20)))
                .build()
                .into(),
            images: image_host(),
            _dir: dir,
        }
    }

    fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let mut response = self
            .agent
            .post(format!("{}{path}", self.url))
            .header("Authorization", &self.auth)
            .send_json(body)
            .unwrap();
        (response.status().as_u16(), response.body_mut().read_json().unwrap())
    }

    fn get(&self, path: &str) -> Value {
        let mut response = self
            .agent
            .get(format!("{}{path}", self.url))
            .header("Authorization", &self.auth)
            .call()
            .unwrap();
        assert_eq!(response.status().as_u16(), 200, "GET {path}");
        response.body_mut().read_json().unwrap()
    }

    fn members(&self, collection_id: &str) -> Vec<String> {
        let collections = self.get("/v1/collections");
        let folder = collections
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == collection_id)
            .unwrap();
        serde_json::from_value(folder["asset_ids"].clone()).unwrap()
    }

    fn has_thumbnail(&self, id: &str) -> bool {
        self.get(&format!("/v1/assets/{id}"))["representations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["kind"] == "thumbnail")
    }
}

/// Serves `/a.png` and `/b.png` to anyone, `/slow.png` after 3 s, `/hotlinked.png` only
/// with a Referer from `https://example.com/`, and 404 otherwise.
fn image_host() -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}", server.server_addr());
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            std::thread::spawn(move || {
                let referer = request
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("Referer"))
                    .map(|h| h.value.as_str().to_owned());
                let allowed = match request.url() {
                    "/a.png" | "/b.png" => true,
                    "/slow.png" => {
                        std::thread::sleep(Duration::from_secs(3));
                        true
                    }
                    "/hotlinked.png" => referer.is_some_and(|r| r.starts_with("https://example.com/")),
                    _ => false,
                };
                let response = if allowed {
                    tiny_http::Response::from_data(PNG.to_vec()).with_header(
                        tiny_http::Header::from_bytes("Content-Type", "image/png").unwrap(),
                    )
                } else {
                    tiny_http::Response::from_data(b"no".to_vec()).with_status_code(404)
                };
                let _ = request.respond(response);
            });
        }
    });
    url
}

impl Fixture {
    fn send(&self, path: &str, bytes: &[u8]) -> (u16, Value) {
        let mut response = self
            .agent
            .post(format!("{}{path}", self.url))
            .header("Authorization", &self.auth)
            .header("Content-Type", "application/octet-stream")
            .send(bytes)
            .unwrap();
        (response.status().as_u16(), response.body_mut().read_json().unwrap())
    }
    fn status(&self, path: &str) -> u16 {
        self.agent
            .get(format!("{}{path}", self.url))
            .header("Authorization", &self.auth)
            .call()
            .unwrap()
            .status()
            .as_u16()
    }
    fn asset(&self, id: &str) -> Value {
        self.get(&format!("/v1/assets/{id}"))
    }
    /// Waits until the asset's original storage is `storage`.
    fn settle(&self, id: &str, storage: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let view = self.asset(id);
            if view["asset"]["original"]["storage"] == storage {
                return view;
            }
            assert!(Instant::now() < deadline, "{id} never became {storage}: {view}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[test]
fn clips_are_cataloged_at_once_and_their_originals_resolve_in_the_background() {
    let f = Fixture::new();
    let (status, folder) = f.post("/v1/collections", json!({"name":"Clips"}));
    assert_eq!(status, 200);
    let folder = folder["id"].as_str().unwrap().to_owned();
    let (_, other) = f.post("/v1/collections", json!({"name":"Other"}));
    let other = other["id"].as_str().unwrap().to_owned();

    // An empty batch is the clipper's capability probe.
    assert_eq!(
        f.post("/v1/clip/batch", json!({"items":[]})),
        (200, json!({"results":[],"instant":true}))
    );

    // Unknown folders fail before anything is stored.
    let (status, error) = f.post(
        "/v1/clip",
        json!({"url":"https://example.com/p","image_url":format!("{}/a.png", f.images),"collection_id":"nope"}),
    );
    assert_eq!(status, 404, "{error}");
    let (status, _) = f.post(
        "/v1/clip/batch",
        json!({"items":[{"url":"https://example.com/p"}],"collection_id":"nope"}),
    );
    assert_eq!(status, 404);
    assert_eq!(f.get("/v1/search").as_array().unwrap().len(), 0);

    // A slow image host: the clip answers at once with the asset cataloged and filed, its
    // original honestly pending; a provisional thumbnail can be attached meanwhile.
    let started = Instant::now();
    let (status, slow) = f.post(
        "/v1/clip",
        json!({"url":"https://example.com/single","title":"Single","image_url":format!("{}/slow.png", f.images),"tags":["t1"],"collection_id":folder,"width":640,"height":480}),
    );
    assert_eq!(status, 200, "{slow}");
    assert!(started.elapsed() < Duration::from_millis(1000), "clip waited on the download");
    let me = vamp_core::current_actor();
    assert_eq!(slow["metadata"]["extra"]["added_by"], json!(me));
    assert_eq!(slow["original"]["storage"], "fetching");
    assert_eq!(slow["original"]["hash_state"], "pending");
    assert_eq!(slow["original"]["path"], "");
    assert_eq!(slow["original"]["locator"], format!("{}/slow.png", f.images));
    assert_eq!((slow["width"].clone(), slow["height"].clone()), (json!(640), json!(480)));
    assert_eq!(slow["name"], "Single");
    let slow = slow["id"].as_str().unwrap().to_owned();
    assert!(f.members(&folder).contains(&slow));
    assert_eq!(f.status(&format!("/v1/assets/{slow}/original")), 503);
    let (status, rep) = f.send(&format!("/v1/assets/{slow}/preview"), PNG);
    assert_eq!(status, 200, "{rep}");
    assert!(rep["path"].as_str().unwrap().starts_with("representations/provisional/"));
    assert!(f.has_thumbnail(&slow), "provisional thumbnail not served");
    assert_eq!(f.status(&format!("/v1/assets/{slow}/thumbnail")), 200);

    // Batch: everything cataloged in request order at once; downloads follow.
    let started = Instant::now();
    let (status, batch) = f.post(
        "/v1/clip/batch",
        json!({
            "collection_id": folder,
            "items": [
                {"url":"https://example.com/gallery","title":"A","image_url":format!("{}/a.png", f.images),"tags":["batch"]},
                {"url":"https://example.com/gallery","image_url":format!("{}/hotlinked.png", f.images)},
                {"url":"https://example.com/gallery","image_url":format!("{}/missing.png", f.images)},
                {"url":"https://example.com/gallery","image_url":format!("{}/b.png", f.images),"collection_id":other},
                {"url":"https://example.com/article","title":"Article"},
                {"url":"ftp://example.com/x","image_url":"ftp://example.com/x.png"},
            ]
        }),
    );
    assert_eq!(status, 200, "{batch}");
    assert!(started.elapsed() < Duration::from_millis(1000), "batch waited on downloads");
    let results = batch["results"].as_array().unwrap();
    assert_eq!(results.len(), 6);
    let id = |i: usize| results[i]["asset"]["id"].as_str().unwrap_or_else(|| panic!("{}", results[i])).to_owned();
    let (a, hotlinked, missing, b, article) = (id(0), id(1), id(2), id(3), id(4));
    assert_eq!(results[5]["error"]["code"], "bad_request");
    assert_eq!(results[0]["asset"]["metadata"]["source_url"], "https://example.com/gallery");
    assert_eq!(results[0]["asset"]["metadata"]["tags"], json!(["batch"]));
    assert_eq!(results[0]["asset"]["name"], "A");
    assert_eq!(results[1]["asset"]["name"], "hotlinked.png");
    assert_eq!(results[4]["asset"]["original"]["storage"], "local", "bookmarks are stored whole");
    let mut filed = f.members(&folder);
    filed.sort();
    let mut expected = vec![slow.clone(), a.clone(), hotlinked.clone(), missing.clone(), article];
    expected.sort();
    assert_eq!(filed, expected);
    assert_eq!(f.members(&other), vec![b.clone()]);

    // Originals land (the page went along as Referer), then real thumbnails render.
    for id in [&a, &hotlinked, &b, &slow] {
        let view = f.settle(id, "local");
        let original = &view["asset"]["original"];
        assert_eq!(original["hash_state"], "hashed");
        assert_eq!(original["byte_size"], PNG.len());
        assert_eq!(original["mime"], "image/png");
        assert!(original["path"].as_str().unwrap().ends_with(".png"));
        assert_eq!(view["asset"]["revision"], 2);
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    for id in [&a, &hotlinked, &slow] {
        loop {
            let reps = f.asset(id)["representations"].clone();
            let real = reps.as_array().unwrap().iter().any(|r| {
                r["kind"] == "thumbnail" && !r["path"].as_str().unwrap().contains("provisional")
            });
            if real {
                break;
            }
            assert!(Instant::now() < deadline, "no real thumbnail for {id}: {reps}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    assert_eq!(f.status(&format!("/v1/assets/{slow}/original")), 200);
    let (status, _) = f.send(&format!("/v1/assets/{slow}/preview"), PNG);
    assert_eq!(status, 409, "no provisional thumbnail once the original landed");

    // A download that fails stays visible with its reason; the client's bytes complete it.
    let failed = f.settle(&missing, "failed");
    let reason = failed["asset"]["metadata"]["extra"]["original_error"].as_str().unwrap().to_owned();
    assert!(reason.contains("missing.png"), "{reason}");
    assert_eq!(f.status(&format!("/v1/assets/{missing}/original")), 404);
    let (status, landed) = f.send(&format!("/v1/assets/{missing}/original?filename=missing.png"), PNG);
    assert_eq!(status, 200, "{landed}");
    assert_eq!(landed["original"]["storage"], "local");
    assert!(landed["metadata"]["extra"].get("original_error").is_none());
    let (status, _) = f.send(&format!("/v1/assets/{missing}/original?filename=again.png"), PNG);
    assert_eq!(status, 409, "an original lands once");

    // Raw upload (the clipper's screenshot path), filed, renditions after the reply.
    let mut response = f
        .agent
        .post(format!(
            "{}/v1/ingest?filename=shot.png&name=Shot&source_url=https://example.com/s&collection_id={other}",
            f.url
        ))
        .header("Authorization", &f.auth)
        .header("Content-Type", "image/png")
        .send(PNG)
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let shot: Value = response.body_mut().read_json().unwrap();
    assert!(f.members(&other).contains(&shot["id"].as_str().unwrap().to_owned()));

    // Event order a replica sees for the slow clip: added (pending), provisional
    // thumbnail, original landed, real renditions.
    let events = f.get("/v1/events?after=0&limit=1000");
    let kinds: Vec<String> = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["asset_id"] == slow.as_str())
        .map(|e| e["kind"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(kinds[0], "media_added", "{kinds:?}");
    assert!(kinds.iter().any(|k| k == "media_updated"), "{kinds:?}");
    assert!(kinds.iter().filter(|k| *k == "representation_ready").count() >= 2, "{kinds:?}");
}
