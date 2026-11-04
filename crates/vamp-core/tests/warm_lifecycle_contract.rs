#![cfg(unix)]
use std::{fs, os::unix::{fs::PermissionsExt, net::UnixListener}, time::{Duration, Instant}};
use tempfile::TempDir;
use vamp_core::Library;
use vamp_protocol::{Metadata, ProcessorSpec};

// Records cold starts and the supplied idle timeout without downloading model weights.
const SERVER: &str = r#"import json, os, signal, socket, sys
path = sys.argv[sys.argv.index('--socket') + 1]
idle = int(sys.argv[sys.argv.index('--idle-seconds') + 1])
with open(sys.argv[1], 'a') as starts: starts.write(str(os.getpid()) + ' ' + str(idle) + '\n')
s = socket.socket(socket.AF_UNIX)
s.bind(path)
os.chmod(path, 0o600)
s.listen(16)
s.settimeout(idle)
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
try:
    while True:
        try: conn, _ = s.accept()
        except socket.timeout: break
        with conn:
            line = conn.makefile().readline()
            if not line: continue
            response = {'observation': {'kind': 'tags', 'model': 'test', 'value': {'tags': ['warm']}}}
            try: conn.sendall((json.dumps(response) + '\n').encode())
            except BrokenPipeError: pass
finally:
    s.close()
    os.unlink(path)
"#;

#[test]
fn stale_socket_recovers_next_batch_reuses_server_and_stop_and_idle_exit_work() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    let mut library = Library::init(&home, "test", None, None).unwrap();
    let script = dir.path().join("processor.py");
    let starts = dir.path().join("starts");
    fs::write(&script, SERVER).unwrap();
    library.processor_add(ProcessorSpec {
        name: "test-warm".into(), events: vec!["media_added".into()],
        command: vec!["python3".into(), script.to_str().unwrap().into(), starts.to_str().unwrap().into()],
        input: "original".into(), output: "observation".into(), timeout_seconds: 10,
        placement: Default::default(),
    }).unwrap();
    library.warm.insert("test-warm".into());
    library.warm_idle_seconds = 60;
    let (socket, log) = library.warm_paths("test-warm").unwrap();
    // A crash leaves the filesystem entry, but no listener.
    drop(UnixListener::bind(&socket).unwrap());
    let first = dir.path().join("first.txt");
    fs::write(&first, "one").unwrap();
    library.add(&first, None, Metadata::default()).unwrap();
    assert_eq!(library.run_jobs(1).unwrap()["completed"], 1);
    assert_eq!(fs::metadata(&socket).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(fs::metadata(&log).unwrap().permissions().mode() & 0o777, 0o600);
    drop(library);

    // A later CLI invocation opens a fresh Library, but must retain the model process.
    let mut library = Library::open(&home, None).unwrap();
    library.warm.insert("test-warm".into());
    library.warm_idle_seconds = 60;
    let second = dir.path().join("second.txt");
    fs::write(&second, "two").unwrap();
    library.add(&second, None, Metadata::default()).unwrap();
    assert_eq!(library.run_jobs(1).unwrap()["completed"], 1);
    let lines = fs::read_to_string(&starts).unwrap();
    assert_eq!(lines.lines().count(), 1, "second batch cold-started the model");
    assert!(lines.trim().ends_with(" 60"));
    let stopped_at = Instant::now();
    assert!(vamp_core::stop_server(&socket).unwrap());
    assert!(stopped_at.elapsed() < Duration::from_secs(3));
    assert!(!vamp_core::stop_server(&socket).unwrap());

    library.warm_idle_seconds = 1;
    let third = dir.path().join("third.txt");
    fs::write(&third, "three").unwrap();
    library.add(&third, None, Metadata::default()).unwrap();
    assert_eq!(library.run_jobs(1).unwrap()["completed"], 1);
    let deadline = Instant::now() + Duration::from_secs(4);
    while socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(!socket.exists(), "warm server failed to exit at its configured idle timeout");
    let lines = fs::read_to_string(&starts).unwrap();
    assert_eq!(lines.lines().count(), 2);
    assert!(lines.lines().last().unwrap().ends_with(" 1"));
}

#[test]
fn long_state_paths_use_a_private_short_socket_directory() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("a-long-library-home-directory".repeat(5));
    let library = Library::init(&home, "test", None, None).unwrap();
    let (socket, _) = library.warm_paths("siglip-embedding").unwrap();
    assert!(socket.as_os_str().len() < 100);
    assert_eq!(fs::metadata(socket.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
    assert!(library.warm_paths("../other").is_err());
}
