use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

/// Manages a collection of threads.
///
/// A new thread is created every time all the existing threads are full.
/// Any idle thread will automatically die after a few seconds.
pub struct TaskPool {
    sharing: Arc<Sharing>,
}

struct Sharing {
    // list of the tasks to be done by worker threads
    todo: Mutex<VecDeque<Box<dyn FnMut() + Send>>>,

    // condvar that will be notified whenever a task is added to `todo`
    condvar: Condvar,

    // number of total worker threads running
    active_tasks: AtomicUsize,

    // number of idle worker threads
    waiting_tasks: AtomicUsize,
}

/// Minimum number of active threads.
static MIN_THREADS: usize = 4;

struct Registration<'a> {
    nb: &'a AtomicUsize,
}

impl<'a> Registration<'a> {
    fn new(nb: &'a AtomicUsize) -> Registration<'a> {
        nb.fetch_add(1, Ordering::Release);
        Registration { nb }
    }
}

impl<'a> Drop for Registration<'a> {
    fn drop(&mut self) {
        self.nb.fetch_sub(1, Ordering::Release);
    }
}

impl TaskPool {
    pub fn new() -> TaskPool {
        let pool = TaskPool {
            sharing: Arc::new(Sharing {
                todo: Mutex::new(VecDeque::new()),
                condvar: Condvar::new(),
                active_tasks: AtomicUsize::new(0),
                waiting_tasks: AtomicUsize::new(0),
            }),
        };

        for _ in 0..MIN_THREADS {
            pool.add_thread(None)
        }

        pool
    }

    /// Executes a function in a thread.
    /// If no thread is available, spawns a new one.
    ///
    /// VAMP patch: a queued task needs an idle thread of its own. A woken thread stops
    /// counting as idle only once it retakes the lock, so tasks already in the queue have
    /// claimed that many idle threads; queueing beyond them left a task (a client
    /// connection) waiting until some other connection closed.
    pub fn spawn(&self, code: Box<dyn FnMut() + Send>) {
        let mut queue = self.sharing.todo.lock().unwrap();

        if self.sharing.waiting_tasks.load(Ordering::Acquire) > queue.len() {
            queue.push_back(code);
            self.sharing.condvar.notify_one();
        } else {
            self.add_thread(Some(code));
        }
    }

    fn add_thread(&self, initial_fn: Option<Box<dyn FnMut() + Send>>) {
        let sharing = self.sharing.clone();

        thread::spawn(move || {
            let sharing = sharing;
            let _active_guard = Registration::new(&sharing.active_tasks);

            if let Some(mut f) = initial_fn {
                f();
            }

            loop {
                let mut task: Box<dyn FnMut() + Send> = {
                    let mut todo = sharing.todo.lock().unwrap();

                    let task;
                    loop {
                        if let Some(poped_task) = todo.pop_front() {
                            task = poped_task;
                            break;
                        }
                        let _waiting_guard = Registration::new(&sharing.waiting_tasks);

                        let received =
                            if sharing.active_tasks.load(Ordering::Acquire) <= MIN_THREADS {
                                todo = sharing.condvar.wait(todo).unwrap();
                                true
                            } else {
                                let (new_lock, waitres) = sharing
                                    .condvar
                                    .wait_timeout(todo, Duration::from_millis(5000))
                                    .unwrap();
                                todo = new_lock;
                                !waitres.timed_out()
                            };

                        if !received && todo.is_empty() {
                            return;
                        }
                    }

                    task
                };

                task();
            }
        });
    }
}

impl Drop for TaskPool {
    fn drop(&mut self) {
        self.sharing
            .active_tasks
            .store(999_999_999, Ordering::Release);
        self.sharing.condvar.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::TaskPool;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    /// Connection tasks block for the connection's lifetime (keep-alive). Two tasks spawned
    /// back to back while a single pool thread was idle used to both be queued for that one
    /// thread: the second never ran while the others stayed busy.
    #[test]
    fn tasks_spawned_back_to_back_all_run_while_the_others_block() {
        for round in 0..300 {
            let pool = TaskPool::new();
            let state = Arc::new((Mutex::new((0usize, false)), Condvar::new()));
            let task = || {
                let state = state.clone();
                Box::new(move || {
                    let (lock, changed) = &*state;
                    let mut guard = lock.lock().unwrap();
                    guard.0 += 1;
                    changed.notify_all();
                    while !guard.1 {
                        guard = changed.wait(guard).unwrap();
                    }
                }) as Box<dyn FnMut() + Send>
            };
            let started = |count: usize| {
                let (lock, changed) = &*state;
                let deadline = Instant::now() + Duration::from_secs(2);
                let mut guard = lock.lock().unwrap();
                while guard.0 < count {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return false;
                    }
                    guard = changed.wait_timeout(guard, left).unwrap().0;
                }
                true
            };
            for _ in 0..3 {
                pool.spawn(task());
            }
            assert!(started(3));
            // One idle thread left: two tasks arrive before it wakes.
            pool.spawn(task());
            pool.spawn(task());
            let all = started(5);
            let (lock, changed) = &*state;
            lock.lock().unwrap().1 = true;
            changed.notify_all();
            assert!(all, "round {}: a task never ran", round);
        }
    }
}
