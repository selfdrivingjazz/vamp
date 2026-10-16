//! `layout` lists exactly what `browse` pages through; the authority renders micro/grid
//! thumbnails and a replica receives micros with the catalog and grid WebPs via packs.
use base64::Engine as _;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use vamp_core::{Library, native};

const FAKE_MEDIA: &str = r#"#!/bin/sh
out=$(sed -n 's/.*"output_path":"\([^"]*\)".*/\1/p')
printf x > "$out"
printf '{"representation":{"path":"%s","width":1,"height":1,"mime":"image/jpeg","byte_size":1}}' "$out"
"#;

fn op(home: &Path, value: Value) -> Value {
    let mut value = value;
    value["home"] = json!(home);
    native::request(serde_json::from_value(value).unwrap()).unwrap()
}

fn jpeg(width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
    });
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
        .encode_image(&image)
        .unwrap();
    out
}

fn ids(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().map_or_else(|| v["id"].as_str().unwrap().to_owned(), str::to_owned))
        .collect()
}

#[test]
fn layout_matches_browse_and_thumbs_reach_replica() {
    let helper_dir = tempfile::tempdir().unwrap();
    let helper = helper_dir.path().join("fake-media");
    std::fs::write(&helper, FAKE_MEDIA).unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY: set before any library in this test binary reads it.
    unsafe { std::env::set_var("VAMP_MEDIA_HELPER", &helper) };

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let library = Library::init(&home, "main", None, None).unwrap();
    let names = ["delta", "alpha", "charlie", "bravo", "echo"];
    let mut assets = Vec::new();
    for (i, name) in names.iter().enumerate() {
        let bytes: Vec<u8> = (0..5_000u32).map(|b| (b * (i as u32 + 3) % 251) as u8).collect();
        let tags: &[&str] = if i % 2 == 0 { &["even"] } else { &[] };
        let id = op(
            &home,
            json!({"operation":"ingest","filename":format!("{name}.png"),"name":name,
                   "bytes_base64":base64::engine::general_purpose::STANDARD.encode(bytes),
                   "metadata":{"tags":tags}}),
        )["id"]
            .as_str()
            .unwrap()
            .to_owned();
        // A real (large) thumbnail in place of the fake helper's.
        std::fs::write(library.fetch(&id, "thumbnail").unwrap(), jpeg(1200, 800)).unwrap();
        assets.push(id);
    }

    // Parity: same ids, same order as paging through browse.
    for filter in [
        json!({}),
        json!({"sort":"name_asc"}),
        json!({"sort":"added_desc","tags":["even"]}),
        json!({"sort":"random","seed":7}),
        json!({"query":"alpha"}),
    ] {
        let mut request = filter.clone();
        request["operation"] = json!("layout");
        let layout = op(&home, request);
        let mut paged = Vec::new();
        for offset in (0..names.len()).step_by(2) {
            let mut request = filter.clone();
            request["operation"] = json!("browse");
            request["offset"] = json!(offset);
            request["limit"] = json!(2);
            paged.extend(ids(&op(&home, request)["items"]));
        }
        assert_eq!(ids(&layout["ids"]), paged, "{filter}");
        assert_eq!(layout["total"], paged.len());
        assert_eq!(layout["widths"].as_array().unwrap().len(), paged.len());
    }

    // Authority renders micro (inline in layout) and grid derivatives.
    assert_eq!(op(&home, json!({"operation":"generate_thumbs"}))["rendered"], names.len());
    let layout = op(&home, json!({"operation":"layout","micro":true}));
    for micro in layout["micro"].as_array().unwrap() {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(micro.as_str().expect("every asset has a micro"))
            .unwrap();
        assert!(bytes.starts_with(&[0xff, 0xd8]) && bytes.len() < 2048, "{}", bytes.len());
    }
    let cells = op(&home, json!({"operation":"items","asset_ids":[assets[1], "missing", assets[0]]}));
    assert_eq!(ids(&cells["items"]), vec![assets[1].clone(), assets[0].clone()]);
    let grid_path = cells["items"][0]["grid_path"].as_str().unwrap();
    assert!(grid_path.ends_with(".webp"), "{grid_path}");
    let grid = image::open(grid_path).unwrap();
    assert_eq!((grid.width(), grid.height()), (800, 533));

    // Replica: micros arrive with the catalog; thumbnails arrive as grid WebPs from packs.
    let token = vamp_server::token(&library.state_dir).unwrap();
    let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
    let url = format!("http://{}", server.server_addr());
    let app = vamp_server::App::new(library, "main").unwrap();
    let serving = {
        let server = server.clone();
        std::thread::spawn(move || vamp_server::serve(server, app, 4))
    };
    // Tier routes: grid WebP served from disk; preview 404 until rendered; packs carry WebP.
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let get = |path: &str| {
        agent
            .get(format!("{url}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .call()
            .unwrap()
    };
    let mut tier = get(&format!("/v1/assets/{}/tier/grid", assets[0]));
    assert_eq!(tier.status().as_u16(), 200);
    assert_eq!(tier.headers()["content-type"], "image/webp");
    let body = tier.body_mut().read_to_vec().unwrap();
    assert_eq!(&body[8..12], b"WEBP");
    let mut missing = get(&format!("/v1/assets/{}/tier/preview", assets[0]));
    assert_eq!(missing.status().as_u16(), 404);
    let error: Value = missing.body_mut().read_json().unwrap();
    assert_eq!(error["error"]["code"], "tier_missing");
    assert_eq!(get("/v1/assets/nope/tier/grid").status().as_u16(), 404);
    let stats: Value = get("/v1/tiers/stats").body_mut().read_json().unwrap();
    assert_eq!(stats["grid"]["count"], names.len());
    assert_eq!(stats["grid"]["pending"], 0);
    assert_eq!(stats["preview"]["count"], 0);
    assert_eq!(stats["preview"]["pending"], names.len());
    let pack = agent
        .post(format!("{url}/v1/thumbnails/pack"))
        .header("Authorization", format!("Bearer {token}"))
        .send_json(json!({"ids":[assets[0]],"kind":"grid"}))
        .unwrap()
        .body_mut()
        .read_to_vec()
        .unwrap();
    let entries = vamp_core::thumbs::decode_pack(&pack).unwrap();
    assert_eq!(entries[0].1, body, "pack serves the grid WebP");

    let replica = tempfile::tempdir().unwrap();
    op(
        replica.path(),
        json!({"operation":"remote_attach","name":"r","remote_url":url,"token":token}),
    );
    let mirrored = op(replica.path(), json!({"operation":"layout","context":"r","micro":true}));
    assert_eq!(mirrored["ids"], layout["ids"]);
    assert_eq!(mirrored["micro"], layout["micro"]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while op(replica.path(), json!({"operation":"sync_status","context":"r"}))["pending_thumbnails"] != 0 {
        assert!(std::time::Instant::now() < deadline, "thumbnails never landed");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let cells = op(replica.path(), json!({"operation":"items","context":"r","asset_ids":assets}));
    for cell in cells["items"].as_array().unwrap() {
        let thumb = image::ImageReader::open(cell["thumbnail_path"].as_str().unwrap())
            .unwrap()
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap();
        assert_eq!(thumb.width(), 800);
    }
    for _ in 0..4 {
        server.unblock();
    }
    serving.join().unwrap();
}
