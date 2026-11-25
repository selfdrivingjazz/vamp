"""Best-effort, content-free usage events from the actual model boundary."""
import json
import os
import socket
from datetime import datetime, timezone
from time import monotonic_ns


class Usage:
    def __init__(self, manifest, provider, model, device):
        endpoint = os.environ.get("VAMP_USAGE_SOCKET")
        if endpoint is None:
            # A local collector can opt in for all invocations of this installation,
            # including shell commands and persistent model servers.
            adjacent = manifest.with_name("vamp-usage.sock")
            try:
                endpoint = str(adjacent) if adjacent.is_socket() else None
            except OSError:
                endpoint = None
        self.endpoint = endpoint
        self.provider = provider
        self.model = model
        self.device = device

    def run(self, processor, kind, items, inference, /, **inputs):
        if not self.endpoint:
            return inference(**inputs)
        started = monotonic_ns()
        ok = False
        try:
            result = inference(**inputs)
            ok = True
            return result
        finally:
            row = {
                "at": datetime.now(timezone.utc).isoformat(),
                "processor": processor, "kind": kind, "provider": self.provider,
                "model": self.model, "device": self.device, "items": items,
                "duration_ms": (monotonic_ns() - started) // 1_000_000, "ok": ok,
            }
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as client:
                    client.setblocking(False)
                    client.sendto((json.dumps(row, separators=(",", ":")) + "\n").encode(), self.endpoint)
            except (AttributeError, OSError, ValueError):
                # Never wait for the collector, retry, or disturb model results.
                pass
