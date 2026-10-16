#!/usr/bin/env python3
"""Generate (or mutate) a synthetic Eagle 4 library for VAMP import/sync tests.

Usage:
  eagle-fixture.py OUT.library --items N [--seed S] [--unreadable] [--no-thumbnail-every K]
  eagle-fixture.py OUT.library --mutate [--seed S] [--count N]

Layout mirrors Eagle 4: metadata.json (folders, smartFolders, quickAccess, tagsGroups),
tags.json, mtime.json, images/<ID>.info/{metadata.json, <name>.<ext>, <name>_thumbnail.png}.
Every 7th thumbnail (index % 7 == 3) holds WebP bytes, like real Eagle libraries; every K-th
item has no thumbnail (Eagle shows small originals directly).
Item 0 is always `red-square` (512x512 png, palettes[0]=[255,0,0] 90%, star 5).
--unreadable chmods every original to 000 (simulates cloud placeholders).
--mutate edits an existing library and prints {changed, deleted, added, removed_folders} JSON.
Python 3 stdlib only; deterministic for a given seed.
"""

import argparse
import base64
import json
import os
import random
import struct
import sys
import zlib
from pathlib import Path

BASE_TIME = 1_700_000_000_000  # ms epoch
# A real 1x1 lossless WebP (VP8L).
WEBP_1X1 = base64.b64decode("UklGRhoAAABXRUJQVlA4TA0AAAAvAAAAEAcQERGIiP4HAA==")
ID_CHARS = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"

COLORS = [
    (255, 0, 0), (0, 0, 255), (0, 160, 0), (255, 200, 0), (128, 0, 128), (255, 128, 0),
    (0, 180, 180), (240, 240, 240), (30, 30, 30), (200, 100, 150), (120, 80, 40), (100, 150, 255),
]
# name -> (width, height) of the original; thumbnails scale to <=256 edge.
SHAPES = {
    "square": (512, 512),
    "landscape": (640, 400),
    "portrait": (400, 640),
    "panorama": (1200, 300),
}
TAG_POOL = [
    "red", "primary", "blue", "green", "texture", "portrait", "landscape", "ui", "logo", "photo",
    "sketch", "reference", "moodboard", "typography", "icon", "pattern", "vintage", "minimal",
]
WORDS = ["sunset", "mock", "brand", "poster", "study", "frame", "shot", "board", "concept", "draft"]

MINIMAL_PDF = (
    b"%PDF-1.1\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n"
    b"2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n"
    b"3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]>>endobj\n"
    b"trailer<</Root 1 0 R>>\n%%EOF\n"
)
FAKE_MP4 = b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom" + b"\x00" * 32


def png(width, height, rows):
    """Encode RGB rows (list of bytes, each width*3) as PNG."""
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    raw = b"".join(b"\x00" + row for row in rows)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 6))
        + chunk(b"IEND", b"")
    )


def two_tone_png(width, height, main, secondary, main_ratio):
    """Main color on top, secondary band at the bottom covering (1 - main_ratio)."""
    split = round(height * main_ratio)
    top, bottom = bytes(main) * width, bytes(secondary) * width
    return png(width, height, [top if y < split else bottom for y in range(height)])


class Cache:
    def __init__(self):
        self.thumbs, self.originals = {}, {}

    def thumbnail(self, shape, main, secondary, ratio):
        key = (shape, main, secondary, ratio)
        if key not in self.thumbs:
            w, h = SHAPES[shape]
            scale = 256 / max(w, h)
            self.thumbs[key] = two_tone_png(max(1, round(w * scale)), max(1, round(h * scale)), main, secondary, ratio / 100)
        return self.thumbs[key]

    def original(self, shape, main, secondary, ratio):
        key = (shape, main, secondary, ratio)
        if key not in self.originals:
            w, h = SHAPES[shape]
            self.originals[key] = two_tone_png(w, h, main, secondary, ratio / 100)
        return self.originals[key]


def make_id(rng, used):
    while True:
        value = "".join(rng.choice(ID_CHARS) for _ in range(13))
        if value not in used:
            used.add(value)
            return value


def build_folders(rng, used):
    folders, flat = [], []

    def node(name, depth, t):
        f = {
            "id": make_id(rng, used),
            "name": name,
            "description": "",
            "children": [],
            "modificationTime": t,
            "tags": [],
            "password": "",
            "passwordTips": "",
        }
        if rng.random() < 0.4:
            f["iconColor"] = rng.choice(["red", "orange", "yellow", "green", "aqua", "blue", "purple", "pink"])
        flat.append((f, depth))
        return f

    t = BASE_TIME
    for top in ["Inspiration", "Clients", "Archive", "Assets"]:
        a = node(top, 1, t)
        folders.append(a)
        for i in range(2):
            b = node(f"{top} {i + 1}", 2, t + 1)
            a["children"].append(b)
            for j in range(2):
                c = node(f"{top} {i + 1}.{j + 1}", 3, t + 2)
                b["children"].append(c)
                if j == 0:
                    c["children"].append(node(f"{top} {i + 1}.{j + 1} deep", 4, t + 3))
    return folders, flat


def smart_folders(rng, used):
    t = BASE_TIME

    def sf(name, match, rules):
        return {
            "id": make_id(rng, used),
            "name": name,
            "description": "",
            "modificationTime": t,
            "conditions": [{"match": match, "boolean": "TRUE", "rules": rules}],
            "children": [],
        }

    return [
        sf("Red or Blue", "OR", [{"property": "tags", "method": "union", "value": ["red", "blue"]}]),
        sf("Top PNGs", "AND", [
            {"property": "rating", "method": ">=", "value": 4},
            {"property": "type", "method": "equal", "value": "png"},
        ]),
        sf("Commented", "AND", [{"property": "comments", "method": "contain", "value": "todo"}]),
        sf("Wide", "AND", [{"property": "width", "method": ">", "value": 1000}]),
    ]


def make_item(rng, used, index, folder_ids, cache, no_thumb_every, t):
    if index == 0:
        name, ext, shape, main, secondary, ratio = "red-square", "png", "square", (255, 0, 0), (255, 255, 255), 90
        tags, star, deleted = ["red", "primary"], 5, False
    else:
        name = f"{rng.choice(WORDS)}-{index:05d}"
        ext = rng.choices(["png", "pdf", "mp4"], weights=[85, 8, 7])[0]
        shape = rng.choice(list(SHAPES))
        main = rng.choice(COLORS)
        secondary = rng.choice([c for c in COLORS if c != main])
        ratio = rng.choice([60, 70, 80, 90])
        tags = rng.sample(TAG_POOL, rng.randint(0, 4))
        star = rng.choice([None, None, 0, 1, 2, 3, 4, 5])
        deleted = rng.random() < 0.05
    width, height = SHAPES[shape] if ext != "mp4" else (1920, 1080)
    if ext == "png":
        original = cache.original(shape, main, secondary, ratio)
    elif ext == "pdf":
        original, (width, height) = MINIMAL_PDF, (612, 792)
    else:
        original = FAKE_MP4
    has_thumb = not (no_thumb_every and index % no_thumb_every == no_thumb_every - 1)
    meta = {
        "id": make_id(rng, used),
        "name": name,
        "size": len(original),
        "btime": t,
        "mtime": t,
        "ext": ext,
        "tags": tags,
        "folders": [folder_ids[0]] if index == 0 else rng.sample(folder_ids, rng.choice([0, 1, 1, 1, 2])),
        "isDeleted": deleted,
        "url": "" if rng.random() < 0.5 else f"https://example.com/{name}",
        "annotation": "" if rng.random() < 0.7 else f"note for {name}",
        "modificationTime": t,
        "height": height,
        "width": width,
        "lastModified": t,
        "palettes": [
            {"color": list(main), "ratio": ratio, "$$hashKey": f"object:{index * 2 + 100}"},
            {"color": list(secondary), "ratio": 100 - ratio, "$$hashKey": f"object:{index * 2 + 101}"},
        ],
    }
    if star is not None:
        meta["star"] = star
    thumb = cache.thumbnail(shape, main, secondary, ratio) if has_thumb else None
    if thumb is not None and index % 7 == 3:
        thumb = WEBP_1X1  # Eagle often writes WebP bytes into <name>_thumbnail.png
    return meta, original, thumb


def write_item(root, meta, original, thumb):
    info = root / "images" / f"{meta['id']}.info"
    info.mkdir(parents=True, exist_ok=True)
    (info / f"{meta['name']}.{meta['ext']}").write_bytes(original)
    if thumb is not None:
        (info / f"{meta['name']}_thumbnail.png").write_bytes(thumb)
    write_json(info / "metadata.json", meta)


def write_json(path, value):
    path.write_text(json.dumps(value, separators=(",", ":")))


def generate(args):
    root = Path(args.out)
    if root.exists():
        sys.exit(f"{root} already exists")
    rng = random.Random(args.seed)
    used = set()
    cache = Cache()
    (root / "images").mkdir(parents=True)
    folders, flat = build_folders(rng, used)
    deepest = [f["id"] for f, d in flat if d >= 3]
    folder_ids = deepest + [f["id"] for f, d in flat if d < 3]
    mtimes = {}
    originals = []
    for i in range(args.items):
        t = BASE_TIME + i * 1000
        meta, original, thumb = make_item(rng, used, i, folder_ids, cache, args.no_thumbnail_every, t)
        write_item(root, meta, original, thumb)
        mtimes[meta["id"]] = t
        originals.append(root / "images" / f"{meta['id']}.info" / f"{meta['name']}.{meta['ext']}")
    mtimes["all"] = args.items
    write_json(root / "mtime.json", mtimes)
    write_json(root / "tags.json", {"historyTags": TAG_POOL[:10], "starredTags": ["red", "reference"]})
    write_json(root / "metadata.json", {
        "folders": folders,
        "smartFolders": smart_folders(rng, used),
        "quickAccess": [{"type": "folder", "id": folders[0]["id"]}],
        "tagsGroups": [
            {"id": make_id(rng, used), "name": "Colors", "tags": ["red", "blue", "green"], "color": "red"},
            {"id": make_id(rng, used), "name": "Kinds", "tags": ["photo", "sketch", "icon"], "color": "blue"},
        ],
        "modificationTime": BASE_TIME + args.items * 1000,
        "applicationVersion": "4.0.0",
    })
    if args.unreadable:
        for path in originals:
            os.chmod(path, 0)
    print(json.dumps({"library": str(root), "items": args.items, "first_item": next(iter(mtimes))}))


def mutate(args):
    root = Path(args.out)
    rng = random.Random(f"mutate-{args.seed}")
    library = json.loads((root / "metadata.json").read_text())
    mtimes = json.loads((root / "mtime.json").read_text())
    ids = sorted(k for k in mtimes if k != "all")
    used = set(ids)
    now = max(mtimes[i] for i in ids) + 60_000
    count = min(args.count, len(ids))
    picked = rng.sample(ids, count)
    to_delete = picked[: max(1, count // 4)]
    to_change = picked[len(to_delete):]
    for n, item_id in enumerate(picked):
        path = root / "images" / f"{item_id}.info" / "metadata.json"
        meta = json.loads(path.read_text())
        t = now + n
        if item_id in to_delete:
            meta["isDeleted"] = True
        else:
            meta["tags"] = sorted(set(meta["tags"]) | {rng.choice(TAG_POOL)}) if n % 2 else meta["tags"][1:]
            meta["star"] = rng.randint(0, 5)
            meta["annotation"] = f"mutated {args.seed}"
        meta["modificationTime"] = meta["lastModified"] = meta["mtime"] = t
        write_json(path, meta)
        mtimes[item_id] = t

    # Remove a leaf folder (one with no children) anywhere in the tree.
    removed = []

    def prune(nodes):
        for i, f in enumerate(nodes):
            if not f["children"] and not removed:
                removed.append(f["id"])
                del nodes[i]
                return
            prune(f["children"])
            if removed:
                return

    prune(library["folders"])
    flat_ids = []

    def walk(nodes):
        for f in nodes:
            flat_ids.append(f["id"])
            walk(f["children"])

    walk(library["folders"])
    cache = Cache()
    added = []
    for n in range(max(1, count // 4)):
        t = now + count + n
        meta, original, thumb = make_item(rng, used, len(ids) + n + 1, flat_ids, cache, 0, t)
        write_item(root, meta, original, thumb)
        mtimes[meta["id"]] = t
        added.append(meta["id"])
    mtimes["all"] = len(mtimes) - 1
    library["modificationTime"] = now + count + len(added)
    write_json(root / "metadata.json", library)
    write_json(root / "mtime.json", mtimes)
    print(json.dumps({"changed": to_change, "deleted": to_delete, "added": added, "removed_folders": removed}))


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("out", help="library directory (e.g. /tmp/x.library)")
    p.add_argument("--items", type=int, default=50)
    p.add_argument("--seed", default="0")
    p.add_argument("--unreadable", action="store_true", help="chmod 000 every original after writing")
    p.add_argument("--no-thumbnail-every", type=int, default=10, metavar="K", help="omit thumbnail on every K-th item (0 = never)")
    p.add_argument("--mutate", action="store_true", help="mutate an existing library and print a JSON change summary")
    p.add_argument("--count", type=int, default=20, help="items touched by --mutate")
    args = p.parse_args()
    mutate(args) if args.mutate else generate(args)


if __name__ == "__main__":
    main()
