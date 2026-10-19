use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::sync::Arc;
use vamp_core::{EmbeddingState, EmbeddingUnavailable, Library, native, remote};
use vamp_protocol::{Metadata, Placement, ProcessorSpec, Remote};

fn image(library: &mut Library, root: &std::path::Path, name: &str) -> vamp_protocol::Asset {
    let path = root.join(name);
    std::fs::write(&path, name.as_bytes()).unwrap();
    library.add(&path, None, Metadata::default()).unwrap()
}

#[test]
fn per_asset_embedding_conflicts_retain_typed_states_through_http_and_replica_native() {
    let home = tempfile::tempdir().unwrap();
    let originals = tempfile::tempdir().unwrap();
    let replica_home = tempfile::tempdir().unwrap();
    let mut library = Library::init(home.path(), "main", None, None).unwrap();
    let none = image(&mut library, originals.path(), "none.png");
    library.processor_add(ProcessorSpec {
        name: "siglip-embedding".into(),
        events: vec!["media_added".into()],
        command: vec!["/must-not-run-a-model".into()],
        input: "original".into(), output: "observation".into(), timeout_seconds: 5,
        placement: Placement::Remote,
    }).unwrap();
    let pending = image(&mut library, originals.path(), "pending.png");
    let failed = image(&mut library, originals.path(), "failed.png");
    let stale = image(&mut library, originals.path(), "stale.png");
    let ready = image(&mut library, originals.path(), "ready.png");
    let claims = library.claim_jobs("fixture", &["siglip-embedding".into()], 10, 60).unwrap();
    let vector = STANDARD.encode([1.0f32, 0.0, 0.0].into_iter().flat_map(f32::to_le_bytes).collect::<Vec<_>>());
    let mut failed_job = String::new();
    for claim in claims {
        if claim.job.asset_id == failed.id {
            failed_job = claim.job.id.clone();
            library.fail_job(&claim.job.id, &claim.lease.id, "timeout reading /private/model/log").unwrap();
        } else if claim.job.asset_id == stale.id || claim.job.asset_id == ready.id {
            let response = serde_json::from_value(json!({"observation":{"kind":"embedding","model":"fixture",
                "value":{"embedding":{"model":"fixture","dim":3,"vector_b64":vector}}}})).unwrap();
            library.complete_job(&claim.job.id, &claim.lease.id, response).unwrap();
        }
        // The remaining claim is only leased; no executable or worker is ever started.
    }
    std::fs::write(&stale.original.path, b"a changed original source").unwrap();
    library.refresh(&stale.id, None).unwrap();
    library.enqueue("siglip-embedding", &stale.id).unwrap();
    let token = vamp_server::token(&library.state_dir).unwrap();
    let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
    let url = format!("http://{}", server.server_addr());
    let app = vamp_server::App::new(library, "main").unwrap();
    {
        let server = server.clone();
        std::thread::spawn(move || vamp_server::serve(server, app, 2));
    }
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    for (asset, state, reason) in [
        (&pending, "pending", None), (&failed, "failed", Some("timeout")),
        (&stale, "stale", None), (&none, "none", Some("not_queued")),
    ] {
        let mut response = agent.get(format!("{url}/v1/assets/{}/similar", asset.id))
            .header("Authorization", format!("Bearer {token}")).call().unwrap();
        assert_eq!(response.status().as_u16(), 409, "derived-state conflict, not bad request");
        let body: Value = response.body_mut().read_json().unwrap();
        assert_eq!(body["error"]["code"], "embedding_unavailable");
        assert_eq!(body["error"]["state"], state);
        assert_eq!(body["error"]["processor"], "siglip-embedding");
        assert_eq!(body["error"]["pending_total"], 2);
        assert_eq!(body["error"]["reason"].as_str(), reason);
        assert!(body.get("items").is_none(), "no synthetic or empty-success neighbours");
        assert!(!body.to_string().contains("/private/model/log"));
        let mut native_response = agent.post(format!("{url}/v1/native"))
            .header("Authorization", format!("Bearer {token}"))
            .send_json(&json!({"operation":"embedding_status","asset_id":asset.id})).unwrap();
        assert_eq!(native_response.status().as_u16(), 409);
        let native_body: Value = native_response.body_mut().read_json().unwrap();
        assert_eq!(native_body, body, "native preflight shares the per-asset error contract");
    }
    let client = remote::Client::new(&Remote { url: url.clone(), token: token.clone(), drive_roots: Default::default() }).unwrap();
    let error = client.similar(&failed.id, 10).unwrap_err();
    let status = error.downcast_ref::<remote::RemoteStatus>().unwrap();
    assert_eq!(status.status, 409);
    assert_eq!(status.code, "embedding_unavailable");
    let typed = EmbeddingUnavailable::from_error(&error).unwrap();
    assert_eq!(typed.state, EmbeddingState::Failed);
    assert_eq!(typed.reason.as_deref(), Some("timeout"));
    assert!(!remote::is_transient(&error), "a catalog conflict is not a network outage");
    let available = client.native(&json!({"operation":"embedding_status","asset_id":ready.id,"model":"fixture"})).unwrap();
    assert_eq!(available["available"], true);
    assert!(client.similar(&ready.id, 10).unwrap().is_empty());
    let native_error = client.native(&json!({"operation":"similar","asset_id":failed.id})).unwrap_err();
    assert_eq!(EmbeddingUnavailable::from_error(&native_error).unwrap(), typed);

    remote::attach_remote(replica_home.path(), "replica", &url, &token).unwrap();
    let request = |operation: &str, asset_id: &str| serde_json::from_value(json!({
        "operation":operation,"home":replica_home.path(),"context":"replica","asset_id":asset_id
    })).unwrap();
    for operation in ["similar", "embedding_status"] {
        let error = native::request(request(operation, &failed.id)).unwrap_err();
        let bridged = EmbeddingUnavailable::from_error(&error).unwrap().to_json();
        assert_eq!(bridged, typed.to_json(), "replica native error retains every typed field");
    }
    let mut replica = Library::open(replica_home.path(), Some("replica")).unwrap();
    for error in [
        replica.similar(&failed.id, 10).unwrap_err(),
        replica.require_embedding(&failed.id, None).unwrap_err(),
    ] {
        assert_eq!(EmbeddingUnavailable::from_error(&error).unwrap(), typed);
    }
    replica.require_embedding(&ready.id, Some("fixture")).unwrap();
    let retried = native::request(request("embedding_retry", &failed.id)).unwrap();
    assert_eq!(retried["job"]["id"], failed_job);
    assert_eq!(retried["job"]["asset_id"], failed.id);
    assert_eq!(retried["job"]["status"], "pending");
    assert_eq!(native::request(request("embedding_retry", &failed.id)).unwrap()["job"]["id"], failed_job);
    assert_eq!(replica.embedding_retry(&failed.id, None).unwrap()["job"]["id"], failed_job);
    let pending_error = native::request(request("embedding_status", &failed.id)).unwrap_err();
    let retried_state = EmbeddingUnavailable::from_error(&pending_error).unwrap();
    assert_eq!(retried_state.state, EmbeddingState::Pending);
    assert_eq!(retried_state.pending_total, Some(3));
    let none_error = native::request(request("embedding_retry", &none.id)).unwrap_err();
    assert_eq!(EmbeddingUnavailable::from_error(&none_error).unwrap().state, EmbeddingState::None);
    server.unblock();
    server.unblock();
}
