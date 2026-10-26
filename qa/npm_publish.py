"""Publish a verified npm tarball; a retry only accepts identical registry bytes."""

import argparse
import base64
import hashlib
import json
from pathlib import Path
import subprocess
from urllib.error import HTTPError
from urllib.request import urlopen

from gate import require
from package import npm_dist_tag, validate_version
from verify import verify


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for key in ("dist", "receipt", "commit", "version"):
        parser.add_argument(f"--{key}", required=True)
    args = parser.parse_args()
    dist = Path(args.dist)
    receipt = verify(Path(args.receipt), dist, args.commit, args.version)
    packages = [name for name in receipt["files"] if name.endswith(".tgz")]
    require(len(packages) == 1, "expected exactly one npm tarball")
    tarball = dist / packages[0]
    integrity = "sha512-" + base64.b64encode(hashlib.sha512(tarball.read_bytes()).digest()).decode()
    try:
        with urlopen("https://registry.npmjs.org/@tik-choco%2Fmistlib/latest", timeout=30) as response:
            current = json.load(response)["version"]
    except HTTPError as error:
        if error.code != 404:
            raise
    else:
        core = lambda value: tuple(map(int, validate_version(value).split("-")[0].split(".")))
        require("-" not in args.version or core(args.version) != core(current),
                "prerelease core is already stable on npm; start a new beta cycle")
        require(core(args.version) >= core(current), "candidate would downgrade the current stable npm release")
    try:
        with urlopen(f"https://registry.npmjs.org/@tik-choco%2Fmistlib/{args.version}", timeout=30) as response:
            existing = json.load(response)
    except HTTPError as error:
        if error.code != 404:
            raise
    else:
        require(existing.get("dist", {}).get("integrity") == integrity, "this npm version already contains different bytes; use a new version")
        print("The identical verified npm tarball is already published.")
        return
    npm_version = subprocess.check_output(["npm", "--version"], text=True).strip()
    require(tuple(map(int, npm_version.split("."))) >= (11, 5, 1), "npm >= 11.5.1 is required for trusted publishing")
    subprocess.run(["npm", "publish", str(tarball.resolve()), "--access", "public", "--tag",
                    npm_dist_tag(args.version)], check=True)


if __name__ == "__main__":
    main()
