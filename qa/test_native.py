"""Exercise the ctypes size contract without loading a release library."""

import ctypes as C
from types import SimpleNamespace
import unittest

from native import digest, generated_bytes, storage_bytes


class StorageBufferTest(unittest.TestCase):
    def test_generated_payload_matches_browser_vectors(self):
        # The same fixed vectors are checked in clients.test.mjs.
        for seed, sha256 in [
            (0x19a50001, "bdf5642785b605326f142f265452b55f96a60027b461bf4a8e1632fe1bea34eb"),
            (0x19a50002, "d670e3563ed51ed082188c390d1beb0c98a06e6df1f9c4da19e229d6a4bd6f70"),
        ]:
            with self.subTest(seed=seed):
                size = 4 * 1024 * 1024 + 1
                self.assertEqual(digest(generated_bytes(seed, size)), {"sha256": sha256, "size": size})

    def test_multiblock_payload_is_sized_before_copying(self):
        data = b"\x00\x80\xff\x19" * (1024 * 1024) + b"\x01"
        capacities = []

        def get(cid, cid_length, buffer, capacity):
            self.assertEqual(cid[:cid_length], b"cid")
            capacities.append(capacity)
            if capacity >= len(data):
                C.memmove(buffer, data, len(data))
            return len(data)

        fetched = storage_bytes(SimpleNamespace(storage_get=get), b"cid")
        self.assertEqual(capacities, [0, len(data)])
        self.assertEqual(fetched, data)
        self.assertEqual(digest(fetched)["size"], 4 * 1024 * 1024 + 1)

    def test_failure_and_changed_length_are_errors(self):
        for lengths in ([0], [10, 0], [10, 9], [10, 11]):
            with self.subTest(lengths=lengths):
                responses = iter(lengths)
                lib = SimpleNamespace(storage_get=lambda *_: next(responses))
                with self.assertRaises(ValueError):
                    storage_bytes(lib, b"cid")


class ConcurrentGetTest(unittest.TestCase):
    def test_native_echo_and_commands_remain_live_during_blocked_get(self):
        """A blocking fake FFI get must not block stdin dispatch or native echo."""
        import io
        import json
        import threading
        import time
        from unittest.mock import patch
        import native

        entered, release, echoed = threading.Event(), threading.Event(), threading.Event()
        payload = b"verified"
        replies = []
        callback = None
        config = {"room": "room", "target": "peer", "token": "test"}

        class Function:
            def __init__(self, fn=lambda *_: None):
                self.fn = fn

            def __call__(self, *args):
                return self.fn(*args)

        def copy_json(value, buffer, capacity):
            data = json.dumps(value).encode()
            C.memmove(buffer, data, min(len(data), capacity))
            return len(data)

        def register(fn):
            nonlocal callback
            callback = fn

        def get(cid, length, buffer, capacity):
            entered.set()
            if not release.wait(5):
                raise RuntimeError("test get was never released")
            if capacity:
                C.memmove(buffer, payload, len(payload))
            return len(payload)

        def send(room, room_len, target, target_len, data, data_len, delivery):
            if data[:data_len] == b"mistlib-storage-control:test:pong:7":
                echoed.set()

        functions = {
            "register_event_callback_v2": register,
            "get_build_info": lambda b, n: copy_json({"version": "unit"}, b, n),
            "get_version": lambda b, n: (C.memmove(b, b"unit", 4), 4)[1],
            "get_stats": lambda b, n: copy_json({"responsive": True}, b, n),
            "storage_get": get, "send_message_in_room": send,
        }
        lib = SimpleNamespace(**{name: Function(fn) for name, fn in functions.items()})
        for name in ("init_with_config", "join_room", "leave_room_id", "leave_all_rooms",
                     "update_position_in_room", "storage_add"):
            setattr(lib, name, Function())

        def commands():
            yield json.dumps({"id": 1, "op": "control-config", **config}) + "\n"
            yield json.dumps({"id": 4, "op": "telemetry-start"}) + "\n"
            yield json.dumps({"id": 2, "op": "get-bench", "cid": "cid", "expected": native.digest(payload)}) + "\n"
            self.assertTrue(entered.wait(2), "get worker did not start")
            ping = b"mistlib-storage-control:test:ping:7"
            callback(0, b"room", 4, b"peer", 4, ping, len(ping))
            yield json.dumps({"id": 3, "op": "stats"}) + "\n"
            self.assertTrue(any(row.get("id") == 3 for row in replies), "stdin blocked behind get")
            self.assertFalse(any(row.get("id") == 2 for row in replies))
            self.assertTrue(echoed.wait(2), "native did not echo while get blocked")
            yield json.dumps({"id": 5, "op": "telemetry-stop"}) + "\n"
            series = next(row["value"] for row in replies if row.get("id") == 5)
            self.assertGreaterEqual(len(series), 2)
            self.assertTrue(all(point["responsive"] for point in series))
            self.assertFalse(any(row.get("id") == 2 for row in replies), "sampling waited for get")
            release.set()
            deadline = time.monotonic() + 2
            while not any(row.get("id") == 2 for row in replies):
                if time.monotonic() >= deadline:
                    self.fail("get worker never emitted its result")
                time.sleep(0.01)

        class Output(io.StringIO):
            def write(self, text):
                if text.strip():
                    replies.append(json.loads(text))
                return len(text)

        with patch.object(native.C, "CDLL", return_value=lib), \
                patch.object(native.sys, "argv", ["native.py", "fake"]), \
                patch.object(native.sys, "stdin", commands()), \
                patch.object(native.sys, "stdout", Output()):
            try:
                native.main()
            finally:
                release.set()
        result = next(row for row in replies if row.get("id") == 2)
        self.assertNotIn("error", result)
        self.assertEqual(result["value"]["sha256"], native.digest(payload)["sha256"])
        self.assertGreater(result["value"]["completionMs"], 0)


class ControlMetricsTest(unittest.TestCase):
    def test_pong_identity_duplicate_censoring_and_missing_samples(self):
        from unittest.mock import patch
        from native import ControlProbe
        control = ControlProbe(lambda *_: None)
        # Avoid launching the timer worker: populate a deterministic probe state.
        control.config = ("room", "peer", "mistlib-storage-control:test:")
        with patch("native.time.monotonic", return_value=1):
            control.start()
        control.pending = {"1": 1, "2": 1}
        with patch("native.time.monotonic", return_value=1.025):
            self.assertFalse(control.receive("wrong-room", "peer", b"mistlib-storage-control:test:pong:1"))
            self.assertFalse(control.receive("room", "wrong-peer", b"mistlib-storage-control:test:pong:1"))
            control.receive("room", "peer", b"mistlib-storage-control:test:pong:1")
            control.receive("room", "peer", b"mistlib-storage-control:test:pong:1")
        result = control.stop()
        self.assertEqual(result["controlSamples"], 1)
        self.assertAlmostEqual(result["controlRttP95Ms"], 25)
        self.assertEqual(result["controlTimeouts"], 1)
        self.assertEqual(result["controlCensored"], 1)
        self.assertEqual(control.stop(), result)
        control.start()
        empty = control.stop()
        self.assertIsNone(empty["controlRttP95Ms"])
        self.assertEqual(empty["controlSamples"], 0)
        control.close()
