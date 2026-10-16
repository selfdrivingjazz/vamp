#!/usr/bin/env python3
"""Reference a bounded Eagle sample; prove local sheets without original access on macOS.

Requires a built/installed vamp and the optional media helper. No Eagle files are
written or moved. --home must be outside the Eagle library. Output stays in home.
"""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("library", type=Path)
    parser.add_argument("--vamp", default="vamp")
    parser.add_argument("--home", type=Path, required=True)
    parser.add_argument("--limit", type=int, default=8, choices=range(1, 101), metavar="1..100")
    args = parser.parse_args()
    library = args.library.resolve(strict=True)
    home = args.home.resolve()
    if home == library or library in home.parents:
        parser.error("--home must be outside the source library")
    executable = str(Path(args.vamp).resolve()) if Path(args.vamp).is_file() else shutil.which(args.vamp)
    if not executable:
        parser.error("vamp executable not found")
    context = "eagle-demo"

    def call(*arguments, offline=False, expect_failure=False):
        command = [executable, "--home", str(home), "--context", context, *map(str, arguments)]
        if offline:
            profile = '(version 1) (allow default) (deny file-read* (subpath ' + json.dumps(str(library)) + '))'
            command = ["/usr/bin/sandbox-exec", "-p", profile, *command]
        result = subprocess.run(command, capture_output=True, text=True, timeout=300)
        if expect_failure:
            if result.returncode == 0:
                raise RuntimeError("original access unexpectedly succeeded in the offline sandbox")
            return json.loads(result.stderr)
        if result.returncode:
            raise RuntimeError(result.stderr)
        return json.loads(result.stdout)

    def snapshot():
        result = {}
        for directory in sorted((library / "images").glob("*.info"))[:args.limit]:
            for path in directory.iterdir():
                if path.is_file():
                    digest = hashlib.sha256()
                    with path.open("rb") as stream:
                        for block in iter(lambda: stream.read(65536), b""):
                            digest.update(block)
                    result[str(path)] = [digest.hexdigest(), path.stat().st_mtime_ns]
        return result

    config = call("contexts", "list")
    if context not in config["contexts"]:
        call("init", "--name", context, "--original-root", library)
    elif config["contexts"][context]["original_root"] != str(library):
        raise RuntimeError("existing eagle-demo context references another source; choose another --home")
    before = snapshot()
    imported = call("import", "eagle", library, "--limit", args.limit)
    assets = imported["assets"]
    if not assets:
        raise RuntimeError(f"no source items imported: {imported['skipped']}")
    representations = []
    for asset in assets:
        representations.append(call("media", "thumbnail", asset["id"]))
        representations.append(call("media", "preview", asset["id"]))
    output = home / "demo" / f"eagle-{uuid.uuid4().hex}.jpg"
    output.parent.mkdir(parents=True, exist_ok=True)
    offline = sys.platform == "darwin" and Path("/usr/bin/sandbox-exec").is_file()
    denied = call("media", "fetch", assets[0]["id"], "--kind", "original", offline=True, expect_failure=True) if offline else None
    sheet = call("media", "sheet", "--ids", ",".join(asset["id"] for asset in assets), "--output", output, offline=offline)
    for asset in assets:
        call("media", "inspect", asset["id"], offline=offline)
        call("media", "fetch", asset["id"], "--kind", "thumbnail", offline=offline)
    if snapshot() != before:
        raise RuntimeError("source file contents or modification times changed during demonstration")
    print(json.dumps({
        "home": str(home), "context": context, "assets": len(assets),
        "skipped": imported["skipped"], "source_files_unchanged": len(before),
        "original_bytes": sum(asset["original"]["byte_size"] for asset in assets),
        "thumbnail_bytes": sum(rep["byte_size"] for rep in representations if rep["kind"] == "thumbnail"),
        "original_access_denied": denied, "offline_verified": offline,
        "offline_note": "OS-level original-read denial verified" if offline else "not verified: macOS sandbox-exec unavailable",
        "sheet": sheet,
    }, indent=2))


if __name__ == "__main__":
    main()
