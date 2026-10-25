# Changelog

## 0.4.1 — 2026-10-24

- Use generic library terminology throughout the documentation, agent guide, import help,
  progress and errors. Import libraries with `vamp import library PATH`; existing scripts
  and catalogs remain compatible.

## 0.4.0 — 2026-10-24

- `vamp setup` is rebuilt around optional extras; `vamp setup media|florence|models`,
  `--install` and `--source` are gone. `vamp setup` (`vamp setup plan [--text]`) is a
  read-only report of this machine (CPU, cores, memory, free disk, NVIDIA or Apple GPU, uv,
  ffmpeg, `vamp-media`, the active library's item count) and of each extra: `previews`,
  `video`, `search` (SigLIP: smart search, Find Similar, Riff, Galaxy) and `captions`
  (Florence-2), with state, download and disk size, seconds per item and whether it is
  recommended (`search` with 8 GB of memory and 5 GB of free disk, `captions` with an NVIDIA
  GPU). `vamp setup install search|captions|recommended [--device]` installs into
  `<home>/tools/models` with the `uv` on PATH or a pinned, SHA256-verified uv downloaded into
  `<home>/tools/bin`, a uv-managed Python 3.11 in `<home>/tools/python` and the weights in
  `<home>/tools/cache/hf`; PyTorch is the CUDA 12.8 build for cuda and the CPU build elsewhere
  on Linux (about 1.2 GB instead of 6.5 GB). It times one cold run of each processor on a
  sample image, registers the processors in the active (or `--context`) library keeping an
  existing registration's `placement` (re-running no longer resets `remote` to `any`) and
  queues the library's items for newly registered processors. `vamp setup decline` records
  a "no". Choices live in `<home>/setup.json`; `vamp init` registers installed extras in new
  libraries. `vamp worker` keeps serving installs made by `setup models --install` and
  updates their scripts. `vamp setup` no longer shows the diagnostics choice
  (`vamp diagnostics status` does), and `vamp-media` is no longer looked up in
  `<home>/tools/bin`: it ships beside `vamp`.
- Referenced originals now keep authority availability (`present` / `missing`, last checked,
  last relinked) in their synced asset documents. Missing opens return typed `original_missing`;
  a bounded server stat sweep does no hashing. Nothing is removed from the catalog.
- `vamp doctor` reports missing originals, verified relink candidates, ambiguity, unreadable
  paths and thumbnail/preview gaps as JSON without changing the catalog. `--fix` applies only
  unique size+BLAKE3 matches; `media relink ID PATH` and `media relink --search DIR` repair
  references without moving, copying or deleting originals. Pending fingerprints cannot relink.
  Local folder intake also retains its chosen root before expanding individual files.
- Per-asset embedding requests now return typed `embedding_unavailable` conflicts (HTTP 409),
  distinguishing queued/running, failed, stale-refresh and unqueued analysis. Errors include
  the known processor, distinct queued image count and a short failure category without
  leaking processor logs. Current-source lookup never substitutes stale vectors or fake
  neighbours; remote, native and CLI error envelopes retain the typed fields.
  `embedding_status` provides a read-only Riff/Galaxy anchor preflight, and explicit
  `embedding_retry` retries only that asset's current failed job without starting a worker.
- Video thumbnails and previews: `vamp media thumbnail ID` and `vamp media preview ID` take the
  first displayed frame of a video as its poster, then make the same JPEG representations and
  WebP grid tiers as for images. This needs the optional `ffmpeg` executable on `PATH` (or
  `VAMP_FFMPEG`); a missing tool or an unreadable video is a generation error, never a made-up
  thumbnail. Originals and provenance are never rewritten. The server serves originals with
  byte ranges and a known `Content-Length`, including large MP4s, for progressive players.
- Videos and animated GIF/WebP also get a `loop` representation: a muted H.264 MP4 of at most
  six seconds, 480 px on the long edge. Server and exploration ingestion generate it with the
  optional `ffmpeg`; `vamp media loop-preview ID` backfills one asset.
  `GET /v1/assets/ID/tier/loop` streams the stored MP4 with byte ranges (404 `tier_missing`
  before it exists) and never encodes during playback. Native `items` and `browse` rows carry
  `has_loop`, and native `stream` (`asset_id`, `kind: "loop" | "original"`) returns a local
  path or the authority URL with its authorization headers, without downloading.
- Replicas keep similarity packs (Riff, Find Similar) warm: packs persist under the library's
  `energy/` directory, a sync no longer fetches a missing one (the first similarity request
  does), and an existing pack is used at once while rate-limited background checks refresh it
  (`If-None-Match`; HTTP 304 when unchanged). A changed pack transfers only the rows affected
  since the cached pack's checkpoint, and a model or dimension change needs a full pack.
  Browsing never waits for the transfer.
- `vamp relay login --pair-code CODE` exchanges a single-use pairing code from the account page
  (valid for ten minutes) for the tunnel credential, with no token download; it cannot be
  combined with `--token-file`. A downloaded tunnel token with mode 0644 is accepted with a
  warning. The saved `relay.json` is always mode 0600, and later reads reject group or world
  permissions. Neither login method prints the code or the token.
- The relay tunnel keeps MCP's per-request `Mcp-Method` and `Mcp-Name` metadata alongside the
  protocol version, for the current stateless protocol and legacy initialization. When the
  local MCP handler rejects a request, stderr reports only its HTTP status and a recognized
  numeric JSON-RPC error code (or `unknown`), never response messages, library content or
  credentials; transport failures use the fixed `transport_error` category.

## 0.3.0 — 2026-10-16

- `vamp mcp` connects MCP clients to the selected library over stdio or, with `--http ADDR`, a
  loopback-only Streamable HTTP endpoint. Read-only mode (`--read-only`; the HTTP default) hides
  and rejects every tool that changes the library. The tools cover metadata and semantic search,
  folders, smart folders, tags, item details, bounded thumbnails and previews, capped originals,
  URL and file ingestion with provenance, ratings, annotations and explorations with change
  review; there is no deletion or shell-execution tool. It speaks MCP 2026-07-28 and the legacy
  2025-11-25, 2025-06-18 and 2025-03-26 revisions.
- `vamp relay login|connect|status|logout`: an owner-only outbound tunnel (no inbound port)
  from this computer to the hosted VAMP Connect relay, which serves the library's MCP tools to
  remote AI clients. Read-only unless `connect --read-write` and the client's OAuth consent both
  allow writes; credentials are never printed.
- Optional crash and error reports, off by default: `vamp diagnostics status|enable|disable|test`
  (`vamp setup` also shows the choice). `enable --endpoint URL [--headers-file PATH]` sends
  OTLP/HTTP JSON log records to that user-chosen OpenTelemetry endpoint only (none is compiled
  in; a missing or unusable endpoint refuses to enable) for commands failing with
  `operation_failed` and for panics: an error category, the command path without arguments,
  component (panic source file and line in VAMP crates), version, OS and architecture; never
  messages, arguments, paths, library content or identifiers. Bounded (3 per process, 10 per
  hour per home under a file lock, 4 KiB, 3 s, no queue, no redirects); output and exit status
  are unchanged. Credentials come from an owner-only headers file (never argv) and live in
  `<home>/diagnostics.json` (mode 600, never printed), separate from the catalog config;
  every send re-reads it, so `disable` (which deletes it) also stops running processes;
  `DO_NOT_TRACK=1` overrides it for a run. `vamp-cli` now always links its HTTP client
  (`ureq`); `vamp-protocol` and `vamp-core` are unchanged.
- Removed `vamp share` (hosted sharing parked).

## 0.2.0 — 2026-10-15

- Open-source the library server and the cloud storage helper under MIT: `vamp-server` (the
  `/v1` HTTP API for replicas, the Vampire apps and the browser clipper; token or Tailscale
  identity auth; background jobs and thumbnail tiers) and `vamp-store` (S3-compatible helper
  for cloud originals) move here from Vampire as fresh files, with the patched `tiny_http`
  0.12.0 they build on (`vendor/tiny_http`, upstream MIT OR Apache-2.0 notices kept). The
  Vampire apps, browser extension and native bridge stay proprietary.
- `vamp serve [--bind HOST:PORT] [--auth token|tailscale] [--no-jobs] [--dist DIR]` runs the
  `vamp-server` beside `vamp` (or on `PATH`) for the current or `--context` library. The
  default bind stays 127.0.0.1:7777 (this machine only); the server now prints its URL, the
  token file and the `vamp join` command for other machines at startup.
- `vamp join TARGET --token TOKEN` (or `VAMP_REMOTE_TOKEN`) attaches a replica of any
  `vamp-server` with its bearer token, not only over a tailnet with identity auth.
  `vamp_core::remote::join` takes the token.
- Release archives add `vamp-server` and `vamp-store` (and tiny_http in
  `THIRD-PARTY-NOTICES.txt`); `vamp self-update` and the installer replace all four binaries.
- Server tests use example identities (`alice@example.com`, `tag:server`, documentation
  addresses) instead of real tailnet names.

## 0.1.0 — 2026-10-15
- Open-source the `vamp` CLI under MIT: `vamp-core` (engine), `vamp-cli` (binary `vamp`),
  `vamp-media` (image helper), the model processors, the agent manual (`docs/agents/SKILL.md`,
  printed by `vamp guide`) and the library test fixture tool now live here, added as fresh files
  (no Vampire history). The Vampire apps, native bridge, `vamp-server` and `vamp-store` stay
  proprietary in Vampire, which builds against these crates by path.
- CLI releases are GitHub releases of this repository (`vamp-VERSION-TARGET.tar.gz` with the
  MIT `LICENSE` and `THIRD-PARTY-NOTICES.txt`); `vamp self-update` and
  `curl -fsSL https://vamp.sh/install | sh` download from here.
- No deployment defaults are compiled in (the former `vamp_release` cfg is gone): an
  authority library records where `vamp-server` serves it with `vamp contexts url NAME URL`
  (`<state_dir>/server-url.json`; no URL prints it, `--clear` forgets it, replicas refuse it),
  and `vamp explore link` falls back to it after `--server-url`/`VAMP_SERVER_URL` and a
  replica's authority URL. `vamp guide` prints the manual as is.
- Library import contract tests format expected dates in Rust instead of BSD-only `date -r`, so they
  pass on Linux.
- Add the DejaVu font license for the note-card glyph atlases
  (`crates/vamp-core/assets/LICENSE-DejaVu.txt`).
- Add native operation `find_duplicates` (intake duplicate screening by BLAKE3, with a
  same-size-and-name fallback for pending fingerprints; answered locally on replicas)
  and its `DuplicateCandidate` / `DuplicateMatch` types (schema key
  `duplicate_candidate`); document the Mac Duplicate File Alert in `apple_client`.
- Separate the MIT VAMP protocol from the proprietary Vampire application, with
  fresh protocol-only Git history.
- Bundle the canonical semantic contract inside `vamp-protocol` so packaged builds
  do not depend on files outside the crate.
- Add a standalone schema-output example; preserve wire version `0.1`, Rust package
  version `0.1.0`, exported types, and serialization behavior.
