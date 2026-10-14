import contextlib
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch
import zipfile

import pages


class PagesTest(unittest.TestCase):
    def test_only_the_latest_published_commit_can_deploy_its_verified_bytes(self):
        for state in ["published", "older version", "unreleased commit", "prerelease"]:
            with self.subTest(state=state), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                version = "1.2.3-rc.1" if state == "prerelease" else "1.2.3"
                commit = "a" * 40
                dist = root / "dist"
                dist.mkdir()
                archive_name = f"mistlib-wasm-v{version}.zip"
                with zipfile.ZipFile(dist / archive_name, "w") as archive:
                    archive.writestr("pkg/mistlib_wasm_bg.wasm", b"verified WASM bytes")
                receipt = root / "quality.json"
                receipt.write_text(json.dumps({"version": version}))
                (root / ".github/pages").mkdir(parents=True)
                (root / ".github/pages/index.html").write_text("__VERSION__ __NPM_VERSION__")
                out = root / "site"
                argv = ["pages.py", "--dist", str(dist), "--receipt", str(receipt), "--commit", commit, "--out", str(out)]
                latest = {"tag_name": "v1.2.4" if state == "older version" else f"v{version}",
                          "target_commitish": "b" * 40 if state == "unreleased commit" else commit}
                output = io.StringIO()
                with contextlib.chdir(root), contextlib.redirect_stdout(output), \
                     patch.object(sys, "argv", argv), patch.dict(os.environ, {"GH_REPO": "fixture/repo", "GH_TOKEN": "fixture"}), \
                     patch.object(pages, "verify", return_value={"files": {archive_name: "verified"}}), \
                     patch.object(pages, "urlopen", return_value=io.BytesIO(json.dumps(latest).encode())) as registry:
                    pages.main()
                if state == "published":
                    self.assertIn("deploy=true", output.getvalue())
                    self.assertEqual((out / "pkg/mistlib_wasm_bg.wasm").read_bytes(), b"verified WASM bytes")
                    self.assertEqual((out / "index.html").read_text(), "v1.2.3 1.2.3")
                else:
                    self.assertIn("deploy=false", output.getvalue())
                    self.assertFalse(out.exists())
                if state == "prerelease":
                    registry.assert_not_called()
