# tiny_http 0.12.0 (patched)

Upstream: https://crates.io/crates/tiny_http 0.12.0 (MIT OR Apache-2.0, see LICENSE-*),
used through `[patch.crates-io]` in the workspace `Cargo.toml`. Tests, benches and
examples were dropped; nothing else changed except:

- `src/util/task_pool.rs` `TaskPool::spawn`: queue a task only while there are more idle
  threads than queued tasks, else start a thread. Upstream queued every task while any
  thread was idle, so two connections accepted before that thread woke were both queued
  for it. Connection tasks run for the connection's lifetime (keep-alive), so the second
  client was never read until some other connection closed: a replica's parallel uploads
  hung on "receive response". Regression test in the same file:
  `cargo test --manifest-path vendor/tiny_http/Cargo.toml --lib task_pool`.

Drop this copy once a released tiny_http fixes the pool.
