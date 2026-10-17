---
name: vamp
description: Search, inspect, fetch, tag, rate, organize and ingest images/media in the owner's VAMP library (reference-only media catalog shared over the tailnet) via the `vamp` CLI. Use when the user asks about their images, references, mood boards, Eagle library, or wants media found, tagged or filed.
---

# VAMP for agents

`vamp` is a reference-only media catalog. One machine (the authority, which runs `vamp serve`,
i.e. `vamp-server`) owns the library; other machines hold replicas that read locally and forward writes.
Every command prints one JSON document on stdout; errors are JSON on stderr with a
nonzero exit (`{"error":{"code","message"}}`). Never parse `--help` text when JSON exists.

## Discover
- `vamp guide` — this manual.
- `vamp commands` — full command/argument tree (types, defaults, choices). Check it before guessing flags.
- `vamp schema` — JSON schemas for every payload (Asset, MetadataPatch, smart-folder rules, ...).
- `vamp contexts list` — which library/context is active; `--context NAME` or `VAMP_CONTEXT` selects one.
- No library yet on this machine? `vamp join HOST[:PORT] [--token TOKEN]` (default port 7777) attaches a replica of the authority at HOST (the token is the authority's `vamp-server token`; on a tailnet the server may admit this machine without one; ask the owner if it is refused).
- `vamp serve [--bind 0.0.0.0:PORT]` — serve the current library to other machines and the Vampire apps (default bind 127.0.0.1:7777, this machine only); it prints the URL, the token file and the join command. Long-running: start it only when asked.
- `vamp self-update [--check]` — replace a release install (`vamp`, `vamp-media`, `vamp-server`, `vamp-store`, from `curl -fsSL https://vamp.sh/install | sh`) with the latest release.
- `vamp diagnostics status` — whether optional crash/error reports are on (off by default), where they go and exactly what they hold. Enabling (`vamp diagnostics enable --endpoint URL`) or disabling is the owner's choice: never change it unless they ask.
- Set `VAMP_ACTOR=agent:<agent-name>` so catalog events record who made a change; what you add is recorded as added by you (`metadata.extra.added_by`), and the owner's app shows the owner's own items by default (agents' under Agents).

## Find
- `vamp media search "red brutalist stair"` — full-text over names, tags, descriptions, captions.
- `vamp media search "moody night street" --semantic` — embedding similarity (meaning, not words). `vamp media search "moody night street" --semantic --hybrid` fuses it with full-text. Prefer semantic for visual/descriptive queries.
- `vamp media similar <id> --limit 20` — assets that look like this one.
- `vamp galaxy layout [--rebuild]` — refresh the 3-D embedding map behind the app's Galaxy view (authority; run nightly). `vamp galaxy search TEXT` ranks like its search bar.
- `vamp media list --query QUERY --limit 100 --offset 0` — metadata-only listing (`--query` optional); paginate with `--offset`.
- `--added-by me|agents|<actor>` (media list/search) — only what the owner added, what agents added, or one actor (e.g. `agent:<your-name>`).
- `vamp tags list`, `vamp collections list`, `vamp smart list`, `vamp sidebar` — vocabulary and structure.

## Inspect and view
- `vamp media inspect <id>` — full asset: `revision`, metadata (tags, rating, description), captions/model tags (observations), representations.
- `vamp media fetch <id> --kind thumbnail|preview|original` — prints a local path you can open/read. Look at `thumbnail`/`preview` first; fetch `original` only when full resolution is needed (it may download from cloud storage).
- `vamp media sheet --query "..." --limit 16 --output out.png` — numbered contact sheet to review many results in one image (or `--ids ID,ID,...`).
- `vamp media export <ids...> --to <dir>` — copy originals into an existing directory for the user.

## Change (always pass `--expected-revision`)
Read `revision` from `inspect`, then pass it so a concurrent edit fails with a conflict instead of being overwritten. On conflict: re-inspect, re-apply, retry.
- Tag/rate/describe: write a MetadataPatch JSON file, e.g. `{"add_tags":["stairs"],"remove_tags":["todo"],"rating":4}`, then `vamp media update <id> --patch patch.json --expected-revision N`. `tags` replaces all tags — prefer `add_tags`/`remove_tags`.
- Accept model-suggested tags: `vamp media accept-tags <id> [TAG...] --expected-revision N`.
- Organize: `vamp collections create NAME [--parent <collection_id>]`, `vamp collections add <collection_id> <asset_id>`, `vamp collections remove <collection_id> <asset_id>`, `vamp collections rename <collection_id> NAME`, `vamp collections move <collection_id> [--parent <collection_id>] [--index N]`, `vamp collections describe <collection_id> [TEXT]` (folder notes). Smart folders: `vamp smart create NAME '{"match":"all","rules":[{"field":"tags","op":"contains","value":"x"}]}'` (fields: name, description, url, tags, ext, rating, width, height, size, created, modified, imported, color, shape, folder).
- Tag vocabulary: `vamp tags rename FROM TO` (merges), `vamp tags delete TAG` — library-wide; confirm with the user first.

## Ingest
- `vamp media add <path> [--name NAME] [--metadata meta.json]` — catalog a file by reference (the file is hashed, never moved or modified).
- Captions, model tags and embeddings are produced asynchronously by processors (GPU worker); `vamp jobs list` shows progress. Do not run `vamp jobs run`/`watch` unless asked.
- Eagle import (`vamp import eagle PATH`) is an owner operation — do not run it.

## Explorations (research missions for the owner)
An exploration is a folder `Explorations/<title>` found by a stable slug (`[a-z0-9-]`; the owner may rename or move it). Its description holds your brief and a running findings summary (the app's inspector shows it when nothing is selected). Findings are assets filed into it or its sub-folders (`people`, `studios/<name>`, `themes`…), each with a name, a why-it-matters note (description), tags and the source page URL.

Where to run: on the authority, with the `vamp` CLI writing directly to the authority library while `vamp-server` runs; SQLite WAL makes that safe and replicas (the owner's Macs) see each change within a second. Replica contexts are read-only: from another machine run the commands over ssh on the authority. Always set `VAMP_ACTOR` (explore commands refuse without it): `explore changes` lists every change made by an actor other than you.

```sh
export VAMP_ACTOR=agent:<your-name>
vamp explore ensure vamp-sh --title "vamp.sh — branding & lander" --brief "What the owner asked for, seeds, what to look for"
# A remote image (downloaded and copied into the library); --url is the page it came from.
vamp explore add vamp-sh --folder studios --name "Bakers — og image" --note "Why it matters" --tags seed,identity --url https://bakers.studio/ --image-url https://…/image.png
# A local file, e.g. a browser screenshot you took (bytes are copied in; delete your temp file afterwards if you like).
vamp explore add vamp-sh --folder studios --name "Bakers — homepage" --note "…" --tags lander,typography --url https://bakers.studio/ --file /tmp/bakers.png
# A text-only finding (a person, studio, link, theme): rendered as a note card image showing kind, name, note, URL and tags.
vamp explore add vamp-sh --folder people --kind person --name "Jane Doe" --note "Type designer behind …" --tags type --url https://janedoe.example/
vamp explore summary vamp-sh "What you found so far and what you will look at next"
vamp explore link vamp-sh            # → {"url":"http://AUTHORITY:PORT/open?library=…&folder=…","app_url":"vampire://open?…","open":"open 'vampire://open?…'"}
vamp explore link vamp-sh --asset <asset_id>   # the same, selecting one item
```
- Give the owner the `url` from `explore link` (an http page on the authority that immediately opens Vampire on that folder; terminals usually only make http(s) links clickable, so never hand over the bare `vampire://` URL). Also give the `open` command as a fallback to paste into a shell (it is quoted; an unquoted `vampire://` URL breaks on `?`/`&`). The app syncs first if the folder is new to it. `explore add` returns the item's raw `link`; for the owner use `vamp explore link SLUG --asset ID` instead. `--server-url URL` (or `VAMP_SERVER_URL`) overrides the authority address; replicas default to their authority, and an authority's own context to the URL set with `vamp contexts url NAME URL`. With no authority known (a local library without one and no `--server-url`), `url` is null: give the `open` command.
- Re-runs are safe: the same `--image-url`, the same bytes, or (note cards) the same `--kind` + `--name` + `--url` (no `--url`: the same kind + name in this exploration; case and spacing in the name are ignored) return `"status":"existing"` (the item is filed into the given folder and gains new tags; name and note stay). Different cards about one page (a studio and each of its people) need different names and stay separate items. If the matching asset is in the Trash, `"status":"trashed"` is returned and the asset is left unchanged (not filed, not re-tagged, not restored).
- Prefer a real screenshot of a page (`--file`) over a note card when you can take one; use note cards for people, ideas and pages you cannot capture. `--image-url` must be an image, not an HTML page.
- `vamp explore show vamp-sh` — brief, summary, folders and every item with its current rating/tags/note; `vamp explore list` — all explorations.
- `vamp explore changes vamp-sh` — what actors other than you changed in the exploration since your last read, then advances your cursor (`--peek` leaves it, `--since N` replays from event N). JSON: `reader`, `since`, `cursor`, `items` (each touched item's current `name`, `rating`, `tags`, `description`, `note` (what you added it with), `trashed`, `deleted`, `folders`, plus its raw `events`: `sequence`, `kind`, `actor`, `at`, `payload`), `folder_events` (renames, new or deleted sub-folders).
- VAMP attaches no meaning to these changes: a rating, a tag, a note, a move or the Trash means whatever the owner says it means. Use the interpretation given in your mission prompt; if there is none, report the raw changes and ask him how to read them rather than assuming a convention.

## Safety rules
- Never delete files. `vamp media remove <id>` moves to the trash (restorable with `vamp media restore <id>`). Do not run `vamp media empty-trash` unless the user explicitly asks.
- Always use `--expected-revision` for updates/removals; never retry a 409/conflict blindly.
- Never write into the Eagle library (`*.library` folders) or the originals' directories.
- Bulk changes (more than ~20 assets, tag renames/deletes, collection deletes): show the plan and get confirmation first.
- Replicas: run `vamp sync` to pull the latest catalog before large reads if results look stale.
