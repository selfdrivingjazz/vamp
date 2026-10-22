//! Lazy replica energy-pack refreshes, serialized across threads and processes.
use super::*;
use fs2::FileExt;

const CHECK_INTERVAL: Duration = Duration::from_secs(30);
const CHECKED: &str = "replica-checked.json";

/// Explicit consumers may wait for their first pack, but still yield the download
/// lane to foreground media. A concurrent successful refresh satisfies this call.
pub fn refresh(library: &Library) -> Result<bool> {
    refresh_with(&library.remote_client()?, &library.state_dir, false)
}

fn refresh_with(client: &crate::remote::Client, state_dir: &Path, background: bool) -> Result<bool> {
    refresh_locked(state_dir, background, || super::refresh_download(client, state_dir))
}

fn recently_checked(marker: &Path) -> bool {
    fs::metadata(marker).and_then(|m| m.modified()).ok()
        .and_then(|at| at.elapsed().ok()).is_some_and(|age| age < CHECK_INTERVAL)
}

fn checked_generation(marker: &Path) -> Option<String> {
    let value: Value = serde_json::from_slice(&fs::read(marker).ok()?).ok()?;
    value["generation"].as_str().map(str::to_owned)
}

fn refresh_locked(state_dir: &Path, background: bool, fetch: impl FnOnce() -> Result<bool>) -> Result<bool> {
    let path = pack_path(state_dir);
    let directory = path.parent().context("missing embeddings directory")?;
    let marker = directory.join(CHECKED);
    let valid = || read_header(&path).is_ok();
    // Background checks never turn a metadata-only replica into a pack consumer.
    if background && (!valid() || recently_checked(&marker)) { return Ok(false); }
    let observed = checked_generation(&marker);
    fs::create_dir_all(directory)?;
    let lock = OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .open(directory.join("replica-refresh.lock"))?;
    if background {
        match lock.try_lock_exclusive() {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) => return Err(error.into()),
        }
    } else {
        lock.lock_exclusive()?;
    }
    // Lock lifetime covers validation, staged publication and the durable completion
    // marker. Its unique contents also coalesce explicit waiters across processes.
    if valid() && (checked_generation(&marker) != observed || (background && recently_checked(&marker))) {
        return Ok(false);
    }
    if background && !valid() { return Ok(false); }
    // Rate-limit warm attempts even on authority failure, without claiming that a
    // waiter has been satisfied. Missing/invalid caches and explicit retries bypass it.
    if valid() { atomic_json(&marker, &json!({"generation": observed}))?; }
    let changed = fetch()?;
    read_header(&path).context("refreshed embeddings pack is unavailable or invalid")?;
    atomic_json(&marker, &json!({"generation": id()}))?;
    Ok(changed)
}

static REFRESHING: std::sync::Mutex<std::collections::BTreeSet<PathBuf>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

pub fn refresh_in_background(library: &Library) {
    let path = pack_path(&library.state_dir);
    if read_header(&path).is_err() || recently_checked(&path.parent().expect("pack directory").join(CHECKED)) {
        return;
    }
    let client = match library.remote_client() {
        Ok(client) => client,
        Err(e) => { eprintln!("embeddings pack: {e:#}"); return; }
    };
    let state_dir = library.state_dir.clone();
    let refreshing = || REFRESHING.lock().unwrap_or_else(|e| e.into_inner());
    if !refreshing().insert(state_dir.clone()) { return; }
    let queued_dir = state_dir.clone();
    if let Err(error) = std::thread::Builder::new().name("energy-refresh".into()).spawn(move || {
        if let Err(e) = refresh_with(&client, &state_dir, true) { eprintln!("embeddings pack: {e:#}"); }
        refreshing().remove(&state_dir);
    }) {
        refreshing().remove(&queued_dir);
        eprintln!("embeddings pack: {error}");
    }
}

/// Catalog sync never waits for embeddings and never fetches a cold pack.
pub fn refresh_after_sync(library: &Library, applied: u64) {
    if applied > 0 { refresh_in_background(library); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier, mpsc};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn cached_pack(state_dir: &Path) {
        let header: PackHeader = serde_json::from_value(json!({
            "format": 1, "version": "cached", "cursor": 0, "model": "test",
            "dim": 2, "palette_dim": PALETTE_DIM, "count": 1,
            "quant": "int8", "ids": ["asset"]
        })).unwrap();
        let path = pack_path(state_dir);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, encode(&header, &[1.0], &[1, 2], &[0; PALETTE_DIM])).unwrap();
    }

    fn mirror(home: &Path, url: String) -> Library {
        let mut library = Library::init(home, "mirror", None, None).unwrap();
        library.context.remote = Some(Remote {
            url, token: "replica-test".into(), drive_roots: Default::default(),
        });
        atomic_json(&home.join("config.json"), &Config {
            current: Some("mirror".into()),
            contexts: std::collections::BTreeMap::from([("mirror".into(), library.context.clone())]),
        }).unwrap();
        library
    }

    #[test]
    fn cold_sync_and_background_requests_never_contact_authority() {
        let root = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let library = mirror(&root.path().join("home"), format!("http://{}", listener.local_addr().unwrap()));
        refresh_after_sync(&library, 1);
        refresh_after_sync(&library, 0);
        refresh_in_background(&library);
        assert!(!refresh_locked(&library.state_dir, true, || panic!("cold background fetch")).unwrap());
        assert!(!pack_path(&library.state_dir).exists());
        assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    }

    #[test]
    fn background_cooldown_survives_library_reopen_but_explicit_checks_are_not_throttled() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let library = mirror(&home, format!("http://{}", listener.local_addr().unwrap()));
        cached_pack(&library.state_dir);
        assert!(!refresh_locked(&library.state_dir, false, || Ok(false)).unwrap());
        drop(library);
        let reopened = Library::open(&home, Some("mirror")).unwrap();
        assert!(!refresh_locked(&reopened.state_dir, true, || panic!("cooldown lost on reopen")).unwrap());
        refresh_after_sync(&reopened, 1);
        refresh_in_background(&reopened);
        assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        let checks = AtomicUsize::new(0);
        for _ in 0..2 {
            refresh_locked(&reopened.state_dir, false, || {
                checks.fetch_add(1, Ordering::SeqCst);
                Ok(false)
            }).unwrap();
        }
        assert_eq!(checks.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn invalid_cache_ignores_recent_marker_and_failed_refresh_can_retry() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path();
        cached_pack(state);
        refresh_locked(state, false, || Ok(false)).unwrap();
        fs::write(pack_path(state), b"truncated").unwrap();
        assert!(refresh_locked(state, false, || anyhow::bail!("authority unavailable")).is_err());
        assert_eq!(fs::read(pack_path(state)).unwrap(), b"truncated");
        assert!(refresh_locked(state, false, || {
            cached_pack(state);
            Ok(true)
        }).unwrap());
        let bytes = fs::read(pack_path(state)).unwrap();
        fs::remove_file(pack_path(state).parent().unwrap().join(CHECKED)).unwrap();
        assert!(refresh_locked(state, true, || anyhow::bail!("temporary failure")).is_err());
        assert_eq!(fs::read(pack_path(state)).unwrap(), bytes);
        assert!(!refresh_locked(state, true, || panic!("failed warm check ignored cooldown")).unwrap());
        let mut retried = false;
        assert!(!refresh_locked(state, false, || { retried = true; Ok(false) }).unwrap());
        assert!(retried, "explicit retries must bypass a failed background check's cooldown");
        fs::remove_file(pack_path(state)).unwrap();
        assert!(refresh_locked(state, false, || { cached_pack(state); Ok(true) }).unwrap());
    }

    #[test]
    fn explicit_waiters_and_background_starts_share_one_locked_refresh() {
        let root = tempfile::tempdir().unwrap();
        cached_pack(root.path());
        let calls = AtomicUsize::new(0);
        let (entered, started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        std::thread::scope(|scope| {
            let state = root.path();
            let calls = &calls;
            let first = scope.spawn(move || refresh_locked(state, false, || {
                calls.fetch_add(1, Ordering::SeqCst);
                entered.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(5)).unwrap();
                Ok(true)
            }));
            started.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(!refresh_locked(root.path(), true, || panic!("background duplicated active refresh")).unwrap());
            let ready = Arc::new(Barrier::new(2));
            let second_ready = ready.clone();
            let (finished, waiting) = mpsc::channel();
            let second = scope.spawn(move || {
                second_ready.wait();
                let result = refresh_locked(state, false, || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(true)
                });
                finished.send(()).unwrap();
                result
            });
            ready.wait();
            assert!(matches!(waiting.recv_timeout(Duration::from_millis(100)), Err(mpsc::RecvTimeoutError::Timeout)));
            release.send(()).unwrap();
            assert!(first.join().unwrap().unwrap());
            assert!(!second.join().unwrap().unwrap());
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn explicit_cold_native_request_reports_invalid_download_then_retries_real_pack() {
        use std::io::{BufRead as _, Write as _};
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let library = mirror(&home, format!("http://{}", listener.local_addr().unwrap()));
        let source = root.path().join("source");
        cached_pack(&source);
        let packed = fs::read(pack_path(&source)).unwrap();
        let server = std::thread::spawn(move || {
            for body in [b"invalid pack".to_vec(), packed] {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() { break; }
                    request.push_str(&line);
                }
                assert!(request.starts_with("GET /v1/embeddings/pack"));
                assert!(request.to_ascii_lowercase().contains("authorization: bearer replica-test\r\n"));
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        let request = || serde_json::from_value(json!({"operation":"energy_pack","home":home,"context":"mirror"})).unwrap();
        assert!(crate::native::request(request()).is_err(), "invalid authority data must not look like an empty pack");
        assert!(!pack_path(&library.state_dir).exists());
        let value = crate::native::request(request()).unwrap();
        assert_eq!(value["version"], "cached");
        assert_eq!(value["count"], 1);
        server.join().unwrap();
    }

    #[test]
    fn native_warm_pack_returns_while_authority_check_is_still_pending() {
        use std::io::{BufRead as _, Write as _};
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let library = mirror(&home, format!("http://{}", listener.local_addr().unwrap()));
        cached_pack(&library.state_dir);
        let (entered, started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).is_ok_and(|n| n > 2) { line.clear(); }
            entered.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(5)).unwrap();
            stream.write_all(b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n").unwrap();
        });
        refresh_after_sync(&library, 1);
        let (returned, result) = mpsc::channel();
        let request_thread = std::thread::spawn(move || {
            let request = serde_json::from_value(json!({"operation":"energy_pack","home":home,"context":"mirror"})).unwrap();
            returned.send(crate::native::request(request)).unwrap();
        });
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        let value = result.recv_timeout(Duration::from_secs(1)).expect("warm pack waited on authority").unwrap();
        assert_eq!(value["version"], "cached");
        assert_eq!(value["count"], 1);
        release.send(()).unwrap();
        server.join().unwrap();
        request_thread.join().unwrap();
        // Wait for the shared writer before dropping its temporary directory.
        refresh_locked(&library.state_dir, false, || Ok(false)).unwrap();
    }
}
