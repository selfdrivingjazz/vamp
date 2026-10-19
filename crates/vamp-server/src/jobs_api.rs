//! Remote job leasing: `vamp worker` on another machine claims processor jobs, runs its own
//! implementation of the named processor, and posts the ToolResponse back. Results go through
//! the same apply path (with staleness check) as local runs.
use super::{App, HttpError, Reply, ok, read_json};
use serde::Deserialize;
use serde_json::json;
use tiny_http::{Method, Request};
use vamp_protocol::{PROTOCOL_VERSION, ToolResponse};

const DEFAULT_LEASE_SECONDS: u64 = 120;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimBody {
    worker_id: String,
    processors: Vec<String>,
    #[serde(default = "one")]
    limit: usize,
    #[serde(default = "lease_seconds")]
    lease_seconds: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HeartbeatBody {
    lease_id: String,
    #[serde(default = "lease_seconds")]
    lease_seconds: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompleteBody {
    lease_id: String,
    response: ToolResponse,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FailBody {
    lease_id: String,
    error: String,
}

fn one() -> usize {
    1
}
fn lease_seconds() -> u64 {
    DEFAULT_LEASE_SECONDS
}

fn conflict(error: anyhow::Error) -> HttpError {
    let message = format!("{error:#}");
    // A reclaimed or finished job: the worker must drop its result, not retry.
    let status = if message.contains("lease conflict") {
        409
    } else {
        HttpError::from(error).status
    };
    HttpError::new(status, message)
}

/// `/v1/jobs/...` routes; `None` when the path is not a jobs route.
pub(crate) fn route(
    app: &App,
    method: &Method,
    segments: &[&str],
    request: &mut Request,
) -> Option<Result<Reply, HttpError>> {
    Some(match (method, segments) {
        (Method::Post, ["v1", "jobs", "claim"]) => claim(app, request),
        (Method::Post, ["v1", "jobs", id, "heartbeat"]) => (|| {
            let body: HeartbeatBody = read_json(request)?;
            let job = app
                .lock()
                .heartbeat_job(id, &body.lease_id, body.lease_seconds)
                .map_err(conflict)?;
            ok(json!({"job":job}))
        })(),
        (Method::Post, ["v1", "jobs", id, "complete"]) => (|| {
            let body: CompleteBody = read_json(request)?;
            let job = app
                .lock()
                .complete_job(id, &body.lease_id, body.response)
                .map_err(conflict)?;
            ok(json!({"job":job}))
        })(),
        (Method::Post, ["v1", "jobs", id, "fail"]) => (|| {
            let body: FailBody = read_json(request)?;
            let job = app
                .lock()
                .fail_job(id, &body.lease_id, &body.error)
                .map_err(conflict)?;
            ok(json!({"job":job}))
        })(),
        _ => return None,
    })
}

fn claim(app: &App, request: &mut Request) -> Result<Reply, HttpError> {
    let body: ClaimBody = read_json(request)?;
    let claimed = app.lock().claim_jobs(
        &body.worker_id,
        &body.processors,
        body.limit,
        body.lease_seconds,
    )?;
    let jobs = claimed
        .into_iter()
        .map(|c| {
            json!({
                "job": c.job,
                "lease": c.lease,
                "input": c.input,
                "input_url": format!("/v1/assets/{}/{}", c.job.asset_id, c.input),
                "request": c.request,
            })
        })
        .collect::<Vec<_>>();
    ok(json!({"protocol_version":PROTOCOL_VERSION,"jobs":jobs}))
}
