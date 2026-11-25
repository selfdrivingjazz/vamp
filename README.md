# VAMP

VAMP is an experimental, reference-only media catalog for people and agents: files stay where
they are, the catalog records them (metadata, tags, folders, thumbnails, model captions and
embeddings), and `vamp` commands print JSON unless noted below. This repository holds the protocol, the
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

Start a library and add a folder (including nested folders):

```sh
vamp init "My refs" ~/Pictures/refs
vamp doctor
vamp serve                      # this machine only; keep this terminal open
```

Or create it with `vamp init "Edge City"` and add files later with
`vamp media add ~/Pictures/refs`. Display names are saved in the library; context keys
remain machine-readable (`Edge City` becomes `edge-city`). `--name "Edge City"` is also
accepted. Folder intake skips hidden entries and symlinks, reports per-file progress on
stderr and prints a JSON summary (`cataloged`, `total`, `failed`, `errors`) on stdout.
Existing references are kept; available `vamp-media` generates thumbnails and previews.

Bare `vamp` in a terminal prints help and the next command. Pipes and agents still receive
JSON errors for missing arguments. `vamp guide` prints the operating manual
(`docs/agents/SKILL.md`, also usable as an agent skill), `vamp commands` the full argument
tree and `vamp schema` the JSON schemas.

For the choices to make before installing, service setup and connecting another device,
see **Set up a new library with a person** in `vamp guide`.

## Optional extras: smart search and captions

The binaries carry no ML runtime. `vamp setup` (`vamp setup plan`) is a read-only report of
this machine (memory, free disk, GPU, ffmpeg, uv) and of each optional extra: its state,
download and disk size, seconds per item and whether it is recommended here. `--text` prints
it for people.

```sh
vamp setup plan --text           # what fits this machine
vamp setup install recommended    # or: vamp setup install search captions [--device cuda]
vamp setup decline captions       # record that you do not want it
```

| Extra | Enables | Recommended |
| --- | --- | --- |
| `previews` | Image thumbnails, previews and colors (`vamp-media`, shipped beside `vamp`) | Built in |
| `video` | Video thumbnails and loops | Needs `ffmpeg` on `PATH` (or `VAMP_FFMPEG`) |
| `search` | Search by meaning, Find Similar, Riff and Galaxy (SigLIP base) | With 8 GB of memory and 5 GB of free disk |
| `captions` | Descriptions and tags for every image (Florence-2 base) | With an NVIDIA GPU |

Extras install under `$VAMP_HOME/tools`: one uv environment (`tools/models`), a uv-managed
Python 3.11 (`tools/python`) and the model weights (`tools/cache/hf`). `vamp` uses the `uv` on
`PATH`, else downloads a pinned, checksum-verified uv into `tools/bin`. PyTorch is the CUDA 12.8
build for `--device cuda` (the default with an NVIDIA GPU), the CPU build elsewhere on Linux and
PyPI's build on macOS. Measured sizes for `search`: about 1.1 GB to download and 2 GB on disk
on a Linux CPU machine, 1 GB and 1.5 GB on macOS, and 4.8 GB and 8 GB with the CUDA build.
`captions` then adds about 0.5 GB.

After installing, each new processor runs once on a sample image to measure this machine's
seconds per item. The extra's processors are then registered in the active (or `--context`)
library, keeping an existing registration's `placement`, and the library's existing items are
queued. Queued items are processed while `vamp serve` runs, or by `vamp jobs run --limit N`.
Use `vamp jobs run --warm --idle-seconds 60 --limit N` to keep installed model processors
loaded between batches (default idle timeout: 900 seconds). The timeout applies when the
server starts; existing servers keep their original timeout. `vamp jobs stop-warm`
stops this library's warm model servers, or use `--processor siglip-embedding` to stop
only search. Socket files are owner-only; long state paths use a private per-user
temporary directory. Concurrent local job drains retain the existing worker-lock refusal.
Choices are kept in `$VAMP_HOME/setup.json`; `vamp init` registers installed extras
with their recorded versions. A GPU machine can run processors for another machine's
library with `vamp worker`.

`recommended` is a bundled, versioned toolkit. `vamp setup plan` shows the profile
and pinned model revisions; a library keeps its own processor and query-model
registrations. v1 uses SigLIP base and Florence-2 base with the same names and weights
as 0.4.2. A newer VAMP can recommend newer versions without switching a library's
existing processors. Pass `--profile VERSION` with named components to select a bundled
version other than the current one. To switch a library, pick a component explicitly:

```sh
vamp --context my-refs setup install search --upgrade
```

Changing the search model queues fresh embeddings from the new processor. Old and new
embedding spaces have separate keys; queries use only the active model, so results
rebuild as jobs finish. Existing originals and older observations are not deleted.

Video thumbnails and previews use the first displayed frame as a poster, then the same
JPEG representations and WebP grid tiers as images. This requires the optional `ffmpeg`
executable on the authority's `PATH` (or `VAMP_FFMPEG` set to its path); a missing tool
or unreadable video produces a generation error, never a fabricated thumbnail.
`vamp media thumbnail ID` and `vamp media preview ID` explicitly generate these
derivatives for an existing video. Originals and provenance are never rewritten.
The authority serves originals with byte ranges and a known `Content-Length`, including
large MP4 range responses, for progressive video players.

For near-instant inline playback, videos and animated GIF/WebP also receive a `loop`
representation: muted H.264/yuv420p MP4, at most 480 px on the long edge, 24 fps,
CRF 28 with a 1 Mbps ceiling, one-second keyframes, faststart, and at most six seconds.
Static images receive no loop. Server/exploration ingestion generates it with the
optional ffmpeg helper; `vamp media loop-preview ID` explicitly backfills one asset.
Loop metadata uses normal representation sync. `GET /v1/assets/ID/tier/loop` streams
the persisted MP4 with byte ranges (404 `tier_missing` before generation), never
hydrating or encoding an original during a playback request.

Native `items`/`browse` rows include `has_loop`; compact layouts carry the same flag
array. Native `stream` (`asset_id`, `kind: "loop" | "original"`) returns
`{url, headers, local}` without downloading: a local filesystem path when available,
otherwise the authority URL and required HTTP authorization headers. Clients pass
those headers directly to their player; tokens never belong in URLs or logs.

Similarity packs are persisted under the library's `energy/` directory. Metadata sync
does not fetch a missing pack: the first explicit similarity consumer requests it.
An existing pack is usable immediately while rate-limited background checks refresh it.
Checks use `If-None-Match` (unchanged: HTTP 304, no pack body). Changed packs can transfer
only rows affected since the cached pack's event checkpoint; source, palette, deletion
and embedding updates are applied atomically to the cached file. A model/dimension
change or missing checkpoint needs a full pack. Concurrent startup/sync requests share
one refresh, and ordinary grid browsing never waits for its network transfer.

## Missing originals and repair

`vamp doctor` prints a read-only JSON report: present/missing counts, hash-verified
relinkable candidates, ambiguous matches, unreadable paths, and derivative tier gaps
(thumbnail, preview, micro, grid and high-resolution preview).
On Unix it checks read permissions/ACLs without opening present originals.
It never removes items or changes original files. `vamp doctor --fix` applies only
unambiguous repairs. `vamp media relink ID PATH` verifies the recorded size and BLAKE3;
`vamp media relink --search DIR` applies unambiguous repairs found below that directory.
`doctor --search DIR` limits a read-only search (repeat for multiple roots).

The authority stats at most 128 catalog rows every 30 seconds, with 10 ms between
files and a separate single I/O worker (2 s deadline); a full pass takes about
`ceil(items / 128) * 32` seconds on responsive storage. No periodic hashing or
filesystem watching occurs. Original opens also record ENOENT immediately and return
HTTP 404 with `error.code: original_missing`, `asset_id`, and the recorded `path`.
Other stat failures are unreadable, not evidence of deletion.

Health lives in `original.availability` (`state`, `last_checked_at`, `relinked_at`,
optional `check_error`) and syncs normally. Old catalogs default to present with no
check time. Repairs keep IDs, revisions, hashes, derivatives and replica caches intact.
Search uses known import roots and imported libraries, plus each missing file’s parent
and ancestors bounded by its configured/imported root; without a boundary it never
walks above the parent. It does not follow directory symlinks. Only same-size files
are hashed, once per scan. No recorded hash (lazy fingerprint still pending), multiple
matches, or an incomplete search prevents automatic repair.

`GET /v1/doctor` and native `doctor` expose the read-only report to authenticated
clients; native `relink {asset_id,path}` performs an explicit verified repair.
On replicas, Locate paths must exist on the authority or use an existing shared-drive
mapping; the app does not upload, copy, or relocate a selected file.


## Share a library

One machine owns a library (the authority); other machines and the Vampire apps attach
replicas that sync from it.

```sh
vamp init "My refs" ~/Pictures/refs   # on the authority; recursively adds the folder
vamp serve --bind tailnet            # other machines on your Tailscale network
```

`vamp serve` runs `vamp-server` for the current library (`--context NAME` for another). It
prints the URL, where the bearer token is (`<state_dir>/server-token`, created on first
start; `vamp-server token` prints it) and the join command. On another machine:

```sh
vamp join http://authority-host:7777 --token TOKEN   # attach a replica and sync it
vamp sync                                            # pull later changes (--watch keeps going)
```

If the bearer token changes, join the same URL under the same context name with the
new token. This updates the credential without replacing the mirror or its caches.

Upgrade the authority before its replicas when rolling out bulk edits. A current
replica sends bulk trash as one request and refuses to fall back to one delete per
asset. Bulk metadata edits are atomic only if the authority also runs the updated
server; older servers that accept bulk edits can commit part of a failed request.

The CLI and the app can sync the same local mirror. Only one pulls at a time; waiting
readers still see the applied changes. If a remote edit succeeds but the mirror cannot
catch up, the native result includes `sync_pending`. Sync again rather than repeating the edit.

The Vampire Mac app offers running local servers and nearby DNS-SD servers in its welcome
and **Connect to Server…** sheet. It checks the server's ID before offering it and doesn't
connect without a choice. The iOS app uses **Connect via Tailnet** (host, port, optional token).
On [Tailscale](https://tailscale.com), `vamp serve --bind tailnet` binds to this machine's
tailnet address and admits allowlisted identities without a token (`vamp-server allow
--user LOGIN` / `--tag tag:NAME`; by default this machine's Tailscale user).
`vamp-server install-service` writes a user service on Linux (systemd) or macOS (launchd)
and prints the command to start it. `vamp storage set s3 …` keeps originals in an
S3-compatible bucket through `vamp-store` (installed beside `vamp`).

- Local discovery uses one owner-only JSON file per server PID under
  `$XDG_STATE_HOME/vamp/servers` (Linux default `~/.local/state/vamp/servers`; macOS
  `~/Library/Application Support/VAMP/servers`). These are per-user runtime records,
  separate from any catalog location. Clients check the PID and the running endpoint;
  a later server start can prune records for dead processes.
- Add `--advertise` to a LAN or Tailscale bind to announce `_vamp._tcp` on the local link.
  TXT carries the library ID, display name, host, and auth mode, and a Tailscale bind
  also announces its tailnet address. Local multicast doesn't cross the tailnet, so
  devices away from this LAN still use the server address. Advertising does not change
  who is allowed to connect.
- `GET /v1/discovery` is an unauthenticated identity check, not an access grant. Neither it
  nor the registry nor DNS-SD contains credentials or catalog paths; reading and changing
  catalog contents still require normal authorization.

## Usage and health reports (optional, off by default)

`vamp` and `vamp-server` send no telemetry unless you opt in. They share the
choice in the same `VAMP_HOME`; configure them separately if they use different
homes. There is no built-in destination. Reports go to an OTLP/HTTP JSON endpoint
you choose, as `POST <endpoint>/v1/logs`.

```sh
vamp diagnostics status    # choice, destination and exact allowed fields
vamp diagnostics enable --endpoint https://telemetry.example.com
vamp diagnostics test      # check that the destination accepts a test report
vamp diagnostics disable   # drop pending events and forget the destination
```

Use HTTPS except for a local loopback collector (e.g. `http://127.0.0.1:4318`).
If your collector requires authentication, pass `--headers-file PATH`. The file
must be readable only by you (`chmod 600` on Unix), with one `NAME=VALUE` per line,
such as `Authorization=Basic …`. `status` redacts the values; the destination
receives these headers, but they are never part of an event. The endpoint
operator controls access logs and retention; the receiver sees your source IP
on the connection.

- **What:** release version, platform, product, policy, fixed feature and
  outcome names, and optional duration, library size and authority/replica
  buckets. No identifiers, exact event times, content, paths, URLs, error
  messages or stack traces. Rust panics in `vamp` and `vamp-server` send only a
  failure marker. Unknown-version opt-ins, invalid arguments, helper process
  crashes and process exits without a Rust panic do not produce reports.
- **Limits:** at most 10 requests per home per hour, up to 16 events per request
  and 32 pending events per product, expiring after 24 hours. The CLI drains on
  later invocations; the server drains in the background. A due CLI batch can
  add a bounded network wait. An ambiguous delivery is not retried. Redirects
  are never followed.
- **Choice:** policy changes require opting in again. `DO_NOT_TRACK=1` (or any
  nonempty value other than `0` or `false`) overrides consent for that run. Start
  or restart `vamp-server` after opting in or changing destinations. Disabling
  stops even a running server after any in-flight request finishes. This does not
  remove reports already received.
- **Local files:** `<home>/diagnostics.json` holds the consent and destination;
  `<home>/diagnostics-state.json` holds pending bucketed reports, the send budget,
  and the last delivery result. Both are mode 600 on Unix.

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
are installed automatically (`vamp setup install search` installs them). A replica reads its
local mirror; run `vamp sync` when stale.

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

The tunnel preserves MCP's per-request `Mcp-Method` and `Mcp-Name` metadata alongside
the protocol version, including the current stateless protocol and legacy initialization.
If the local MCP handler rejects a request, stderr reports only its HTTP status and a
recognized numeric JSON-RPC error code (or `unknown`), never response messages, library
content or credentials. Transport failures use the fixed `transport_error` category.


## Contents

| Path | Crate / file | What it is |
| --- | --- | --- |
| `crates/vamp-protocol` | `vamp-protocol` | Serialized types, generated JSON schemas and the semantic contract (`protocol/contract.json`) |
| `crates/vamp-core` | `vamp-core` | The engine: SQLite catalog, library import, thumbnails, processors and jobs, explorations, replica sync (feature `remote`), native operations (feature `native`) |
| `crates/vamp-cli` | `vamp-cli` (binary `vamp`) | The command-line tool |
| `crates/vamp-media` | `vamp-media` | Optional helper `vamp` and `vamp-core` call for image decoding (HEIC/AVIF/PSD included), video posters (optional ffmpeg), thumbnails and colors |
| `crates/vamp-server` | `vamp-server` | HTTP server for one authority library: the `/v1` API replicas, the Vampire apps and the browser clipper use; token or Tailscale identity auth; background processor jobs and thumbnail tiers (`vamp serve` runs it) |
| `crates/vamp-store` | `vamp-store` | S3-compatible object storage helper for cloud originals (one JSON request on stdin, one response on stdout) |
| `vendor/tiny_http` | `tiny_http` 0.12.0 (patched) | The HTTP/1.1 server `vamp-server` uses, with a connection-pool fix (`vendor/tiny_http/VENDORED.md`); MIT OR Apache-2.0 |
| `processors/` | | Optional local model processors (Florence-2 captions/tags, SigLIP embeddings) installed by `vamp setup install captions\|search` |
| `docs/agents/SKILL.md` | | The manual `vamp guide` prints |
| `tools/` | | Synthetic library fixtures for tests and note-card glyph atlases |
| `examples/` | | Bounded library import and catalog examples |

The protocol version is `0.1`; the Rust packages are `0.4.2`. Only `vamp-protocol` is meant
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
`vamp-server` beside `vamp` or on `PATH` (`cargo build --release -p vamp-server`). Tests need `python3` (library
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
