"""Assemble Pages from the latest successful stable release's verified archive."""

import argparse
import json
import os
from pathlib import Path
import re
from urllib.request import Request, urlopen
import zipfile

from gate import load, require
from verify import verify


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for key in ("dist", "receipt", "commit", "out"):
        parser.add_argument(f"--{key}", required=True)
    args = parser.parse_args()
    version = load(Path(args.receipt))["version"]
    receipt = verify(Path(args.receipt), Path(args.dist), args.commit, version)
    if "-" in version:
        print("deploy=false")
        return
    request = Request(f"https://api.github.com/repos/{os.environ['GH_REPO']}/releases/latest",
                      headers={"Authorization": f"Bearer {os.environ['GH_TOKEN']}", "Accept": "application/vnd.github+json"})
    with urlopen(request, timeout=30) as response:
        latest = json.load(response)
    if latest["tag_name"] != f"v{version}" or latest.get("target_commitish") != args.commit:
        print("deploy=false")
        return
    out = Path(args.out)
    require(not out.exists(), "site output must be a fresh directory")
    out.mkdir(parents=True)
    archive_name = f"mistlib-wasm-v{version}.zip"
    require(archive_name in receipt["files"], "missing verified WASM archive")
    with zipfile.ZipFile(Path(args.dist) / archive_name) as archive:
        for member in archive.infolist():
            path = Path(member.filename)
            require(not path.is_absolute() and ".." not in path.parts, "invalid archive path")
            if member.filename.startswith("pkg/"):
                archive.extract(member, out)
    html = Path(".github/pages/index.html").read_text().replace("__VERSION__", f"v{version}").replace("__NPM_VERSION__", version)
    require(not re.search(r"__[A-Z_]+__", html), "unresolved landing page placeholder")
    (out / "index.html").write_text(html)
    (out / ".nojekyll").touch()
    print("deploy=true")


if __name__ == "__main__":
    main()
