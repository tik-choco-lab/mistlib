"""Publishing must use verified bytes, stop on registry errors, and be retry-safe."""

import base64
import hashlib
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch
from urllib.error import HTTPError

import npm_publish


class PublishTest(unittest.TestCase):
    def test_registry_outcomes_and_exact_tarball(self):
        with tempfile.TemporaryDirectory() as temp:
            dist = Path(temp)
            tarball = dist / "candidate.tgz"
            tarball.write_bytes(b"the exact verified artifact")
            integrity = "sha512-" + base64.b64encode(hashlib.sha512(tarball.read_bytes()).digest()).decode()
            argv = ["npm_publish.py", "--dist", str(dist), "--receipt", "quality.json",
                    "--commit", "a" * 40, "--version", "1.2.3"]
            for outcome in ["new", "identical", "different", "unauthorized", "unavailable", "downgrade"]:
                with self.subTest(outcome=outcome), patch.object(sys, "argv", argv), \
                     patch.object(npm_publish, "verify", return_value={"files": {tarball.name: "unused"}}) as verify, \
                     patch.object(npm_publish, "urlopen") as registry, \
                     patch.object(npm_publish.subprocess, "check_output", return_value="11.12.1"), \
                     patch.object(npm_publish.subprocess, "run") as publish:
                    def response(url, **kwargs):
                        if outcome in ("unauthorized", "unavailable"):
                            raise HTTPError(url, {"unauthorized": 401, "unavailable": 503}[outcome], "fixture", {}, None)
                        if url.endswith('/latest'):
                            return io.BytesIO(json.dumps({"version": "2.0.0" if outcome == "downgrade" else "1.0.0"}).encode())
                        if outcome == "new":
                            raise HTTPError(url, 404, "fixture", {}, None)
                        return io.BytesIO(json.dumps({"dist": {"integrity": integrity if outcome == "identical" else "different"}}).encode())
                    registry.side_effect = response
                    if outcome in ("different", "unauthorized", "unavailable", "downgrade"):
                        with self.assertRaises((ValueError, HTTPError)):
                            npm_publish.main()
                        publish.assert_not_called()
                    else:
                        npm_publish.main()
                        if outcome == "new":
                            publish.assert_called_once_with(["npm", "publish", str(tarball), "--access", "public", "--tag", "latest"], check=True)
                        else:
                            publish.assert_not_called()
                    verify.assert_called_once_with(Path("quality.json"), dist, "a" * 40, "1.2.3")

    def test_failed_verification_never_contacts_registry(self):
        with patch.object(sys, "argv", ["npm_publish.py", "--dist", "dist", "--receipt", "quality.json", "--commit", "a" * 40, "--version", "1.2.3"]), \
             patch.object(npm_publish, "verify", side_effect=ValueError("changed bytes")), \
             patch.object(npm_publish, "urlopen") as registry, patch.object(npm_publish.subprocess, "run") as publish:
            with self.assertRaises(ValueError):
                npm_publish.main()
            registry.assert_not_called()
            publish.assert_not_called()
