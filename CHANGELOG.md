# Changelog

## Unreleased

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
