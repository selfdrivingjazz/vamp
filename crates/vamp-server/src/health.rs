//! Liveness for `GET /v1/health` and the watchdog: HTTP worker progress, subsystem
//! heartbeats and library-lock hold time. A wedged process exits non-zero so launchd
//! (`KeepAlive`) restarts it.
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// HTTP workers busy with no request finished (nor body chunk sent) for this long: wedged.
pub const HTTP_STALL: Duration = Duration::from_secs(60);
/// Every request I/O thread stuck on one operation for this long: wedged (only a restart
/// frees threads parked in a dead cloud provider's syscalls).
pub const IO_WEDGE: Duration = Duration::from_secs(600);
/// Exit status of a watchdog restart.
pub const WATCHDOG_EXIT: i32 = 70;

/// Clock in milliseconds since this tracker's creation; every method takes `now` so tests
/// drive time explicitly.
pub struct HttpProgress {
    epoch: Instant,
    /// Per worker: the request it is handling (url, start ms).
    slots: Mutex<Vec<Option<(String, u64)>>>,
    last_progress: AtomicU64,
}

impl Default for HttpProgress {
    fn default() -> Self {
        Self {
            epoch: Instant::now(),
            slots: Mutex::default(),
            last_progress: AtomicU64::new(0),
        }
    }
}

impl HttpProgress {
    pub fn now(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }
    fn slots(&self) -> std::sync::MutexGuard<'_, Vec<Option<(String, u64)>>> {
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Registers `threads` workers; returns the first worker index.
    pub fn add_workers(&self, threads: usize) -> usize {
        let mut slots = self.slots();
        let first = slots.len();
        slots.resize(first + threads, None);
        first
    }
    pub fn begin(&self, worker: usize, url: &str, now: u64) {
        if let Some(slot) = self.slots().get_mut(worker) {
            *slot = Some((url.to_owned(), now));
        }
    }
    pub fn end(&self, worker: usize, now: u64) {
        if let Some(slot) = self.slots().get_mut(worker) {
            *slot = None;
        }
        self.progress(now);
    }
    /// A response body chunk went out (long downloads are progress too).
    pub fn progress(&self, now: u64) {
        self.last_progress.fetch_max(now, Ordering::Relaxed);
    }
    /// `Some(diagnosis)` when every worker is busy, each on a request older than `limit`,
    /// and none progressed for `limit`.
    pub fn check(&self, now: u64, limit: Duration) -> Option<String> {
        let slots = self.slots();
        if slots.is_empty() || slots.iter().any(Option::is_none) {
            return None;
        }
        let newest_start = slots.iter().flatten().map(|(_, s)| *s).max().unwrap_or(0);
        let last = self.last_progress.load(Ordering::Relaxed).max(newest_start);
        let idle = now.saturating_sub(last);
        if idle < limit.as_millis() as u64 {
            return None;
        }
        let requests = slots
            .iter()
            .flatten()
            .map(|(url, start)| format!("{url} ({}s)", now.saturating_sub(*start) / 1000))
            .collect::<Vec<_>>();
        Some(format!(
            "all {} HTTP workers busy, no progress for {}s: {}",
            slots.len(),
            idle / 1000,
            requests.join(", ")
        ))
    }
    pub fn report(&self, now: u64) -> Value {
        let slots = self.slots();
        let mut busy: Vec<(&str, u64)> = slots
            .iter()
            .flatten()
            .map(|(url, start)| (url.as_str(), now.saturating_sub(*start)))
            .collect();
        busy.sort_by_key(|(_, age)| std::cmp::Reverse(*age));
        json!({
            "threads": slots.len(),
            "busy": busy.len(),
            "last_progress_ms_ago": now.saturating_sub(self.last_progress.load(Ordering::Relaxed)),
            "oldest_request": busy.first().map(|(url, age)| json!({"url":url,"age_ms":age})),
        })
    }
}

/// Process-wide liveness shared by the HTTP workers, background threads and `/v1/health`.
pub struct Health {
    pub http: HttpProgress,
    beats: Mutex<HashMap<&'static str, u64>>,
    /// When the library lock was taken (ms on the `http` clock, +1; 0 = free).
    pub(crate) lock_since: AtomicU64,
    /// Media request threads (see `serve`) and how many are handling a request.
    pub(crate) media_threads: AtomicUsize,
    pub(crate) media_busy: AtomicUsize,
}

impl Default for Health {
    fn default() -> Self {
        Self {
            http: HttpProgress::default(),
            beats: Mutex::default(),
            lock_since: AtomicU64::new(0),
            media_threads: AtomicUsize::new(0),
            media_busy: AtomicUsize::new(0),
        }
    }
}

impl Health {
    /// A background loop (`jobs`, `events`) is alive.
    pub fn beat(&self, subsystem: &'static str) {
        let now = self.http.now();
        self.beats
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(subsystem, now);
    }
    pub(crate) fn lock_held_ms(&self, now: u64) -> Option<u64> {
        match self.lock_since.load(Ordering::Relaxed) {
            0 => None,
            since => Some(now.saturating_sub(since - 1)),
        }
    }
    pub fn subsystems(&self, now: u64) -> Value {
        let beats = self.beats.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = serde_json::Map::new();
        for (name, at) in beats.iter() {
            out.insert(
                (*name).into(),
                json!({"last_beat_ms_ago": now.saturating_sub(*at)}),
            );
        }
        out.insert(
            "library_lock".into(),
            json!({"held_ms": self.lock_held_ms(now)}),
        );
        out.insert(
            "media".into(),
            json!({
                "threads": self.media_threads.load(Ordering::Relaxed),
                "busy": self.media_busy.load(Ordering::Relaxed),
            }),
        );
        Value::Object(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_fires_only_when_all_workers_stop_progressing() {
        let http = HttpProgress::default();
        http.add_workers(2);
        let limit = Duration::from_secs(60);
        // Idle workers are never a stall, however long nothing happens.
        assert!(http.check(600_000, limit).is_none());

        http.begin(0, "/v1/assets/a/original", 1_000);
        http.begin(1, "/v1/assets/b/original", 2_000);
        http.progress(5_000);
        assert!(http.check(64_000, limit).is_none(), "progress 59s ago");
        let diagnosis = http.check(65_000, limit).expect("stalled");
        assert!(diagnosis.contains("/v1/assets/a/original (64s)"), "{diagnosis}");

        // A finished request is progress, and a free worker means requests still get served.
        http.end(1, 66_000);
        assert!(http.check(200_000, limit).is_none());
        http.begin(1, "/v1/hello", 200_000);
        assert!(http.check(259_999, limit).is_none(), "newest request is 59s old");
        assert!(http.check(260_000, limit).is_some());

        // Streaming body chunks keep a long download alive.
        http.progress(300_000);
        assert!(http.check(359_999, limit).is_none());
    }
}
