# Changelog

## Unreleased

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
