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

## Crash and error reports (optional, off by default)

`vamp` works fully offline with no account, and sends nothing anywhere unless you opt in.
`vamp setup` and `vamp diagnostics status` show the choice: whether reports are on, where they
go (credentials redacted) and exactly what a report holds. There is no VAMP-run collector;
reports go only to an OpenTelemetry endpoint you choose, as OTLP/HTTP JSON log records
POSTed to `<endpoint>/v1/logs` (an OpenTelemetry Collector or Grafana Alloy, e.g.
`http://localhost:4318`, or Grafana Cloud's OTLP endpoint with its `Authorization` header).
Credentials go in a headers file, never on the command line, so they stay out of shell history
and the process list: one `NAME=VALUE` per line (blank lines and `#` comments skipped),
readable only by you.

```sh
umask 077
touch ~/.vamp-otlp-headers
chmod 600 ~/.vamp-otlp-headers
# In your editor, put one NAME=VALUE header per line in this file.
# Example format: Authorization=Basic <base64 of instance-id:token>
vamp diagnostics enable --endpoint https://otlp-gateway-<region>.grafana.net/otlp \
  --headers-file ~/.vamp-otlp-headers
vamp diagnostics test      # send one test report; fails unless the endpoint accepts it
vamp diagnostics disable   # stop, and forget the endpoint and its credentials
```

Edit header values in the protected file, not in command arguments or shell
history. `enable` refuses a headers file other users can read and never prints
its contents; you may delete it after enabling. Credentials require HTTPS unless
the collector is on loopback (`localhost`, `127.0.0.1` or `::1`).

- **When:** a command that fails with `operation_failed`, and a Rust panic on any `vamp`
  thread (after the panic message is printed). Nothing is sent for commands that succeed.
  Not covered: invalid arguments, output errors, crashes without a panic (signals such as
  SIGKILL or SIGSEGV, aborts, out-of-memory, stack overflow) and failures inside helper
  processes (`vamp-media`, model processors). Exit status, unwinding and output never change.
- **What:** kind (error, panic, test), an error category from a fixed list (`io.not_found`,
  `json`, `network`, `other`, …), the command path without arguments (`media inspect`), the
  component and, for a panic in VAMP's own source, its crate-relative file and line, the
  `vamp` version, OS and CPU architecture, and the time. Never messages, stack traces,
  arguments, local paths, asset ids, tags, URLs, environment, library content or any machine,
  user or install identifier.
- **Receiver:** the chosen collector sees transport metadata such as your source IP and
  any authentication headers you configure. Its operator controls retention and deletion.
- **Limits:** at most 3 reports per process and 10 per hour per home (counted under a short
  file lock, `<home>/diagnostics.lock`, so parallel `vamp` runs share the budget), 4 KiB
  each, sent synchronously (2 s connect, 3 s total; a failing command can take that much
  longer to exit), never queued or retried, no redirects followed.
- **Storage:** `<home>/diagnostics.json` (mode 600, separate from the catalog config; holds the
  endpoint, the headers copied from the headers file and the consent time) and
  `<home>/diagnostics-state.json` (send budget and the last delivery result shown by
  `status`). Consent is to one version of the report contents: a release that changes them
  stops reporting until you enable it again. Every send re-reads `diagnostics.json`, so
  `disable`, or a changed endpoint or headers, also stops reports from already running
  `vamp jobs watch`, `vamp sync --watch` and `vamp worker` processes. A request
  already in flight can finish; disabling does not erase reports already received.
- **Precedence:** `DO_NOT_TRACK` set to anything but empty, `0` or `false` (e.g.
  `DO_NOT_TRACK=1`) turns reports off for that run whatever `diagnostics.json` says
  (`status` shows `"do_not_track":true`, `"enabled":false`; `test` refuses). Without it, reports
  are on only while `diagnostics.json` holds consent for the current report version.

## Connect an AI assistant

`vamp mcp` connects an MCP client to the selected authority or replica context. It uses
stdio by default (only protocol messages go to stdout):

```sh
claude mcp add vamp -- vamp mcp --read-only
# To select a particular library:
claude mcp add vamp -- vamp --context refs mcp --read-only
```

For Claude Desktop, merge this entry into `mcpServers` in
`~/Library/Application Support/Claude/claude_desktop_config.json` on macOS (on Windows,
`%APPDATA%\Claude\claude_desktop_config.json`), preserving other entries. Replace the
executable with the absolute path printed by `command -v vamp`, then restart Claude Desktop:

```json
{
  "mcpServers": {
    "vamp": {
      "command": "/absolute/path/to/vamp",
      "args": ["--context", "refs", "mcp", "--read-only"],
      "env": { "VAMP_ACTOR": "agent:claude-desktop" }
    }
  }
}
```

Vampire for Mac has **Settings → Agents → Connect to Claude Desktop**: it detects Claude,
asks before changing its configuration, preserves other servers and backs up the old file.
It uses the app's bundled CLI and the current library.

For clients that accept Streamable HTTP:

```sh
vamp mcp --http 127.0.0.1:7790
# MCP endpoint: http://127.0.0.1:7790/mcp (read-only by default)
```

HTTP binds only to loopback and validates Host/Origin to prevent DNS rebinding. It is a
local endpoint, not a publicly authenticated service: do not reverse-proxy it to the
Internet. `--read-write` explicitly enables HTTP mutations; stdio is writable unless
`--read-only` is passed. Read-only mode hides and rejects mutation tools, and exploration
changes never advances a read-only reader's cursor. `VAMP_HOME`/`--home` and
`VAMP_CONTEXT`/`--context` work as for other commands. The actor is `VAMP_ACTOR`, or
`agent:<client-name>` (agent identities also travel with authenticated replica writes).

Tools cover metadata/semantic search, folders, smart folders, tags, item details, bounded
image thumbnails/previews, explicit capped originals, URL/file ingestion with provenance,
tagging, ratings, annotations, folder creation and explorations (including change review).
No deletion or shell-execution tool is exposed. Treat media and catalog text as untrusted
data. Semantic search needs the library's existing embeddings/query embedder; no models
are installed automatically. A replica reads its local mirror; run `vamp sync` when stale.

The server supports MCP revision **2026-07-28** (`server/discover`, per-request metadata)
and legacy initialize-based **2025-11-25**, **2025-06-18**, **2025-03-26** clients.
HTTP uses POST `/mcp`, JSON responses, no sessions and no GET event stream.
For inspection: `npx @modelcontextprotocol/inspector vamp mcp --read-only`, or start the
HTTP server above and choose Streamable HTTP with its `/mcp` URL in Inspector.

### Hosted remote access (optional subscription)

The open-source tunnel client also runs on Linux servers. VAMP Connect provides an
OAuth-protected remote MCP URL for ChatGPT and Claude; it requires an active relay
entitlement. The computer serving the library must remain awake and connected. Content
is streamed through the relay, not stored there, and no public library links are created.

```sh
vamp relay login                  # opens the account page, also prints its URL
# Sign in and copy the single-use pairing code from the account page:
vamp relay login --pair-code CODE # no token download needed
vamp relay status                 # prints the remote MCP URL, never the credential
vamp --context refs relay connect # keep running; read-only by default
```

Pairing codes expire after ten minutes and can be exchanged only once. They are not
long-lived tunnel credentials; do not share them. `--pair-code` cannot be combined with
`--token-file`. Vampire for Mac can pair directly from the account page's
`vampire://relay/pair?code=...` link.

If direct pairing is unavailable, download a tunnel token from the account page and run
`vamp relay login --token-file /path/to/tunnel-token.json`. Browser downloads with mode
0644 are accepted with a warning; `chmod 600` before import is optional extra protection.
After a successful import, delete the downloaded file as prompted. The saved credential
in `VAMP_HOME/relay.json` is always created with mode 0600 on Unix, and later reads reject
group/world permissions. Neither login method prints the code or tunnel token.

Add the URL from `relay status` as a custom MCP connector in your AI client and approve
OAuth consent. Remote write access requires both `relay connect --read-write` on the
computer and a write-capable OAuth grant; either side remaining read-only prevents writes.
`vamp relay logout` removes this computer's saved credential; revoke tunnel tokens and
AI-client grants on the account page when needed. Vampire for Mac exposes the read-only
tunnel in **Settings → Agents → Use from ChatGPT and Claude anywhere**.


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

The protocol version is `0.1`; the Rust packages are `0.3.0`. Only `vamp-protocol` is meant
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
