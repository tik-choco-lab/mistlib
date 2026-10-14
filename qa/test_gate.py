"""Negative controls for the mechanism that authorizes publication."""

from copy import deepcopy
import hashlib
import io
import json
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile

from gate import evaluate, load
from package import sha256
from verify import verify

POLICY = Path(__file__).with_name("policy.json")
COMMIT = "a" * 40
VERSION = "1.2.3"


class GateTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.dist, self.reports = self.root / "dist", self.root / "reports"
        self.dist.mkdir()
        self.reports.mkdir()
        self.policy = load(POLICY)
        self.manifests = {}
        for target in self.policy["nativeTargets"]:
            identity = f"native-{target}"
            binary = f"library-{target}"
            data = binary.encode()
            archive = self.dist / f"{identity}.zip"
            with zipfile.ZipFile(archive, "w") as output:
                output.writestr(binary, data)
            self.manifests[identity] = {"schema": 1, "id": identity, "commit": COMMIT, "version": VERSION,
                "binary": binary, "binarySha256": hashlib.sha256(data).hexdigest(), "files": {archive.name: sha256(archive)}}
        files = {"package.json": json.dumps({"name": "@tik-choco/mistlib", "version": VERSION,
                 "type": "module", "main": "mistlib_wasm.js", "types": "mistlib_wasm.d.ts"}).encode(),
                 **{name: name.encode() for name in ["mistlib_wasm.js", "mistlib_wasm_bg.wasm", "mistlib_wasm.d.ts", "LICENSE", "README.md"]}}
        tarball = self.dist / "package.tgz"
        with tarfile.open(tarball, "w:gz") as output:
            for name, data in files.items():
                member = tarfile.TarInfo(f"package/{name}")
                member.size = len(data)
                output.addfile(member, io.BytesIO(data))
        archive = self.dist / "wasm.zip"
        with zipfile.ZipFile(archive, "w") as output:
            for name, data in files.items():
                output.writestr(f"pkg/{name}", data)
        self.manifests["wasm"] = {"schema": 1, "id": "wasm", "commit": COMMIT, "version": VERSION,
            "package": tarball.name, "files": {tarball.name: sha256(tarball), archive.name: sha256(archive)}}
        for identity, manifest in self.manifests.items():
            self.write(self.dist / f"candidate-{identity}.json", manifest)
        self.report_data = {}
        platforms = {"linux-x86_64": ("linux", "x64", "x86_64-unknown-linux-gnu"),
                     "windows-x86_64": ("win32", "x64", "x86_64-pc-windows-msvc"),
                     "macos-aarch64": ("darwin", "arm64", "aarch64-apple-darwin")}
        for target in self.policy["nativeTargets"] + self.policy["browsers"]:
            browser = target in self.policy["browsers"]
            identity = f"browser-{target}" if browser else f"native-{target}"
            platform, architecture, triple = platforms["linux-x86_64" if browser else target]
            info = {"version": VERSION, "commit": COMMIT, "profile": "release", "dirty": False, "target": triple}
            infos = [deepcopy(info) for _ in range(3)]
            if browser:
                infos += [{**info, "target": "wasm32-unknown-unknown"} for _ in range(6)]
            cases = []
            for pair in (["browser-browser", "browser-native", "native-browser"] if browser else ["native-native"]):
                for phase in self.policy["phases"]:
                    metrics = {"sent": 128, "received": 128, "deliveryMs": 100, "connectionMs": 500}
                    metrics.update({"delivery": {}, "aoi": {"samples": 6, "missing": 0, "false": 0, "convergenceMs": 500},
                        "multi-room": {"roomLeaks": 0, "observationMs": 1000},
                        "leave-rejoin": {"cycles": 3, "sent": 384, "received": 384},
                        "relay-outage": {"disconnectedSockets": 2}, "peer-restart": {"restarts": 1},
                        "storage": {"transfers": 2, "corruptions": 0}, "soak": {"observedMs": 15000, "samples": 10}}[phase])
                    cases.append({"id": f"{pair}/{phase}", "passed": True, "durationMs": 1000, "metrics": metrics})
            native = self.manifests["native-linux-x86_64" if browser else identity]
            report = {"schema": 1, "id": identity, "passed": True, "commit": COMMIT, "version": VERSION,
                "policySha256": sha256(POLICY), "nativeSha256": native["binarySha256"],
                "packageSha256": sha256(tarball) if browser else None, "platform": platform, "architecture": architecture,
                "browserVersion": "test-fixture", "nodeVersion": "v24.0.0", "buildInfo": infos, "cases": cases}
            self.report_data[identity] = report
            self.write(self.reports / f"{identity}.json", report)

    @staticmethod
    def write(path, value):
        path.write_text(json.dumps(value))

    def evaluate(self):
        return evaluate(self.dist, self.reports, COMMIT, VERSION, POLICY)

    def test_complete_evidence_and_identical_artifacts_pass(self):
        receipt = self.evaluate()
        path = self.root / "quality.json"
        self.write(path, receipt)
        self.assertEqual(verify(path, self.dist, COMMIT, VERSION), receipt)

    def test_negative_controls_block_promotion(self):
        identity = "browser-chromium"
        original = self.report_data[identity]

        def mutate_case(report, phase, field, value):
            next(case for case in report["cases"] if case["id"].endswith('/' + phase))["metrics"][field] = value

        mutations = {
            "failed run": lambda r: r.update(passed=False),
            "missing phase": lambda r: r["cases"].pop(),
            "duplicate phase": lambda r: r["cases"].append(r["cases"][0]),
            "stale commit": lambda r: r.update(commit="b" * 40),
            "wrong policy": lambda r: r.update(policySha256="0" * 64),
            "wrong native bytes": lambda r: r.update(nativeSha256="0" * 64),
            "wrong npm bytes": lambda r: r.update(packageSha256="0" * 64),
            "wrong platform": lambda r: r.update(platform="darwin"),
            "wrong loaded target": lambda r: r["buildInfo"][0].update(target="unknown"),
            "dirty build": lambda r: r["buildInfo"][0].update(dirty=True),
            "development mode": lambda r: r.update(allowDirty=True),
            "no loaded identities": lambda r: r.update(buildInfo=[]),
            "lost message": lambda r: mutate_case(r, "delivery", "received", 0),
            "NaN timing": lambda r: mutate_case(r, "delivery", "deliveryMs", float('nan')),
            "boolean timing": lambda r: mutate_case(r, "delivery", "deliveryMs", True),
            "timeout": lambda r: mutate_case(r, "delivery", "deliveryMs", 10001),
            "AOI missing": lambda r: mutate_case(r, "aoi", "missing", 1),
            "room leakage": lambda r: mutate_case(r, "multi-room", "roomLeaks", 1),
            "boolean loss counter": lambda r: mutate_case(r, "multi-room", "roomLeaks", False),
            "no fault": lambda r: mutate_case(r, "relay-outage", "disconnectedSockets", 0),
            "no restart": lambda r: mutate_case(r, "peer-restart", "restarts", 0),
            "slow total recovery": lambda r: next(case for case in r["cases"] if case["id"].endswith('/peer-restart')).update(durationMs=60001),
            "too few rejoins": lambda r: mutate_case(r, "leave-rejoin", "cycles", 1),
            "no soak": lambda r: mutate_case(r, "soak", "observedMs", 0),
            "corrupt storage": lambda r: mutate_case(r, "storage", "corruptions", 1),
        }
        path = self.reports / f"{identity}.json"
        for name, mutate in mutations.items():
            with self.subTest(name=name):
                report = deepcopy(original)
                mutate(report)
                self.write(path, report)
                with self.assertRaises(ValueError):
                    self.evaluate()
        self.write(path, original)

    def test_missing_report_and_duplicate_json_are_rejected(self):
        path = self.reports / "browser-firefox.json"
        path.unlink()
        with self.assertRaisesRegex(ValueError, "missing reports"):
            self.evaluate()
        path.write_text('{"passed":false,"passed":true}')
        with self.assertRaisesRegex(ValueError, "duplicate JSON"):
            self.evaluate()

    def test_changed_shipping_bytes_block_gate_and_publish(self):
        receipt = self.evaluate()
        path = self.root / "quality.json"
        self.write(path, receipt)
        (self.dist / "package.tgz").write_bytes(b"changed after QA")
        with self.assertRaises(ValueError):
            self.evaluate()
        with self.assertRaisesRegex(ValueError, "hash mismatch"):
            verify(path, self.dist, COMMIT, VERSION)

    def test_receipt_without_actual_quality_evidence_cannot_publish(self):
        receipt = self.evaluate()
        path = self.root / "quality.json"
        receipt["reports"] = {}
        self.write(path, receipt)
        with self.assertRaisesRegex(ValueError, "missing quality evidence"):
            verify(path, self.dist, COMMIT, VERSION)

    def test_failed_rerun_removes_previous_successful_receipt(self):
        receipt = self.root / "quality.json"
        self.write(receipt, self.evaluate())
        (self.reports / "browser-firefox.json").unlink()
        result = subprocess.run([sys.executable, str(Path(__file__).with_name("gate.py")),
            "--dist", str(self.dist), "--reports", str(self.reports), "--commit", COMMIT,
            "--version", VERSION, "--out", str(receipt)], capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(receipt.exists())


if __name__ == "__main__":
    unittest.main()
