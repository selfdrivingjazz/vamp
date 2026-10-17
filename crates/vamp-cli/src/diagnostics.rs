//! Optional crash and error reports, off unless the user enables them with
//! `vamp diagnostics enable --endpoint URL`. Reports go to that user-chosen OpenTelemetry
//! (OTLP/HTTP JSON) logs endpoint and nowhere else; no endpoint is compiled in.
//!
//! The choice is CLI-local (`<home>/diagnostics.json`, mode 600; separate from the catalog
//! config) and is consent to [`POLICY`]: a later release that changes what a report holds
//! stops reporting until the user enables it again. `DO_NOT_TRACK=1` in the environment
//! overrides that consent for the run. Without consent, or under `DO_NOT_TRACK`, nothing here
//! touches the network. A report holds only the fields of [`SENDS`]: bounded categories and names
//! defined by this program, never messages, arguments, paths, library content or identifiers.
//! Sending never changes a command's output or exit status.

use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write as _,
    panic::{Location, PanicHookInfo},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// What a report contains, versioned; consent recorded for another version is not used.
const POLICY: u32 = 1;
const CONFIG_FILE: &str = "diagnostics.json";
const STATE_FILE: &str = "diagnostics-state.json";
/// Serializes the send budget and receipts across processes (`File::lock`: released when the
/// holder exits, so a crash leaves no stale lock). Never deleted, since it may be held.
const LOCK_FILE: &str = "diagnostics.lock";
const LOCK_WAIT: Duration = Duration::from_secs(1);
/// Reports one process sends at most (a failing command plus panics on other threads).
const MAX_PER_PROCESS: u32 = 3;
/// Reports one home sends per hour at most, across processes (agents run vamp in loops).
const MAX_PER_HOUR: u32 = 10;
const MAX_BODY: usize = 4096;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const SEND_TIMEOUT: Duration = Duration::from_secs(3);
/// The crates whose source paths are public and safe to name in a panic report.
const CRATES: [&str; 4] = ["vamp-cli", "vamp-core", "vamp-protocol", "vamp-media"];

const SENDS: [&str; 7] = [
    "kind: error (a command failed), panic (vamp crashed) or test",
    "error category from a fixed list (io.not_found, io.permission_denied, json, network, panic, other, ...)",
    "the command path without its arguments (e.g. \"media inspect\")",
    "component (vamp-cli, vamp-core, ...) and, for a panic in VAMP's own source, its source file and line",
    "vamp version",
    "operating system and CPU architecture",
    "the time of the report",
];
const NEVER_SENDS: [&str; 5] = [
    "error or panic messages, stack traces",
    "command arguments, file paths or names, asset ids, tags, search text, URLs, environment",
    "library contents, metadata or media",
    "host, user, machine, install or session identifiers",
    "anything about commands that succeed (no usage analytics)",
];
const COVERS: &str = "Commands that fail with operation_failed, and Rust panics on any vamp thread (reported after the panic message is printed; exit status and unwinding are unchanged: 101 when the panic ends the process, and a panic a job runner catches stays caught). Not covered: invalid arguments, output errors, crashes that kill the process without a panic (signals such as SIGKILL or SIGSEGV, aborts, out-of-memory, stack overflow), and failures inside helper processes (vamp-media, model processors).";

static SENT: AtomicU32 = AtomicU32::new(0);

/// `<home>/diagnostics.json`: present only while reporting is enabled.
#[derive(Serialize, Deserialize)]
struct Settings {
    policy: u32,
    /// OTLP base URL (like `OTEL_EXPORTER_OTLP_ENDPOINT`); logs go to `<endpoint>/v1/logs`.
    endpoint: String,
    /// Request headers, e.g. `Authorization`; values are credentials and are never printed.
    #[serde(default)]
    headers: Vec<(String, String)>,
    consented_at_unix: u64,
}

/// `<home>/diagnostics-state.json`: the hourly send budget and the last delivery attempt.
#[derive(Serialize, Deserialize, Default)]
struct State {
    window_start_unix: u64,
    sent: u32,
    last_delivery: Option<Delivery>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Delivery {
    at_unix: u64,
    delivered: bool,
    /// HTTP status of the response, if one arrived.
    status: Option<u16>,
    /// `timeout`, `dns`, `connect`, `tls`, `http_status`, `io` or `other` when not delivered.
    error: Option<String>,
}

/// A report: only static names, categories and versions.
struct Report<'a> {
    kind: &'static str,
    category: &'a str,
    operation: &'a str,
    component: &'static str,
    site: Option<(String, u32)>,
}

/// Reporting enabled with current consent; the only way anything is sent.
pub struct Reporter {
    home: PathBuf,
    settings: Settings,
}

fn now() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

/// The saved choice. Errors are fixed texts: the file may hold credentials, and a parser error
/// would quote them.
fn settings(home: &Path) -> Result<Option<Settings>> {
    match fs::read(home.join(CONFIG_FILE)) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|_| {
            anyhow::anyhow!(
                "{CONFIG_FILE} is not valid, so reporting is off; run `vamp diagnostics disable`, then enable again"
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => bail!(
            "cannot read {CONFIG_FILE} ({:?}), so reporting is off",
            error.kind()
        ),
    }
}

/// An exclusive lock on `<home>/diagnostics.lock`, held until the file is dropped; `None` if
/// it cannot be taken within [`LOCK_WAIT`].
fn lock(home: &Path) -> Option<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(home.join(LOCK_FILE)).ok()?;
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Some(file),
            Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
}

fn state(home: &Path) -> State {
    fs::read(home.join(STATE_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Write `bytes` to `path` through a sibling file created with mode 600 (unix), then rename.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&temporary);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("creating {}", temporary.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path).with_context(|| format!("writing {}", path.display()))
}

fn remove(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

fn endpoint(text: &str) -> Result<String> {
    const EXAMPLE: &str = "e.g. http://localhost:4318 (an OpenTelemetry Collector or Grafana Alloy) or a Grafana Cloud OTLP endpoint https://otlp-gateway-<region>.grafana.net/otlp";
    let text = text.trim().trim_end_matches('/');
    ensure!(
        !text.is_empty(),
        "--endpoint is empty; give the OTLP/HTTP endpoint to report to, {EXAMPLE}"
    );
    // Checked on the text: URI parsing drops a fragment, and `<endpoint>/v1/logs` would then
    // post to the wrong path.
    ensure!(!text.contains('#'), "--endpoint must not have a #fragment");
    let uri: ureq::http::Uri = text
        .parse()
        .with_context(|| format!("--endpoint is not a URL; {EXAMPLE}"))?;
    ensure!(
        matches!(uri.scheme_str(), Some("http" | "https")),
        "--endpoint must be an http(s) URL, {EXAMPLE}"
    );
    let authority = uri
        .authority()
        .with_context(|| format!("--endpoint has no host, {EXAMPLE}"))?;
    ensure!(
        !authority.as_str().contains('@'),
        "--endpoint must not contain credentials; put them in a --headers-file"
    );
    ensure!(
        uri.query().is_none(),
        "--endpoint must not have a query string"
    );
    Ok(text.to_owned())
}

/// A `--headers-file`: UTF-8 `NAME=VALUE` lines (blank lines and `#` comments skipped), owner-
/// only on unix. Errors give the line number only, never text that may hold a credential.
fn headers(path: &Path) -> Result<Vec<(String, String)>> {
    const MAX_FILE: u64 = 64 << 10;
    let metadata = fs::metadata(path)
        .with_context(|| format!("cannot read --headers-file {}", path.display()))?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_FILE,
        "--headers-file must be a file of at most 64 KiB"
    );
    #[cfg(unix)]
    ensure!(
        std::os::unix::fs::PermissionsExt::mode(&metadata.permissions()) & 0o077 == 0,
        "--headers-file {} is readable by other users; chmod 600 it",
        path.display()
    );
    let text = String::from_utf8(
        fs::read(path).with_context(|| format!("cannot read --headers-file {}", path.display()))?,
    )
    .map_err(|_| anyhow::anyhow!("--headers-file is not UTF-8"))?;
    let mut headers = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let number = index + 1;
        let (name, value) = line.split_once('=').with_context(|| {
            format!("--headers-file line {number} is not NAME=VALUE (e.g. Authorization=Basic <base64 of instance:token>)")
        })?;
        let (name, value) = (name.trim(), value.trim());
        ensure!(
            !name.is_empty() && ureq::http::HeaderName::from_bytes(name.as_bytes()).is_ok(),
            "--headers-file line {number}: the NAME is not a valid HTTP header name"
        );
        ensure!(
            !value.is_empty() && ureq::http::HeaderValue::from_str(value).is_ok(),
            "--headers-file line {number}: the VALUE is empty or not a valid HTTP header value"
        );
        headers.push((name.to_owned(), value.to_owned()));
    }
    Ok(headers)
}

fn argv(home: &Path, command: &[&str]) -> Vec<Value> {
    ["vamp", "--home"]
        .iter()
        .map(|s| json!(s))
        .chain([json!(home)])
        .chain(command.iter().map(|s| json!(s)))
        .collect()
}

/// The choice as `vamp diagnostics status` and `vamp setup` show it; credentials redacted.
pub fn status(home: &Path) -> Result<Value> {
    let (settings, error) = match settings(home) {
        Ok(settings) => (settings, None),
        Err(error) => (None, Some(format!("{error:#}"))),
    };
    let consented = settings.as_ref().is_some_and(|s| s.policy == POLICY);
    let do_not_track = do_not_track();
    let mut out = json!({
        "capability": "diagnostics",
        "enabled": consented && !do_not_track,
        "consented": consented,
        "do_not_track": do_not_track,
        "default": "off",
        "config": home.join(CONFIG_FILE),
        "endpoint": settings.as_ref().map(|s| &s.endpoint),
        "logs_url": settings.as_ref().map(|s| format!("{}/v1/logs", s.endpoint)),
        "headers": settings.as_ref().map_or_else(serde_json::Map::new, |s| s.headers.iter()
            .map(|(name, _)| (name.clone(), json!("<redacted>"))).collect()),
        "consented_at_unix": settings.as_ref().map(|s| s.consented_at_unix),
        "policy": POLICY,
        "last_delivery": state(home).last_delivery,
        "sends": SENDS,
        "never_sends": NEVER_SENDS,
        "covers": COVERS,
        "limits": {"per_process": MAX_PER_PROCESS, "per_hour": MAX_PER_HOUR, "max_bytes": MAX_BODY,
            "timeout_seconds": SEND_TIMEOUT.as_secs(), "queue": "none (sent synchronously, never retried)"},
        "enable_argv": argv(home, &["diagnostics", "enable", "--endpoint", "OTLP_HTTP_URL", "--headers-file", "HEADERS_FILE"]),
        "disable_argv": argv(home, &["diagnostics", "disable"]),
        "test_argv": argv(home, &["diagnostics", "test"]),
    });
    if let Some(settings) = &settings
        && settings.policy != POLICY
    {
        out["note"] = json!(format!(
            "reporting is paused: it was enabled for report policy {} and this vamp sends policy {POLICY}; review `sends` and enable again",
            settings.policy
        ));
    }
    if consented && do_not_track {
        out["note"] = json!(
            "reporting is off for this run: DO_NOT_TRACK is set, which overrides the consent in diagnostics.json"
        );
    }
    if let Some(error) = error {
        out["error"] = json!(error);
    }
    Ok(out)
}

/// Explicit opt-in: record consent to [`POLICY`] with a validated destination. Header values
/// are copied from `headers_file` into `diagnostics.json` (mode 600).
pub fn enable(home: &Path, endpoint_text: &str, headers_file: Option<&Path>) -> Result<Value> {
    let endpoint = endpoint(endpoint_text)?;
    let headers = headers_file.map(headers).transpose()?.unwrap_or_default();
    if !headers.is_empty() && endpoint.starts_with("http://") {
        let uri: ureq::http::Uri = endpoint.parse()?;
        let host = uri
            .host()
            .unwrap_or("")
            .trim_start_matches('[')
            .trim_end_matches(']');
        let loopback = host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        ensure!(
            loopback,
            "collector credentials require HTTPS unless the collector is loopback"
        );
    }
    fs::create_dir_all(home).with_context(|| format!("creating {}", home.display()))?;
    let settings = Settings {
        policy: POLICY,
        endpoint,
        headers,
        consented_at_unix: now().as_secs(),
    };
    write_private(
        &home.join(CONFIG_FILE),
        &serde_json::to_vec_pretty(&settings)?,
    )?;
    status(home)
}

/// Opt-out: forget the destination, its credentials and the send history. Taken under the
/// send lock, so no running vamp is between its consent check and its state write; reporters
/// in running processes re-check consent before every send and stop. The lock file stays.
pub fn disable(home: &Path) -> Result<Value> {
    let guard = if home.is_dir() { lock(home) } else { None };
    remove(&home.join(CONFIG_FILE))?;
    remove(&home.join(STATE_FILE))?;
    drop(guard);
    status(home)
}

/// Send one `test` report now and say whether the endpoint accepted it.
pub fn test(home: &Path) -> Result<Value> {
    ensure!(
        !do_not_track(),
        "DO_NOT_TRACK is set, which turns diagnostics off for this run; unset it to send a test report"
    );
    let Some(reporter) = reporter(home) else {
        bail!(
            "diagnostics are off; enable them first: vamp diagnostics enable --endpoint OTLP_HTTP_URL"
        );
    };
    let report = Report {
        kind: "test",
        category: "test",
        operation: "diagnostics test",
        component: "vamp-cli",
        site: None,
    };
    let delivery = reporter
        .send(&report)
        .context("not sent: the per-process or hourly limit was reached, the send state was busy or unwritable, or reporting was turned off meanwhile; try again later")?;
    ensure!(
        delivery.delivered,
        "the endpoint did not accept the test report ({}{})",
        delivery.error.as_deref().unwrap_or("error"),
        delivery
            .status
            .map(|s| format!(", HTTP {s}"))
            .unwrap_or_default()
    );
    Ok(
        json!({"sent": true, "logs_url": format!("{}/v1/logs", reporter.settings.endpoint), "delivery": delivery}),
    )
}

/// `DO_NOT_TRACK` set to anything but empty, `0` or `false` (consoledonottrack.com): no
/// reports, whatever `diagnostics.json` says.
fn do_not_track() -> bool {
    std::env::var_os("DO_NOT_TRACK").is_some_and(|value| {
        let value = value.to_string_lossy();
        let value = value.trim();
        !(value.is_empty() || value == "0" || value.eq_ignore_ascii_case("false"))
    })
}

/// The reporter if the user enabled reporting under the current policy and `DO_NOT_TRACK`
/// is not set; else `None`, and nothing is sent.
pub fn reporter(home: &Path) -> Option<Arc<Reporter>> {
    if do_not_track() {
        return None;
    }
    let settings = settings(home).ok().flatten()?;
    (settings.policy == POLICY).then(|| {
        Arc::new(Reporter {
            home: home.to_path_buf(),
            settings,
        })
    })
}

/// The invoked command path from parsed arguments: subcommand names only, never values.
pub fn operation(matches: &clap::ArgMatches) -> String {
    let mut names = Vec::new();
    let mut matches = matches;
    while let Some((name, sub)) = matches.subcommand() {
        names.push(name);
        matches = sub;
    }
    names.join(" ")
}

/// Bounded category of a failed command, from the error types in its chain.
fn category(error: &anyhow::Error) -> String {
    for cause in error.chain() {
        if let Some(error) = cause.downcast_ref::<std::io::Error>() {
            let kind = format!("{:?}", error.kind());
            let mut snake = String::from("io.");
            for (i, c) in kind.chars().take(40).enumerate() {
                if c.is_ascii_uppercase() && i > 0 {
                    snake.push('_');
                }
                snake.push(c.to_ascii_lowercase());
            }
            return snake;
        }
        if cause.is::<serde_json::Error>() {
            return "json".into();
        }
        if cause.is::<ureq::Error>() {
            return "network".into();
        }
    }
    "other".into()
}

/// Component and, inside VAMP's own crates, the crate-relative source file and line (never
/// the directories above it, which hold the builder's home or checkout).
fn site(file: &str, line: u32) -> (&'static str, Option<(String, u32)>) {
    let file = file.replace('\\', "/");
    for krate in CRATES {
        let crate_dir = format!("/{krate}/src/");
        let found = file
            .rfind(&crate_dir)
            .map(|at| at + 1)
            .or_else(|| file.starts_with(&crate_dir[1..]).then_some(0));
        if let Some(at) = found {
            let relative = &file[at + krate.len() + 1..];
            let safe = relative.len() <= 120
                && relative
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/'));
            return (krate, safe.then(|| (relative.to_owned(), line)));
        }
    }
    ("dependency", None)
}

fn attribute(key: &str, value: &str) -> Value {
    json!({"key": key, "value": {"stringValue": value}})
}

/// The OTLP/HTTP JSON logs request for one report.
fn body(report: &Report<'_>, time: Duration) -> Vec<u8> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        os => os,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        arch => arch,
    };
    let operation: String = report.operation.chars().take(64).collect();
    let mut attributes = vec![
        attribute("app.diagnostics.kind", report.kind),
        attribute("error.type", report.category),
        attribute("app.component", report.component),
        attribute("app.operation", &operation),
    ];
    if let Some((file, line)) = &report.site {
        attributes.push(attribute("code.file.path", file));
        attributes
            .push(json!({"key": "code.line.number", "value": {"intValue": line.to_string()}}));
    }
    let (severity, text) = if report.kind == "panic" {
        (21, "FATAL")
    } else {
        (17, "ERROR")
    };
    let nanos = time.as_nanos().to_string();
    serde_json::to_vec(&json!({"resourceLogs": [{
        "resource": {"attributes": [
            attribute("service.name", "vamp"),
            attribute("service.version", env!("CARGO_PKG_VERSION")),
            attribute("os.type", os),
            attribute("host.arch", arch),
        ]},
        "scopeLogs": [{
            "scope": {"name": "vamp.diagnostics", "version": POLICY.to_string()},
            "logRecords": [{
                "timeUnixNano": nanos,
                "observedTimeUnixNano": nanos,
                "severityNumber": severity,
                "severityText": text,
                "body": {"stringValue": format!("vamp {}: {}", report.kind, report.category)},
                "attributes": attributes,
            }],
        }],
    }]}))
    .expect("report serializes")
}

fn failure(error: &ureq::Error) -> &'static str {
    match error {
        ureq::Error::StatusCode(_) => "http_status",
        ureq::Error::Timeout(_) => "timeout",
        ureq::Error::HostNotFound => "dns",
        ureq::Error::ConnectionFailed => "connect",
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) | ureq::Error::Pem(_) => "tls",
        ureq::Error::Io(_) => "io",
        _ => "other",
    }
}

impl Reporter {
    /// Report a command that failed with `error`; never fails.
    pub fn error(&self, operation: &str, error: &anyhow::Error) {
        let category = category(error);
        let _ = self.send(&Report {
            kind: "error",
            category: &category,
            operation,
            component: "vamp-cli",
            site: None,
        });
    }

    /// Report panics on every thread after the existing hook prints them; unwinding and the
    /// exit status are unchanged.
    pub fn install_panic_hook(self: Arc<Self>, operation: String) {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info: &PanicHookInfo<'_>| {
            previous(info);
            let (component, source) = info
                .location()
                .map_or(("unknown", None), |l: &Location<'_>| {
                    site(l.file(), l.line())
                });
            let _ = self.send(&Report {
                kind: "panic",
                category: "panic",
                operation: &operation,
                component,
                site: source,
            });
        }));
    }

    /// The consent this reporter was created under still holds: same policy, destination and
    /// headers. Read again before every send, so `disable` or a changed destination stops
    /// long-running processes (`jobs watch`, `sync --watch`, `worker`).
    fn consented(&self) -> bool {
        settings(&self.home).ok().flatten().is_some_and(|s| {
            s.policy == POLICY
                && s.endpoint == self.settings.endpoint
                && s.headers == self.settings.headers
        })
    }

    /// One bounded, synchronous POST if consent still holds and the per-process and hourly
    /// budgets allow it; `None` otherwise. The budget is reserved and the receipt recorded under
    /// the home's lock, each from a fresh read, so concurrent processes never lose updates.
    fn send(&self, report: &Report<'_>) -> Option<Delivery> {
        let now = now();
        let state_path = self.home.join(STATE_FILE);
        {
            let _lock = lock(&self.home)?;
            if !self.consented() || SENT.fetch_add(1, Ordering::Relaxed) >= MAX_PER_PROCESS {
                return None;
            }
            let mut state = state(&self.home);
            if now.as_secs().saturating_sub(state.window_start_unix) >= 3600 {
                state.window_start_unix = now.as_secs();
                state.sent = 0;
            }
            if state.sent >= MAX_PER_HOUR {
                return None;
            }
            state.sent += 1;
            write_private(&state_path, &serde_json::to_vec(&state).ok()?).ok()?;
        }

        let body = body(report, now);
        if body.len() > MAX_BODY {
            return None;
        }
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_global(Some(SEND_TIMEOUT))
            .max_redirects(0)
            .user_agent(concat!("vamp/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        let mut request = agent
            .post(format!("{}/v1/logs", self.settings.endpoint))
            .header("content-type", "application/json");
        for (name, value) in &self.settings.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let delivery = match request.send(&body[..]) {
            Ok(response) => {
                let status = response.status().as_u16();
                Delivery {
                    at_unix: now.as_secs(),
                    delivered: response.status().is_success(),
                    status: Some(status),
                    error: (!response.status().is_success()).then(|| "http_status".into()),
                }
            }
            Err(error) => Delivery {
                at_unix: now.as_secs(),
                delivered: false,
                status: match error {
                    ureq::Error::StatusCode(status) => Some(status),
                    _ => None,
                },
                error: Some(failure(&error).into()),
            },
        };
        // Only the receipt changes; the budget comes from a fresh read. A home disabled
        // meanwhile stays clean.
        if let Some(_lock) = lock(&self.home)
            && self.consented()
        {
            let mut state = state(&self.home);
            state.last_delivery = Some(delivery.clone());
            let _ = serde_json::to_vec(&state).map(|bytes| write_private(&state_path, &bytes));
        }
        Some(delivery)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead as _, BufReader, Read as _},
        net::TcpListener,
    };

    fn keys(body: &Value) -> Vec<String> {
        let record = &body["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
        let resource = &body["resourceLogs"][0]["resource"]["attributes"];
        resource
            .as_array()
            .unwrap()
            .iter()
            .chain(record["attributes"].as_array().unwrap())
            .map(|a| a["key"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn error_reports_hold_only_allowlisted_fields() {
        let secret = "/home/someone/Pictures/private-shoot/IMG_0001.heic";
        let error = anyhow::Error::from(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("open {secret}"),
        ))
        .context(format!("adding {secret} with tag nda-client"));
        let category = category(&error);
        assert_eq!(category, "io.not_found");
        let report = Report {
            kind: "error",
            category: &category,
            operation: "media add",
            component: "vamp-cli",
            site: None,
        };
        let bytes = body(&report, Duration::from_secs(1));
        assert!(bytes.len() <= MAX_BODY);
        let text = String::from_utf8(bytes).unwrap();
        for leak in ["someone", "Pictures", "IMG_0001", "nda-client", "open "] {
            assert!(!text.contains(leak), "report leaks {leak:?}: {text}");
        }
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            keys(&value),
            [
                "service.name",
                "service.version",
                "os.type",
                "host.arch",
                "app.diagnostics.kind",
                "error.type",
                "app.component",
                "app.operation"
            ]
        );
        assert_eq!(
            value["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]["severityNumber"],
            17
        );
    }

    #[test]
    fn panic_sites_name_only_vamp_source() {
        assert_eq!(
            site("/home/someone/src/vamp/crates/vamp-core/src/library.rs", 7),
            ("vamp-core", Some(("src/library.rs".into(), 7)))
        );
        assert_eq!(
            site("crates/vamp-cli/src/main.rs", 3),
            ("vamp-cli", Some(("src/main.rs".into(), 3)))
        );
        assert_eq!(
            site("/home/someone/.cargo/registry/src/x/serde-1.0/src/de.rs", 1),
            ("dependency", None)
        );
        assert_eq!(site("/x/not-vamp-core/src/lib.rs", 1), ("dependency", None));
        assert_eq!(site("/x/vamp-core/src/a b.rs", 1), ("vamp-core", None));
        let here = Location::caller();
        assert_eq!(
            site(here.file(), here.line()),
            ("vamp-cli", Some(("src/diagnostics.rs".into(), here.line())))
        );
    }

    #[test]
    fn enabling_needs_a_usable_destination() {
        let home = tempfile::tempdir().unwrap();
        for bad in [
            "",
            "localhost:4318",
            "ftp://collector:21",
            "https://user:pw@otlp.example/otlp",
            "https://otlp.example/otlp?key=1",
            "https://otlp.example/otlp#x",
            "not a url",
        ] {
            assert!(enable(home.path(), bad, None).is_err(), "accepted {bad:?}");
        }
        let files = tempfile::tempdir().unwrap();
        for (name, text) in [
            ("no-equals", "Authorization Basic c2VjcmV0\n"),
            ("bad-name", "Bad Name=c2VjcmV0\n"),
            ("empty-value", "Authorization=\n"),
            ("bad-value", "Authorization=Basic c2Vj\u{7}cmV0\n"),
        ] {
            let path = headers_file(files.path(), name, text);
            let error = format!(
                "{:#}",
                enable(home.path(), "http://127.0.0.1:4318", Some(path.as_path())).unwrap_err()
            );
            assert!(
                !error.contains("c2Vj") && !error.contains("Bad Name"),
                "{name}: {error}"
            );
            assert!(error.contains("line 1"), "{name}: {error}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let path = headers_file(files.path(), "shared", "Authorization=Basic c2VjcmV0\n");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            let error = format!(
                "{:#}",
                enable(home.path(), "http://127.0.0.1:4318", Some(path.as_path())).unwrap_err()
            );
            assert!(error.contains("chmod 600"), "{error}");
        }
        let missing = files.path().join("absent");
        assert!(
            enable(
                home.path(),
                "http://127.0.0.1:4318",
                Some(missing.as_path())
            )
            .is_err()
        );
        assert!(!home.path().join(CONFIG_FILE).exists());
        assert!(reporter(home.path()).is_none());
        assert_eq!(status(home.path()).unwrap()["enabled"], false);
    }

    #[test]
    fn collector_credentials_require_tls_outside_loopback() {
        let home = tempfile::tempdir().unwrap();
        let files = tempfile::tempdir().unwrap();
        let file = headers_file(
            files.path(),
            "private",
            "Authorization=Bearer private-canary\n",
        );
        for url in [
            "http://example.com:4318",
            "http://192.0.2.1:4318",
            "http://[2001:db8::1]:4318",
        ] {
            let error = enable(home.path(), url, Some(&file))
                .unwrap_err()
                .to_string();
            assert!(error.contains("require HTTPS"));
            assert!(!error.contains("private-canary"));
            assert!(!home.path().join(CONFIG_FILE).exists());
        }
        for url in [
            "http://localhost:4318",
            "http://127.0.0.1:4318",
            "http://[::1]:4318",
            "https://example.com/otlp",
        ] {
            enable(home.path(), url, Some(&file)).unwrap();
        }
    }

    #[test]
    fn consent_for_another_policy_is_not_used() {
        let home = tempfile::tempdir().unwrap();
        enable(home.path(), "http://127.0.0.1:9", None).unwrap();
        let path = home.path().join(CONFIG_FILE);
        let mut settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        settings["policy"] = json!(POLICY + 1);
        fs::write(&path, settings.to_string()).unwrap();
        assert!(reporter(home.path()).is_none());
        let status = status(home.path()).unwrap();
        assert_eq!(status["enabled"], false);
        assert!(status["note"].as_str().unwrap().contains("paused"));
    }

    #[test]
    fn malformed_settings_never_echo_their_content() {
        let home = tempfile::tempdir().unwrap();
        fs::write(
            home.path().join(CONFIG_FILE),
            r#"{"policy":1,"endpoint":"http://127.0.0.1:9","headers":["Authorization: Basic c2VjcmV0"],"consented_at_unix":1}"#,
        )
        .unwrap();
        assert!(reporter(home.path()).is_none());
        let status = status(home.path()).unwrap();
        assert_eq!(status["enabled"], false);
        assert!(status["error"].as_str().unwrap().contains("not valid"));
        assert!(!status.to_string().contains("c2VjcmV0"), "{status}");
    }

    /// A `--headers-file` with owner-only permissions.
    fn headers_file(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        write_private(&path, text.as_bytes()).unwrap();
        path
    }

    struct Request {
        head: String,
        body: String,
    }

    /// A local OTLP/HTTP receiver; each request is queued before its 200 response is written.
    fn receiver() -> (String, std::sync::mpsc::Receiver<Request>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://127.0.0.1:{}/otlp",
            listener.local_addr().unwrap().port()
        );
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut reader = BufReader::new(stream.unwrap());
                let mut head = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                    head.push_str(&line);
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let body = String::from_utf8(body).unwrap();
                if sender.send(Request { head, body }).is_err() {
                    return;
                }
                let _ = reader.get_mut().write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                );
            }
        });
        (url, receiver)
    }

    /// Built directly so a DO_NOT_TRACK in the test environment does not skip it.
    fn running_reporter(home: &Path) -> Arc<Reporter> {
        Arc::new(Reporter {
            home: home.to_path_buf(),
            settings: settings(home).unwrap().unwrap(),
        })
    }

    const TEST_REPORT: Report<'static> = Report {
        kind: "test",
        category: "test",
        operation: "jobs watch",
        component: "vamp-cli",
        site: None,
    };

    /// A reporter in a long-running process stops as soon as the user disables reporting or
    /// changes its policy, destination or credentials, and resumes only under the same consent.
    #[test]
    fn revoked_consent_stops_a_running_reporter() {
        let quiet = Duration::from_millis(300);
        let home = tempfile::tempdir().unwrap();
        let files = tempfile::tempdir().unwrap();
        let old = headers_file(files.path(), "old", "Authorization=Basic b2xk\n");
        let new = headers_file(files.path(), "new", "Authorization=Basic bmV3\n");
        let (url, requests) = receiver();
        enable(home.path(), &url, Some(old.as_path())).unwrap();
        let reporter = running_reporter(home.path());
        assert!(reporter.send(&TEST_REPORT).unwrap().delivered);
        requests.recv_timeout(Duration::from_secs(5)).unwrap();

        disable(home.path()).unwrap();
        assert!(reporter.send(&TEST_REPORT).is_none());
        assert!(requests.recv_timeout(quiet).is_err());
        assert!(!home.path().join(STATE_FILE).exists());

        let (other, other_requests) = receiver();
        enable(home.path(), &other, Some(old.as_path())).unwrap();
        assert!(reporter.send(&TEST_REPORT).is_none());
        enable(home.path(), &url, Some(new.as_path())).unwrap();
        assert!(reporter.send(&TEST_REPORT).is_none());
        enable(home.path(), &url, Some(old.as_path())).unwrap();
        let path = home.path().join(CONFIG_FILE);
        let mut saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        saved["policy"] = json!(POLICY + 1);
        fs::write(&path, saved.to_string()).unwrap();
        assert!(reporter.send(&TEST_REPORT).is_none());
        assert!(requests.recv_timeout(quiet).is_err());
        assert!(other_requests.recv_timeout(quiet).is_err());

        enable(home.path(), &url, Some(old.as_path())).unwrap();
        assert!(reporter.send(&TEST_REPORT).unwrap().delivered);
        assert!(
            requests
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .head
                .contains("b2xk")
        );
    }

    const PANIC_CHILD_HOME: &str = "VAMP_DIAGNOSTICS_PANIC_CHILD_HOME";

    /// Child half of `panic_hook_reports_without_the_message`, run alone in its own process so
    /// the process-wide panic hook never sees another test's panic. A no-op in the normal suite.
    #[test]
    fn panic_hook_child() {
        let Some(home) = std::env::var_os(PANIC_CHILD_HOME) else {
            return;
        };
        running_reporter(Path::new(&home)).install_panic_hook("media inspect".into());
        let unwound = std::panic::catch_unwind(|| panic!("secret asset 0f3a in /home/someone"));
        assert!(unwound.is_err(), "the panic must still unwind");
    }

    /// A panic in an opted-in process reaches the endpoint as a FATAL record without its
    /// message, after the panic message is printed, and still unwinds.
    #[test]
    fn panic_hook_reports_without_the_message() {
        let home = tempfile::tempdir().unwrap();
        let files = tempfile::tempdir().unwrap();
        let (url, requests) = receiver();
        let headers = headers_file(files.path(), "headers", "Authorization=Basic dGVzdA==\n");
        enable(home.path(), &url, Some(headers.as_path())).unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "diagnostics::tests::panic_hook_child",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(PANIC_CHILD_HOME, home.path())
            .env_remove("DO_NOT_TRACK")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&child.stderr);
        assert!(child.status.success(), "{stderr}");
        assert!(
            stderr.contains("secret asset 0f3a"),
            "the panic message is still printed: {stderr}"
        );
        let Request { head, body } = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(head.starts_with("POST /otlp/v1/logs "), "{head}");
        assert!(head.contains("Basic dGVzdA=="));
        assert!(
            !body.contains("secret") && !body.contains("0f3a") && !body.contains("someone"),
            "{body}"
        );
        let value: Value = serde_json::from_str(&body).unwrap();
        let record = &value["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0];
        assert_eq!(record["severityNumber"], 21);
        assert!(body.contains("\"app.operation\"") && body.contains("media inspect"));
        assert!(body.contains("src/diagnostics.rs"));
        let status = status(home.path()).unwrap();
        assert_eq!(status["last_delivery"]["delivered"], true);
        assert!(!status.to_string().contains("dGVzdA=="));
    }
}
