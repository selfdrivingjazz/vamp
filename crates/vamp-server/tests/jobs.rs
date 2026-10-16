use serde_json::{Value, json};
use std::sync::Arc;
use vamp_core::Library;
use vamp_protocol::{Metadata, Placement, ProcessorSpec};

#[test]
fn remote_worker_claims_downloads_and_completes_over_http() {
    let home = tempfile::tempdir().unwrap();
    let originals = tempfile::tempdir().unwrap();
    let file = originals.path().join("bat.txt");
    std::fs::write(&file, b"original bytes").unwrap();
    let mut library = Library::init(home.path(), "main", None, None).unwrap();
    library
        .processor_add(ProcessorSpec {
            name: "caption".into(),
            events: vec!["media_added".into()],
            command: vec!["/nonexistent".into()],
            input: "original".into(),
            output: "observation".into(),
            timeout_seconds: 5,
            placement: Placement::Remote,
        })
        .unwrap();
    let asset = library.add(&file, None, Metadata::default()).unwrap();
    let token = vamp_server::token(&library.state_dir).unwrap();
    let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
    let url = format!("http://{}", server.server_addr());
    let app = vamp_server::App::new(library, "main").unwrap();
    {
        let server = server.clone();
        std::thread::spawn(move || vamp_server::serve(server, app, 2));
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let post = |path: &str, body: Value| -> (u16, Value) {
        let mut response = agent
            .post(format!("{url}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send_json(&body)
            .unwrap();
        let status = response.status().as_u16();
        (
            status,
            response.body_mut().read_json().unwrap_or(Value::Null),
        )
    };
    let claim = json!({"worker_id":"gpu","processors":["caption"],"limit":4,"lease_seconds":60});
    let (status, body) = post("/v1/jobs/claim", claim.clone());
    assert_eq!(status, 200, "{body}");
    let jobs = body["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1);
    let job = &jobs[0];
    let (id, lease) = (
        job["job"]["id"].as_str().unwrap(),
        job["lease"]["id"].as_str().unwrap(),
    );
    assert_eq!(
        job["input_url"],
        format!("/v1/assets/{}/original", asset.id)
    );
    assert_eq!(job["request"]["asset"]["id"], asset.id.as_str());
    // The input URL serves the bytes the processor needs.
    let mut input = agent
        .get(format!("{url}{}", job["input_url"].as_str().unwrap()))
        .header("Authorization", format!("Bearer {token}"))
        .call()
        .unwrap();
    assert_eq!(input.body_mut().read_to_vec().unwrap(), b"original bytes");
    // Already leased: a second claim gets nothing.
    assert_eq!(post("/v1/jobs/claim", claim).1["jobs"], json!([]));

    let (status, _) = post(
        &format!("/v1/jobs/{id}/heartbeat"),
        json!({"lease_id":lease}),
    );
    assert_eq!(status, 200);
    let response =
        json!({"observation":{"kind":"description","model":"m","value":{"text":"a bat"}}});
    let (status, _) = post(
        &format!("/v1/jobs/{id}/complete"),
        json!({"lease_id":"stale","response":response}),
    );
    assert_eq!(status, 409, "foreign lease must conflict");
    let (status, body) = post(
        &format!("/v1/jobs/{id}/complete"),
        json!({"lease_id":lease,"response":response}),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["job"]["status"], "completed");
    let (status, _) = post(
        &format!("/v1/jobs/{id}/fail"),
        json!({"lease_id":lease,"error":"late"}),
    );
    assert_eq!(status, 409, "finished job cannot be failed");
    let inspected: Value = agent
        .get(format!("{url}/v1/assets/{}", asset.id))
        .header("Authorization", format!("Bearer {token}"))
        .call()
        .unwrap()
        .body_mut()
        .read_json()
        .unwrap();
    assert_eq!(inspected["observations"][0]["value"]["text"], "a bat");
    assert_eq!(
        post("/v1/jobs/nope/heartbeat", json!({"lease_id":lease})).0,
        404
    );
    server.unblock();
    server.unblock();
}
