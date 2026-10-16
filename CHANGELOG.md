# Changelog

## Unreleased

- Share v0, the public half: `vamp share login|logout|account|plan|create|list|publish|revoke`
  publishes a frozen snapshot of a folder (with sub-folders), an exploration or a selection to
  a share host (default `https://app.vamp.sh`, `VAMP_SHARE_HOST`/`--host` override). Items in
  the Trash or tagged `private` are left out; metadata is scrubbed to the chosen fields before
  anything is sent; only the existing grid/hires/micro derivatives are uploaded,
  content-addressed by SHA-256, so interrupted creates resume and re-publishes upload only
  what changed. The `vamp-share` manifest v1 (`vamp schema` `share_manifest`), the host API and
  the native ops `share_*` are in the contract section `share`.

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
  printed by `vamp guide`) and the Eagle test fixture tool now live here, added as fresh files
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
- Eagle contract tests format expected dates in Rust instead of BSD-only `date -r`, so they
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
