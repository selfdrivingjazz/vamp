use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use vamp_core::{Backend, Library, StorageConfig};
use vamp_protocol::Metadata;

const FAKE_MEDIA: &str = r#"#!/bin/sh
out=$(sed -n 's/.*"output_path":"\([^"]*\)".*/\1/p')
printf x > "$out"
printf '{"representation":{"path":"%s","width":1,"height":1,"mime":"image/jpeg","byte_size":1}}' "$out"
"#;

/// A cloud-only original whose backend read blocks (FIFO) must not stall other requests,
/// and concurrent requests for it must share one verified download.
#[test]
fn slow_cloud_download_does_not_block_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let originals = dir.path().join("originals");
    let cloud = dir.path().join("cloud");
    std::fs::create_dir_all(&originals).unwrap();
    std::fs::create_dir_all(&cloud).unwrap();
    let helper = dir.path().join("fake-media");
    std::fs::write(&helper, FAKE_MEDIA).unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut library =
        Library::init(&dir.path().join("home"), "main", Some(&originals), None).unwrap();
    library
        .storage_set(StorageConfig {
            backend: Backend::Folder {
                root: cloud.to_string_lossy().into(),
            },
            delete_local_after_upload: true,
            delete_local_anywhere: false,
        })
        .unwrap();
    let bytes: Vec<u8> = (0..300_000u32).map(|i| (i * 13 % 251) as u8).collect();
    let file = originals.join("slow.png");
    std::fs::write(&file, &bytes).unwrap();
    let id = library.add(&file, None, Metadata::default()).unwrap().id;
    library.generate(&id, "thumbnail", &helper).unwrap();
    library.generate(&id, "preview", &helper).unwrap();
    library.run_jobs(10).unwrap();
    assert!(!file.exists(), "original should be evicted");
    let locator = library
        .inspect(&id)
        .unwrap()
        .asset
        .original
        .locator
        .unwrap();
    let object = cloud.join(locator.strip_prefix("folder://default/").unwrap());
    // Throttled backend: reads block until the test writes into the FIFO.
    std::fs::remove_file(&object).unwrap();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&object)
            .status()
            .unwrap()
            .success()
    );

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
    let download = |agent: ureq::Agent, url: String, auth: String| {
        std::thread::spawn(move || {
            let mut response = agent.get(url).header("Authorization", auth).call().unwrap();
            assert_eq!(response.status().as_u16(), 200);
            response
                .body_mut()
                .with_config()
                .limit(u64::MAX)
                .read_to_vec()
                .unwrap()
        })
    };
    let original_url = format!("{url}/v1/assets/{id}/original");
    let first = download(agent.clone(), original_url.clone(), auth.clone());
    let second = download(agent.clone(), original_url, auth.clone());
    std::thread::sleep(Duration::from_millis(300));

    let started = Instant::now();
    let status = agent
        .get(format!("{url}/v1/library"))
        .header("Authorization", &auth)
        .call()
        .unwrap()
        .status()
        .as_u16();
    assert_eq!(status, 200);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(!first.is_finished() && !second.is_finished());

    // Release the slow backend. A second concurrent reader of the FIFO would split the
    // bytes and fail verification, so identical results prove the downloads were deduped.
    let mut writer = std::fs::OpenOptions::new()
        .write(true)
        .open(&object)
        .unwrap();
    writer.write_all(&bytes).unwrap();
    drop(writer);
    assert_eq!(first.join().unwrap(), bytes);
    assert_eq!(second.join().unwrap(), bytes);

    for _ in 0..4 {
        server.unblock();
    }
    serving.join().unwrap();
}
