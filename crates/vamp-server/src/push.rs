//! Push for replicas: `GET /v1/events/stream?after=N&wait=S` long-polls until the
//! authority has events past `N` (answering like `GET /v1/events`) or `S` seconds pass
//! (empty list). Writers may be other processes (CLI, app, importer) on the same catalog,
//! so a watcher polls the newest event sequence every [`POLL`] and wakes waiters.
use crate::App;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(100);
pub const MAX_WAIT: u64 = 60;

#[derive(Default)]
pub struct Hub {
    last: Mutex<u64>,
    changed: Condvar,
}

impl Hub {
    fn publish(&self, sequence: u64) {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if sequence != *last {
            *last = sequence;
            self.changed.notify_all();
        }
    }
    /// Wait up to `wait` for the newest sequence to pass `after`; true when it has.
    pub fn wait_past(&self, after: u64, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        while *last <= after {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            last = self
                .changed
                .wait_timeout(last, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        true
    }
}

/// Poll the catalog's newest event sequence until the app is dropped.
pub fn spawn_watcher(app: Weak<App>) {
    std::thread::spawn(move || {
        while let Some(app) = app.upgrade() {
            app.health.beat("events");
            if let Ok(sequence) = app.lock().last_event() {
                app.hub.publish(sequence);
            }
            drop(app);
            std::thread::sleep(POLL);
        }
    });
}

/// Long-poll requests get their own thread so they never starve the worker pool.
pub fn is_stream(request: &tiny_http::Request) -> bool {
    let url = request.url();
    url == "/v1/events/stream" || url.starts_with("/v1/events/stream?")
}

pub fn spawn_stream(app: Arc<App>, request: tiny_http::Request) {
    std::thread::spawn(move || crate::handle(&app, request));
}
