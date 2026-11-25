#!/usr/bin/env python3
"""Local SigLIP image/text embedding processor for VAMP's JSON tool protocol.

operation "process" embeds input_path (image); operation "embed_text" embeds request.text.
Both emit {"kind":"embedding","value":{"embedding":{model,dim,vector_b64}}} with an
L2-normalized float32 little-endian vector in the shared image/text space.

--serve --socket PATH keeps the model loaded and answers one line-delimited JSON
ToolRequest per connection with one ToolResponse line (or {"error":...}); the socket is
created 0600 and the server exits after --idle-seconds without connections.
"""
import argparse
import base64
import contextlib
import json
import os
from pathlib import Path
import signal
import socket
import sys

MODEL_ID = "google/siglip-base-patch16-224"
MODEL_REVISION = "7fd15f0689c79d79e38b1c2e2e2370a7bf2761ed"
# SigLIP text towers were trained on fixed-length padded sequences.
TEXT_LENGTH = 64
MAX_REQUEST = 1024 * 1024


def resolve_device(device):
    import torch

    mps = torch.backends.mps.is_available()
    if device == "auto":
        return "cuda" if torch.cuda.is_available() else "mps" if mps else "cpu"
    if device == "mps" and not mps:
        raise RuntimeError("MPS is not available; use --device cpu")
    if device == "cuda" and not torch.cuda.is_available():
        raise RuntimeError("CUDA is not available; use --device cpu")
    return device


def load_model(path):
    import torch
    from transformers import AutoModel, AutoProcessor

    processor = AutoProcessor.from_pretrained(str(path), local_files_only=True)
    model, info = AutoModel.from_pretrained(
        str(path), local_files_only=True, use_safetensors=True,
        dtype=torch.float32, output_loading_info=True,
    )
    if info.get("missing_keys") or info.get("mismatched_keys") or info.get("error_msgs"):
        raise RuntimeError(f"incompatible SigLIP checkpoint: {info}")
    model.eval()
    return processor, model


def prepare(manifest):
    from huggingface_hub import snapshot_download

    path = snapshot_download(
        MODEL_ID, revision=MODEL_REVISION,
        allow_patterns=["*.json", "*.safetensors", "spiece.model"],
    )
    load_model(path)
    payload = {"model": MODEL_ID, "revision": MODEL_REVISION, "path": path}
    manifest.parent.mkdir(parents=True, exist_ok=True)
    temporary = manifest.with_suffix(".tmp")
    temporary.write_text(json.dumps(payload), encoding="utf-8")
    temporary.replace(manifest)
    return payload


def loaded(manifest, device):
    state = json.loads(manifest.read_text(encoding="utf-8"))
    if state.get("model") != MODEL_ID or state.get("revision") != MODEL_REVISION:
        raise ValueError("model manifest is incompatible; run vamp setup install search")
    device = resolve_device(device)
    processor, model = load_model(state["path"])
    model.to(device)
    return processor, model, device


def embed(request, processor, model, device):
    import numpy as np
    import torch

    if request.get("protocol_version") != "0.1":
        raise ValueError("unsupported or missing protocol_version; expected 0.1")
    operation = request.get("operation")
    with torch.inference_mode():
        if operation == "process":
            from PIL import Image, ImageOps

            input_path = request.get("input_path")
            if not isinstance(input_path, str) or not Path(input_path).is_file():
                raise ValueError("input_path must identify an existing local preview file")
            with Image.open(input_path) as source:
                image = ImageOps.exif_transpose(source).convert("RGB")
            inputs = processor(images=image, return_tensors="pt").to(device)
            features = model.get_image_features(**inputs)
        elif operation == "embed_text":
            text = request.get("text")
            if not isinstance(text, str) or not text.strip():
                raise ValueError("embed_text requires non-empty text")
            inputs = processor(text=[text.strip()], padding="max_length",
                               max_length=TEXT_LENGTH, truncation=True,
                               return_tensors="pt").to(device)
            features = model.get_text_features(**inputs)
        else:
            raise ValueError(f"unsupported operation {operation!r}; expected process or embed_text")
    vector = torch.nn.functional.normalize(features[0].float(), dim=-1).cpu().numpy()
    data = vector.astype("<f4").tobytes()
    assert len(data) == 4 * vector.shape[0] and np.isfinite(vector).all()
    return {"representation": None, "tiles": None, "observation": {
        "kind": "embedding", "model": MODEL_ID,
        "value": {"embedding": {"model": MODEL_ID, "dim": int(vector.shape[0]),
                                "vector_b64": base64.b64encode(data).decode("ascii")},
                  "revision": MODEL_REVISION, "device": device},
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


def serve(manifest, device, path, idle_seconds):
    path = Path(path)
    if path.exists():
        probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            probe.connect(str(path))
            print(f"siglip: server already listening on {path}", file=sys.stderr)
            return
        except OSError:
            path.unlink()
        finally:
            probe.close()
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    # Bind before loading so concurrent spawners see the socket and queue on it.
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
        print(f"siglip: serving {MODEL_ID} on {device} at {path}", file=sys.stderr)
        server.settimeout(idle_seconds)
        while True:
            try:
                conn, _ = server.accept()
            except socket.timeout:
                break
            with conn:
                conn.settimeout(30)
                try:
                    response = embed(json.loads(read_line(conn)), processor, model, device)
                except Exception as error:  # one bad request must not stop the server
                    response = {"error": f"{type(error).__name__}: {error}"}
                with contextlib.suppress(OSError):
                    conn.sendall(json.dumps(response).encode() + b"\n")
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
                        help="auto selects cuda, then mps, when available")
    parser.add_argument("--serve", action="store_true", help="persistent socket server")
    parser.add_argument("--socket", help="Unix socket path for --serve")
    parser.add_argument("--idle-seconds", type=int, default=900)
    args = parser.parse_args()
    os.environ["HF_HUB_DISABLE_TELEMETRY"] = "1"
    os.environ["TOKENIZERS_PARALLELISM"] = "false"
    if not args.prepare:
        os.environ["HF_HUB_OFFLINE"] = "1"
        os.environ["TRANSFORMERS_OFFLINE"] = "1"
    with contextlib.redirect_stdout(sys.stderr):
        if args.serve:
            if not args.socket:
                raise ValueError("--serve requires --socket")
            serve(args.manifest, args.device, args.socket, args.idle_seconds)
            return
        if args.prepare:
            result = prepare(args.manifest)
        else:
            request = json.load(sys.stdin)
            processor, model, device = loaded(args.manifest, args.device)
            result = embed(request, processor, model, device)
    print(json.dumps(result, ensure_ascii=False))


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"siglip: {type(error).__name__}: {error}", file=sys.stderr)
        sys.exit(1)
