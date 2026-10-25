"""Fail closed unless all required consumers verified these exact artifacts."""

import argparse
import json
import math
from pathlib import Path
import re
import tarfile
import zipfile

from package import sha256


def require(condition, message):
    if not condition:
        raise ValueError(message)


def number(value, minimum=0):
    return type(value) in (int, float) and math.isfinite(value) and value >= minimum


def load(path):
    def unique(pairs):
        result = {}
        for key, value in pairs:
            require(key not in result, f"duplicate JSON key: {key}")
            result[key] = value
        return result
    return json.loads(path.read_text(), object_pairs_hook=unique,
                      parse_constant=lambda value: (_ for _ in ()).throw(ValueError(f"invalid number {value}")))


def evaluate(dist, reports, commit, version, policy_path):
    policy = load(policy_path)
    require(policy.get("schema") == 1, "unknown policy schema")
    for field in ("nativeTargets", "browsers", "phases"):
        values = policy.get(field)
        require(type(values) is list and len(values) > 0 and len(values) == len(set(values)), f"empty/duplicate policy {field}")
    require(set(policy["phases"]) == {"delivery", "aoi", "multi-room", "leave-rejoin", "relay-outage", "peer-restart", "storage", "storage-wan", "soak"}, "incomplete phase policy")
    for field in ("messagesPerDirection", "connectionDeadlineMs", "deliveryDeadlineMs", "negativeObservationMs", "soakSeconds", "rejoinCycles"):
        require(number(policy.get(field), 1), f"invalid policy {field}")
    wan = policy.get("wanStorage", {})
    for field in ("bytes", "delayMs", "rateKbit", "queueMs", "deadlineMs"):
        require(type(wan.get(field)) is int and wan[field] > 0, f"invalid policy wanStorage.{field}")
    # Same shared-band propagation + queue budget and 1200 B assumption as suite.mjs.
    limit_packets = math.ceil((2 * wan["delayMs"] + wan["queueMs"]) / 1000 * wan["rateKbit"] * 1000 / 8 / 1200)
    require(re.fullmatch(r"[0-9a-f]{40}", commit), "expected a full commit SHA")
    manifests = {}
    for path in dist.glob("candidate-*.json"):
        manifest = load(path)
        identity = manifest.get("id")
        require(identity not in manifests, f"duplicate manifest {identity}")
        require(manifest.get("schema") == 1 and manifest.get("commit") == commit and manifest.get("version") == version, f"stale manifest {path.name}")
        require(type(manifest.get("files")) is dict and len(manifest["files"]) > 0, f"empty artifacts {identity}")
        for name, digest in manifest["files"].items():
            require(Path(name).name == name and name not in (".", ".."), "invalid artifact path")
            require(re.fullmatch(r"[0-9a-f]{64}", digest) and sha256(dist / name) == digest, f"artifact hash mismatch {name}")
        if identity.startswith("native-"):
            require(len(manifest["files"]) == 1, f"unexpected native files {identity}")
            with zipfile.ZipFile(dist / next(iter(manifest["files"]))) as archive:
                import hashlib
                require(archive.namelist().count(manifest["binary"]) == 1, f"missing/duplicate library {identity}")
                require(hashlib.sha256(archive.read(manifest["binary"])).hexdigest() == manifest["binarySha256"], f"packaged library differs {identity}")
        manifests[identity] = manifest
    expected_manifests = {f"native-{target}" for target in policy["nativeTargets"]} | {"wasm"}
    require(set(manifests) == expected_manifests, "missing/unexpected candidate manifests")
    wasm = manifests["wasm"]
    require(len(wasm["files"]) == 2 and wasm.get("package") in wasm["files"], "missing npm/zip inventory")
    wasm_zip = [name for name in wasm["files"] if name.endswith(".zip")]
    require(len(wasm_zip) == 1, "missing WASM archive")
    with tarfile.open(dist / wasm["package"], "r:gz") as packed, zipfile.ZipFile(dist / wasm_zip[0]) as archive:
        members = [member for member in packed.getmembers() if member.isfile()]
        require(len({m.name for m in members}) == len(members), "duplicate npm file")
        require({"package/mistlib_wasm.js", "package/mistlib_wasm_bg.wasm", "package/mistlib_wasm.d.ts", "package/LICENSE", "package/README.md", "package/package.json"}.issubset({m.name for m in members}), "incomplete npm package")
        for member in members:
            require(member.name.startswith("package/") and ".." not in Path(member.name).parts, "invalid npm member")
            name = "pkg/" + member.name.removeprefix("package/")
            require(archive.namelist().count(name) == 1 and archive.read(name) == packed.extractfile(member).read(), f"WASM zip and tested npm package differ: {name}")
        metadata = json.loads(packed.extractfile("package/package.json").read())
        require(metadata.get("name") == "@tik-choco/mistlib" and metadata.get("version") == version, "wrong npm package metadata")
        require(metadata.get("type") == "module" and metadata.get("main") == "mistlib_wasm.js" and
                metadata.get("types") == "mistlib_wasm.d.ts", "broken npm JavaScript/TypeScript entrypoints")
    required_reports = {f"native-{target}" for target in policy["nativeTargets"]} | {f"browser-{browser}" for browser in policy["browsers"]}
    seen = set()
    verified_reports = {}
    for path in reports.glob("*.json"):
        report = load(path)
        identity = report.get("id")
        require(identity in required_reports and identity not in seen, f"unexpected/duplicate report {identity}")
        seen.add(identity)
        require(report.get("schema") == 1 and report.get("passed") is True, f"failed/incomplete report {identity}")
        require(not report.get("error"), f"run contains an error {identity}")
        require(report.get("allowDirty", False) is False, f"development-only run {identity}")
        require(report.get("commit") == commit and report.get("version") == version, f"stale report {identity}")
        require(report.get("policySha256") == sha256(policy_path), f"different policy {identity}")
        browser = identity.startswith("browser-")
        platforms = {"native-linux-x86_64": ("linux", "x64", "x86_64-unknown-linux-gnu"),
                     "native-windows-x86_64": ("win32", "x64", "x86_64-pc-windows-msvc"),
                     "native-macos-aarch64": ("darwin", "arm64", "aarch64-apple-darwin")}
        platform, architecture, target = platforms["native-linux-x86_64" if browser else identity]
        require(report.get("platform") == platform and report.get("architecture") == architecture, f"wrong execution platform {identity}")
        require(type(report.get("nodeVersion")) is str and re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", report["nodeVersion"]), f"missing Node runtime version {identity}")
        native = manifests["native-linux-x86_64" if browser else identity]
        require(report.get("nativeSha256") == native["binarySha256"], f"different native artifact {identity}")
        if browser:
            wasm = manifests["wasm"]
            require(report.get("packageSha256") == wasm["files"][wasm["package"]], f"different npm artifact {identity}")
            require(type(report.get("browserVersion")) is str and report["browserVersion"], "missing browser version")
        pairs = ["browser-browser", "browser-native", "native-browser"] if browser else ["native-native"]
        expected_cases = {f"{pair}/{phase}" for pair in pairs for phase in policy["phases"]}
        cases = report.get("cases", [])
        require(len(cases) == len(expected_cases) and {case.get("id") for case in cases} == expected_cases, f"missing/duplicate cases {identity}")
        infos = report.get("buildInfo", [])
        require(len(infos) >= 3 * len(pairs), f"missing loaded build identities {identity}")
        require(all(info.get("version") == version and info.get("commit") == commit and info.get("dirty") is False and info.get("profile") == "release" for info in infos), f"wrong loaded build {identity}")
        require({info.get("target") for info in infos} == ({target, "wasm32-unknown-unknown"} if browser else {target}), f"wrong loaded target {identity}")
        for case in cases:
            require(case.get("passed") is True and number(case.get("durationMs"), 0.001), f"failed/unmeasured case {identity}/{case.get('id')}")
            require(not case.get("error"), f"case contains an error {identity}/{case.get('id')}")
            phase = case["id"].split("/")[1]
            metrics = case.get("metrics", {})
            for counter in ("sent", "received", "transfers", "corruptions", "bytes", "samples", "missing", "false", "roomLeaks", "cycles", "disconnectedSockets", "restarts"):
                if counter in metrics:
                    require(type(metrics[counter]) is int and metrics[counter] >= 0, f"invalid counter {identity}/{phase}/{counter}")
            if phase == "storage":
                require(metrics.get("transfers") == 2 and metrics.get("corruptions") == 0, f"storage contract {identity}")
            elif phase == "storage-wan":
                require(metrics.get("transfers") == 2 and metrics.get("corruptions") == 0 and
                        metrics.get("bytes") == wan["bytes"], f"WAN storage contract {identity}")
                require(type(metrics.get("shaped")) is bool and (platform != "linux" or metrics["shaped"] is True),
                        f"WAN storage shaping required on Linux {identity}")
                applied = (wan["delayMs"], wan["rateKbit"]) if metrics["shaped"] else (None, None)
                require((metrics.get("delayMs"), metrics.get("rateKbit")) == applied,
                        f"WAN storage shaping parameters {identity}")
                queue_fields = ("queueMs", "limitPackets", "queueProfile")
                require(all(field in metrics for field in queue_fields), f"missing WAN storage queue parameters {identity}")
                if metrics["shaped"]:
                    require(type(metrics["queueMs"]) is int and type(metrics["limitPackets"]) is int and
                            (metrics["queueMs"], metrics["limitPackets"], metrics["queueProfile"]) ==
                            (wan["queueMs"], limit_packets, "bounded"), f"WAN storage queue parameters {identity}")
                else:
                    require(all(metrics[field] is None for field in queue_fields), f"unshaped WAN storage queue parameters {identity}")
                timings = metrics.get("transferMs")
                require(type(timings) is list and len(timings) == 2 and
                        all(number(value, 0.001) and value <= wan["deadlineMs"] for value in timings),
                        f"WAN storage per-direction deadline {identity}")
                if metrics["shaped"]:
                    floor = 0.9 * wan["bytes"] * 8 / wan["rateKbit"]
                    require(all(value >= floor for value in timings), f"WAN transfer below shaped-link floor {identity}")
            elif phase == "aoi":
                require(metrics.get("samples") == 6 and metrics.get("missing") == 0 and metrics.get("false") == 0, f"AOI contract {identity}")
                require(number(metrics.get("convergenceMs"), 0.001) and metrics["convergenceMs"] <= policy["connectionDeadlineMs"], f"AOI convergence deadline {identity}")
            else:
                sent = metrics.get("sent")
                require(type(sent) is int and sent >= 2 * policy["messagesPerDirection"] and metrics.get("received") == sent, f"missing messages {identity}/{phase}")
                require(number(metrics.get("deliveryMs"), 0.001) and metrics["deliveryMs"] <= policy["deliveryDeadlineMs"], f"delivery deadline {identity}/{phase}")
            if phase in ("delivery", "multi-room", "leave-rejoin", "peer-restart"):
                require(number(metrics.get("connectionMs"), 0.001) and metrics["connectionMs"] <= policy["connectionDeadlineMs"], f"connection deadline {identity}/{phase}")
            if phase == "multi-room":
                require(sent >= 4 * policy["messagesPerDirection"] and metrics.get("roomLeaks") == 0 and number(metrics.get("observationMs"), policy["negativeObservationMs"]), "missing room isolation observation")
            if phase == "leave-rejoin":
                require(metrics.get("cycles") == policy["rejoinCycles"] and sent >= 4 * policy["messagesPerDirection"] * policy["rejoinCycles"], "missing rejoin cycles")
            if phase == "relay-outage":
                require(number(metrics.get("disconnectedSockets"), 2), "relay fault was not injected")
            if phase == "peer-restart":
                require(metrics.get("restarts") == 1, "peer was not restarted")
                require(case["durationMs"] <= 2 * policy["connectionDeadlineMs"], "total crash detection/recovery deadline")
            if phase == "soak":
                require(number(metrics.get("observedMs"), policy["soakSeconds"] * 1000) and number(metrics.get("samples"), 2), "insufficient soak samples")
        verified_reports[identity] = report
    require(seen == required_reports, f"missing reports: {sorted(required_reports - seen)}")
    # Includes only shipping files; publishers recheck these immediately before use.
    files = {name: digest for manifest in manifests.values() for name, digest in manifest["files"].items()}
    return {"schema": 1, "passed": True, "commit": commit, "version": version,
            "policySha256": sha256(policy_path), "files": files, "reports": verified_reports}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for key in ("dist", "reports", "commit", "version", "out"):
        parser.add_argument(f"--{key}", required=True)
    parser.add_argument("--policy", default=str(Path(__file__).with_name("policy.json")))
    args = parser.parse_args()
    output = Path(args.out)
    output.unlink(missing_ok=True)  # A failed rerun must not leave a successful receipt behind.
    result = evaluate(Path(args.dist), Path(args.reports), args.commit, args.version, Path(args.policy))
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
