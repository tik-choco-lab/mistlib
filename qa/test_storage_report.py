"""Research summaries retain failures and pair only matching seeds/directions."""

import contextlib
import io
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import storage_ab
from storage_report import summarise, print_series


def row(label, success=True, seed=10, repetition=1, completion=100):
    return {"schema": 1, "label": label, "pair": "chromium-chromium", "direction": "chromium->chromium",
            "order": "forward", "profile": "clean", "size": 1024, "seed": seed, "repetition": repetition,
            "success": success, "completionMs": completion if success else None,
            "goodputBps": 81920 if success else 0, "controlRttP95Ms": None,
            "controlSamples": None, "controlTimeouts": None}


class StorageReportTest(unittest.TestCase):
    def test_failures_and_missing_metrics_are_retained(self):
        summary = summarise([row("A"), row("A", False)])[0]
        self.assertEqual(summary["successes"], 1)
        self.assertEqual(summary["attempts"], 2)
        self.assertEqual(summary["medianCompletionMs"], 100)
        self.assertEqual(summary["medianGoodputBps"], 40960)
        self.assertIsNone(summary["medianControlRttP95Ms"])
        self.assertEqual(summary["controlMissingAttempts"], 2)

    def test_telemetry_summary_and_ascii_timeline(self):
        value = row("telemetry", False)
        value.update(id="run", partialBytes=500, maxNoProgressMs=1000,
                     duplicateBytes=20, steadyGoodputBps=4000, metricsComplete=True)
        summary = summarise([value])[0]
        self.assertEqual(summary["medianSteadyGoodputBps"], 4000)
        self.assertEqual(summary["medianPartialBytes"], 500)
        self.assertEqual(summary["metricsComplete"], 1)
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "run.series.json"
            value["seriesPath"] = str(path)
            def point(t, count):
                return {"t": t, "rx": {"storage": {"acceptedBytes": count},
                        "rtc": [{"currentRoundTripTime": 0.025}]},
                        "tx": {"nodes": [{"transport": {"reliableBufferedAmount": 123}}]}}
            path.write_text(json.dumps({"series": [point(0, 0), point(1000, 500)]}))
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                print_series(value)
            self.assertIn("rx bytes/s", output.getvalue())
            self.assertIn("500.0", output.getvalue())
            self.assertIn("123.0", output.getvalue())
            self.assertIn("25.0", output.getvalue())

    def test_pairing_requires_same_seed_and_retains_failed_pairs(self):
        rows = [row("A"), row("B", completion=120), row("A", False, 11), row("B", seed=11), row("A", seed=12)]
        pairs = storage_ab.paired_summary(rows, "A", "B")
        self.assertEqual(pairs[0]["pairs"], 2)
        self.assertEqual(pairs[0]["bothSuccessful"], 1)
        self.assertEqual(pairs[0]["medianCompletionDeltaMs_BminusA"], 20)
        self.assertIsNone(pairs[0]["medianControlRttP95DeltaMs_BminusA"])

    def test_ab_alternates_order_shares_seeds_and_builds_once(self):
        with tempfile.TemporaryDirectory() as temp:
            out = Path(temp) / "attempts.jsonl"
            plan = [{"pair": "chromium-chromium", "profile": "clean", "size": 1024,
                     "seed": r * 10, "repetition": r, "order": "forward", "direction": "chromium->chromium"}
                    for r in (1, 2)]
            calls = []

            def run(command, **kwargs):
                calls.append(command)
                if "--prepare-only" in command:
                    return subprocess.CompletedProcess(command, 0)
                def arg(key):
                    return command[command.index(key) + 1]
                attempt = row(arg("--label"), seed=int(arg("--seed")), repetition=int(arg("--repetition")))
                with out.open("a") as stream:
                    stream.write(json.dumps(attempt) + "\n")
                return subprocess.CompletedProcess(command, 0, json.dumps(attempt) + "\n")

            with patch.object(storage_ab.sys, "argv", ["storage_ab.py", "--out", str(out)]), \
                    patch.object(storage_ab.subprocess, "check_output", return_value=json.dumps(plan)), \
                    patch.object(storage_ab.subprocess, "run", side_effect=run), \
                    contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(storage_ab.main(), 0)
            self.assertEqual(sum("--prepare-only" in command for command in calls), 1)
            attempts = [json.loads(line) for line in out.read_text().splitlines()]
            self.assertEqual([attempt["label"] for attempt in attempts], ["A", "B", "B", "A"])
            self.assertEqual([attempt["seed"] for attempt in attempts], [10, 10, 20, 20])
            self.assertEqual(len(json.loads(out.with_suffix(".summary.json").read_text())["paired"]), 1)
