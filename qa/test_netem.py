"""Run only a fake tc executable; these unit tests never shape the host."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


@unittest.skipUnless(shutil.which("bash"), "bash is required")
class NetemTest(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.log = self.root / "commands"
        tc = self.root / "tc"
        tc.write_text('#!/usr/bin/env bash\n'
                      'echo "$*" >> "$TC_LOG"\n'
                      'if [[ "$*" == *"${SIGNAL_TC:-never-match}"* ]]; then kill -TERM "$PPID"; fi\n'
                      '[[ "$*" != *"${FAIL_TC:-never-match}"* ]]\n')
        tc.chmod(0o755)

    def run_script(self, *args, fail="never-match", signal="never-match"):
        return subprocess.run(["bash", str(Path(__file__).with_name("netem.sh")), *args],
                              env={**os.environ, "PATH": str(self.root) + os.pathsep + os.environ["PATH"],
                                   "TC_LOG": str(self.log), "FAIL_TC": fail, "SIGNAL_TC": signal},
                              capture_output=True, text=True, timeout=5)

    def test_udp_only_filters_and_parameterized_netem(self):
        self.assertEqual(self.run_script("on", "150", "1000").returncode, 0)
        commands = self.log.read_text().splitlines()
        self.assertEqual(len(commands), 4)
        self.assertIn("prio bands 3 priomap " + " ".join(["0"] * 16), commands[0])
        self.assertIn("parent 1:3 handle 30: netem delay 150ms rate 1000kbit", commands[1])
        self.assertNotIn(" limit ", commands[1])
        self.assertIn("protocol ip pref 1 u32 match ip protocol 17 0xff flowid 1:3", commands[2])
        self.assertIn("protocol ipv6 pref 2 u32 match ip6 protocol 17 0xff flowid 1:3", commands[3])

    def test_explicit_packet_limit(self):
        self.assertEqual(self.run_script("on", "150", "1000", "53").returncode, 0)
        commands = self.log.read_text().splitlines()
        self.assertEqual(commands[1], "qdisc add dev lo parent 1:3 handle 30: netem delay 150ms rate 1000kbit limit 53")

    def test_random_and_burst_loss(self):
        for loss in [("random", "1%"), ("gemodel", "0.1%", "10%", "100%", "0%")]:
            self.assertEqual(self.run_script("on", "50", "10000", "313", "loss", *loss).returncode, 0)
            self.assertIn("netem delay 50ms rate 10000kbit limit 313 loss " + " ".join(loss),
                          self.log.read_text())

    def test_failed_root_add_does_not_delete_an_existing_qdisc(self):
        self.assertNotEqual(self.run_script("on", fail="qdisc add dev lo root").returncode, 0)
        self.assertNotIn("qdisc del", self.log.read_text())

    def test_invalid_loss_does_not_touch_qdiscs(self):
        for loss in [("random", "101%"), ("random", "-1%"), ("random", "1"),
                     ("random", "1%", "extra"), ("gemodel", "1%"),
                     ("gemodel", "0.1%", "10%", "100%", "bad"), ("bogus", "1%")]:
            self.assertNotEqual(self.run_script("on", "50", "10000", "313", "loss", *loss).returncode, 0)
        self.assertFalse(self.log.exists())

    def test_partial_setup_is_removed_and_off_is_idempotent(self):
        self.assertNotEqual(self.run_script("on", fail="netem delay").returncode, 0)
        self.assertEqual(self.log.read_text().splitlines()[-1], "qdisc del dev lo root")
        for _ in range(2):
            self.assertEqual(self.run_script("off", fail="qdisc del").returncode, 0)

    def test_signal_during_setup_removes_the_owned_root(self):
        self.assertEqual(self.run_script("on", signal="netem delay").returncode, 143)
        self.assertEqual(self.log.read_text().splitlines()[-1], "qdisc del dev lo root")

    def test_bad_arguments_do_not_change_qdiscs(self):
        for args in [("unknown",), ("on", "-1"), ("on", "150", "0"),
                     *(("on", "150", "1000", limit) for limit in ("0", "-1", "1.5", "abc", "", "53 extra")),
                     ("on", "150", "1000", "53", "extra")]:
            self.assertNotEqual(self.run_script(*args).returncode, 0)
        self.assertFalse(self.log.exists())
