//! Remote job leases: claim, local-worker exclusion, staleness, expiry and reclaim.
use serde_json::json;
use std::fs;
use tempfile::TempDir;
use vamp_core::{Library, MAX_LEASE_ATTEMPTS};
use vamp_protocol::{Metadata, ObservationOutput, Placement, ProcessorSpec, ToolResponse};

fn spec(name: &str, placement: Placement) -> ProcessorSpec {
    ProcessorSpec {
        name: name.into(),
        events: vec!["media_added".into()],
        // Never executed locally in these tests unless placement allows it.
        command: vec!["/nonexistent/local-command".into()],
        input: "original".into(),
        output: "observation".into(),
        timeout_seconds: 5,
        placement,
    }
}

fn setup(placement: Placement) -> (TempDir, Library, std::path::PathBuf, String) {
    let dir = TempDir::new().unwrap();
    let source = dir.path().join("a.txt");
    fs::write(&source, b"first revision").unwrap();
    let mut lib = Library::init(&dir.path().join("state"), "t", None, None).unwrap();
    lib.processor_add(spec("caption", placement)).unwrap();
    let asset = lib.add(&source, None, Metadata::default()).unwrap();
    (dir, lib, source, asset.id)
}

fn caption(text: &str) -> ToolResponse {
    ToolResponse {
        observation: Some(ObservationOutput {
            kind: "description".into(),
            model: Some("m".into()),
            value: json!({"text":text}),
        }),
        ..ToolResponse::default()
    }
}

fn names() -> Vec<String> {
    vec!["caption".into()]
}

#[test]
fn one_active_job_per_asset_and_processor_with_bulk_enqueue_and_cancel() {
    let (_dir, mut lib, _, asset) = setup(Placement::Remote);
    let queued = lib.jobs(Some("pending"), 10).unwrap();
    assert_eq!(queued.len(), 1, "media_added trigger queues one job");
    // Explicit enqueue and bulk enqueue reuse the active job instead of duplicating it.
    assert_eq!(lib.enqueue("caption", &asset).unwrap().id, queued[0].id);
    let all = lib.enqueue_all("caption", false).unwrap();
    assert_eq!(
        (all["enqueued"].as_u64(), all["already_queued"].as_u64()),
        (Some(0), Some(1))
    );
    let c = lib.claim_jobs("w", &names(), 1, 60).unwrap().remove(0);
    assert_eq!(
        lib.enqueue("caption", &asset).unwrap().id,
        c.job.id,
        "running counts as active"
    );
    lib.complete_job(&c.job.id, &c.lease.id, caption("a bat"))
        .unwrap();
    let missing = lib.enqueue_all("caption", true).unwrap();
    assert_eq!(
        (
            missing["enqueued"].as_u64(),
            missing["already_completed"].as_u64()
        ),
        (Some(0), Some(1))
    );
    assert_eq!(lib.enqueue_all("caption", false).unwrap()["enqueued"], 1);
    assert!(lib.cancel_jobs("running", None).is_err());
    assert_eq!(lib.cancel_jobs("pending", Some("other")).unwrap(), 0);
    assert_eq!(lib.cancel_jobs("pending", Some("caption")).unwrap(), 1);
    assert!(lib.jobs(Some("pending"), 10).unwrap().is_empty());
    assert_eq!(lib.jobs(Some("completed"), 10).unwrap().len(), 1);
}

#[test]
fn claimed_job_is_exclusive_and_completes_through_apply_path() {
    let (_dir, mut lib, _, asset) = setup(Placement::Any);
    assert!(
        lib.claim_jobs("w", &["other".into()], 5, 60)
            .unwrap()
            .is_empty()
    );
    let claimed = lib.claim_jobs("w1", &names(), 5, 60).unwrap();
    assert_eq!(claimed.len(), 1);
    let c = &claimed[0];
    assert_eq!(c.input, "original");
    assert_eq!(c.request.operation, "process");
    assert!(c.request.input_path.is_none());
    assert_eq!(c.request.asset.as_ref().unwrap().id, asset);
    // Nobody else gets it, and the local worker neither runs nor fails a leased job.
    assert!(lib.claim_jobs("w2", &names(), 5, 60).unwrap().is_empty());
    assert_eq!(lib.run_jobs(10).unwrap()["failed"], 0);
    assert_eq!(lib.jobs(Some("running"), 10).unwrap().len(), 1);

    assert!(lib.heartbeat_job(&c.job.id, "wrong", 60).is_err());
    lib.heartbeat_job(&c.job.id, &c.lease.id, 60).unwrap();
    let err = lib
        .complete_job(&c.job.id, "wrong", caption("x"))
        .unwrap_err();
    assert!(format!("{err:#}").contains("lease conflict"));
    let done = lib
        .complete_job(&c.job.id, &c.lease.id, caption("a bat"))
        .unwrap();
    assert_eq!(done.status, "completed");
    let observations = lib.inspect(&asset).unwrap().observations;
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].value["text"], "a bat");
    // A duplicate completion is rejected rather than applied twice.
    assert!(
        lib.complete_job(&c.job.id, &c.lease.id, caption("again"))
            .is_err()
    );
}

#[test]
fn placement_routes_jobs_between_local_and_remote_workers() {
    let (_dir, mut lib, _, _) = setup(Placement::Remote);
    // The local worker leaves remote jobs pending (it would fail running /nonexistent).
    let run = lib.run_jobs(10).unwrap();
    assert_eq!(run["failed"], 0);
    assert_eq!(lib.jobs(Some("pending"), 10).unwrap().len(), 1);
    // Local placement hides the job from claims; the current registration decides.
    lib.processor_placement("caption", Placement::Local)
        .unwrap();
    assert!(lib.claim_jobs("w", &names(), 5, 60).unwrap().is_empty());
    lib.processor_placement("caption", Placement::Remote)
        .unwrap();
    assert_eq!(lib.claim_jobs("w", &names(), 5, 60).unwrap().len(), 1);
    // Workers cannot upload representations, so remote placement needs observations.
    let mut thumb = spec("thumb", Placement::Remote);
    thumb.output = "thumbnail".into();
    assert!(lib.processor_add(thumb).is_err());
}

#[test]
fn result_for_replaced_original_is_stale_and_fails_the_job() {
    let (_dir, mut lib, source, asset) = setup(Placement::Remote);
    let c = lib.claim_jobs("w", &names(), 1, 60).unwrap().remove(0);
    fs::write(&source, b"second revision with new content").unwrap();
    lib.refresh(&asset, Some(1)).unwrap();
    let err = lib
        .complete_job(&c.job.id, &c.lease.id, caption("old"))
        .unwrap_err();
    assert!(format!("{err:#}").contains("stale result"));
    let failed = lib.jobs(Some("failed"), 10).unwrap();
    assert_eq!(failed.len(), 1);
    assert!(lib.inspect(&asset).unwrap().observations.is_empty());
}

#[test]
fn expired_lease_is_reclaimed_and_late_result_rejected() {
    let (_dir, mut lib, _, asset) = setup(Placement::Remote);
    let first = lib.claim_jobs("dead", &names(), 1, 1).unwrap().remove(0);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let second = lib.claim_jobs("alive", &names(), 1, 60).unwrap();
    assert_eq!(second.len(), 1, "expired job must be claimable again");
    let second = &second[0];
    assert_eq!(second.job.id, first.job.id);
    assert_eq!(second.job.attempts, 2);
    let err = lib
        .complete_job(&first.job.id, &first.lease.id, caption("late"))
        .unwrap_err();
    assert!(format!("{err:#}").contains("lease conflict"));
    assert!(
        lib.heartbeat_job(&first.job.id, &first.lease.id, 60)
            .is_err()
    );
    lib.complete_job(&second.job.id, &second.lease.id, caption("fresh"))
        .unwrap();
    let observations = lib.inspect(&asset).unwrap().observations;
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].value["text"], "fresh");
}

#[test]
fn repeatedly_expiring_job_fails_after_attempt_limit() {
    let (_dir, mut lib, _, _) = setup(Placement::Remote);
    for _ in 0..MAX_LEASE_ATTEMPTS {
        assert_eq!(lib.claim_jobs("crashy", &names(), 1, 1).unwrap().len(), 1);
        std::thread::sleep(std::time::Duration::from_millis(1100));
    }
    assert!(lib.claim_jobs("w", &names(), 1, 60).unwrap().is_empty());
    let failed = lib.jobs(Some("failed"), 10).unwrap().remove(0);
    assert!(failed.error.unwrap().contains("expired after 3 attempts"));
    // Explicit retry re-queues it.
    lib.retry_job(&failed.id).unwrap();
    assert_eq!(lib.claim_jobs("w", &names(), 1, 60).unwrap().len(), 1);
}
