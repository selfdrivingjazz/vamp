import json
from pathlib import Path
import socket
import tempfile
import unittest
from unittest.mock import patch

from vamp_usage import Usage


@unittest.skipUnless(hasattr(socket, "AF_UNIX"), "requires Unix sockets")
class UsageTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(dir="/tmp")
        self.addCleanup(temporary.cleanup)
        self.manifest = Path(temporary.name) / "weights.json"
        self.endpoint = self.manifest.with_name("vamp-usage.sock")
        environment = patch.dict("os.environ", clear=True)
        environment.start()
        self.addCleanup(environment.stop)

    def usage(self):
        return Usage(self.manifest, "provider", "provider/model", "cpu")

    def listener(self):
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        self.addCleanup(listener.close)
        listener.bind(str(self.endpoint))
        listener.setblocking(False)
        return listener

    def test_one_metadata_record_per_actual_batch(self):
        listener = self.listener()
        usage = self.usage()
        self.assertEqual(3, usage.run("test", "media-analysis", 3, len_text, text="xyz"))
        data = listener.recv(4096)
        self.assertTrue(data.endswith(b"\n"))
        row = json.loads(data)
        self.assertEqual({"at", "kind", "processor", "provider", "model", "device", "duration_ms", "items", "ok"},
                         row.keys())
        self.assertEqual(("media-analysis", "test", "provider", "provider/model", "cpu", 3, True),
                         tuple(row[k] for k in ("kind", "processor", "provider", "model", "device", "items", "ok")))
        self.assertGreaterEqual(row["duration_ms"], 0)
        self.assertNotIn(b"xyz", data)
        with self.assertRaises(BlockingIOError):
            listener.recv(4096)

    def test_error_logged_without_message_or_replacing_it(self):
        listener = self.listener()
        error = RuntimeError("private input path")

        def fail():
            raise error

        with self.assertRaises(RuntimeError) as thrown:
            self.usage().run("test", "query-embedding", 1, fail)
        self.assertIs(thrown.exception, error)
        data = listener.recv(4096)
        self.assertFalse(json.loads(data)["ok"])
        self.assertNotIn(b"private", data)
        with self.assertRaises(BlockingIOError):
            listener.recv(4096)

    def test_unconfigured_does_not_open_a_socket(self):
        with patch("vamp_usage.socket.socket") as factory:
            self.assertEqual(1, self.usage().run("test", "media-analysis", 1, lambda: 1))
        factory.assert_not_called()

    def test_explicit_opt_out_and_unreachable_listener_leave_result_unchanged(self):
        listener = self.listener()
        with patch.dict("os.environ", VAMP_USAGE_SOCKET=""):
            self.assertEqual(1, self.usage().run("test", "media-analysis", 1, lambda: 1))
        with self.assertRaises(BlockingIOError):
            listener.recv(4096)
        with patch.dict("os.environ", VAMP_USAGE_SOCKET=str(self.endpoint.with_suffix(".absent"))):
            self.assertEqual(2, self.usage().run("test", "media-analysis", 1, lambda: 2))

    def test_delivery_never_waits_for_listener(self):
        self.listener()
        with patch("vamp_usage.socket.socket") as factory:
            client = factory.return_value.__enter__.return_value
            client.sendto.side_effect = BlockingIOError
            self.assertEqual(1, self.usage().run("test", "media-analysis", 1, lambda: 1))
            self.assertEqual(["setblocking", "sendto"], [call[0] for call in client.method_calls])
            client.setblocking.assert_called_once_with(False)


def len_text(text):
    return len(text)
