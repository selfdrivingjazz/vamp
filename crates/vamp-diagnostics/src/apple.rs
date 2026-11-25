//! Private Apple diagnostics: separate consent from aggregate usage, same lock, delivery budget,
//! and HTTP transport. Only this projection, never the source report, crosses the network.
use super::*;

pub const ROUTE: &str = "/v1/crash-reports";
pub const MAX_BYTES: usize = 12 * 1024;
const POLICY: u32 = 1;
const CHOICE: &str = "apple-choice.json";
const PENDING: &str = "apple-pending.json";
const MAX_PENDING: usize = 16;
const MAX_FRAMES: usize = 24;
const MAX_STACKS: usize = 6;
const EXPIRY: u64 = 7 * 86400;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind { Crash, Hang, CpuException, DiskWriteException, UncleanExit }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source { MetricKit, MacCrashLog, MainThread, PreviousSession }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform { Macos, Ios }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category { Lifecycle, Navigation, Images, Engine, Network, Sync, Error, Ui, Perf, Intake }

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Frame {
    /// Mach-O image UUID and image-relative PC. No absolute pointers, symbols or paths.
    pub uuid: String,
    pub offset: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub policy: u32,
    pub occurrence: String,
    pub kind: Kind,
    pub source: Source,
    pub platform: Platform,
    pub version: String,
    pub build: String,
    pub os: String,
    pub model: String,
    pub signal: Option<u8>,
    pub exception_type: Option<u16>,
    pub exception_code: Option<u64>,
    pub duration_ms: Option<u64>,
    pub stacks: Vec<Vec<Frame>>,
    pub categories: Vec<Category>,
}

fn hex_id(text: &str) -> bool {
    text.len() == 32 && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn version(text: &str) -> bool {
    (1..=32).contains(&text.len()) && text.split('.').all(|part| {
        !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())
    })
}

fn model(text: &str) -> bool {
    text == "unknown" || (text.len() <= 24 && ["MacBookPro", "MacBookAir", "MacBook", "Macmini", "MacPro", "iMacPro", "iMac", "Mac", "iPhone", "iPad", "iPod"].iter().any(|prefix| {
        text.strip_prefix(prefix).and_then(|s| s.split_once(',')).is_some_and(|(major, minor)| {
            [major, minor].into_iter().all(|n| (1..=3).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit()))
        })
    }))
}

impl Report {
    pub fn valid(&self) -> bool {
        let compatible = match self.source {
            Source::MetricKit => self.kind != Kind::UncleanExit,
            Source::MacCrashLog => self.kind == Kind::Crash && self.platform == Platform::Macos,
            Source::MainThread => self.kind == Kind::Hang && self.platform == Platform::Macos,
            Source::PreviousSession => self.kind == Kind::UncleanExit && self.platform == Platform::Macos,
        };
        self.policy == POLICY && compatible && hex_id(&self.occurrence)
            && version(&self.version) && version(&self.build) && version(&self.os) && model(&self.model)
            && self.signal.is_none_or(|sig| sig <= 64)
            && (self.kind == Kind::Crash || (self.signal.is_none() && self.exception_type.is_none() && self.exception_code.is_none()))
            && self.duration_ms.is_none_or(|ms| self.kind == Kind::Hang && ms <= 3_600_000)
            && self.categories.len() <= 8 && (self.kind == Kind::UncleanExit || self.categories.is_empty())
            && (self.kind != Kind::UncleanExit || self.stacks.is_empty()) && self.stacks.len() <= MAX_STACKS
            && self.stacks.iter().all(|stack| !stack.is_empty() && stack.len() <= MAX_FRAMES
                && stack.iter().all(|frame| hex_id(&frame.uuid) && frame.offset <= u32::MAX as u64))
    }
}

#[derive(Serialize, Deserialize)]
struct Choice { policy: u32, endpoint: String, enabled: bool, since: u64 }

#[derive(Serialize, Deserialize)]
struct Queued { at: u64, report: Report }

fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// HTTPS, or a numerical Tailscale IPv4/loopback origin on a direct no-proxy connection.
/// No user-supplied endpoint, credentials, query, redirects, or DNS for plaintext.
fn collector(text: &str) -> Result<String> {
    let uri: ureq::http::Uri = text.parse()?;
    if uri.scheme_str() == Some("http") {
        let host = uri.host().unwrap_or_default().parse::<std::net::Ipv4Addr>()?;
        let [a,b,..] = host.octets();
        ensure!((host.is_loopback() || (a == 100 && (64..128).contains(&b)))
            && uri.path() == "/" && uri.query().is_none() && !text.contains(['@', '#']), "invalid private collector");
        Ok(text.trim_end_matches('/').to_owned())
    } else {
        endpoint(text)
    }
}

/// `None` keeps the saved decision; the first private build defaults to crash reports only.
/// DNT blocks collection and drops pending reports, without changing the toggle decision.
pub fn choice(home: &Path, endpoint: &str, enabled: Option<bool>) -> Result<Value> {
    let endpoint = collector(endpoint)?;
    fs::create_dir_all(home)?;
    let _guard = config_lock(home)?;
    let path = home.join(CHOICE);
    let previous = read::<Choice>(&path)?;
    let dnt = do_not_track();
    let mut choice = previous.unwrap_or_else(|| Choice { policy: POLICY, endpoint: endpoint.clone(), enabled: true, since: 0 });
    let changed = choice.policy != POLICY || choice.endpoint != endpoint;
    if changed || choice.since == 0 || enabled.is_some() || dnt {
        choice.since = (now().as_millis() as u64).max(choice.since.saturating_add(1));
        if changed { choice.enabled = false; }
        if let Some(enabled) = enabled { choice.enabled = enabled; }
        choice.policy = POLICY;
        choice.endpoint = endpoint;
        remove(&home.join(PENDING))?;
        write_private(&path, &serde_json::to_vec(&choice)?)?;
    }
    Ok(json!({"consented": choice.enabled, "enabled": choice.enabled && !dnt, "since": choice.since, "do_not_track": dnt}))
}

/// Store before attempting delivery. Retrying is safe: the collector recognizes the same occurrence.
/// Neither an off choice nor DNT can send, including when a queued call arrives after revocation.
pub fn send(home: &Path, endpoint: &str, since: u64, report: Option<Report>) -> Result<Value> {
    if do_not_track() { return Ok(json!({"accepted":false})); }
    let endpoint = collector(endpoint)?;
    let Some(_guard) = lock(home) else { return Ok(json!({"accepted":false})); };
    let Some(choice) = read::<Choice>(&home.join(CHOICE))? else { return Ok(json!({"accepted":false})); };
    if !choice.enabled || choice.policy != POLICY || choice.endpoint != endpoint || choice.since != since {
        return Ok(json!({"accepted":false}));
    }
    let queue_path = home.join(PENDING);
    let now = now().as_secs();
    let mut queued = read::<VecDeque<Queued>>(&queue_path)?.unwrap_or_default();
    queued.retain(|q| now.saturating_sub(q.at) < EXPIRY);
    if let Some(report) = report {
        ensure!(report.valid(), "invalid crash diagnostics");
        ensure!(serde_json::to_vec(&report)?.len() <= MAX_BYTES, "crash diagnostics exceed byte limit");
        if !queued.iter().any(|q| q.report.occurrence == report.occurrence) {
            if queued.len() == MAX_PENDING { queued.pop_front(); }
            queued.push_back(Queued { at: now, report });
        }
        write_private(&queue_path, &serde_json::to_vec(&queued)?)?;
    }
    let state_path = home.join(STATE_FILE);
    let mut state = state(home);
    while let Some(next) = queued.front() {
        if !state.reserve(now) { break; }
        save_state(&state_path, &state)?;
        let body = serde_json::to_vec(&next.report)?;
        match post(&format!("{endpoint}{ROUTE}"), &[], &body) {
            Ok(response) if response.status().is_success() => {},
            // A malformed payload is never going to succeed, and must not block later reports.
            Err(ureq::Error::StatusCode(400 | 413 | 415)) => {},
            _ => break,
        }
        queued.pop_front();
    }
    if queued.is_empty() { remove(&queue_path)?; }
    else { write_private(&queue_path, &serde_json::to_vec(&queued)?)?; }
    Ok(json!({"accepted":true}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_send_when_disabled_or_do_not_track() {
        const CHILD: &str = "VAMP_APPLE_CRASH_TEST";
        if let Ok(mode) = std::env::var(CHILD) {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path();
            let since = choice(home, &url, Some(true)).unwrap()["since"].as_u64().unwrap();
            if mode == "off" { choice(home, &url, Some(false)).unwrap(); }
            let report = serde_json::from_value(json!({"policy":1,"occurrence":"1".repeat(32),
                "kind":"hang","source":"main_thread","platform":"macos","version":"0.2.8",
                "build":"1","os":"27.0","model":"Mac16,1","stacks":[],"categories":[]})).unwrap();
            assert_eq!(send(home, &url, since, Some(report)).unwrap()["accepted"], false);
            assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
            assert!(!home.join(PENDING).exists());
            return;
        }
        for (mode, dnt) in [("off", "0"), ("dnt", "1")] {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "apple::tests::no_send_when_disabled_or_do_not_track"])
                .env(CHILD, mode).env("DO_NOT_TRACK", dnt).output().unwrap();
            assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stdout));
        }
    }

    #[test]
    fn rejects_content_and_unbounded_fields() {
        let mut body = json!({"policy":1,"occurrence":"1".repeat(32),"kind":"crash","source":"metric_kit",
            "platform":"macos","version":"0.2.8","build":"1","os":"27.0","model":"Mac16,1",
            "signal":5,"stacks":[[{"uuid":"a".repeat(32),"offset":1234}]],"categories":[]});
        assert!(serde_json::from_value::<Report>(body.clone()).unwrap().valid());
        for key in ["path", "filename", "url", "tag", "caption", "query", "ip", "contents"] {
            body[key] = json!("secret");
            assert!(serde_json::from_value::<Report>(body.clone()).is_err());
            body.as_object_mut().unwrap().remove(key);
        }
        body["stacks"][0][0]["symbol"] = json!("private");
        assert!(serde_json::from_value::<Report>(body.clone()).is_err());
        body["stacks"] = json!([]);
        body["model"] = json!("Mac16,1/My Library");
        assert!(!serde_json::from_value::<Report>(body.clone()).unwrap().valid());
        body["model"] = json!("Mac16,1");
        body["build"] = json!("1".repeat(33));
        assert!(!serde_json::from_value::<Report>(body).unwrap().valid());
    }
}
