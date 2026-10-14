"""Reproduce the release consumers locally without publishing or minting a receipt."""

import argparse
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile
import zipfile

from package import BINARIES, ROOT, version


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["native", "browser"])
    parser.add_argument("--browser", choices=["chromium", "firefox"], default="chromium")
    parser.add_argument("--soak-seconds", type=int, default=15)
    args = parser.parse_args()
    targets = {("Linux", "x86_64"): "linux-x86_64", ("Windows", "AMD64"): "windows-x86_64",
               ("Darwin", "arm64"): "macos-aarch64"}
    target = targets.get((platform.system(), platform.machine()))
    if not target:
        parser.error("host is not in qa/policy.json's native support matrix")
    env = os.environ.copy()
    env["MISTLIB_BUILD_COMMIT"] = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    env.pop("MISTLIB_BUILD_DIRTY", None)  # Let the build record the actual working-tree state.
    env["QA_PYTHON"] = sys.executable

    def run(*command, cwd=ROOT):
        executable = shutil.which(str(command[0])) or str(command[0])
        subprocess.run([executable, *map(str, command[1:])], cwd=cwd, env=env, check=True)

    run("npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund", cwd=ROOT / "qa")
    run("cargo", "build", "--release", "--locked", "-p", "mistlib-native")
    dist = ROOT / "dist" / "qa-local"
    run(sys.executable, "qa/package.py", "native", "--binary", ROOT / "target" / "release" / BINARIES[target], "--target", target, "--out", dist)
    if args.mode == "browser":
        run("wasm-pack", "build", "mistlib-wasm", "--target", "web", "--release", "--", "--locked")
        run(sys.executable, "qa/package.py", "wasm", "--pkg", "mistlib-wasm/pkg", "--out", dist)
        run("node", "qa/node_modules/playwright/cli.js", "install", args.browser)
    with tempfile.TemporaryDirectory(prefix="mistlib-qa-unpack-") as temp:
        with zipfile.ZipFile(dist / f"mistlib-native-{target}-v{version()}.zip") as archive:
            archive.extractall(temp)
        command = ["node", "qa/suite.mjs", "--native", Path(temp) / BINARIES[target], "--target", target,
                   "--commit", env["MISTLIB_BUILD_COMMIT"], "--version", version(), "--soak-seconds", args.soak_seconds,
                   "--allow-dirty", "--out", ROOT / "qa" / "results" / f"local-{args.browser if args.mode == 'browser' else target}.json"]
        if args.mode == "browser":
            command += ["--browser", args.browser, "--package", dist / f"tik-choco-mistlib-{version()}.tgz"]
        run(*command)


if __name__ == "__main__":
    main()
