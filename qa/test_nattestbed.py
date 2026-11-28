import asyncio
import json
import struct
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

try:
    from . import nattestbed as nat
except ImportError:
    import nattestbed as nat


class NftRulesTests(unittest.TestCase):
    def test_no_nat(self):
        self.assertEqual(nat.nft_rules("a", "none"), "")
        self.assertEqual(nat.nft_rules("b", "none"), "")
        self.assertEqual(nat.Side("a", "none").peer_ip, "198.18.2.2")
        self.assertEqual(nat.Side("b", "none").peer_ip, "198.18.3.2")

    def test_port_preserving_and_filtering(self):
        for role, address in (("a", "198.19.1.2"), ("b", "198.19.2.2")):
            rules = nat.nft_rules(role, "eim")
            self.assertIn(f'oifname "n{role}-wan" ip saddr {address} masquerade\n', rules)
            self.assertIn("ct state established,related accept", rules)
            self.assertIn("policy drop", rules)
            self.assertIn(f'iifname "n{role}-wan" ip daddr 198.18.{int(role == "b")}.2 ip protocol udp drop', rules)
            self.assertNotIn("fully-random", rules)
            self.assertNotIn("dnat to", rules)

    def test_symmetric(self):
        rules = nat.nft_rules("b", "symmetric")
        self.assertIn('oifname "nb-wan" ip saddr 198.19.2.2 masquerade fully-random', rules)
        self.assertNotIn("dnat to", rules)

    def test_forward(self):
        rules = nat.nft_rules("a", "forward", 45678)
        self.assertIn('iifname "na-wan" ip daddr 198.18.0.2 udp dport 45678 dnat to 198.19.1.2:45678', rules)
        self.assertIn("ct status dnat accept", rules)
        self.assertNotIn("fully-random", rules)

    def test_invalid_rules(self):
        for args in (("wan", "eim"), ("a", "unknown"), ("a", "forward", 0), ("a", "forward", 65536)):
            with self.subTest(args=args), self.assertRaises(ValueError):
                nat.nft_rules(*args)

    def test_directional_udp_policer_rules(self):
        ab = {"rateKbit": 2000, "burstBytes": 32768}
        ba = {"rateKbit": 123, "burstBytes": 1500}
        state = {"policerAB": ab, "policerBA": ba}
        rules = nat.policer_rules(state, initial=True)
        self.assertIn("type filter hook forward priority filter; policy accept", rules)
        self.assertIn('add rule ip mistnat_policer ab iifname "wan-na" oifname "wan-nb" ip protocol udp '
                      'limit rate over 250000 bytes/second burst 32768 bytes counter drop', rules)
        self.assertIn('add rule ip mistnat_policer ba iifname "wan-nb" oifname "wan-na" ip protocol udp '
                      'limit rate over 15375 bytes/second burst 1500 bytes counter drop', rules)
        self.assertNotIn("hook input", rules)
        # Update/reset just the changed token bucket.
        self.assertEqual(nat.policer_rules({"policerAB": None, "policerBA": ba}, directions=["ab"]),
                         "flush chain ip mistnat_policer ab\n")
        self.assertIn("flush chain ip mistnat_policer ba\nadd rule", nat.policer_rules(state, directions=["ba"]))


class JobTests(unittest.TestCase):
    def setUp(self):
        self.data = {"natA": "eim", "natB": "forward", "a": ["echo", "a b"], "b": ["true"]}

    def job(self, **fields):
        return nat.parse_job(json.dumps({**self.data, **fields}))

    def test_defaults_and_blank_line(self):
        self.assertIsNone(nat.parse_job("  \n"))
        self.assertEqual(self.job(), nat.Job("eim", "forward", ("echo", "a b"), ("true",)))
        self.assertEqual(nat.parse_job(json.dumps(self.data), cpus=4).cpus, 4)

    def test_overrides(self):
        self.assertEqual(self.job(netem="delay 20ms loss 1%", timeout=0.5, cpus=3, forwardPort=50505),
                         nat.Job("eim", "forward", ("echo", "a b"), ("true",), "delay 20ms loss 1%", 3, 0.5, 50505))

    def test_keeps_literal_argv(self):
        self.assertEqual(self.job(a=["echo", "x > y", "$HOME", ""]).a, ("echo", "x > y", "$HOME", ""))

    def test_wan_and_pairs(self):
        job = self.job(wan=["relay", "--port", "7777"], wanReady={"tcpPort": 7777, "timeoutS": 10},
                       pairs=2, b=[["bbr"], ["cubic", "x y"]], pairStartOffsetsS=[0.25, 0])
        self.assertEqual(job.wan_commands(), (("relay", "--port", "7777"),))
        self.assertEqual(job.wanReady, {"tcpPort": 7777, "timeoutS": 10})
        self.assertEqual(job.peer_commands("a"), (("echo", "a b"),) * 2)
        self.assertEqual(job.peer_commands("b"), (("bbr",), ("cubic", "x y")))
        self.assertEqual(job.pairStartOffsetsS, [0.25, 0])
        self.assertEqual(self.job(wan=[["first"], ["second", ""]]).wan_commands(),
                         (("first",), ("second", "")))
        self.assertEqual(self.job(wan=["relay"], wanReady={"tcpPort": 7777}).wanReady,
                         {"tcpPort": 7777})
        self.assertEqual(self.job().wan_commands(), ())
        self.assertEqual(self.job().pairs, 1)
        self.assertIsNone(self.job().pairStartOffsetsS)

    def test_invalid_wan_and_pairs(self):
        for fields in ({"wan": "relay"}, {"wan": None}, {"wan": [""]}, {"wan": [["relay"], []]},
                       {"wan": ["relay", ["other"]]}, {"wanReady": {"tcpPort": 7777}},
                       *({"wan": ["relay"], "wanReady": ready} for ready in (
                           {}, 1, {"tcpPort": 0}, {"tcpPort": 65536}, {"tcpPort": True},
                           {"tcpPort": 7777, "timeoutS": 0}, {"tcpPort": 7777, "timeoutS": float("nan")},
                           {"tcpPort": 7777, "unknown": 1})),
                       {"pairs": 0}, {"pairs": True}, {"pairs": 1.5}, {"pairs": 2, "a": [["a"]]},
                       {"pairs": 2, "b": [["b"], [""]]}, {"a": [["a"], ["b"]]},
                       {"pairs": 2, "pairStartOffsetsS": [0]}, {"pairStartOffsetsS": [-1]},
                       {"pairStartOffsetsS": [True]}, {"pairStartOffsetsS": [float("inf")]},
                       {"pairStartOffsetsS": "0"}):
            with self.subTest(fields=fields), self.assertRaises(ValueError):
                self.job(**fields)

    def test_invalid_jobs(self):
        for fields in ({"natA": "fullcone"}, {"natB": None}, {"netem": 123}, {"netem": "'"},
                       {"netem": " "}, {"cpus": 0}, {"cpus": True}, {"timeout": 0}, {"timeout": float("inf")},
                       {"timeout": float("nan")}, {"forwardPort": 65536}, {"forwardPort": "80"},
                       {"a": "true"}, {"a": []}, {"a": [""]}, {"b": ["echo", 3]}, {"b": ["echo\0"]},
                       {"unexpected": "field"}):
            with self.subTest(fields=fields), self.assertRaises(ValueError):
                self.job(**fields)
        for line in ("[]", "null", "{}", "{bad"):
            with self.subTest(line=line), self.assertRaises(ValueError):
                nat.parse_job(line)

    def test_profiles_and_overrides(self):
        profiles = nat.load_profiles()
        self.assertEqual({name for name in profiles if not name.startswith("_")},
                         {"lan", "lan-bn", "domestic", "intl-good", "intl-typical", "intl-peak",
                          "intl-peak-tloss", "intl-severe", "intl-asym", "intl-shift"})
        for name in profiles:
            if not name.startswith("_"):
                with self.subTest(name=name):
                    self.assertEqual(self.job(profile=name).profile, name)
        peak = self.job(profile="intl-peak", pathAB="delay 99ms", bottleneckAB=None,
                        jitterNoReorder=True)
        self.assertEqual(peak.pathAB, "delay 99ms")
        self.assertEqual(peak.pathBA, profiles["intl-peak"]["pathBA"])
        self.assertIsNone(peak.bottleneckAB)
        self.assertEqual(peak.bottleneckBA, {"rateKbit": 10000, "queueMs": 1000})
        self.assertTrue(peak.jitterNoReorder)
        self.assertEqual(self.job(profile="intl-shift", schedule=[]).schedule, [])
        self.assertEqual(self.job(profile="intl-severe", policerAB=None).policerBA,
                         {"rateKbit": 2000, "burstBytes": 32768})
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "profiles.json"
            path.write_text(json.dumps({"_comment": "test", "local": {"pathAB": "delay 2ms"}}))
            job = nat.parse_job(json.dumps({**self.data, "profile": "local"}), profiles=nat.load_profiles(path))
            self.assertEqual(job.pathAB, "delay 2ms")
        for name in ("unknown", "_comment", None, 1):
            with self.subTest(name=name), self.assertRaises(ValueError):
                self.job(profile=name)

    def test_gilbert_elliott_mean_loss(self):
        for name, expected in (("intl-typical", 0.01), ("intl-peak", 0.05), ("intl-severe", 0.125)):
            job = self.job(profile=name)
            for path in (job.pathAB, job.pathBA):
                words = path.split()
                idx = words.index("gemodel")
                p, r, bad, good = (float(value.rstrip("%")) / 100 for value in words[idx+1:idx+5])
                mean_loss = (p * bad + r * good) / (p + r)
                self.assertAlmostEqual(mean_loss, expected, places=8)
                self.assertAlmostEqual(1 / r, 5)

    def test_rate_and_queue(self):
        self.assertEqual(nat.path_arguments("delay 75ms 25ms", {"rateKbit": 10000, "queueMs": 200}),
                         ["delay", "75ms", "25ms", "rate", "10000kbit", "limit", "167"])
        self.assertEqual(nat.path_arguments(None, {"rateKbit": 10000, "limitPackets": 500}),
                         ["rate", "10000kbit", "limit", "500"])
        self.assertEqual(nat.path_arguments("delay 2ms rate 2mbit limit 60", None),
                         ["delay", "2ms", "rate", "2mbit", "limit", "60"])
        self.assertEqual(nat.queue_limit({"rateKbit": 10000, "queueMs": 1000}), 834)
        self.assertEqual(nat.path_arguments("delay 25ms 3ms", None, no_reorder=True),
                         ["delay", "25ms", "3ms", "rate", "100gbit"])
        self.assertEqual(nat.path_arguments("lat 8ms drop 0.1% rat 2mbit lim 60", None),
                         ["delay", "8ms", "loss", "0.1%", "rate", "2mbit", "limit", "60"])

    def test_legacy_netem_exclusion(self):
        self.assertEqual(self.job(netem="delay 5ms").netem, "delay 5ms")
        for fields in ({"pathAB": "delay 5ms"}, {"pathBA": "delay 5ms"},
                       {"bottleneckAB": {"rateKbit": 10000}}, {"jitterNoReorder": True},
                       {"schedule": [{"atS": 1, "pathBA": "delay 5ms"}]},
                       {"profile": "domestic"}):
            with self.subTest(fields=fields), self.assertRaisesRegex(ValueError, "netem cannot"):
                self.job(netem="delay 5ms", **fields)

    def test_schedule_parsing_and_inheritance(self):
        events = [{"atS": 0, "pathAB": "delay 1ms"},
                  {"atS": 2.5, "bottleneckAB": {"rateKbit": 1000, "limitPackets": 50}},
                  {"atS": 3, "pathBA": "loss gemodel 1% 20% 100% 0%",
                   "policerBA": {"rateKbit": 2000, "burstBytes": 32768}},
                  {"atS": 4, "bottleneckAB": None, "policerBA": None}]
        job = self.job(schedule=events)
        self.assertEqual(job.schedule, events)
        self.assertEqual(job.managed_paths(), {"ab", "ba"})
        self.assertTrue(nat.needs_policer(job))
        for bad in (None, {}, [{"atS": 0}], [{"atS": -1, "pathAB": None}],
                    [{"atS": float("nan"), "pathAB": None}], [{"atS": True, "pathAB": None}],
                    [{"atS": 1, "pathAB": None}, {"atS": 1, "pathBA": None}],
                    [{"atS": 2, "pathAB": None}, {"atS": 1, "pathBA": None}],
                    [{"atS": 1, "pathAB": "", "wat": 1}],
                    [{"atS": 1, "pathAB": "delay 5ms rate 1mbit", "bottleneckAB": {"rateKbit": 2000}}]):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                self.job(schedule=bad)
        with self.assertRaisesRegex(ValueError, "distribution"):
            self.job(pathAB="delay 10ms 2ms distribution normal", schedule=[{"atS": 1, "pathAB": "delay 5ms"}])

    def test_invalid_impairments(self):
        entry = {"dir": "ab", "proto": "tcp", "cc": "cubic", "flows": 1, "startS": 0, "durationS": 1}
        self.assertEqual(self.job(cross=[entry]).cross, [entry])
        for fields in ({"pathAB": 1}, {"pathBA": ""}, {"jitterNoReorder": 1},
                       {"pathAB": "delay 10ms reorder 1%", "jitterNoReorder": True},
                       {"pathAB": "delay 10ms reor 1%", "jitterNoReorder": True},
                       {"pathAB": "delay 10ms 1ms rate 0bit", "jitterNoReorder": True},
                       {"bottleneckAB": {"rateKbit": 0}}, {"bottleneckBA": {"rateKbit": True}},
                       {"bottleneckAB": {"rateKbit": 1000, "queueMs": 10, "limitPackets": 2}},
                       {"bottleneckAB": {"rateKbit": 1000, "queueMs": float("inf")}},
                       {"policerAB": {"rateKbit": 2000}}, {"policerBA": {"rateKbit": 2000, "burstBytes": 0}},
                       {"cross": None}, {"cross": [{**entry, "dir": "aa"}]},
                       {"cross": [{**entry, "proto": "udp"}]}, {"cross": [{**entry, "cc": "reno"}]},
                       {"cross": [{**entry, "flows": True}]}, {"cross": [{**entry, "startS": -1}]},
                       {"cross": [{**entry, "durationS": 0}]}):
            with self.subTest(fields=fields), self.assertRaises(ValueError):
                self.job(**fields)


class LossTimeTests(unittest.TestCase):
    config = {"rate": 0.05, "meanBurstMs": 25}

    def setUp(self):
        self.data = {"natA": "none", "natB": "none", "a": ["true"], "b": ["true"]}

    def job(self, **fields):
        return nat.parse_job(json.dumps({**self.data, **fields}))

    def periods(self, seed, count, config=None):
        generator = nat.loss_time_periods(config or self.config, seed)
        return [next(generator) for _ in range(count)]

    def test_schedule_statistics_with_fixed_seed(self):
        periods = self.periods(7, 80000)
        self.assertTrue(all(a[0] != b[0] for a, b in zip(periods, periods[1:])), "states must alternate")
        bad = [length for state, length in periods if state]
        good = [length for state, length in periods if not state]
        fraction = sum(bad) / (sum(bad) + sum(good))
        # ~40000 cycles: sampling sigma of the fraction ~3.4e-4, of the mean ~0.5 %.
        self.assertAlmostEqual(fraction, 0.05, delta=0.002)
        self.assertAlmostEqual(nat.statistics.mean(bad), 25, delta=25 * 0.03)
        self.assertAlmostEqual(nat.statistics.mean(good), 25 * 0.95 / 0.05, delta=475 * 0.03)
        for values in (bad, good):  # exponential: coefficient of variation 1
            self.assertAlmostEqual(nat.statistics.pstdev(values) / nat.statistics.mean(values), 1, delta=0.05)
        intervals = nat.loss_time_schedule(self.config, 7, 2e7)
        stats = nat.interval_stats(intervals, 0, 2e7)
        self.assertAlmostEqual(stats["badFraction"], 0.05, delta=0.002)
        self.assertAlmostEqual(stats["meanBad"], 25, delta=0.75)
        self.assertTrue(all(0 <= s < e <= 2e7 for s, e in intervals))

    def test_initial_state_is_stationary(self):
        first_bad = sum(self.periods(seed, 1)[0][0] for seed in range(4000))
        self.assertAlmostEqual(first_bad / 4000, 0.05, delta=0.015)

    def test_schedule_is_reproducible(self):
        self.assertEqual(self.periods(42, 200), self.periods(42, 200))
        self.assertEqual(nat.loss_time_schedule(self.config, 42, 10000),
                         nat.loss_time_schedule(self.config, 42, 10000))
        self.assertNotEqual(self.periods(42, 200), self.periods(43, 200))
        # Fixed golden prefix guards against accidental generator changes,
        # which would silently change every recorded seed's schedule.
        self.assertEqual([(state, round(length, 6)) for state, length in self.periods(1, 3)],
                         [(False, 893.074226), (True, 36.074223), (False, 139.870266)])

    def test_representative_seed(self):
        seed = nat.representative_seed(self.config, 10)
        self.assertEqual(seed, nat.representative_seed(self.config, 10))
        self.assertGreater(nat.representative_seed(self.config, 10, seed + 1), seed)
        intervals = nat.loss_time_schedule(self.config, seed, 12000)
        for offset in (0, 500, 1000, 1500, 2000):
            stats = nat.interval_stats(intervals, offset, offset + 10000)
            self.assertAlmostEqual(stats["badFraction"], 0.05, delta=0.005)
            self.assertAlmostEqual(stats["meanBad"], 25, delta=25 * 0.15)

    def test_validation(self):
        for value in (self.config, {**self.config, "seed": 0}, {**self.config, "seed": 2 ** 64 - 1},
                      {"rate": 0.5, "meanBurstMs": 1}, {"rate": 0.999, "meanBurstMs": 1e6}):
            with self.subTest(value=value):
                nat.validate_loss_time(value, "lossTimeAB")
        for value in ({}, [], 0.05, {"rate": 0.05}, {"meanBurstMs": 25},
                      {**self.config, "seed": -1}, {**self.config, "seed": 2 ** 64},
                      {**self.config, "seed": 1.0}, {**self.config, "seed": True},
                      {**self.config, "seed": "1"}, {**self.config, "extra": 1},
                      {"rate": 0, "meanBurstMs": 25}, {"rate": 1, "meanBurstMs": 25},
                      {"rate": -0.1, "meanBurstMs": 25}, {"rate": True, "meanBurstMs": 25},
                      {"rate": "0.05", "meanBurstMs": 25}, {"rate": float("nan"), "meanBurstMs": 25},
                      {"rate": 0.05, "meanBurstMs": 0}, {"rate": 0.05, "meanBurstMs": 0.5},
                      {"rate": 0.05, "meanBurstMs": float("inf")}, {"rate": 0.05, "meanBurstMs": -25},
                      {"rate": 0.05, "meanBurstMs": None}):
            with self.subTest(value=value):
                with self.assertRaises(ValueError):
                    nat.validate_loss_time(value, "lossTimeAB")
                with self.assertRaises(ValueError):
                    self.job(lossTimeBA=value)
        with self.assertRaises(ValueError):
            next(nat.loss_time_periods({"rate": 1, "meanBurstMs": 25}, 1))

    def test_job_fields_and_exclusions(self):
        job = self.job(lossTimeAB=self.config)
        self.assertEqual(job.lossTimeAB, self.config)
        self.assertIsNone(job.lossTimeBA)
        self.assertEqual(job.loss_time_directions(), ["ab"])
        self.assertEqual(self.job().loss_time_directions(), [])
        with self.assertRaisesRegex(ValueError, "netem cannot"):
            self.job(netem="delay 5ms", lossTimeBA=self.config)
        with self.assertRaises(ValueError):  # Static for the whole job, not schedulable.
            self.job(schedule=[{"atS": 1, "lossTimeAB": self.config}])

    def test_profile_parsing(self):
        profiles = nat.load_profiles()
        tloss, peak = self.job(profile="intl-peak-tloss"), self.job(profile="intl-peak")
        for direction in ("AB", "BA"):
            path = getattr(tloss, "path" + direction)
            self.assertNotIn("loss", path)
            self.assertEqual(path + " " + "loss gemodel 1.052631579% 20% 100% 0%",
                             getattr(peak, "path" + direction))
            self.assertEqual(getattr(tloss, "bottleneck" + direction), getattr(peak, "bottleneck" + direction))
            self.assertEqual(getattr(tloss, "lossTime" + direction), {"rate": 0.05, "meanBurstMs": 25})
            self.assertIsNone(getattr(peak, "lossTime" + direction))
        self.assertEqual(set(profiles["intl-peak-tloss"]) - {"lossTimeAB", "lossTimeBA"},
                         set(profiles["intl-peak"]))
        override = self.job(profile="intl-peak-tloss", lossTimeAB=None,
                            lossTimeBA={"rate": 0.01, "meanBurstMs": 40, "seed": 9})
        self.assertIsNone(override.lossTimeAB)
        self.assertEqual(override.lossTimeBA["seed"], 9)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "profiles.json"
            path.write_text(json.dumps({"bad": {"lossTimeAB": {"rate": 2, "meanBurstMs": 25}}}))
            with self.assertRaises(ValueError):
                nat.parse_job(json.dumps({**self.data, "profile": "bad"}), profiles=nat.load_profiles(path))

    def test_seed_derivation(self):
        explicit = self.job(lossTimeAB={**self.config, "seed": 5}, lossTimeBA={**self.config, "seed": 5})
        self.assertEqual((nat.loss_time_seed(explicit, "ab", 3), nat.loss_time_seed(explicit, "ba")), (5, 5))
        job = self.job(profile="intl-peak-tloss")
        seed = nat.loss_time_seed(job, "ab")
        self.assertEqual(seed, nat.loss_time_seed(self.job(profile="intl-peak-tloss"), "ab"))
        self.assertTrue(0 <= seed < 2 ** 64)
        self.assertNotEqual(seed, nat.loss_time_seed(job, "ba"))
        self.assertNotEqual(seed, nat.loss_time_seed(job, "ab", job_id=2))
        self.assertNotEqual(seed, nat.loss_time_seed(self.job(profile="intl-peak-tloss", timeout=61), "ab"))
        record = nat.new_loss_time_record(job, "ba", 4)
        self.assertEqual((record["seed"], record["device"], record["status"]),
                         (nat.loss_time_seed(job, "ba", 4), "nb-wan", "notStarted"))

    def test_gact_replace_message(self):
        message = nat.gact_replace_message(7, 1, True)
        length, kind, flags, seq, pid = struct.unpack_from("=IHHII", message)
        self.assertEqual((length, kind, flags, seq, pid), (len(message), 48, 0x105, 7, 0))
        self.assertEqual(len(message) % 4, 0)
        # tcamsg, TCA_ACT_TAB{ 1{ KIND "gact", OPTIONS{ PARMS tc_gact } } }
        self.assertEqual(message[16:20], b"\0\0\0\0")
        self.assertEqual(struct.unpack_from("=HH", message, 20), (len(message) - 20, 1))
        self.assertEqual(struct.unpack_from("=HH", message, 24), (len(message) - 24, 1))
        self.assertEqual(message[28:40], struct.pack("=HH", 9, 1) + b"gact\0\0\0\0")
        self.assertEqual(struct.unpack_from("=HHHH", message, 40), (28, 2, 24, 2))
        self.assertEqual(len(message), 68)
        self.assertEqual(struct.unpack_from("=IIiii", message, 48), (1, 0, 2, 0, 0))
        self.assertEqual(struct.unpack_from("=IIiii", nat.gact_replace_message(8, 3, False), 48),
                         (3, 0, 0, 0, 0))

    def test_loss_runs(self):
        stamps = {0: 0, 1: 1_000_000, 4: 4_000_000, 5: 5_000_000, 7: 7_100_000, 8: 8_000_000}
        self.assertEqual(nat.loss_runs(stamps, 1), {"lostBelowHighest": 3, "lossRunCount": 2,
                                                    "lossRunMeanPackets": 1.5, "lossRunMeanMs": 1.55})
        self.assertEqual(nat.loss_runs({0: 0, 1: 1}, 1)["lossRunCount"], 0)
        self.assertIsNone(nat.loss_runs({}, 1)["lossRunMeanMs"])

    def test_result_summary(self):
        record = {"rate": 0.05, "meanBurstMs": 25, "seed": 1, "device": "na-wan", "status": "completed",
                  "exitCode": 0, "startedAt": 100.0, "stoppedAt": 110.0,
                  "intervals": [(100.5, 100.53), (105.0, 105.47)]}
        result = nat.loss_time_result(record, 100.25)
        self.assertEqual((result["badPeriods"], result["badMs"], result["activeMs"]), (2, 500.0, 10000.0))
        self.assertEqual(result["realizedBadFraction"], 0.05)
        self.assertEqual(result["badIntervalsS"], [[0.25, 0.28], [4.75, 5.22]])
        self.assertEqual((result["startedAtS"], result["stoppedAtS"]), (-0.25, 9.75))
        self.assertNotIn("intervals", result)
        # A killed controller has no summary: fall back to the last interval end.
        killed = nat.loss_time_result({**record, "status": "killed", "stoppedAt": None}, None)
        self.assertEqual((killed["activeMs"], killed["startedAtS"]), (5470.0, 0.0))
        empty = nat.loss_time_result({**record, "status": "notStarted", "startedAt": None,
                                      "stoppedAt": None, "intervals": []}, None)
        self.assertEqual((empty["badPeriods"], empty["realizedBadFraction"]), (0, None))
        summary = nat.make_summary(0, self.job(lossTimeAB=self.config), (0, 0), {}, {}, 1,
                                   loss_time={"ab": result})
        self.assertEqual((summary["lossTimeAB"], summary["lossTime"]["ab"]), (self.config, result))


class StunTests(unittest.TestCase):
    transaction = bytes.fromhex("b7e7a701bc34d686fa87dfae")

    def test_wire_encoding(self):
        request = nat.stun_message(0x0001, self.transaction)
        self.assertEqual(request.hex(), "000100002112a442b7e7a701bc34d686fa87dfae")
        response = nat.stun_response(request, ("192.0.2.1", 32853))
        # RFC 5769 IPv4 sample XOR-MAPPED-ADDRESS (192.0.2.1:32853).
        self.assertEqual(response.hex(), "0101000c2112a442b7e7a701bc34d686fa87dfae002000080001a147e112a643")
        self.assertEqual(nat.stun_mapped(response, self.transaction), ("192.0.2.1", 32853))

    def test_roundtrip_and_padded_attributes(self):
        request = nat.stun_message(1, self.transaction, b"\x80\x22\x00\x01x\0\0\0")
        for address in (("198.18.0.2", 40000), ("198.18.1.2", 65535), ("203.0.113.4", 1)):
            response = nat.stun_response(request, address)
            _, _, attributes = nat.stun_header(response)
            padded = nat.stun_message(0x0101, self.transaction, b"\x80\x22\x00\x01x\0\0\0" + attributes)
            self.assertEqual(nat.stun_mapped(padded, self.transaction), address)

    def test_reject_invalid_messages(self):
        request = nat.stun_message(1, self.transaction)
        response = nat.stun_response(request, ("198.18.0.2", 40000))
        for invalid in (b"", request[:-1], request + b"xxxx", b"\xc0" + request[1:], request[:4] + b"bad!" + request[8:], response):
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                nat.stun_response(invalid, ("198.18.0.2", 40000))
        for attributes in (b"", struct.pack("!HH", 0x0020, 8), struct.pack("!HHI", 0x0020, 4, 0),
                           struct.pack("!HHBBHI", 0x0020, 8, 0, 2, 1, 0)):
            with self.subTest(attributes=attributes), self.assertRaises(ValueError):
                nat.stun_mapped(nat.stun_message(0x0101, self.transaction, attributes), self.transaction)
        with self.assertRaises(ValueError):
            nat.stun_mapped(response, b"x" * 12)
        with self.assertRaises(ValueError):
            nat.stun_mapped(request, self.transaction)


class SummaryTests(unittest.TestCase):
    def test_extract_link_counters(self):
        stats = {"rx": {"bytes": 100, "packets": 2}, "tx": {"bytes": 150, "packets": 3}}
        expected = {"rxBytes": 100, "txBytes": 150, "rxPackets": 2, "txPackets": 3}
        for key in ("stats64", "stats"):
            self.assertEqual(nat.link_counters([{"ifname": "lo"}, {"ifname": "na-wan", key: stats}], "na-wan"), expected)

    def test_delta_and_timeout_summary(self):
        job = nat.Job("eim", "symmetric", ("true",), ("true",), "delay 5ms")
        before = {"na-wan": dict(zip(nat.COUNTERS, (10, 20, 1, 2)))}
        after = {"na-wan": dict(zip(nat.COUNTERS, (110, 220, 3, 5)))}
        summary = nat.make_summary(3, job, (0, 124), before, after, 1.5)
        self.assertEqual(summary, {"type": "nattestbed", "slot": 3, "natA": "eim", "natB": "symmetric",
                                   "netem": "delay 5ms", "exitA": 0, "exitB": 124,
                                   "exits": [{"pair": 0, "a": 0, "b": 124}], "wanServices": [],
                                   "wan": {"na-wan": {"rxBytes": 100, "txBytes": 200, "rxPackets": 2, "txPackets": 3}},
                                   "durationS": 1.5, "profile": None, "pathAB": None, "pathBA": None,
                                   "bottleneckAB": None, "bottleneckBA": None, "policerAB": None,
                                   "policerBA": None, "jitterNoReorder": False,
                                   "schedule": [], "cross": [], "lossTimeAB": None,
                                   "lossTimeBA": None, "lossTime": {}, "blockQuic": False,
                                   "quicBlockDrops": None, "error": None})
        self.assertFalse(nat.succeeded(summary))
        self.assertTrue(nat.succeeded(nat.make_summary(3, job, (0, 0), before, after, 1.5)))
        with self.assertRaises(ValueError):
            nat.make_summary(3, job, (0, 0), after, before, 1.5)

    def test_block_quic(self):
        job = nat.parse_job(json.dumps({"natA": "eim", "natB": "eim", "a": ["true"], "b": ["true"],
                                        "blockQuic": True}))
        self.assertTrue(job.blockQuic)
        self.assertFalse(nat.Job("eim", "eim", ("true",), ("true",)).blockQuic)
        with self.assertRaises(ValueError):
            nat.Job("eim", "eim", ("true",), ("true",), blockQuic=1)
        summary = nat.make_summary(0, job, (0, 0), {}, {}, 1, quic_block_drops=7)
        self.assertEqual((summary["blockQuic"], summary["quicBlockDrops"]), (True, 7))
        # Only the long-header bit pattern (11) is dropped, in the forward path.
        self.assertIn("hook forward", nat.QUIC_BLOCK_RULES)
        self.assertIn("@th,64,2 3 counter drop", nat.QUIC_BLOCK_RULES)

    def test_multiple_pairs_and_services(self):
        job = nat.Job("eim", "eim", ("true",), ("true",), pairs=2, wan=("relay",))
        services = [{"index": 0, "status": "stopped", "exitCode": -15}]
        summary = nat.make_summary(0, job, [(0, 0), (1, 124)], {}, {}, 1, wan_services=services)
        self.assertEqual(summary["exits"], [{"pair": 0, "a": 0, "b": 0}, {"pair": 1, "a": 1, "b": 124}])
        self.assertEqual((summary["exitA"], summary["exitB"]), (0, 0))
        self.assertEqual(summary["wanServices"], services)
        self.assertFalse(nat.succeeded(summary))
        self.assertTrue(nat.succeeded(nat.make_summary(0, job, [(0, 0), (0, 0)], {}, {}, 1,
                                                       wan_services=services)))
        self.assertFalse(nat.succeeded(nat.make_summary(0, job, [(0, 0), (None, None)], {}, {}, 1)))

    def test_peer_line_tags(self):
        self.assertEqual(json.loads(nat.tag_peer_line(b'{"type":"probe","testbedPair":99,"testbedRole":"b"}\n', 2, "a")),
                         {"type": "probe", "testbedPair": 2, "testbedRole": "a"})
        for line in (b" log line \n", b"[]\n", b'"text"\n', b"123\n", b"null\n", b"{broken\n", b"\xff\n"):
            self.assertEqual(nat.tag_peer_line(line, 0, "b"), line)

    def test_collect_split_and_large_lines(self):
        async def check(output):
            class Process:
                stdout = asyncio.StreamReader()

                async def wait(self):
                    return 0

            process = Process()
            reader = asyncio.create_task(nat.collect(process, output, 1, "b"))
            process.stdout.feed_data(b'log\n{"value":"')
            await asyncio.sleep(0)
            process.stdout.feed_data(b'x' * 100000 + b'"}\n{"final":true}')
            process.stdout.feed_eof()
            self.assertEqual(await reader, 0)

        with tempfile.TemporaryFile() as output:
            asyncio.run(check(output))
            output.seek(0)
            log, large, final = output.readlines()
        self.assertEqual(log, b"log\n")
        self.assertEqual(json.loads(large), {"value": "x" * 100000, "testbedPair": 1, "testbedRole": "b"})
        self.assertEqual(json.loads(final), {"final": True, "testbedPair": 1, "testbedRole": "b"})

    def test_names(self):
        self.assertEqual(nat.names_for(1), {role: "mistnat-1-" + role for role in nat.ROLES})
        with self.assertRaises(ValueError):
            nat.names_for(-1)

    def test_cross_summary(self):
        record = nat.new_cross_record({"startS": 5})
        record["sender"]["samples"] = [{"second": 0, "durationS": 1, "bytes": 100},
                                         {"second": 1, "durationS": 0.5, "bytes": 25}]
        record["sink"]["samples"] = [{"second": 0, "durationS": 1, "bytes": 80},
                                       {"second": 1, "durationS": 0.75, "bytes": 45}]
        summary = nat.cross_summary(record)
        self.assertEqual(summary["sentBytes"], 125)
        self.assertEqual(summary["receivedBytes"], 125)
        self.assertEqual(summary["series"][0]["atS"], 5)
        self.assertEqual(summary["series"][0]["sentBitsPerS"], 800)
        self.assertEqual(summary["series"][1]["receivedBitsPerS"], 480)


class SafetyTests(unittest.TestCase):
    def test_topology_commands_stay_inside_namespaces(self):
        names = nat.names_for(2)
        created = []
        job = nat.Job("none", "forward", ("true",), ("true",), "delay 5ms")
        with patch.object(nat, "root_command") as root, patch.object(nat, "install_nft") as nft:
            nat.setup_topology(names, job, created)
        self.assertEqual(set(created), set(names.values()))
        for call in root.call_args_list:
            command = call.args
            if command[:3] == ("ip", "netns", "add"):
                self.assertIn(command[3], names.values())
            elif command[:2] == ("ip", "-n"):
                self.assertIn(command[2], names.values())
                if "veth" in command:
                    self.assertEqual(command[-2], "netns")
                    self.assertIn(command[-1], names.values())
                if command[3:6] == ("route", "add", "default"):
                    self.assertNotEqual(command[2], names["wan"])
            else:
                self.assertEqual(command[:3], ("ip", "netns", "exec"))
                self.assertIn(command[3], names.values())
                if command[4] == "tc":
                    self.assertIn(command[8], {interface for _, interface in nat.WAN_INTERFACES})
        nft.assert_called_once_with(names["nb"], nat.nft_rules("b", "forward"))
        shaping = [call.args for call in root.call_args_list if "tc" in call.args]
        self.assertEqual(len(shaping), 4)
        self.assertEqual({(command[3], command[8]) for command in shaping},
                         {(names[role], interface) for role, interface in nat.WAN_INTERFACES})

    def test_directional_topology_and_ordered_rate_locations(self):
        names = nat.names_for(2)
        job = nat.Job("none", "none", ("true",), ("true",),
                      pathAB="delay 75ms 25ms", bottleneckAB={"rateKbit": 10000, "limitPackets": 80},
                      jitterNoReorder=True)
        with patch.object(nat, "root_command") as root:
            nat.setup_topology(names, job, [])
        shaping = [call.args for call in root.call_args_list if "tc" in call.args]
        self.assertEqual(shaping, [
            ("ip", "netns", "exec", names["na"], "tc", "qdisc", "add", "dev", "na-wan",
             "root", "handle", "1:", "netem", "delay", "75ms", "25ms", "rate", "10000kbit", "limit", "80"),
        ])

    def test_scheduled_changes_are_scoped_and_rates_cleared(self):
        names = nat.names_for(2)
        events = [{"atS": 0, "pathAB": "delay 10ms", "bottleneckAB": None,
                   "policerBA": {"rateKbit": 1000, "burstBytes": 2000}}]
        job = nat.Job("none", "none", ("true",), ("true",), pathAB="delay 5ms",
                      bottleneckAB={"rateKbit": 10000}, schedule=events)
        records = [{**event, "appliedAtS": None} for event in events]
        with patch.object(nat, "ns_command") as cmd, patch.object(nat, "install_nft") as nft:
            asyncio.run(nat.apply_schedule(names, job, nat.time.monotonic(), records))
        cmd.assert_called_once_with(names["na"], "tc", "qdisc", "change", "dev", "na-wan",
                                    "root", "handle", "1:", "netem", "delay", "10ms", "rate", "0bit",
                                    "corrupt", "0%", "0%")
        self.assertEqual(nft.call_args.args[0], names["wan"])
        self.assertIn("flush chain ip mistnat_policer ba", nft.call_args.args[1])
        self.assertNotIn("flush chain ip mistnat_policer ab", nft.call_args.args[1])
        self.assertGreaterEqual(records[0]["appliedAtS"], 0)

    def test_schedule_resets_correlations_and_preserves_requested_corrupt(self):
        names = nat.names_for(2)
        job = nat.Job("none", "none", ("true",), ("true",),
                      pathAB="delay 20ms 5ms 30% loss 2% 20% corrupt 10% reorder 10%")
        old = job.path_state()
        new = {**old, "pathAB": "delay 10ms 2ms corrupt 1%"}
        with patch.object(nat, "ns_command") as cmd:
            nat.configure_path(names, "ab", new, False, old=old)
        self.assertEqual(cmd.call_args.args[-4:], ("1%", "0%", "reorder", "0%"))

    def test_policer_checks_resident_module_before_creating_namespace(self):
        job = nat.Job("none", "none", ("true",), ("true",),
                      policerAB={"rateKbit": 2000, "burstBytes": 32768})
        with patch.object(nat, "module_resident", return_value=False), patch.object(nat, "root_command") as root:
            with self.assertRaisesRegex(RuntimeError, "nft_limit"):
                nat.setup_topology(nat.names_for(2), job, [])
        root.assert_not_called()

    def test_policer_uses_resident_module_in_wan_only(self):
        job = nat.Job("none", "none", ("true",), ("true",),
                      policerAB={"rateKbit": 2000, "burstBytes": 32768})
        names = nat.names_for(2)
        with patch.object(nat, "module_resident", return_value=True) as resident, \
                patch.object(nat, "root_command") as root, patch.object(nat, "install_nft") as nft:
            nat.setup_topology(names, job, [])
        resident.assert_called_once_with("nft_limit")
        nft.assert_called_once_with(names["wan"], nat.policer_rules(job.path_state(), initial=True))
        self.assertFalse(any("modprobe" in call.args for call in root.call_args_list))

    def test_loss_time_stage_stays_in_nat_namespaces(self):
        names = nat.names_for(2)
        config = {"rate": 0.05, "meanBurstMs": 25}
        job = nat.Job("none", "none", ("true",), ("true",), pathAB="delay 5ms",
                      lossTimeAB=config, lossTimeBA=config)
        with patch.object(nat, "module_resident", return_value=True), \
                patch.object(nat, "root_command") as root:
            nat.setup_topology(names, job, [])
        shaping = [call.args for call in root.call_args_list if "tc" in call.args]
        match_all = ("u32", "match", "u32", "0", "0", "action", "gact", "pass", "index", "1")
        self.assertEqual(shaping[1:], [
            ("ip", "netns", "exec", names["na"], "tc", "qdisc", "add", "dev", "na-wan", "clsact"),
            ("ip", "netns", "exec", names["na"], "tc", "filter", "add", "dev", "na-wan", "egress",
             "protocol", "ip", "pref", "1", *match_all),
            ("ip", "netns", "exec", names["nb"], "tc", "qdisc", "add", "dev", "nb-wan", "clsact"),
            ("ip", "netns", "exec", names["nb"], "tc", "filter", "add", "dev", "nb-wan", "egress",
             "protocol", "ip", "pref", "1", *match_all),
        ])
        self.assertEqual(shaping[0][3:9], (names["na"], "tc", "qdisc", "add", "dev", "na-wan"))

    def test_loss_time_checks_resident_modules_before_creating_namespace(self):
        job = nat.Job("none", "none", ("true",), ("true",), lossTimeBA={"rate": 0.05, "meanBurstMs": 25})
        with patch.object(nat, "module_resident", side_effect=lambda name: name != "act_gact"), \
                patch.object(nat, "root_command") as root:
            with self.assertRaisesRegex(RuntimeError, "act_gact"):
                nat.setup_topology(nat.names_for(2), job, [])
        root.assert_not_called()

    def test_protected_operation_finishes_after_cancellation(self):
        async def check():
            started, release, finished = asyncio.Event(), asyncio.Event(), asyncio.Event()

            async def operation():
                started.set()
                await release.wait()
                finished.set()

            task = asyncio.create_task(nat.protected(operation()))
            await started.wait()
            task.cancel()
            await asyncio.sleep(0)
            self.assertFalse(task.done())
            release.set()
            with self.assertRaises(asyncio.CancelledError):
                await task
            self.assertTrue(finished.is_set())

        asyncio.run(check())


if __name__ == "__main__":
    unittest.main()
