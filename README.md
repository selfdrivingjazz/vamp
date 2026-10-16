# VAMP

VAMP is an experimental, reference-only media catalog for people and agents: files stay where
they are, the catalog records them (metadata, tags, folders, thumbnails, model captions and
embeddings), and every `vamp` command prints JSON. This repository holds the protocol, the
engine, the `vamp` command-line tool, the `vamp-server` library server and the `vamp-store`
cloud storage helper, all MIT-licensed. Pre-alpha: expect breaking changes.

## Install

```sh
curl -fsSL https://vamp.sh/install | sh
```

Installs `vamp`, its optional `vamp-media` helper, the `vamp-server` library server and the
`vamp-store` S3 helper (macOS on Apple silicon or Intel, Linux x86_64 or aarch64, static) from
this repository's [GitHub releases](https://github.com/selfdrivingjazz/vamp/releases),
checksum-verified. `vamp self-update` (`--check` only reports) moves a release install (all
four binaries) to the latest release. Each release archive holds `vamp`, `vamp-media`,
`vamp-server`, `vamp-store`, `LICENSE` and `THIRD-PARTY-NOTICES.txt`.

Then `vamp guide` prints the operating manual (`docs/agents/SKILL.md`, also usable as an agent
skill), `vamp commands` the full argument tree and `vamp schema` the JSON schemas.

## Share a library

One machine owns a library (the authority); other machines and the Vampire apps attach
replicas that sync from it.

```sh
vamp init --name refs            # on the authority
for f in ~/Pictures/refs/*.jpg; do vamp media add "$f"; done   # or vamp import eagle LIBRARY
vamp serve --bind 0.0.0.0:7777   # default 127.0.0.1:7777: this machine only
```

`vamp serve` runs `vamp-server` for the current library (`--context NAME` for another). It
prints the URL, where the bearer token is (`<state_dir>/server-token`, created on first
start; `vamp-server token` prints it) and the join command. On another machine:

```sh
vamp join http://authority-host:7777 --token TOKEN   # attach a replica and sync it
vamp sync                                            # pull later changes (--watch keeps going)
```

The Vampire Mac app connects with **Connect to Server…** (mode *URL + token*); the iOS app
with **Connect via Tailnet** (host, port, optional token). On a [Tailscale](https://tailscale.com) network, `vamp serve --bind
tailnet` binds this machine's tailnet address and admits allowlisted identities without a
token (`vamp-server allow --user LOGIN` / `--tag tag:NAME`; by default this machine's own
Tailscale user). `vamp-server install-agent` writes a macOS launch agent; on Linux run
`vamp-server serve` under systemd. `vamp storage set s3 …` keeps originals in an
S3-compatible bucket through `vamp-store` (installed beside `vamp`).

## Contents

| Path | Crate / file | What it is |
| --- | --- | --- |
| `crates/vamp-protocol` | `vamp-protocol` | Serialized types, generated JSON schemas and the semantic contract (`protocol/contract.json`) |
| `crates/vamp-core` | `vamp-core` | The engine: SQLite catalog, Eagle import, thumbnails, processors and jobs, explorations, replica sync (feature `remote`), native operations (feature `native`) |
| `crates/vamp-cli` | `vamp-cli` (binary `vamp`) | The command-line tool |
| `crates/vamp-media` | `vamp-media` | Optional helper `vamp` and `vamp-core` call for image decoding (HEIC/AVIF/PSD included), thumbnails and colors |
| `crates/vamp-server` | `vamp-server` | HTTP server for one authority library: the `/v1` API replicas, the Vampire apps and the browser clipper use; token or Tailscale identity auth; background processor jobs and thumbnail tiers (`vamp serve` runs it) |
| `crates/vamp-store` | `vamp-store` | S3-compatible object storage helper for cloud originals (one JSON request on stdin, one response on stdout) |
| `vendor/tiny_http` | `tiny_http` 0.12.0 (patched) | The HTTP/1.1 server `vamp-server` uses, with a connection-pool fix (`vendor/tiny_http/VENDORED.md`); MIT OR Apache-2.0 |
| `processors/` | | Optional local model processors (Florence-2 captions/tags, SigLIP embeddings) installed by `vamp setup models --install` |
| `docs/agents/SKILL.md` | | The manual `vamp guide` prints |
| `tools/` | | `eagle-fixture.py` (synthetic Eagle libraries for tests), `card-font.py` (note-card glyph atlases) |
| `examples/` | | `eagle-demo.py` (catalog a bounded Eagle sample) |

The protocol version is `0.1`; the Rust packages are `0.2.0`. Only `vamp-protocol` is meant
for crates.io; the others are `publish = false`.

## Build and test

```sh
cargo build --workspace --all-features
cargo test --workspace --all-features
cargo run -p vamp-cli --features remote --bin vamp -- guide
cargo run -p vamp-protocol --example schema   # the bundle `vamp schema` prints
```

Feature `remote` of `vamp-cli` adds replica contexts of a `vamp-server` authority (`vamp join`,
`vamp contexts remote`, `vamp sync`) and `vamp self-update`. `vamp serve` needs a
`vamp-server` beside `vamp` or on `PATH` (`cargo build --release -p vamp-server`). Tests need `python3` (Eagle
fixtures). Release archives are built with `VAMP_RELEASE_TARGET=<target triple>` set, which
is what lets `vamp self-update` pick its asset; source builds refuse to self-update.

## Vampire

Vampire is a separate, proprietary application built on VAMP: the Mac and iOS apps, the
browser extension and the native bridge they use. It is not in this repository and is not
covered by its license. Protocol use, independent implementations, the `vamp` CLI,
`vamp-server` and `vamp-store` do not require Vampire.

| Project | Contents | License |
| --- | --- | --- |
| This repository (`selfdrivingjazz/vamp`, public) | Protocol, engine, `vamp`, `vamp-media`, `vamp-server`, `vamp-store`, model processors, agent manual; releases | MIT |
| Vampire (private) | Mac and iOS apps, browser extension, native bridge (`vamp-bridge`), app packaging and distribution | Proprietary |

## License

MIT; see `LICENSE`. The note-card glyph atlases in `crates/vamp-core/assets` are rendered from
the DejaVu fonts (`crates/vamp-core/assets/LICENSE-DejaVu.txt`). `vendor/tiny_http` is a
patched copy of tiny_http under its upstream MIT OR Apache-2.0 license
(`vendor/tiny_http/LICENSE-MIT`, `LICENSE-APACHE`). Dependencies keep their own licenses;
release archives list them in `THIRD-PARTY-NOTICES.txt`.
