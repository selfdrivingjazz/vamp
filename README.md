# VAMP

VAMP is an experimental, reference-only media catalog for people and agents: files stay where
they are, the catalog records them (metadata, tags, folders, thumbnails, model captions and
embeddings), and every `vamp` command prints JSON. This repository holds the protocol, the
engine and the `vamp` command-line tool, all MIT-licensed. Pre-alpha: expect breaking changes.

## Install

```sh
curl -fsSL https://vamp.sh/install | sh
```

Installs `vamp` and its optional `vamp-media` helper (macOS on Apple silicon or Intel, Linux
x86_64 or aarch64, static) from this repository's
[GitHub releases](https://github.com/selfdrivingjazz/vamp/releases), checksum-verified.
`vamp self-update` (`--check` only reports) moves a release install to the latest release.
Each release archive holds `vamp`, `vamp-media`, `LICENSE` and `THIRD-PARTY-NOTICES.txt`.

Then `vamp guide` prints the operating manual (`docs/agents/SKILL.md`, also usable as an agent
skill), `vamp commands` the full argument tree and `vamp schema` the JSON schemas.

## Contents

| Path | Crate / file | What it is |
| --- | --- | --- |
| `crates/vamp-protocol` | `vamp-protocol` | Serialized types, generated JSON schemas and the semantic contract (`protocol/contract.json`) |
| `crates/vamp-core` | `vamp-core` | The engine: SQLite catalog, Eagle import, thumbnails, processors and jobs, explorations, replica sync (feature `remote`), native operations (feature `native`) |
| `crates/vamp-cli` | `vamp-cli` (binary `vamp`) | The command-line tool |
| `crates/vamp-media` | `vamp-media` | Optional helper `vamp` and `vamp-core` call for image decoding (HEIC/AVIF/PSD included), thumbnails and colors |
| `processors/` | | Optional local model processors (Florence-2 captions/tags, SigLIP embeddings) installed by `vamp setup models --install` |
| `docs/agents/SKILL.md` | | The manual `vamp guide` prints |
| `tools/` | | `eagle-fixture.py` (synthetic Eagle libraries for tests), `card-font.py` (note-card glyph atlases) |
| `examples/` | | `eagle-demo.py` (catalog a bounded Eagle sample) |

The protocol version is `0.1`; the Rust packages are `0.1.0`. Only `vamp-protocol` is meant
for crates.io; the others are `publish = false`.

## Build and test

```sh
cargo build --workspace --all-features
cargo test --workspace --all-features
cargo run -p vamp-cli --features remote --bin vamp -- guide
cargo run -p vamp-protocol --example schema   # the bundle `vamp schema` prints
```

Feature `remote` of `vamp-cli` adds replica contexts of a `vamp-server` authority (`vamp join`,
`vamp contexts remote`, `vamp sync`) and `vamp self-update`. Tests need `python3` (Eagle
fixtures). Release archives are built with `VAMP_RELEASE_TARGET=<target triple>` set, which
is what lets `vamp self-update` pick its asset; source builds refuse to self-update.

## Vampire

Vampire is a separate, proprietary application built on VAMP: the Mac and iOS apps, the
browser extension, the native bridge, the `vamp-server` authority and the `vamp-store` cloud
helper. It is not in this repository and is not covered by its license. Protocol use,
independent implementations and the `vamp` CLI do not require Vampire.

## License

MIT; see `LICENSE`. The note-card glyph atlases in `crates/vamp-core/assets` are rendered from
the DejaVu fonts (`crates/vamp-core/assets/LICENSE-DejaVu.txt`). Dependencies keep their own
licenses; release archives list them in `THIRD-PARTY-NOTICES.txt`.
