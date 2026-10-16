//! Replica browsing over a slow link: a view (`touch`) comes back from the authority as a
//! `media_viewed` event that is applied without fetching the asset and reported by kind and
//! asset (so the app patches one cell instead of reloading); grid tiles the bulk download
//! got before the authority's grid tier existed are upgraded on demand; and the authority
//! renders its grid tier from the hires tier once that is larger than the thumbnail.
use base64::Engine as _;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use vamp_core::{Library, native, thumbs};

const FAKE_MEDIA: &str = r#"#!/bin/sh
out=$(sed -n 's/.*"output_path":"\([^"]*\)".*/\1/p')
printf x > "$out"
printf '{"representation":{"path":"%s","width":512,"height":341,"mime":"image/jpeg","byte_size":1}}' "$out"
"#;

fn op(home: &Path, value: Value) -> Value {
    let mut value = value;
    value["home"] = json!(home);
    native::request(serde_json::from_value(value).unwrap()).unwrap()
}

fn image(width: u32, height: u32) -> image::DynamicImage {
    image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 90])
    }))
}

fn jpeg(width: u32, height: u32) -> Vec<u8> {
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
        .encode_image(&image(width, height).to_rgb8())
        .unwrap();
    out
}

fn edge(path: &str) -> u32 {
    let (w, h) = image::image_dimensions(path).unwrap_or_else(|_| {
        image::ImageReader::open(path).unwrap().with_guessed_format().unwrap().into_dimensions().unwrap()
    });
    w.max(h)
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < Duration::from_secs(30), "timed out: {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn views_patch_in_place_and_grid_tiles_upgrade_on_demand() {
    let helper_dir = tempfile::tempdir().unwrap();
    let helper = helper_dir.path().join("fake-media");
    std::fs::write(&helper, FAKE_MEDIA).unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY: set before any library in this test binary reads it.
    unsafe { std::env::set_var("VAMP_MEDIA_HELPER", &helper) };

    let dir = tempfile::tempdir().unwrap();
    let auth_home = dir.path().join("authority");
    let library = Library::init(&auth_home, "main", None, None).unwrap();
    let mut assets = Vec::new();
    for i in 0..3u32 {
        let bytes: Vec<u8> = (0..5_000u32).map(|b| (b * (i + 3) % 251) as u8).collect();
        let id = op(
            &auth_home,
            json!({"operation":"ingest","filename":format!("{i}.png"),
                   "bytes_base64":base64::engine::general_purpose::STANDARD.encode(bytes)}),
        )["id"]
            .as_str()
            .unwrap()
            .to_owned();
        // A real 512 px thumbnail (what an Eagle library provides).
        std::fs::write(library.fetch(&id, "thumbnail").unwrap(), jpeg(512, 341)).unwrap();
        assets.push(id);
    }

    let token = vamp_server::token(&library.state_dir).unwrap();
    let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
    let url = format!("http://{}", server.server_addr());
    let app = vamp_server::App::new(library, "main").unwrap();
    let serving = {
        let server = server.clone();
        std::thread::spawn(move || vamp_server::serve(server, app, 4))
    };

    // The replica's bulk download runs before the authority rendered any grid tier: its
    // tiles are the 512 px thumbnails.
    let home = dir.path().join("replica");
    op(&home, json!({"operation":"remote_attach","name":"r","remote_url":url,"token":token}));
    wait_until("tiles", || {
        let status = op(&home, json!({"operation":"sync_status","context":"r"}));
        status["pending_thumbnails"] == 0 && status["downloading"] == false
    });
    let cells = op(&home, json!({"operation":"items","context":"r","asset_ids":assets}));
    for cell in cells["items"].as_array().unwrap() {
        assert_eq!(edge(cell["thumbnail_path"].as_str().unwrap()), 512);
        assert_eq!(cell["grid_path"], Value::Null);
    }

    // No grid tier yet: checked once, then not asked again for a while.
    let first = op(&home, json!({"operation":"fetch_grid","context":"r","asset_ids":assets[..1]}));
    assert_eq!((first["checked"].clone(), first["upgraded"].clone()), (json!(1), json!([])));
    let again = op(&home, json!({"operation":"fetch_grid","context":"r","asset_ids":assets[..1]}));
    assert_eq!(again["checked"], 0);

    // The authority renders grids (from the 512 px thumbnails), then hires tiers land and the
    // grids that were smaller are rendered again from them.
    let mut authority = Library::open(&auth_home, None).unwrap();
    assert_eq!(thumbs::generate_all(&mut authority).unwrap()["rendered"], 3);
    let sources = thumbs::pending_hires(&authority, 8).unwrap();
    assert_eq!(sources.len(), 3);
    let outcomes: Vec<_> = sources
        .iter()
        .map(|_| {
            let bytes = thumbs::webp(&image(1600, 1066), 80.0).unwrap();
            thumbs::HiresOutcome::Rendered { bytes, width: 1600, height: 1066, from: "original" }
        })
        .collect();
    thumbs::store_hires(&mut authority, &sources, &outcomes).unwrap();
    assert_eq!(thumbs::pending_count(&authority).unwrap(), 3);
    assert_eq!(thumbs::generate_all(&mut authority).unwrap()["rendered"], 3);
    let grid = thumbs::tier_path(&authority, &assets[1], "grid").unwrap().unwrap();
    assert_eq!(edge(grid.to_str().unwrap()), 800);

    // Visible tiles upgrade to the authority's grid tier; items list it as the grid path.
    // The first asset was checked recently and waits for its recheck.
    let upgraded = op(&home, json!({"operation":"fetch_grid","context":"r","asset_ids":assets,"transfer_id":"grid-1","background":true}));
    assert_eq!(upgraded["upgraded"], json!([assets[1], assets[2]]), "{upgraded}");
    assert!(upgraded["bytes"].as_u64().unwrap() > 0);
    let cells = op(&home, json!({"operation":"items","context":"r","asset_ids":assets[1..]}));
    for cell in cells["items"].as_array().unwrap() {
        assert_eq!(edge(cell["grid_path"].as_str().unwrap()), 800);
    }
    let done = op(&home, json!({"operation":"fetch_grid","context":"r","asset_ids":assets}));
    assert_eq!(done["checked"], 0);
    // Already-upgraded ids are reported as local (a cancelled batch's cells catch up).
    assert_eq!(done["local"], json!([assets[1], assets[2]]));
    assert_eq!(done["upgraded"], json!([]));

    // A view: recorded locally at once, forwarded, and echoed back as `media_viewed`
    // carrying its time: the sync reports it by kind and asset and needs no view fetch.
    op(&home, json!({"operation":"sync","context":"r"}));
    op(&home, json!({"operation":"touch","context":"r","asset_id":assets[2]}));
    wait_until("view reaches the authority", || {
        authority.inspect(&assets[2]).unwrap().asset.last_viewed_at.is_some()
    });
    let synced = op(&home, json!({"operation":"sync","context":"r"}));
    assert_eq!(synced["applied"], 1, "{synced}");
    assert_eq!(synced["kinds"], json!({"media_viewed": 1}));
    assert_eq!(synced["asset_ids"], json!([assets[2]]));
    let mirrored = op(&home, json!({"operation":"inspect","context":"r","asset_id":assets[2]}));
    let viewed = authority.inspect(&assets[2]).unwrap().asset.last_viewed_at.unwrap();
    assert_eq!(mirrored["asset"]["last_viewed_at"], json!(viewed));
    assert_eq!(op(&home, json!({"operation":"sidebar","context":"r"}))["recently_used"], 1);

    for _ in 0..4 {
        server.unblock();
    }
    serving.join().unwrap();
}
