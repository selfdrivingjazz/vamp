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
