//! `vamp worker`: run this machine's processor implementations for an authority's jobs.
//!
//! The authority owns processor registrations and the queue; this process claims jobs for the
//! processor names it implements locally (from `vamp setup install search|captions`), downloads the
//! job's input representation, runs the local command, and posts the ToolResponse back. Leases
//! are heartbeated while a processor runs; a crashed worker's jobs are reclaimed by the
//! authority when the lease expires, so restarts need no local state.
use anyhow::{Context as _, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use vamp_core::remote::Client;
use vamp_protocol::{ProcessorSpec, Remote, ToolRequest, ToolResponse};

pub struct Options {
    pub server: String,
    pub token: String,
    pub worker_id: String,
    pub processors: Vec<String>,
    pub concurrency: usize,
    pub lease_seconds: u64,
    pub poll_seconds: u64,
    pub exit_when_idle: bool,
}

/// Bearer token from `--token`, else `--token-file`; empty when neither is given, in which
/// case the authority must admit this machine by Tailscale identity.
pub fn token(token: Option<String>, file: Option<&Path>) -> Result<String> {
    if let Some(token) = token.filter(|t| !t.trim().is_empty()) {
        return Ok(token.trim().to_owned());
    }
    let Some(file) = file else {
        return Ok(String::new());
    };
    let token = fs::read_to_string(file)
        .with_context(|| format!("cannot read token file {}", file.display()))?;
    ensure!(
        !token.trim().is_empty(),
        "token file {} is empty",
        file.display()
    );
    Ok(token.trim().to_owned())
}

pub fn default_worker_id() -> String {
    let host = fs::read_to_string("/etc/hostname")
        .ok()
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
                .filter(|h| !h.is_empty())
        })
        .unwrap_or_else(|| "worker".into());
    format!("{host}:{}", std::process::id())
}

fn log(value: Value) {
    eprintln!("{value}");
}

struct Shared {
    client: Client,
    /// name -> (local spec, accepts --serve)
    local: BTreeMap<String, (ProcessorSpec, bool)>,
    options: Options,
    scratch: PathBuf,
    /// Sockets and logs of warm `--serve` processors; survives worker restarts so a
    /// restarted worker reuses still-running servers.
    servers: PathBuf,
    idle: AtomicBool,
}

/// Runs until killed (or, with `exit_when_idle`, until a claim returns no jobs).
pub fn run(home: &Path, local: Vec<(ProcessorSpec, bool)>, options: Options) -> Result<Value> {
    ensure!(
        (1..=32).contains(&options.concurrency),
        "--concurrency must be 1–32"
    );
    ensure!(
        vamp_core::LEASE_SECONDS.contains(&options.lease_seconds),
        "--lease-seconds out of range"
    );
    let mut local: BTreeMap<String, (ProcessorSpec, bool)> = local
        .into_iter()
        .map(|(spec, serve)| (spec.name.clone(), (spec, serve)))
        .collect();
    if !options.processors.is_empty() {
        for name in &options.processors {
            ensure!(
                local.contains_key(name),
                "processor {name} is not installed on this machine (have: {:?})",
                local.keys().collect::<Vec<_>>()
            );
        }
        local.retain(|name, _| options.processors.contains(name));
    }
    ensure!(!local.is_empty(), "no local processors to offer");
    let client = Client::new(&Remote {
        url: options.server.clone(),
        token: options.token.clone(),
        drive_roots: Default::default(),
    })?;
    let library: Value = client.get_json("/library", &[])?;
    // Leftover inputs of a killed worker; their jobs are reclaimed by lease expiry.
    let scratch = home.join("tools/worker");
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch)?;
    let servers = home.join("tools/worker-servers");
    fs::create_dir_all(&servers)?;
    log(
        json!({"event":"worker_started","worker_id":options.worker_id,
        "server":options.server,"library_id":library["library_id"],
        "processors":local.keys().collect::<Vec<_>>(),"concurrency":options.concurrency}),
    );
    let shared = Arc::new(Shared {
        client,
        local,
        scratch,
        servers,
        idle: AtomicBool::new(false),
        options,
    });
    let slots = (0..shared.options.concurrency)
        .map(|slot| {
            let shared = shared.clone();
            thread::spawn(move || slot_loop(&shared, slot))
        })
        .collect::<Vec<_>>();
    let mut stats = json!({"completed":0,"failed":0});
    for slot in slots {
        let (completed, failed) = slot
            .join()
            .map_err(|_| anyhow::anyhow!("worker slot panicked"))?;
        stats["completed"] = json!(stats["completed"].as_u64().unwrap() + completed);
        stats["failed"] = json!(stats["failed"].as_u64().unwrap() + failed);
    }
    Ok(stats)
}

fn slot_loop(shared: &Shared, slot: usize) -> (u64, u64) {
    let options = &shared.options;
    let names: Vec<&String> = shared.local.keys().collect();
    let worker_id = format!("{}#{slot}", options.worker_id);
    let (mut completed, mut failed) = (0, 0);
    let mut backoff = options.poll_seconds;
    loop {
        if options.exit_when_idle && shared.idle.load(Ordering::Relaxed) {
            return (completed, failed);
        }
        let claimed: Result<Value> = shared.client.post_json(
            "/jobs/claim",
            &json!({"worker_id":worker_id,"processors":names,"limit":1,
                "lease_seconds":options.lease_seconds}),
        );
        let jobs = match claimed {
            Ok(body) => {
                backoff = options.poll_seconds;
                body["jobs"].as_array().cloned().unwrap_or_default()
            }
            Err(error) => {
                // Authority restarting or unreachable: keep trying, never exit.
                log(json!({"event":"claim_failed","slot":slot,"error":format!("{error:#}")}));
                thread::sleep(Duration::from_secs(backoff));
                backoff = (backoff * 2).min(60);
                continue;
            }
        };
        let Some(claim) = jobs.into_iter().next() else {
            if options.exit_when_idle {
                shared.idle.store(true, Ordering::Relaxed);
                return (completed, failed);
            }
            thread::sleep(Duration::from_secs(options.poll_seconds));
            continue;
        };
        match process(shared, &claim) {
            Ok(true) => completed += 1,
            Ok(false) => failed += 1,
            Err(error) => {
                failed += 1;
                log(
                    json!({"event":"job_error","job_id":claim["job"]["id"],"error":format!("{error:#}")}),
                );
            }
        }
    }
}

/// Heartbeats a lease until dropped; `lost` turns true once the authority reclaimed it.
struct Heartbeat {
    stop: Option<mpsc::Sender<()>>,
    handle: Option<thread::JoinHandle<()>>,
    lost: Arc<AtomicBool>,
}

impl Heartbeat {
    fn start(client: Client, job_id: String, lease_id: String, lease_seconds: u64) -> Self {
        let (stop, rx) = mpsc::channel::<()>();
        let lost = Arc::new(AtomicBool::new(false));
        let flag = lost.clone();
        let every = Duration::from_secs((lease_seconds / 3).max(1));
        let handle = thread::spawn(move || {
            while let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(every) {
                let result: Result<Value> = client.post_json(
                    &format!("/jobs/{job_id}/heartbeat"),
                    &json!({"lease_id":lease_id,"lease_seconds":lease_seconds}),
                );
                if let Err(error) = result {
                    let message = format!("{error:#}");
                    log(json!({"event":"heartbeat_failed","job_id":job_id,"error":message}));
                    if message.contains("409") || message.contains("404") {
                        flag.store(true, Ordering::Relaxed);
                        return;
                    }
                }
            }
        });
        Self {
            stop: Some(stop),
            handle: Some(handle),
            lost,
        }
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Run one claimed job; Ok(true) when the authority accepted the result.
fn process(shared: &Shared, claim: &Value) -> Result<bool> {
    let options = &shared.options;
    let job_id = claim["job"]["id"]
        .as_str()
        .context("claim without job id")?
        .to_owned();
    let lease_id = claim["lease"]["id"]
        .as_str()
        .context("claim without lease")?
        .to_owned();
    let processor = claim["job"]["processor"]["name"]
        .as_str()
        .unwrap_or_default();
    let (spec, serve) = shared
        .local
        .get(processor)
        .with_context(|| format!("authority sent unrequested processor {processor}"))?;
    let mut request: ToolRequest = serde_json::from_value(claim["request"].clone())?;
    let input_kind = claim["input"].as_str().unwrap_or("preview");
    let input_url = claim["input_url"]
        .as_str()
        .context("claim without input_url")?;
    let dir = shared.scratch.join(&job_id);
    fs::create_dir_all(&dir)?;
    let result = (|| -> Result<(ToolResponse, Value)> {
        let heartbeat = Heartbeat::start(
            shared.client.clone(),
            job_id.clone(),
            lease_id.clone(),
            options.lease_seconds,
        );
        let started = Instant::now();
        let extension = match input_kind {
            "original" => request
                .asset
                .as_ref()
                .and_then(|a| Path::new(&a.original.path).extension())
                .and_then(|e| e.to_str())
                .unwrap_or("bin")
                .to_owned(),
            _ => "jpg".into(),
        };
        let input = dir.join(format!("input.{extension}"));
        let expected = request
            .asset
            .as_ref()
            .filter(|a| input_kind == "original" && !a.original.pending())
            .map(|a| a.original.blake3.clone());
        shared.client.download(
            input_url.strip_prefix("/v1").unwrap_or(input_url),
            &input,
            expected.as_deref(),
        )?;
        let downloaded = started.elapsed();
        request.input_path = Some(input.to_str().context("non-UTF-8 scratch path")?.to_owned());
        #[cfg(unix)]
        let response = if *serve {
            vamp_core::run_served(
                &spec.command,
                &request,
                spec.timeout_seconds,
                &shared.servers.join(format!("{processor}.sock")),
                &shared.servers.join(format!("{processor}.log")),
            )?
        } else {
            vamp_core::run_tool(&spec.command, &request, spec.timeout_seconds)?
        };
        #[cfg(not(unix))]
        let response = {
            let _ = serve;
            vamp_core::run_tool(&spec.command, &request, spec.timeout_seconds)?
        };
        let ran = started.elapsed() - downloaded;
        ensure!(
            !heartbeat.lost.load(Ordering::Relaxed),
            "lease lost while processing; result discarded"
        );
        drop(heartbeat);
        Ok((
            response,
            json!({"download_ms":downloaded.as_millis(),"process_ms":ran.as_millis()}),
        ))
    })();
    let _ = fs::remove_dir_all(&dir);
    match result {
        Ok((response, timing)) => {
            let posted: Result<Value> = shared.client.post_json(
                &format!("/jobs/{job_id}/complete"),
                &json!({"lease_id":lease_id,"response":response}),
            );
            match posted {
                Ok(_) => {
                    log(
                        json!({"event":"job_completed","job_id":job_id,"processor":processor,
                        "asset_id":claim["job"]["asset_id"],"timing":timing}),
                    );
                    Ok(true)
                }
                Err(error) => {
                    log(
                        json!({"event":"job_rejected","job_id":job_id,"processor":processor,
                        "error":format!("{error:#}")}),
                    );
                    Ok(false)
                }
            }
        }
        Err(error) => {
            let message = format!("{error:#}");
            log(
                json!({"event":"job_failed","job_id":job_id,"processor":processor,"error":message}),
            );
            if !message.contains("lease lost") {
                let _: Result<Value> = shared.client.post_json(
                    &format!("/jobs/{job_id}/fail"),
                    &json!({"lease_id":lease_id,"error":message}),
                );
            }
            Ok(false)
        }
    }
}

/// systemd --user unit running `vamp worker` with the given arguments; returns
/// (unit path, unit text). Writes nothing.
pub fn service_unit(
    home: &Path,
    program: &Path,
    server: &str,
    token_file: Option<&Path>,
    processors: &[String],
    concurrency: usize,
) -> Result<(PathBuf, String)> {
    if server.contains(char::is_whitespace) {
        bail!("server URL cannot contain whitespace");
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("cannot locate the user config directory (HOME unset)")?;
    let quote = |p: &Path| -> Result<String> {
        let text = p.to_str().context("service paths must be UTF-8")?;
        ensure!(!text.contains('"'), "service paths cannot contain quotes");
        Ok(format!("\"{text}\""))
    };
    let mut exec = format!(
        "{} --home {} worker --server {server} --concurrency {concurrency}",
        quote(program)?,
        quote(home)?,
    );
    if let Some(file) = token_file {
        exec.push_str(&format!(" --token-file {}", quote(file)?));
    }
    for name in processors {
        exec.push_str(&format!(" --processors {name}"));
    }
    // PATH lets processors find uv-managed Python; restarts cover authority/network outages.
    let path = std::env::var("PATH").unwrap_or_default();
    let unit = format!(
        "[Unit]\nDescription=VAMP remote processor worker ({server})\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nExecStart={exec}\nEnvironment=\"PATH={path}\"\nRestart=always\nRestartSec=10\n\n[Install]\nWantedBy=default.target\n"
    );
    Ok((config.join("systemd/user/vamp-worker.service"), unit))
}
