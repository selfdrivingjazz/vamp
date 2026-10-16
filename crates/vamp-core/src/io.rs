//! Bounded thread pools for filesystem calls that can block indefinitely: originals and
//! Eagle library files on cloud-synced folders (Google Drive / iCloud File Provider), where
//! `open`/`stat`/`read` may hang until the provider recovers. Callers wait with a deadline
//! and get [`Stalled`]; the stuck call stays parked on a pool thread, never the caller's.
//!
//! The deadline is an inactivity limit: long reads extend it with [`tick`] (per chunk),
//! and steps with their own timeout (helper subprocesses) announce it with [`expect`].
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, mpsc};
use std::time::{Duration, Instant};

/// Inactivity deadline for background (jobs worker, imports, CLI) original reads.
pub const DRIVE_DEADLINE: Duration = Duration::from_secs(60);
/// Threads of the shared background pool (Eagle thumbnail copies read 24 files at once).
const BACKGROUND_THREADS: usize = 32;

/// An operation made no progress before its deadline (time queued behind stuck operations
/// counts); it may still finish later.
#[derive(Debug, Clone)]
pub struct Stalled {
    pub label: String,
    pub waited: Duration,
}

impl std::fmt::Display for Stalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "original unavailable: {}: no progress for {}s (cloud storage not responding)",
            self.label,
            self.waited.as_secs()
        )
    }
}

impl std::error::Error for Stalled {}

/// Whether `error` (anywhere in its chain) is a [`Stalled`] operation: retry later.
pub fn is_stalled(error: &anyhow::Error) -> bool {
    error.chain().any(|e| e.is::<Stalled>())
}
static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);
static BACKGROUND: LazyLock<Pool> = LazyLock::new(|| Pool::new("background", BACKGROUND_THREADS));

fn epoch() -> Instant {
    *EPOCH
}

fn now_ms() -> u64 {
    epoch().elapsed().as_millis() as u64
}

type Job = Box<dyn FnOnce() + Send>;

struct Task {
    id: u64,
    label: String,
    job: Job,
    cancelled: Arc<AtomicBool>,
    due: Arc<AtomicU64>,
    base: Duration,
}

struct Running {
    label: String,
    started: Instant,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Task>,
    idle: usize,
    spawned: usize,
    running: HashMap<u64, Running>,
    next_id: u64,
}

struct Inner {
    name: &'static str,
    threads: usize,
    state: Mutex<State>,
    ready: Condvar,
    completed: AtomicU64,
    stalled: AtomicU64,
}

/// A bounded pool; threads start on demand up to `threads` and then live for the process.
pub struct Pool {
    inner: Arc<Inner>,
}

/// One running operation (oldest first in [`PoolStats::running`]).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunningOp {
    pub label: String,
    pub age_ms: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PoolStats {
    pub name: &'static str,
    pub threads: usize,
    pub busy: usize,
    pub queued: usize,
    pub completed: u64,
    /// Operations whose caller gave up (deadline or saturation).
    pub stalled: u64,
    pub running: Vec<RunningOp>,
}

impl PoolStats {
    pub fn oldest_ms(&self) -> u64 {
        self.running.first().map_or(0, |r| r.age_ms)
    }
    /// Every thread is busy with an operation older than `age`.
    pub fn wedged(&self, age: Duration) -> bool {
        self.busy >= self.threads
            && self
                .running
                .iter()
                .all(|r| r.age_ms >= age.as_millis() as u64)
    }
}

thread_local! {
    static CURRENT: RefCell<Option<(Arc<AtomicU64>, Duration)>> = const { RefCell::new(None) };
}

/// Inside a pool operation: progress was made; the deadline restarts from now.
pub fn tick() {
    CURRENT.with(|c| {
        if let Some((due, base)) = &*c.borrow() {
            due.store(now_ms() + base.as_millis() as u64, Ordering::Relaxed);
        }
    });
}

/// Inside a pool operation: the next step may take up to `duration` without progress
/// (e.g. a helper subprocess with its own timeout).
pub fn expect(duration: Duration) {
    CURRENT.with(|c| {
        if let Some((due, _)) = &*c.borrow() {
            due.store(now_ms() + duration.as_millis() as u64, Ordering::Relaxed);
        }
    });
}

/// The shared pool for background work (jobs worker, Eagle thumbnails, CLI).
pub fn background() -> &'static Pool {
    &BACKGROUND
}

impl Pool {
    pub fn new(name: &'static str, threads: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                name,
                threads: threads.max(1),
                state: Mutex::default(),
                ready: Condvar::new(),
                completed: AtomicU64::new(0),
                stalled: AtomicU64::new(0),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Runs `f` on a pool thread and waits until it finishes or makes no progress for
    /// `deadline` (see [`tick`]/[`expect`]). On [`Stalled`] the operation keeps running (or,
    /// if it never started, is dropped) and its result is discarded.
    pub fn run<T: Send + 'static>(
        &self,
        label: impl Into<String>,
        deadline: Duration,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, Stalled> {
        self.run_capped(label, deadline, None, f)
    }

    /// [`Pool::run`], but the caller also gives up after `cap` in total, however much
    /// progress the operation reports (it keeps running).
    pub fn run_capped<T: Send + 'static>(
        &self,
        label: impl Into<String>,
        deadline: Duration,
        cap: Option<Duration>,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, Stalled> {
        let label = label.into();
        let started = Instant::now();
        let cap_at = cap.map(|c| now_ms() + c.as_millis() as u64);
        let (send, receive) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let due = Arc::new(AtomicU64::new(now_ms() + deadline.as_millis() as u64));
        {
            let mut state = self.lock();
            state.next_id += 1;
            let id = state.next_id;
            state.queue.push_back(Task {
                id,
                label: label.clone(),
                job: Box::new(move || {
                    let _ = send.send(f());
                }),
                cancelled: cancelled.clone(),
                due: due.clone(),
                base: deadline,
            });
            if state.idle == 0 && state.spawned < self.inner.threads {
                state.spawned += 1;
                let inner = self.inner.clone();
                std::thread::Builder::new()
                    .name(format!("vamp-io-{}", self.inner.name))
                    .spawn(move || worker(inner))
                    .expect("spawn I/O thread");
            } else {
                self.inner.ready.notify_one();
            }
        }
        loop {
            let due_at = due.load(Ordering::Relaxed).min(cap_at.unwrap_or(u64::MAX));
            let remaining = due_at.saturating_sub(now_ms());
            if remaining == 0 {
                cancelled.store(true, Ordering::Relaxed);
                // A result may have landed just now.
                if let Ok(value) = receive.try_recv() {
                    return Ok(value);
                }
                self.inner.stalled.fetch_add(1, Ordering::Relaxed);
                return Err(Stalled {
                    label,
                    waited: started.elapsed(),
                });
            }
            match receive.recv_timeout(Duration::from_millis(remaining.min(250))) {
                Ok(value) => return Ok(value),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // The operation panicked.
                    return Err(Stalled {
                        label,
                        waited: started.elapsed(),
                    });
                }
            }
        }
    }

    /// [`Pool::run`] for fallible operations; a stall becomes an error ([`is_stalled`]).
    pub fn try_run<T: Send + 'static>(
        &self,
        label: impl Into<String>,
        deadline: Duration,
        f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        self.run(label, deadline, f)?
    }

    pub fn stats(&self) -> PoolStats {
        let state = self.lock();
        let mut running: Vec<RunningOp> = state
            .running
            .values()
            .map(|r| RunningOp {
                label: r.label.clone(),
                age_ms: r.started.elapsed().as_millis() as u64,
            })
            .collect();
        running.sort_by_key(|r| std::cmp::Reverse(r.age_ms));
        PoolStats {
            name: self.inner.name,
            threads: self.inner.threads,
            busy: state.running.len(),
            queued: state
                .queue
                .iter()
                .filter(|t| !t.cancelled.load(Ordering::Relaxed))
                .count(),
            completed: self.inner.completed.load(Ordering::Relaxed),
            stalled: self.inner.stalled.load(Ordering::Relaxed),
            running,
        }
    }
}

fn worker(inner: Arc<Inner>) {
    loop {
        let task = {
            let mut state = inner.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                match state.queue.pop_front() {
                    // Its caller already gave up: never start it.
                    Some(task) if task.cancelled.load(Ordering::Relaxed) => continue,
                    Some(task) => {
                        state.running.insert(
                            task.id,
                            Running {
                                label: task.label.clone(),
                                started: Instant::now(),
                            },
                        );
                        break task;
                    }
                    None => {
                        state.idle += 1;
                        state = inner.ready.wait(state).unwrap_or_else(|e| e.into_inner());
                        state.idle -= 1;
                    }
                }
            }
        };
        CURRENT.with(|c| *c.borrow_mut() = Some((task.due.clone(), task.base)));
        let job = task.job;
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
        CURRENT.with(|c| *c.borrow_mut() = None);
        inner.completed.fetch_add(1, Ordering::Relaxed);
        inner
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .running
            .remove(&task.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stalled_operation_parks_and_others_proceed() {
        let pool = Pool::new("test", 2);
        let (release, gate) = mpsc::channel::<()>();
        let gate = Arc::new(Mutex::new(gate));
        let held = gate.clone();
        let error = pool
            .run("stuck", Duration::from_millis(100), move || {
                let _ = held.lock().unwrap().recv();
            })
            .unwrap_err();
        assert!(error.waited >= Duration::from_millis(100));
        assert_eq!(pool.stats().busy, 1);
        assert_eq!(pool.run("ok", Duration::from_secs(5), || 7).unwrap(), 7);
        release.send(()).unwrap();
        let start = Instant::now();
        while pool.stats().busy > 0 && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(pool.stats().busy, 0);
    }

    #[test]
    fn ticks_extend_the_deadline() {
        let pool = Pool::new("test", 1);
        let value = pool
            .run("slow", Duration::from_millis(150), || {
                for _ in 0..6 {
                    std::thread::sleep(Duration::from_millis(60));
                    tick();
                }
                1
            })
            .unwrap();
        assert_eq!(value, 1);
    }
}
