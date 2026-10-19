#![cfg(unix)]
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};
use tempfile::TempDir;
use vamp_core::{EmbedderSpec, Library};
use vamp_protocol::{Metadata, ProcessorSpec};

/// Fake SigLIP: an original's bytes (or embed_text's text) are the base64 vector itself.
const FAKE_EMBEDDER: &str = r#"#!/bin/sh
req=$(cat)
case "$req" in
  *'"operation":"embed_text"'*) v=$(printf '%s' "$req" | sed -n 's/.*"text":"\([^"]*\)".*/\1/p') ;;
  *) v=$(cat "$(printf '%s' "$req" | sed -n 's/.*"input_path":"\([^"]*\)".*/\1/p')") ;;
esac
printf '{"observation":{"kind":"embedding","model":"fake-clip","value":{"embedding":{"model":"fake-clip","dim":3,"vector_b64":"%s"}}}}' "$v"
"#;
const FAKE_TAGGER: &str = r#"#!/bin/sh
cat >/dev/null
printf '{"observation":{"kind":"tags","model":"fake-florence","value":{"tags":["Cat","sofa","cat","window"]}}}'
"#;
/// Fake persistent embedder: one-shot answers [1,0,0]; the --serve socket answers [0,1,0],
/// so ranking reveals which path served a query.
const FAKE_SERVER: &str = r#"#!/usr/bin/env python3
import base64, json, os, socket, struct, sys
def answer(v):
    b = base64.b64encode(struct.pack("<3f", *v)).decode()
    return {"observation": {"kind": "embedding", "model": "fake-clip",
            "value": {"embedding": {"model": "fake-clip", "dim": 3, "vector_b64": b}}}}
if "--serve" in sys.argv:
    path = sys.argv[sys.argv.index("--socket") + 1]
    s = socket.socket(socket.AF_UNIX); s.bind(path); s.listen(4); s.settimeout(5)
    try:
        while True:
            c, _ = s.accept()
            c.makefile().readline()
            c.sendall((json.dumps(answer([0, 1, 0])) + "\n").encode()); c.close()
    except socket.timeout:
        os.unlink(path)
else:
    sys.stdin.read()
    print(json.dumps(answer([1, 0, 0])))
"#;

fn vector(values: [f32; 3]) -> String {
    STANDARD.encode(
        values
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>(),
    )
}
fn script(dir: &Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path.to_str().unwrap().to_owned()
}
fn processor(name: &str, command: String) -> ProcessorSpec {
    ProcessorSpec {
        name: name.into(),
        events: vec!["media_added".into()],
        command: vec![command],
        input: "original".into(),
        output: "observation".into(),
        timeout_seconds: 10,
        placement: Default::default(),
    }
}
fn library(dir: &TempDir) -> Library {
    Library::init(&dir.path().join("home"), "test", None, None).unwrap()
}

#[test]
fn semantic_and_similar_rank_by_cosine_over_current_originals_only() {
    let dir = TempDir::new().unwrap();
    let mut lib = library(&dir);
    let embedder = script(dir.path(), "embed.sh", FAKE_EMBEDDER);
    lib.processor_add(processor("embed", embedder.clone()))
        .unwrap();
    lib.embedder_set(EmbedderSpec {
        command: vec![embedder],
        model: "fake-clip".into(),
        timeout_seconds: 10,
        serve: false,
    })
    .unwrap();
    let mut ids = Vec::new();
    // Unnormalized input: storage must normalize before cosine ranking.
    for (name, v) in [
        ("a", [2.0, 0.0, 0.0]),
        ("b", [0.8, 0.6, 0.0]),
        ("c", [0.0, 0.0, 1.0]),
    ] {
        let path = dir.path().join(name);
        fs::write(&path, vector(v)).unwrap();
        ids.push(lib.add(&path, None, Metadata::default()).unwrap().id);
    }
    assert_eq!(lib.run_jobs(10).unwrap()["completed"], 3);

    let ranked = lib
        .semantic_search(&vector([1.0, 0.1, 0.0]), 10, false, None)
        .unwrap();
    let order: Vec<_> = ranked.iter().map(|s| s.asset.id.as_str()).collect();
    assert_eq!(order, [&ids[0], &ids[1], &ids[2]]);
    assert!((ranked[0].score - 1.0 / 1.01f32.sqrt()).abs() < 1e-5);
    assert!(ranked[2].score.abs() < 1e-6);

    // The vector is stored out of band: not in the observation nor the full-text index.
    let view = lib.inspect(&ids[1]).unwrap();
    assert_eq!(view.observations[0].value["embedding"]["dim"], 3);
    assert!(
        view.observations[0].value["embedding"]
            .get("vector_b64")
            .is_none()
    );
    assert!(
        lib.list(Some(&vector([0.8, 0.6, 0.0])), 10, 0, None)
            .unwrap()
            .is_empty()
    );
    // The similarity pack holds every current vector and is reused while nothing changed.
    let (path, before) = lib.energy_pack().unwrap().unwrap();
    assert_eq!((before.count, before.dim, before.model.as_str()), (3, 3, "fake-clip"));
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(before.ids, sorted);
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    assert_eq!(lib.energy_pack().unwrap().unwrap().1.version, before.version);
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);

    // A refreshed original no longer matches its old vector, in search or similarity.
    fs::write(dir.path().join("a"), vector([0.0, 1.0, 0.0])).unwrap();
    lib.refresh(&ids[0], None).unwrap();
    let ranked = lib
        .semantic_search(&vector([1.0, 0.0, 0.0]), 10, false, None)
        .unwrap();
    assert!(ranked.iter().all(|s| s.asset.id != ids[0]));
    let similar = lib.similar(&ids[1], 10).unwrap();
    assert_eq!(similar.len(), 1);
    assert_eq!(similar[0].asset.id, ids[2]);
    assert!(lib.similar(&ids[0], 10).is_err());
    // …and the pack is rebuilt without the stale vector.
    let (_, after) = lib.energy_pack().unwrap().unwrap();
    assert_ne!(after.version, before.version);
    assert_eq!(after.count, 2);
    assert!(!after.ids.contains(&ids[0]));
}

#[test]
fn malformed_embedding_fails_job_without_storing_or_annotating() {
    let dir = TempDir::new().unwrap();
    let mut lib = library(&dir);
    lib.processor_add(processor(
        "embed",
        script(dir.path(), "embed.sh", FAKE_EMBEDDER),
    ))
    .unwrap();
    let path = dir.path().join("short");
    // Two floats while the processor claims dim 3.
    fs::write(&path, STANDARD.encode([0u8; 8])).unwrap();
    let asset = lib.add(&path, None, Metadata::default()).unwrap();
    assert_eq!(lib.run_jobs(10).unwrap()["failed"], 1);
    assert!(lib.inspect(&asset.id).unwrap().observations.is_empty());
    assert!(lib.similar(&asset.id, 10).is_err());
}

#[test]
fn suggested_tags_are_searchable_and_only_join_user_tags_when_accepted() {
    let dir = TempDir::new().unwrap();
    let mut lib = library(&dir);
    lib.processor_add(processor(
        "tags",
        script(dir.path(), "tags.sh", FAKE_TAGGER),
    ))
    .unwrap();
    let path = dir.path().join("photo");
    fs::write(&path, b"pixels").unwrap();
    let metadata = Metadata {
        tags: vec!["Sofa".into()],
        ..Metadata::default()
    };
    let asset = lib.add(&path, None, metadata).unwrap();
    lib.run_jobs(10).unwrap();

    let view = lib.inspect(&asset.id).unwrap();
    assert_eq!(view.asset.metadata.tags, ["Sofa"]);
    assert_eq!(view.suggested_tags, ["cat", "window"]);
    assert_eq!(lib.list(Some("window"), 10, 0, None).unwrap()[0].id, asset.id);

    assert!(lib.accept_tags(&asset.id, &["dog".into()], None).is_err());
    assert!(
        lib.accept_tags(&asset.id, &["cat".into()], Some(99))
            .is_err()
    );
    let updated = lib.accept_tags(&asset.id, &["CAT".into()], None).unwrap();
    assert_eq!(updated.metadata.tags, ["Sofa", "cat"]);
    assert_eq!(lib.inspect(&asset.id).unwrap().suggested_tags, ["window"]);
    let updated = lib.accept_tags(&asset.id, &[], None).unwrap();
    assert_eq!(updated.metadata.tags, ["Sofa", "cat", "window"]);
    assert!(lib.accept_tags(&asset.id, &[], None).is_err());
}

#[cfg(feature = "native")]
#[test]
fn native_semantic_browse_falls_back_to_full_text_without_embedder() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let mut lib = library(&dir);
    let path = dir.path().join("sunset.txt");
    fs::write(&path, b"x").unwrap();
    lib.add(&path, None, Metadata::default()).unwrap();
    drop(lib);
    let request = serde_json::from_value(serde_json::json!({
        "operation":"browse","home":home,"query":"sunset","semantic":true
    }))
    .unwrap();
    let response = vamp_core::native::request(request).unwrap();
    assert_eq!(response["semantic"], false);
    assert_eq!(response["total"], 1);
}

#[test]
fn persistent_embedder_starts_on_first_query_and_serves_later_ones() {
    let dir = TempDir::new().unwrap();
    let mut lib = library(&dir);
    let embed = script(dir.path(), "embed.sh", FAKE_EMBEDDER);
    lib.processor_add(processor("embed", embed)).unwrap();
    let mut ids = Vec::new();
    for (name, v) in [("x", [1.0, 0.0, 0.0]), ("y", [0.0, 1.0, 0.0])] {
        let path = dir.path().join(name);
        fs::write(&path, vector(v)).unwrap();
        ids.push(lib.add(&path, None, Metadata::default()).unwrap().id);
    }
    lib.run_jobs(10).unwrap();
    lib.embedder_set(EmbedderSpec {
        command: vec![script(dir.path(), "server.py", FAKE_SERVER)],
        model: "fake-clip".into(),
        timeout_seconds: 10,
        serve: true,
    })
    .unwrap();
    // No server yet: this query is answered one-shot while the server starts.
    assert_eq!(
        lib.semantic_search("q", 1, false, None).unwrap()[0].asset.id,
        ids[0]
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if lib.semantic_search("q", 1, false, None).unwrap()[0].asset.id == ids[1] {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "server never answered"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[cfg(feature = "native")]
#[test]
fn native_semantic_browse_ranks_only_filtered_candidates() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let mut lib = library(&dir);
    let embedder = script(dir.path(), "embed.sh", FAKE_EMBEDDER);
    lib.processor_add(processor("embed", embedder.clone()))
        .unwrap();
    lib.embedder_set(EmbedderSpec {
        command: vec![embedder],
        model: "fake-clip".into(),
        timeout_seconds: 10,
        serve: false,
    })
    .unwrap();
    let mut ids = Vec::new();
    for (name, v, tags) in [
        ("best", [1.0, 0.0, 0.0], vec![]),
        ("kept", [0.6, 0.8, 0.0], vec!["keep".to_string()]),
        ("also", [0.0, 0.0, 1.0], vec!["keep".to_string()]),
    ] {
        let path = dir.path().join(name);
        fs::write(&path, vector(v)).unwrap();
        let metadata = Metadata {
            tags,
            ..Metadata::default()
        };
        ids.push(lib.add(&path, None, metadata).unwrap().id);
    }
    lib.run_jobs(10).unwrap();
    drop(lib);
    let browse = |extra: serde_json::Value| {
        let mut request = serde_json::json!({"operation":"browse","home":home,
            "query":vector([1.0, 0.0, 0.0]),"semantic":true,"limit":1});
        request
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        vamp_core::native::request(serde_json::from_value(request).unwrap()).unwrap()
    };
    let all = browse(serde_json::json!({}));
    assert_eq!(
        (all["semantic"].clone(), all["total"].clone()),
        (true.into(), 3.into())
    );
    assert_eq!(all["items"][0]["id"], ids[0]);
    let tagged = browse(serde_json::json!({"tags":["keep"]}));
    assert_eq!(tagged["total"], 2);
    assert_eq!(tagged["items"][0]["id"], ids[1]);
    assert_eq!(tagged["has_more"], true);
    let untagged = browse(serde_json::json!({"untagged":true,"offset":1}));
    assert_eq!(untagged["total"], 1);
    assert!(untagged["items"].as_array().unwrap().is_empty());
}

fn unavailable(error: &anyhow::Error) -> vamp_core::EmbeddingUnavailable {
    vamp_core::EmbeddingUnavailable::from_error(error).unwrap().clone()
}

fn queued_image(lib: &mut Library, dir: &TempDir, name: &str) -> vamp_protocol::Asset {
    let path = dir.path().join(name);
    fs::write(&path, name.as_bytes()).unwrap();
    lib.add(&path, None, Metadata::default()).unwrap()
}

fn complete_embedding(lib: &mut Library, claim: &vamp_core::ClaimedJob) {
    let response = serde_json::from_value(serde_json::json!({
        "observation":{"kind":"embedding","model":"fake-clip",
            "value":{"embedding":{"model":"fake-clip","dim":3,"vector_b64":vector([1.0,0.0,0.0])}}}
    })).unwrap();
    lib.complete_job(&claim.job.id, &claim.lease.id, response).unwrap();
}

#[test]
fn embedding_unavailable_none_ignores_caption_jobs_and_preserves_argument_errors() {
    let dir = TempDir::new().unwrap();
    let mut lib = library(&dir);
    lib.processor_add(processor("caption", "/must-not-run-caption".into())).unwrap();
    let asset = queued_image(&mut lib, &dir, "none.png");
    let before = lib.jobs(None, 100).unwrap().len();
    let error = lib.similar(&asset.id, 10).unwrap_err().context("similar request");
    let missing = unavailable(&error);
    assert_eq!(missing.state, vamp_core::EmbeddingState::None);
    assert_eq!(missing.processor, None);
    assert_eq!(missing.pending_total, None);
    assert_eq!(missing.reason.as_deref(), Some("no_processor"));
    let json = missing.to_json();
    assert_eq!(json["code"], "embedding_unavailable");
    assert_eq!(json["state"], "none");
    assert!(json.get("processor").is_none());
    assert!(json.get("pending_total").is_none());
    assert_eq!(lib.jobs(None, 100).unwrap().len(), before, "read errors cannot enqueue");
    assert!(vamp_core::EmbeddingUnavailable::from_error(&lib.similar(&asset.id, 0).unwrap_err()).is_none());
    assert!(vamp_core::EmbeddingUnavailable::from_error(&lib.similar("missing", 10).unwrap_err()).is_none());
    assert!(lib.embedding_retry(&asset.id, None).is_err());
}

#[test]
fn embedding_pending_counts_distinct_source_current_live_images_not_jobs() {
    let dir = TempDir::new().unwrap();
    let mut lib = library(&dir);
    lib.embedder_set(EmbedderSpec {
        command: vec!["/must-not-run-embedding".into()],
        model: "fake-clip".into(), timeout_seconds: 10, serve: false,
    }).unwrap();
    for name in ["siglip-embedding", "other-image-analysis"] {
        lib.processor_add(processor(name, "/must-not-run-embedding".into())).unwrap();
    }
    lib.processor_add(processor("caption", "/must-not-run-caption".into())).unwrap();
    let first = queued_image(&mut lib, &dir, "first.png");
    queued_image(&mut lib, &dir, "second.jpg");
    queued_image(&mut lib, &dir, "not-an-image.txt");
    let trashed = queued_image(&mut lib, &dir, "trashed.png");
    lib.remove(&trashed.id, None).unwrap();
    let old = queued_image(&mut lib, &dir, "old.png");
    fs::write(&old.original.path, b"changed original source").unwrap();
    lib.refresh(&old.id, None).unwrap();

    let pending = unavailable(&lib.similar(&first.id, 10).unwrap_err());
    assert_eq!(pending.state, vamp_core::EmbeddingState::Pending);
    assert_eq!(pending.pending_total, Some(2));
    assert!(pending.processor.is_some());
    let old_missing = unavailable(&lib.similar(&old.id, 10).unwrap_err());
    assert_eq!(old_missing.state, vamp_core::EmbeddingState::None);
    assert_eq!(old_missing.reason.as_deref(), Some("not_queued"));
    assert_eq!(old_missing.pending_total, Some(2));
}

#[test]
fn embedding_running_job_remains_pending_without_executing_a_processor() {
    let dir = TempDir::new().unwrap();
    let mut lib = library(&dir);
    lib.processor_add(processor("siglip-embedding", "/must-not-run".into())).unwrap();
    let asset = queued_image(&mut lib, &dir, "pending.png");
    let pending = unavailable(&lib.similar(&asset.id, 10).unwrap_err());
    assert_eq!(pending.state, vamp_core::EmbeddingState::Pending);
    assert_eq!(pending.processor.as_deref(), Some("siglip-embedding"));
    assert_eq!(pending.pending_total, Some(1));
    assert_eq!(pending.reason, None);
    let claims = lib.claim_jobs("fixture", &["siglip-embedding".into()], 1, 60).unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].job.status, "running");
    assert_eq!(unavailable(&lib.similar(&asset.id, 10).unwrap_err()), pending);
}

#[test]
fn embedding_failed_retry_requeues_only_the_exact_current_job() {
    let dir = TempDir::new().unwrap();
    let mut lib = library(&dir);
    lib.processor_add(processor("siglip-embedding", "/must-not-run".into())).unwrap();
    let first = queued_image(&mut lib, &dir, "first.png");
    let second = queued_image(&mut lib, &dir, "second.png");
    let claims = lib.claim_jobs("fixture", &["siglip-embedding".into()], 2, 60).unwrap();
    for claim in &claims {
        lib.fail_job(&claim.job.id, &claim.lease.id, "timeout reading /private/raw/path").unwrap();
    }
    let missing = unavailable(&lib.similar(&first.id, 10).unwrap_err());
    assert_eq!(missing.state, vamp_core::EmbeddingState::Failed);
    assert_eq!(missing.pending_total, Some(0));
    assert_eq!(missing.reason.as_deref(), Some("timeout"));
    assert!(!missing.to_json().to_string().contains("/private/raw/path"));
    let retried = lib.embedding_retry(&first.id, None).unwrap();
    assert_eq!(retried["job"]["asset_id"], first.id);
    assert_eq!(retried["job"]["status"], "pending");
    assert_eq!(lib.embedding_retry(&first.id, None).unwrap()["job"]["id"], retried["job"]["id"]);
    let pending = lib.jobs(Some("pending"), 100).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].asset_id, first.id);
    assert_eq!(lib.jobs(Some("failed"), 100).unwrap()[0].asset_id, second.id);
}

#[test]
fn embedding_stale_requires_refresh_work_and_current_failure_overrides_old_vector() {
    let dir = TempDir::new().unwrap();
    let mut lib = library(&dir);
    lib.processor_add(processor("custom-image-analysis", "/must-not-run".into())).unwrap();
    let asset = queued_image(&mut lib, &dir, "stale.png");
    let claim = lib.claim_jobs("fixture", &["custom-image-analysis".into()], 1, 60).unwrap().remove(0);
    complete_embedding(&mut lib, &claim);
    lib.require_embedding(&asset.id, Some("fake-clip")).unwrap();
    assert!(lib.similar(&asset.id, 10).unwrap().is_empty(), "never synthesize neighbours");
    assert_eq!(lib.embedding_retry(&asset.id, None).unwrap()["available"], true);
    fs::write(&asset.original.path, b"new original source").unwrap();
    lib.refresh(&asset.id, None).unwrap();
    let not_queued = unavailable(&lib.similar(&asset.id, 10).unwrap_err());
    assert_eq!(not_queued.state, vamp_core::EmbeddingState::None);
    assert_eq!(not_queued.reason.as_deref(), Some("not_queued"));
    lib.enqueue("custom-image-analysis", &asset.id).unwrap();
    let stale = unavailable(&lib.similar(&asset.id, 10).unwrap_err());
    assert_eq!(stale.state, vamp_core::EmbeddingState::Stale);
    assert_eq!(stale.pending_total, Some(1));
    assert_eq!(stale.processor.as_deref(), Some("custom-image-analysis"));
    let claim = lib.claim_jobs("fixture", &["custom-image-analysis".into()], 1, 60).unwrap().remove(0);
    lib.fail_job(&claim.job.id, &claim.lease.id, "processor exited unsuccessfully").unwrap();
    let failed = unavailable(&lib.similar(&asset.id, 10).unwrap_err());
    assert_eq!(failed.state, vamp_core::EmbeddingState::Failed);
    assert_eq!(failed.reason.as_deref(), Some("processor_failed"));
    lib.embedding_retry(&asset.id, Some("fake-clip")).unwrap();
    assert_eq!(unavailable(&lib.similar(&asset.id, 10).unwrap_err()).state, vamp_core::EmbeddingState::Stale);
    let claim = lib.claim_jobs("fixture", &["custom-image-analysis".into()], 1, 60).unwrap().remove(0);
    complete_embedding(&mut lib, &claim);
    lib.require_embedding(&asset.id, Some("fake-clip")).unwrap();
    assert_eq!(unavailable(&lib.require_embedding(&asset.id, Some("other-model")).unwrap_err()).state, vamp_core::EmbeddingState::None);
}

#[cfg(feature = "native")]
#[test]
fn native_embedding_preflight_and_retry_preserve_the_typed_error() {
    let dir = TempDir::new().unwrap();
    let mut lib = library(&dir);
    lib.processor_add(processor("siglip-embedding", "/must-not-run".into())).unwrap();
    let asset = queued_image(&mut lib, &dir, "native.png");
    for operation in ["similar", "embedding_status"] {
        let request = serde_json::from_value(serde_json::json!({"operation":operation,"asset_id":asset.id})).unwrap();
        let error = vamp_core::native::dispatch(&mut lib, request).unwrap_err();
        let json = unavailable(&error).to_json();
        assert_eq!(json["code"], "embedding_unavailable");
        assert_eq!(json["state"], "pending");
        assert_eq!(json["processor"], "siglip-embedding");
        assert_eq!(json["pending_total"], 1);
    }
    let claim = lib.claim_jobs("fixture", &["siglip-embedding".into()], 1, 60).unwrap().remove(0);
    lib.fail_job(&claim.job.id, &claim.lease.id, "timeout").unwrap();
    let request = serde_json::from_value(serde_json::json!({"operation":"embedding_retry","asset_id":asset.id})).unwrap();
    assert_eq!(vamp_core::native::dispatch(&mut lib, request).unwrap()["job"]["status"], "pending");
}
