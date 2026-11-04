#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
import asyncio
import json
from pathlib import Path
import tempfile
import unittest

try:
    from .nostr_load import udp_drop_commands, udp_restore_commands, all_connected, Recorder, Native
    from .nostr_load_report import analyze, parse_log, table, relay_series
except ImportError:
    from nostr_load import udp_drop_commands, udp_restore_commands, all_connected, Recorder, Native
    from nostr_load_report import analyze, parse_log, table, relay_series


class HarnessTests(unittest.TestCase):
    def test_udp_filter_is_namespace_only_and_preserves_tcp(self):
        commands = udp_drop_commands("mistlab-22")
        self.assertEqual(len(commands), 3)
        self.assertTrue(all(command[:5] == ("ip", "netns", "exec", "mistlab-22", "tc") for command in commands))
        for command in commands[1:]:
            self.assertEqual(command[-4:], ("ip_proto", "udp", "action", "drop"))
        with self.assertRaises(ValueError):
            udp_drop_commands("host")

    def test_s5_restores_only_its_private_udp_filter(self):
        commands = udp_restore_commands("mistlab-22")
        self.assertEqual(commands, [("ip", "netns", "exec", "mistlab-22", "tc", "qdisc", "del", "dev", "lo", "clsact")])
        with self.assertRaises(ValueError):
            udp_restore_commands("host")

    def test_s5_waits_for_every_node_and_tracks_disconnects(self):
        class Client:
            def __init__(self, peers):
                self.connected_peers = set(peers)
        a, b = Client(["b"]), Client([])
        self.assertFalse(all_connected([a, b]))
        b.connected_peers.add("a")
        self.assertTrue(all_connected([a, b]))
        a.connected_peers.clear()
        self.assertFalse(all_connected([a, b]))
        self.assertFalse(all_connected([]))

    def test_recorder_filters_sdp_and_non_measurement_logs(self):
        with tempfile.TemporaryDirectory() as temporary:
            recorder = Recorder(Path(temporary) / "run")
            recorder.log("node-00", "unrelated diagnostic with SDP")
            recorder.log("node-00", "[NostrStats] node=node-00 req=2")
            recorder.emit("node-00", "event", kind="connected", peer="node-01")
            recorder.close()
            rows = [json.loads(line) for line in (Path(temporary) / "run/events.jsonl").read_text().splitlines()]
            self.assertEqual(len(rows), 2)
            self.assertEqual(rows[1]["kind"], "connected")
            self.assertNotIn("SDP", (Path(temporary) / "run/measurements.log").read_text())

    def test_native_protocol_records_events_and_completes_requests(self):
        async def run():
            with tempfile.TemporaryDirectory() as temporary:
                recorder = Recorder(Path(temporary) / "run")
                client = Native("node-00", recorder)
                class Process:
                    stdout = asyncio.StreamReader()
                client.process = Process()
                pending = asyncio.get_running_loop().create_future()
                client.pending[1] = pending
                for value in ({"ready": {"profile": "release"}},
                              {"event": {"kind": "connected", "sender": "node-01", "room": "room", "data": []}},
                              {"id": 1, "value": None}):
                    client.process.stdout.feed_data((json.dumps(value) + "\n").encode())
                client.process.stdout.feed_eof()
                await client.output()
                self.assertEqual((await client.ready)["profile"], "release")
                self.assertEqual(await pending, {"id": 1, "value": None})
                recorder.close()
                rows = [json.loads(line) for line in (Path(temporary) / "run/events.jsonl").read_text().splitlines()]
                self.assertEqual(rows[1]["kind"], "connected")
        asyncio.run(run())

    def test_native_process_exit_fails_pending_requests(self):
        async def run():
            with tempfile.TemporaryDirectory() as temporary:
                recorder = Recorder(Path(temporary) / "run")
                client = Native("node-00", recorder)
                class Process:
                    stdout = asyncio.StreamReader()
                client.process = Process()
                pending = asyncio.get_running_loop().create_future()
                client.pending[1] = pending
                client.process.stdout.feed_eof()
                await client.output()
                for future in (client.ready, pending):
                    with self.assertRaises(RuntimeError):
                        await future
                recorder.close()
        asyncio.run(run())


class ReportTests(unittest.TestCase):
    def test_s5_recovery_and_policy_transition_counts(self):
        records = [
            {"t": 0, "source": "harness", "record": "manifest", "scenario": "S5"},
            {"t": 1, "source": "node-00", "record": "join"},
            {"t": 2, "source": "node-00", "record": "event", "kind": "connected"},
            {"t": 32, "source": "harness", "record": "udp_restore"},
            {"t": 35, "source": "node-00", "record": "log", "line": "[NostrGovernor] action=cooldown_enter relay=local rate=2"},
            {"t": 36, "source": "node-00", "record": "log", "line": "[IdentityRecovery] action=skip reason=isolation_recovery"},
            {"t": 37, "source": "harness", "record": "all_reconnected", "recovery_seconds": 5.125},
            {"t": 40, "source": "harness", "record": "measurement_end"},
            {"t": 41, "source": "harness", "record": "result", "success": True},
        ]
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "events.jsonl"
            path.write_text("\n".join(json.dumps(r) for r in records))
            rows, *_ = analyze(path)
        self.assertEqual(rows[0]["recovery_seconds"], 5.125)
        self.assertEqual(rows[0]["transitions"], {"NostrGovernor:cooldown_enter": 1, "IdentityRecovery:skip": 1})
        self.assertIn("S5 recovery(s)", table(rows))
        self.assertIn("5.125", table(rows))
        self.assertIn("IdentityRecovery:skip", table(rows))

    def test_metadata_unknown_and_quoted_rejection_reason(self):
        tag, fields = parse_log('10 LOG2 [NostrOK] event_id=id type=unknown accepted=false message="rate-limited: slow down"')
        self.assertEqual(tag, "NostrOK")
        self.assertEqual(fields["message"], '"rate-limited: slow down"')

    def test_per_second_counts_reset_and_failure_are_retained(self):
        rows = [
            {"t": 0, "source": "harness", "record": "manifest", "scenario": "S2"},
            {"t": 1, "source": "node-00", "record": "join"},
            {"t": 1.1, "source": "node-00", "record": "log", "line": "[NostrSend] type=offer event_id=1"},
            {"t": 1.2, "source": "node-00", "record": "log", "line": "[NostrSend] type=offer event_id=2"},
            {"t": 1.3, "source": "node-00", "record": "log", "line": "[NostrSend] type=req"},
            {"t": 1.4, "source": "node-00", "record": "log", "line": "[NostrOK] type=offer accepted=false"},
            {"t": 2, "source": "node-00", "record": "log", "line": "[ConnTiming] peer=node-01 kind=attempt_start"},
            {"t": 2.1, "source": "node-00", "record": "log", "line": "[NostrReset] reason=isolation_recovery"},
            {"t": 2.2, "source": "node-00", "record": "log", "line": "[IceCandidate] peer=node-01 pc_id=1 action=generated type=host"},
            {"t": 3, "source": "node-00", "record": "log", "line": "[NostrStats] targets=3 unanswered=2"},
            {"t": 4, "source": "harness", "record": "measurement_end"},
            {"t": 4.1, "source": "node-00", "record": "event", "kind": "connected"},
            {"t": 5, "source": "harness", "record": "failure", "error": "test failure"},
        ]
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "events.jsonl"
            path.write_text("\n".join(json.dumps(row) for row in rows))
            summary, series, candidates, _, _ = analyze(path)
        row = summary[0]
        self.assertTrue(row["failed"])
        self.assertFalse(row["connected"])
        self.assertEqual(row["events"], 2)
        self.assertEqual(row["peak_events_per_s"], 2)
        self.assertEqual(row["req"], 1)
        self.assertEqual(row["rejected_by_type"], {"offer": 1})
        self.assertEqual(row["resets"], 1)
        self.assertEqual(row["targets"], 3)
        self.assertEqual(row["events_per_s"], round(2/3, 6))
        self.assertEqual(candidates[0]["host"], 1)
        self.assertIn("S2", table(summary))
        self.assertEqual(series[0]["publish:offer"], 2)

    def test_cumulative_samples_recover_dropped_logs_without_inventing_peak_times(self):
        rows = [
            {"t": 0, "source": "node-00", "record": "join"},
            {"t": .2, "source": "node-00", "record": "log", "line": "[NostrSend] type=answer"},
            {"t": 1, "source": "node-00", "record": "log", "line": "[NostrStats] type=answer publish=3 accepted=2 rejected=1"},
            {"t": 1.2, "source": "node-00", "record": "log", "line": "[NostrSend] type=answer"},
            {"t": 2, "source": "harness", "record": "measurement_end"},
            {"t": 3, "source": "harness", "record": "result", "success": True},
        ]
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "events.jsonl"
            path.write_text("\n".join(json.dumps(row) for row in rows))
            summary, *_ = analyze(path)
        self.assertEqual(summary[0]["events"], 4)
        self.assertEqual(summary[0]["publish_logs_missing"], 2)
        self.assertEqual(summary[0]["peak_events_per_s"], 1)
        self.assertEqual(summary[0]["accepted_by_type"], {"answer": 2})
        self.assertEqual(summary[0]["rejected_by_type"], {"answer": 1})

    def test_relay_series_counts_copies_without_ffi_callback_loss(self):
        records = []
        for t, accepted, rejected, req in [(1, 5, 1, 2), (2, 8, 2, 4)]:
            records.append({"t": t, "record": "relay_stats", "stats": {
                "time_ms": 1000 * t, "final": False, "connections": {
                    "1": {"kinds": {"25050": {"accepted": accepted, "rejected": rejected}}, "req": req}}}})
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "events.jsonl"
            path.write_text("\n".join(json.dumps(row) for row in records))
            rows = relay_series(path)
        self.assertEqual([row["events"] for row in rows], [6, 4])
        self.assertEqual([row["req"] for row in rows], [2, 2])
        self.assertEqual(rows[1]["kinds"]["25050"], {"accepted": 3, "rejected": 1})

    def test_connection_time_is_relative_to_each_nodes_join(self):
        records = [
            {"t": 100, "source": "node-02", "record": "join"},
            {"t": 102, "source": "node-02", "record": "event", "kind": "connected"},
            {"t": 110, "source": "harness", "record": "measurement_end"},
            {"t": 111, "source": "harness", "record": "result", "success": True},
        ]
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "events.jsonl"
            path.write_text("\n".join(json.dumps(row) for row in records))
            rows, *_ = analyze(path)
        self.assertTrue(rows[0]["connected"])
        self.assertEqual(rows[0]["first_connected_s"], 2)
        self.assertEqual(rows[0]["seconds"], 10)
        self.assertFalse(rows[0]["failed"])


if __name__ == "__main__":
    unittest.main()
