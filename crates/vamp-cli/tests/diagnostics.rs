//! Opt-in diagnostics through the `vamp` binary: off on a fresh home, enabling needs a usable
//! destination, a failing command reports a bounded OTLP record only while enabled, and the
//! command's own output and exit status never change.

use serde_json::Value;
use std::{
    io::{BufRead as _, BufReader, Read as _, Write as _},
    net::TcpListener,
    path::Path,
    process::Command,
    sync::mpsc,
    time::Duration,
};

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn vamp(home: &Path, args: &[&str]) -> Run {
    vamp_with(home, args, &[])
}

fn vamp_with(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Run {
    let output = Command::new(env!("CARGO_BIN_EXE_vamp"))
        .arg("--home")
        .arg(home)
        .args(args)
        .env_remove("VAMP_HOME")
        .env_remove("VAMP_CONTEXT")
        .env_remove("DO_NOT_TRACK")
        .envs(env.iter().copied())
        .output()
        .unwrap();
    Run {
        code: output.status.code().unwrap(),
        stdout: String::from_utf8(output.stdout).unwrap(),
        stderr: String::from_utf8(output.stderr).unwrap(),
    }
}

fn json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

struct Request {
    head: String,
    body: String,
}

/// A local OTLP/HTTP receiver: each request is queued before the 200 response is written,
/// so it is queued by the time the (synchronous) sender exits.
fn receiver() -> (String, mpsc::Receiver<Request>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}/otlp",
        listener.local_addr().unwrap().port()
    );
    let (sender, receiver) = mpsc::channel();
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
            let _ = reader
                .get_mut()
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        }
    });
    (url, receiver)
}

fn attributes(body: &Value) -> Vec<(String, String)> {
    let resource = &body["resourceLogs"][0]["resource"]["attributes"];
    let record = &body["resourceLogs"][0]["scopeLogs"][0]["logRecords"][0]["attributes"];
    resource
        .as_array()
        .unwrap()
        .iter()
        .chain(record.as_array().unwrap())
        .map(|a| {
            let value = &a["value"];
            let value = value["stringValue"].as_str().map_or_else(
                || value["intValue"].as_str().unwrap_or_default().to_owned(),
                str::to_owned,
            );
            (a["key"].as_str().unwrap().to_owned(), value)
        })
        .collect()
}

#[test]
fn fresh_home_is_off() {
    let home = tempfile::tempdir().unwrap();
    let status = vamp(home.path(), &["diagnostics", "status"]);
    assert_eq!(status.code, 0, "{}", status.stderr);
    let status = json(&status.stdout);
    assert_eq!(status["enabled"], false);
    assert_eq!(status["default"], "off");
    assert!(status["endpoint"].is_null());
    assert!(!status["sends"].as_array().unwrap().is_empty());
    assert!(!status["never_sends"].as_array().unwrap().is_empty());
    assert!(!home.path().join("diagnostics.json").exists());
}

#[test]
fn enabling_without_a_usable_destination_stays_off() {
    let home = tempfile::tempdir().unwrap();
    let missing = vamp(home.path(), &["diagnostics", "enable"]);
    assert_eq!(missing.code, 2);
    assert_eq!(
        json(missing.stderr.trim())["error"]["code"],
        "invalid_arguments"
    );
    for endpoint in [
        "",
        "collector:4318",
        "ftp://collector",
        "https://id:secret-token@otlp.example/otlp",
        "https://otlp.example/otlp#x",
    ] {
        let run = vamp(
            home.path(),
            &["diagnostics", "enable", "--endpoint", endpoint],
        );
        assert_eq!(run.code, 1, "accepted {endpoint:?}");
        assert!(!run.stderr.contains("secret-token"), "{}", run.stderr);
    }
    let run = vamp(home.path(), &["diagnostics", "test"]);
    assert_eq!(run.code, 1);
    assert!(run.stderr.contains("diagnostics are off"), "{}", run.stderr);
    assert!(!home.path().join("diagnostics.json").exists());
    assert_eq!(
        json(&vamp(home.path(), &["diagnostics", "status"]).stdout)["enabled"],
        false
    );
}

#[test]
fn opted_in_failures_report_bounded_records_until_disabled() {
    let home = tempfile::tempdir().unwrap();
    let (endpoint, requests) = receiver();

    // A failing command with diagnostics off: same error, nothing sent.
    let off = vamp(home.path(), &["media", "inspect", "private-asset-7f3e"]);
    assert_eq!(off.code, 1);
    assert!(requests.recv_timeout(Duration::from_millis(300)).is_err());

    let files = tempfile::tempdir().unwrap();
    let headers = headers_file(
        files.path(),
        "# Grafana Cloud\nAuthorization=Basic c2VjcmV0\n",
    );
    let enabled = vamp(
        home.path(),
        &[
            "diagnostics",
            "enable",
            "--endpoint",
            &endpoint,
            "--headers-file",
            headers.to_str().unwrap(),
        ],
    );
    assert_eq!(enabled.code, 0, "{}", enabled.stderr);
    assert!(!enabled.stdout.contains("c2VjcmV0"));
    let status = json(&enabled.stdout);
    assert_eq!(status["enabled"], true);
    assert_eq!(status["headers"]["Authorization"], "<redacted>");
    assert_eq!(status["logs_url"], format!("{endpoint}/v1/logs"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(home.path().join("diagnostics.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "diagnostics.json is readable by others");
    }

    let test = vamp(home.path(), &["diagnostics", "test"]);
    assert_eq!(test.code, 0, "{}", test.stderr);
    assert_eq!(json(&test.stdout)["delivery"]["delivered"], true);
    let request = requests.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(request.head.contains("c2VjcmV0"));

    // The same failure while enabled: identical stderr and exit status, plus one record.
    let on = vamp(home.path(), &["media", "inspect", "private-asset-7f3e"]);
    assert_eq!(on.code, off.code);
    assert_eq!(on.stderr, off.stderr);
    assert_eq!(on.stdout, off.stdout);
    let request = requests.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        request.head.starts_with("POST /otlp/v1/logs HTTP/1.1\r\n"),
        "{}",
        request.head
    );
    assert!(
        request
            .head
            .to_ascii_lowercase()
            .contains("content-type: application/json")
    );
    assert!(request.body.len() <= 4096);
    let home_text = home.path().to_str().unwrap();
    for leak in ["private-asset-7f3e", home_text, "inspect private"] {
        assert!(
            !request.body.contains(leak),
            "report leaks {leak:?}: {}",
            request.body
        );
    }
    let body = json(&request.body);
    let attributes = attributes(&body);
    let keys: Vec<&str> = attributes.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        keys,
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
    let get = |key: &str| attributes.iter().find(|(k, _)| k == key).unwrap().1.clone();
    assert_eq!(get("app.diagnostics.kind"), "error");
    assert_eq!(get("app.operation"), "media inspect");
    assert_eq!(get("service.name"), "vamp");
    let category = get("error.type");
    assert!(
        category == "other"
            || category == "json"
            || category == "network"
            || category.starts_with("io."),
        "{category}"
    );

    // Opt out: the endpoint and credentials are forgotten and failures send nothing.
    let disabled = vamp(home.path(), &["diagnostics", "disable"]);
    assert_eq!(disabled.code, 0, "{}", disabled.stderr);
    assert_eq!(json(&disabled.stdout)["enabled"], false);
    assert!(!home.path().join("diagnostics.json").exists());
    let after = vamp(home.path(), &["media", "inspect", "private-asset-7f3e"]);
    assert_eq!(after.code, off.code);
    assert_eq!(after.stderr, off.stderr);
    assert!(requests.recv_timeout(Duration::from_millis(500)).is_err());
}

#[test]
fn an_unreachable_endpoint_never_changes_the_failure() {
    let home = tempfile::tempdir().unwrap();
    let off = vamp(home.path(), &["media", "inspect", "x"]);
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let enabled = vamp(
        home.path(),
        &[
            "diagnostics",
            "enable",
            "--endpoint",
            &format!("http://127.0.0.1:{port}"),
        ],
    );
    assert_eq!(enabled.code, 0, "{}", enabled.stderr);
    let on = vamp(home.path(), &["media", "inspect", "x"]);
    assert_eq!(
        (on.code, &on.stdout, &on.stderr),
        (off.code, &off.stdout, &off.stderr)
    );
    let status = json(&vamp(home.path(), &["diagnostics", "status"]).stdout);
    assert_eq!(status["last_delivery"]["delivered"], false);
    let test = vamp(home.path(), &["diagnostics", "test"]);
    assert_eq!(test.code, 1);
    assert!(test.stderr.contains("did not accept"), "{}", test.stderr);
}

#[test]
fn do_not_track_overrides_consent() {
    let home = tempfile::tempdir().unwrap();
    let (endpoint, requests) = receiver();
    let enabled = vamp(
        home.path(),
        &["diagnostics", "enable", "--endpoint", &endpoint],
    );
    assert_eq!(enabled.code, 0, "{}", enabled.stderr);
    let dnt = [("DO_NOT_TRACK", "1")];
    let status = json(&vamp_with(home.path(), &["diagnostics", "status"], &dnt).stdout);
    assert_eq!(status["enabled"], false);
    assert_eq!(status["consented"], true);
    assert_eq!(status["do_not_track"], true);
    let failed = vamp_with(home.path(), &["media", "inspect", "x"], &dnt);
    assert_eq!(failed.code, 1);
    let test = vamp_with(home.path(), &["diagnostics", "test"], &dnt);
    assert_eq!(test.code, 1);
    assert!(test.stderr.contains("DO_NOT_TRACK"), "{}", test.stderr);
    assert!(requests.recv_timeout(Duration::from_millis(500)).is_err());
    assert!(!home.path().join("diagnostics-state.json").exists());
    // Consent is kept: without DO_NOT_TRACK the same failure reports again.
    let failed = vamp_with(
        home.path(),
        &["media", "inspect", "x"],
        &[("DO_NOT_TRACK", "0")],
    );
    assert_eq!(failed.code, 1);
    assert!(requests.recv_timeout(Duration::from_secs(5)).is_ok());
}

/// An owner-only `--headers-file`.
fn headers_file(dir: &Path, text: &str) -> std::path::PathBuf {
    let path = dir.join("otlp-headers");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
        .open(&path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    path
}

/// Parallel failing runs share the hourly budget: exactly 10 reports, and the state counts
/// every one of them (no process overwrites another's reservation or receipt).
#[test]
fn parallel_failures_share_the_hourly_budget() {
    let home = tempfile::tempdir().unwrap();
    let (endpoint, requests) = receiver();
    let enabled = vamp(
        home.path(),
        &["diagnostics", "enable", "--endpoint", &endpoint],
    );
    assert_eq!(enabled.code, 0, "{}", enabled.stderr);
    let children: Vec<_> = (0..16)
        .map(|_| {
            Command::new(env!("CARGO_BIN_EXE_vamp"))
                .arg("--home")
                .arg(home.path())
                .args(["media", "inspect", "x"])
                .env_remove("VAMP_HOME")
                .env_remove("VAMP_CONTEXT")
                .env_remove("DO_NOT_TRACK")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut child in children {
        assert_eq!(child.wait().unwrap().code(), Some(1));
    }
    let mut received = 0;
    while requests.recv_timeout(Duration::from_millis(500)).is_ok() {
        received += 1;
    }
    assert_eq!(received, 10);
    let state = json(&std::fs::read_to_string(home.path().join("diagnostics-state.json")).unwrap());
    assert_eq!(state["sent"], 10);
    assert_eq!(state["last_delivery"]["delivered"], true);
    let status = json(&vamp(home.path(), &["diagnostics", "status"]).stdout);
    assert_eq!(status["last_delivery"]["delivered"], true);
}
