//! Shared, opt-in diagnostics for VAMP and the Vampire apps. Consent is scoped
//! to the diagnostics home, endpoint, and policy. Only typed, bucketed events
//! reach the user-chosen OTLP/HTTP endpoint.

pub mod apple;
mod event;
pub use event::{Event, Feature, LibraryRole, Outcome, Product};
use event::{Component, Record};

use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// What a report contains, versioned; consent recorded for another version is not used.
const POLICY: u32 = 2;
const CONFIG_FILE: &str = "diagnostics.json";
const STATE_FILE: &str = "diagnostics-state.json";
/// Serializes the send budget and receipts across processes (`File::lock`: released when the
/// holder exits, so a crash leaves no stale lock). Never deleted, since it may be held.
const LOCK_FILE: &str = "diagnostics.lock";
const LOCK_WAIT: Duration = Duration::from_secs(1);
const MAX_PER_HOUR: u32 = 10;
const MAX_BODY: usize = 16 * 1024;
const MAX_BATCH: usize = 16;
const MAX_PENDING: usize = 32;
const MAX_AGE: u64 = 24 * 3600;
const MIN_INTERVAL: u64 = 15;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const SEND_TIMEOUT: Duration = Duration::from_secs(3);

const SENDS: &[&str] = &[
    "Product, release version, platform and telemetry policy version",
    "Event kind (use, failure, Rust panic or test), feature and outcome (ok or failed)",
    "When measured: library role (authority or replica), item-count range and time range",
];
const NEVER_SENDS: &[&str] = &[
    "Files, filenames, paths, URLs, hosts, tags, captions, searches, library or item IDs, or library contents",
    "Email, tokens, machine or install IDs, hardware details, exact times, error messages or stack traces",
];
const COVERS: &str = "Instrumented feature use and handled failures, and Rust panics in vamp and vamp-server. Does not send Apple crash reports, stack traces, or unexpected process exits (such as signals or out-of-memory).";

/// `<home>/diagnostics.json`: present only while reporting is enabled.
#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct Settings {
    policy: u32,
    /// OTLP base URL (like `OTEL_EXPORTER_OTLP_ENDPOINT`); logs go to `<endpoint>/v1/logs`.
    endpoint: String,
    /// Request headers, e.g. `Authorization`; values are credentials and are never printed.
    #[serde(default)]
    headers: Vec<(String, String)>,
    consented_at_unix: u64,
    /// Local only: invalidate reporters buffered under a previous opt-in, even to the same URL.
    #[serde(default)]
    generation: String,
}

/// Local send state. Queues never share records across products or release versions.
#[derive(Serialize, Deserialize, Default)]
struct State {
    window_start_unix: u64,
    sent: u32,
    last_delivery: Option<Delivery>,
    #[serde(default)]
    queues: [Queue; 3],
}

#[derive(Serialize, Deserialize, Default)]
struct Queue {
    version: String,
    last_attempt_unix: Option<u64>,
    pending: VecDeque<Pending>,
}

#[derive(Serialize, Deserialize)]
struct Pending {
    at_unix: u64,
    record: Record,
}

impl State {
    fn reserve(&mut self, now: u64) -> bool {
        if now < self.window_start_unix || now - self.window_start_unix >= 3600 {
            self.window_start_unix = now;
            self.sent = 0;
        }
        if self.sent >= MAX_PER_HOUR {
            return false;
        }
        self.sent += 1;
        true
    }
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

/// Reporting enabled with current consent; the only way anything is sent.
pub struct Reporter {
    home: PathBuf,
    settings: Settings,
    product: Product,
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

fn lock_file(home: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(home.join(LOCK_FILE))
}

fn config_lock(home: &Path) -> Result<fs::File> {
    let file = lock_file(home).context("opening the diagnostics lock")?;
    file.lock().context("locking diagnostics settings")?;
    Ok(file)
}

/// Senders give up under contention; an explicit consent change waits.
fn lock(home: &Path) -> Option<fs::File> {
    let file = lock_file(home).ok()?;
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

fn save_state(path: &Path, state: &State) -> Result<()> {
    write_private(path, &serde_json::to_vec(state)?)
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
    if uri.scheme_str() == Some("http") {
        let host = uri.host().unwrap_or_default().trim_start_matches('[').trim_end_matches(']');
        ensure!(
            host.eq_ignore_ascii_case("localhost")
                || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback()),
            "collectors require HTTPS outside loopback"
        );
    }
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

fn current(settings: &Settings) -> bool {
    settings.policy == POLICY
        && !settings.generation.is_empty()
        && endpoint(&settings.endpoint).is_ok_and(|url| url == settings.endpoint)
}

/// The choice as `vamp diagnostics status` shows it; credentials redacted.
pub fn status(home: &Path) -> Result<Value> {
    let (settings, error) = match settings(home) {
        Ok(settings) => (settings, None),
        Err(error) => (None, Some(format!("{error:#}"))),
    };
    let consented = settings.as_ref().is_some_and(current);
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
        "last_delivery": consented.then(|| state(home).last_delivery).flatten(),
        "sends": SENDS,
        "never_sends": NEVER_SENDS,
        "covers": COVERS,
        "fields": {
            "service.name": Component::VALUES,
            "service.version": "Major.minor.patch only",
            "os.type": event::PLATFORMS,
            "app.diagnostics.kind": event::Kind::VALUES,
            "app.feature": Feature::VALUES,
            "app.outcome": Outcome::VALUES,
            "app.duration_bucket": event::DURATIONS,
            "app.size_bucket": event::SIZES,
            "app.library_role": LibraryRole::VALUES
        },
        "limits": {"requests_per_hour": MAX_PER_HOUR, "max_batch": MAX_BATCH,
            "max_bytes": MAX_BODY, "timeout_seconds": SEND_TIMEOUT.as_secs(),
            "max_pending_per_product": MAX_PENDING, "queue_expiry_hours": MAX_AGE / 3600,
            "min_seconds_between_automatic_requests_per_product": MIN_INTERVAL},
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
    fs::create_dir_all(home).with_context(|| format!("creating {}", home.display()))?;
    let guard = config_lock(home)?;
    // Never deliver events collected under the previous choice to a new destination.
    remove(&home.join(STATE_FILE))?;
    let settings = Settings {
        policy: POLICY,
        endpoint,
        headers,
        consented_at_unix: now().as_secs(),
        generation: uuid::Uuid::new_v4().simple().to_string(),
    };
    write_private(
        &home.join(CONFIG_FILE),
        &serde_json::to_vec_pretty(&settings)?,
    )?;
    drop(guard);
    status(home)
}

/// Opt out, dropping the destination, credentials and pending events. A send
/// that already holds the lock can finish before this returns, never after it.
pub fn disable(home: &Path) -> Result<Value> {
    let guard = if home.is_dir() { Some(config_lock(home)?) } else { None };
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
    let Some(reporter) = reporter(home, Product::cli()) else {
        bail!(
            "diagnostics are off; enable them first: vamp diagnostics enable --endpoint OTLP_HTTP_URL"
        );
    };
    let delivery = reporter
        .urgent(Record::test())
        .context("not sent: the hourly limit was reached, send state was busy or unwritable, or reporting was turned off")?;
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
pub fn reporter(home: &Path, product: Product) -> Option<Arc<Reporter>> {
    if do_not_track() {
        return None;
    }
    let settings = settings(home).ok().flatten()?;
    current(&settings).then(|| {
        Arc::new(Reporter {
            home: home.to_path_buf(),
            settings,
            product,
        })
    })
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

fn post(url: &str, headers: &[(String, String)], body: &[u8]) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(Some(SEND_TIMEOUT))
        .max_redirects(0)
        .proxy(None)
        .user_agent("vamp-diagnostics")
        .build()
        .into();
    let mut request = agent.post(url).header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(name.as_str(), value.as_str());
    }
    request.send(body)
}

impl Reporter {
    /// Record only bounded dimensions; queue a batch and try delivery if due.
    pub fn record(&self, event: Event) {
        self.record_batch([event]);
    }

    /// Like `record`, for a bounded group drained off a request or UI thread.
    pub fn record_batch(&self, events: impl IntoIterator<Item = Event>) {
        let _ = self.update(events, None);
    }

    /// Try pending work without adding an observation.
    pub fn flush(&self) {
        self.record_batch([]);
    }

    /// Preserve panic output and unwinding without sending a message or location.
    pub fn install_panic_hook(self: Arc<Self>, feature: Feature) {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            previous(info);
            let _ = self.urgent(Record::panic(feature));
        }));
    }

    fn urgent(&self, record: Record) -> Option<Delivery> {
        self.update([], Some(record))
    }

    fn update(&self, events: impl IntoIterator<Item = Event>, urgent: Option<Record>) -> Option<Delivery> {
        if do_not_track() {
            return None;
        }
        // Recheck consent under the same lock that enable/disable take, and keep
        // the lock through the bounded HTTP attempt and its local receipt.
        let _guard = lock(&self.home)?;
        if settings(&self.home).ok().flatten().as_ref() != Some(&self.settings) {
            return None;
        }
        let now = now().as_secs();
        let state_path = self.home.join(STATE_FILE);
        let mut state = state(&self.home);
        let index = match self.product.component {
            Component::Cli => 0,
            Component::Server => 1,
            Component::Apple => 2,
        };
        let queue = &mut state.queues[index];
        let pending_before = queue.pending.len();
        if queue.version != self.product.version {
            queue.pending.clear();
            queue.last_attempt_unix = None;
            queue.version.clear();
            queue.version.push_str(&self.product.version);
        }
        queue.pending.retain(|event| now.saturating_sub(event.at_unix) < MAX_AGE);
        let mut changed = queue.pending.len() != pending_before;
        for event in events.into_iter().take(MAX_PENDING) {
            if event.feature == Feature::Diagnostics {
                continue;
            }
            if queue.pending.len() >= MAX_PENDING {
                queue.pending.pop_front();
            }
            queue.pending.push_back(Pending { at_unix: now, record: event.bucket() });
            changed = true;
        }
        let due = !queue.pending.is_empty()
            && queue.last_attempt_unix.is_none_or(|last| now < last || now - last >= MIN_INTERVAL);
        if (urgent.is_none() && !due) || !state.reserve(now) {
            if changed {
                let _ = save_state(&state_path, &state);
            }
            return None;
        }

        let mut batch = [Record::test(); MAX_BATCH];
        let mut reserved = [0; MAX_BATCH];
        let len = if let Some(record) = urgent {
            batch[0] = record;
            1
        } else {
            let queue = &mut state.queues[index];
            queue.last_attempt_unix = Some(now);
            let len = queue.pending.len().min(MAX_BATCH);
            for (i, event) in queue.pending.drain(..len).enumerate() {
                batch[i] = event.record;
                reserved[i] = event.at_unix;
            }
            len
        };
        // Remove the batch before POSTing. A process killed during delivery
        // must not replay an already accepted batch on its next launch.
        save_state(&state_path, &state).ok()?;
        let bytes = event::body(&self.product, POLICY, &batch[..len]);
        let (delivery, offline) = self.post(now, &bytes)?;
        if offline && urgent.is_none() {
            let queue = &mut state.queues[index];
            for i in (0..len).rev() {
                queue.pending.push_front(Pending { at_unix: reserved[i], record: batch[i] });
            }
        }
        state.last_delivery = Some(delivery);
        let _ = save_state(&state_path, &state);
        state.last_delivery
    }

    fn post(&self, now: u64, body: &[u8]) -> Option<(Delivery, bool)> {
        if body.len() > MAX_BODY {
            return None;
        }
        Some(match post(&format!("{}/v1/logs", self.settings.endpoint), &self.settings.headers, body) {
            Ok(response) => {
                let status = response.status();
                (Delivery {
                    at_unix: now,
                    delivered: status.is_success(),
                    status: Some(status.as_u16()),
                    error: (!status.is_success()).then(|| "http_status".into()),
                }, false)
            }
            Err(error) => {
                let offline = matches!(error, ureq::Error::HostNotFound | ureq::Error::ConnectionFailed)
                    || matches!(&error, ureq::Error::Io(e) if e.kind() == std::io::ErrorKind::ConnectionRefused);
                (Delivery {
                    at_unix: now,
                    delivered: false,
                    status: match error {
                        ureq::Error::StatusCode(status) => Some(status),
                        _ => None,
                    },
                    error: Some(failure(&error).into()),
                }, offline)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead as _, BufReader, Read as _},
        net::TcpListener,
    };

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
        assert!(reporter(home.path(), Product::cli()).is_none());
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
        settings["policy"] = json!(POLICY - 1);
        fs::write(&path, settings.to_string()).unwrap();
        assert!(reporter(home.path(), Product::cli()).is_none());
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
        assert!(reporter(home.path(), Product::cli()).is_none());
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
        serve_receiver(listener)
    }

    fn serve_receiver(listener: TcpListener) -> (String, std::sync::mpsc::Receiver<Request>) {
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

    #[test]
    fn offline_observations_batch_after_reconnect() {
        let home = tempfile::tempdir().unwrap();
        let socket = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap();
        drop(socket);
        enable(home.path(), &format!("http://{address}/otlp"), None).unwrap();
        let reporter = running_reporter(home.path());
        reporter.record(Event::new(Feature::Search, true));
        reporter.record(Event::new(Feature::GridLoad, true));
        let mut local = state(home.path());
        assert_eq!(local.queues[0].pending.len(), 2);
        assert_eq!(local.sent, 1);
        let (_, requests) = serve_receiver(TcpListener::bind(address).unwrap());
        local.queues[0].last_attempt_unix = Some(now().as_secs() - MIN_INTERVAL);
        save_state(&home.path().join(STATE_FILE), &local).unwrap();
        reporter.flush();
        let request = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        let payload: Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(payload["resourceLogs"][0]["scopeLogs"][0]["logRecords"].as_array().unwrap().len(), 2);
        let local = state(home.path());
        assert!(local.queues[0].pending.is_empty());
        assert!(local.last_delivery.unwrap().delivered);
    }

    fn running_reporter(home: &Path) -> Arc<Reporter> {
        reporter(home, Product::cli()).unwrap()
    }

    /// An old handle cannot send after revocation, even after a fresh opt-in
    /// to the original destination with the original credentials.
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
        assert!(reporter.urgent(Record::test()).unwrap().delivered);
        requests.recv_timeout(Duration::from_secs(5)).unwrap();

        disable(home.path()).unwrap();
        assert!(reporter.urgent(Record::test()).is_none());
        assert!(requests.recv_timeout(quiet).is_err());
        assert!(!home.path().join(STATE_FILE).exists());

        let (other, other_requests) = receiver();
        enable(home.path(), &other, Some(old.as_path())).unwrap();
        assert!(reporter.urgent(Record::test()).is_none());
        enable(home.path(), &url, Some(new.as_path())).unwrap();
        assert!(reporter.urgent(Record::test()).is_none());
        enable(home.path(), &url, Some(old.as_path())).unwrap();
        let path = home.path().join(CONFIG_FILE);
        let mut saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        saved["policy"] = json!(POLICY - 1);
        fs::write(&path, saved.to_string()).unwrap();
        assert!(reporter.urgent(Record::test()).is_none());
        assert!(requests.recv_timeout(quiet).is_err());
        assert!(other_requests.recv_timeout(quiet).is_err());

        enable(home.path(), &url, Some(old.as_path())).unwrap();
        assert!(reporter.urgent(Record::test()).is_none());
        assert!(running_reporter(home.path()).urgent(Record::test()).unwrap().delivered);
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
        running_reporter(Path::new(&home)).install_panic_hook(Feature::Browse);
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
                "tests::panic_hook_child",
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
        assert!(body.contains("\"app.feature\"") && body.contains("\"browse\""));
        assert!(!body.contains(".rs"));
        let status = status(home.path()).unwrap();
        assert_eq!(status["last_delivery"]["delivered"], true);
        assert!(!status.to_string().contains("dGVzdA=="));
    }
}
