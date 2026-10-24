#!/usr/bin/env python3
"""Local, offline-at-inference Florence-2 processor for VAMP's JSON tool protocol.

--task description (default) emits a detailed caption observation; --task tags emits
{"tags": [...]} derived from <OD> labels and a short <CAPTION>.

--serve --socket PATH keeps the model loaded and answers one line-delimited JSON
ToolRequest per connection with one ToolResponse line (or {"error":...}); the socket is
created 0600 and the server exits after --idle-seconds without connections.
"""
import argparse
import contextlib
import json
import os
from pathlib import Path
import re
import signal
import socket
import sys

MAX_REQUEST = 1024 * 1024

MODEL_ID = "florence-community/Florence-2-base"
MODEL_REVISION = "00921df66db728a9ceb750f5eca43e5c203a2051"
TASK = "<MORE_DETAILED_CAPTION>"
MAX_TAGS = 12
STOPWORDS = frozenset("""
a an the and or but of in on at to for from with without by into onto over under near
is are was were be been being has have had it its this that these those there their
his her he she they them we you i our your as up down out off some any all each one
two three four five several many few other another very just while which who whom
image picture photo photograph shows showing shown view close closeup front back
left right top bottom side middle center background foreground scene appears seen
""".split())


def tags_from(labels, caption):
    """Lowercase, deduplicated tags: detected object labels first, then caption words."""
    tags = []
    for text in list(labels) + re.findall(r"[a-zA-Z][a-zA-Z'-]+", caption or ""):
        tag = text.strip().lower()
        if (len(tag) < 3 or tag in STOPWORDS or tag in tags
                or (" " not in tag and tag.endswith("ing"))):
            continue
        tags.append(tag)
        if len(tags) == MAX_TAGS:
            break
    return tags


def load_model(path):
    import torch
    from transformers import AutoProcessor, Florence2ForConditionalGeneration

    processor = AutoProcessor.from_pretrained(
        str(path), local_files_only=True, trust_remote_code=False
    )
    model, info = Florence2ForConditionalGeneration.from_pretrained(
        str(path), local_files_only=True, trust_remote_code=False,
        use_safetensors=True, dtype=torch.float32, attn_implementation="eager",
        output_loading_info=True,
    )
    # A legacy Microsoft checkpoint has different keys. Never caption with
    # randomly initialized parameters if an incompatible checkpoint is supplied.
    if info.get("missing_keys") or info.get("mismatched_keys") or info.get("error_msgs"):
        raise RuntimeError(f"incompatible Florence checkpoint: {info}")
    model.eval()
    return processor, model


def prepare(manifest):
    from huggingface_hub import snapshot_download

    # Only native model data is fetched; no repository Python is executed.
    path = snapshot_download(
        MODEL_ID, revision=MODEL_REVISION,
        allow_patterns=["*.json", "*.txt", "*.safetensors", "*.model"],
    )
    load_model(path)
    payload = {"model": MODEL_ID, "revision": MODEL_REVISION, "path": path}
    manifest.parent.mkdir(parents=True, exist_ok=True)
    temporary = manifest.with_suffix(".tmp")
    temporary.write_text(json.dumps(payload), encoding="utf-8")
    temporary.replace(manifest)
    return payload


def generate(processor, model, image, task, device):
    import torch

    inputs = processor(text=task, images=image, return_tensors="pt").to(device)
    with torch.inference_mode():
        generated = model.generate(
            **inputs, max_new_tokens=512, num_beams=3, do_sample=False,
        )
    text = processor.batch_decode(generated, skip_special_tokens=False)[0]
    return processor.post_process_generation(text, task=task, image_size=image.size).get(task)


def loaded(manifest, device):
    state = json.loads(manifest.read_text(encoding="utf-8"))
    if state.get("model") != MODEL_ID or state.get("revision") != MODEL_REVISION:
        raise ValueError("model manifest is incompatible; run vamp setup install captions")
    import torch

    if device == "auto":
        device = ("cuda" if torch.cuda.is_available()
                  else "mps" if torch.backends.mps.is_available() else "cpu")
    elif device == "mps" and not torch.backends.mps.is_available():
        raise RuntimeError("MPS is not available; use --device cpu")
    elif device == "cuda" and not torch.cuda.is_available():
        raise RuntimeError("CUDA is not available; use --device cpu")
    processor, model = load_model(state["path"])
    model.to(device)
    return processor, model, device


def process(request, processor, model, device, task):
    if request.get("protocol_version") != "0.1":
        raise ValueError("unsupported or missing protocol_version; expected 0.1")
    input_path = request.get("input_path")
    if not isinstance(input_path, str) or not Path(input_path).is_file():
        raise ValueError("input_path must identify an existing local preview file")
    from PIL import Image, ImageOps

    with Image.open(input_path) as source:
        image = ImageOps.exif_transpose(source).convert("RGB")
    print(f"Florence-2 {task} locally on {device}", file=sys.stderr)
    if task == "tags":
        detected = generate(processor, model, image, "<OD>", device) or {}
        caption = generate(processor, model, image, "<CAPTION>", device)
        tags = tags_from(detected.get("labels", []), caption)
        return {"representation": None, "tiles": None, "observation": {
            "kind": "tags", "model": MODEL_ID,
            "value": {"tags": tags, "caption": caption, "revision": MODEL_REVISION,
                      "task": "<OD>+<CAPTION>", "device": device},
        }}
    description = generate(processor, model, image, TASK, device)
    if not isinstance(description, str) or not description.strip():
        raise RuntimeError("Florence-2 returned no detailed caption")
    return {"representation": None, "tiles": None, "observation": {
        "kind": "description", "model": MODEL_ID,
        "value": {"text": description.strip(), "revision": MODEL_REVISION,
                  "task": TASK, "device": device},
    }}


def read_line(conn):
    data = b""
    while not data.endswith(b"\n"):
        chunk = conn.recv(65536)
        if not chunk:
            break
        data += chunk
        if len(data) > MAX_REQUEST:
            raise ValueError("request exceeds 1 MiB")
    return data


def serve(manifest, device, task, path, idle_seconds):
    path = Path(path)
    if path.exists():
        probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            probe.connect(str(path))
            print(f"florence: server already listening on {path}", file=sys.stderr)
            return
        except OSError:
            path.unlink()
        finally:
            probe.close()
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    # Bind before loading so concurrent clients see the socket and queue on it.
    previous = os.umask(0o177)
    try:
        server.bind(str(path))
    finally:
        os.umask(previous)
    os.chmod(path, 0o600)
    identity = path.stat().st_ino
    server.listen(16)
    # Default SIGTERM would skip `finally` and leave a stale socket behind.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    try:
        processor, model, device = loaded(manifest, device)
        print(f"florence: serving {MODEL_ID} {task} on {device} at {path}", file=sys.stderr)
        server.settimeout(idle_seconds)
        while True:
            try:
                conn, _ = server.accept()
            except socket.timeout:
                break
            with conn:
                conn.settimeout(30)
                try:
                    request = json.loads(read_line(conn))
                    conn.settimeout(None)
                    response = process(request, processor, model, device, task)
                except Exception as error:  # one bad request must not stop the server
                    response = {"error": f"{type(error).__name__}: {error}"}
                with contextlib.suppress(OSError):
                    conn.sendall(json.dumps(response, ensure_ascii=False).encode() + b"\n")
    finally:
        server.close()
        with contextlib.suppress(OSError):
            if path.stat().st_ino == identity:
                path.unlink()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--prepare", action="store_true", help="explicitly download/cache and verify model")
    parser.add_argument("--device", choices=["auto", "cpu", "cuda", "mps"], default="cpu",
                        help="CPU float32 by default; auto selects CUDA, then MPS, when available")
    parser.add_argument("--task", choices=["description", "tags"], default="description")
    parser.add_argument("--serve", action="store_true", help="persistent socket server")
    parser.add_argument("--socket", help="Unix socket path for --serve")
    parser.add_argument("--idle-seconds", type=int, default=900)
    args = parser.parse_args()
    os.environ["HF_HUB_DISABLE_TELEMETRY"] = "1"
    os.environ["TOKENIZERS_PARALLELISM"] = "false"
    if not args.prepare:
        os.environ["HF_HUB_OFFLINE"] = "1"
        os.environ["TRANSFORMERS_OFFLINE"] = "1"
    # Third-party progress and diagnostics must never contaminate protocol stdout.
    with contextlib.redirect_stdout(sys.stderr):
        if args.serve:
            if not args.socket:
                raise ValueError("--serve requires --socket")
            serve(args.manifest, args.device, args.task, args.socket, args.idle_seconds)
            return
        if args.prepare:
            result = prepare(args.manifest)
        else:
            request = json.load(sys.stdin)
            processor, model, device = loaded(args.manifest, args.device)
            result = process(request, processor, model, device, args.task)
    print(json.dumps(result, ensure_ascii=False))


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"florence: {type(error).__name__}: {error}", file=sys.stderr)
        sys.exit(1)
