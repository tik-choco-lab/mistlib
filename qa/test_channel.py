"""Channel checks in an isolated tree; git and release are harmless fixtures."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

from package import ROOT


class ChannelTest(unittest.TestCase):
    def test_targets_and_stable_guards(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "scripts/publish").mkdir(parents=True)
            (root / "qa").mkdir()
            (root / "bin").mkdir()
            (root / "public/.git").mkdir(parents=True)
            shutil.copyfile(ROOT / "scripts/publish/channel.sh", root / "scripts/publish/channel.sh")
            shutil.copyfile(ROOT / "qa/package.py", root / "qa/package.py")
            release = root / "scripts/publish/release.sh"
            release.write_text('#!/bin/bash\nprintf "release:%s\\n" "$@"\n')
            release.chmod(0o755)
            git = root / "bin/git"
            git.write_text('''#!/bin/bash
if [[ "$1" == -C ]]; then shift 2; fi
case "$1" in
  tag) printf '%s' "${FIXTURE_TAG:-}" ;;
  log) printf '%s\\n' "${FIXTURE_COMMIT:-}" ;;
  diff) [[ "$*" != *..HEAD* ]] || printf '%s' "${FIXTURE_FILES:-}" ;;
  *) exit 99 ;;
esac
''')
            git.chmod(0o755)

            def run(version, *args, **fixtures):
                for crate in ("mistlib-core", "mistlib-native", "mistlib-wasm"):
                    (root / crate).mkdir(exist_ok=True)
                    (root / crate / "Cargo.toml").write_text(f'[package]\nversion = "{version}"\n')
                return subprocess.run(["bash", str(root / "scripts/publish/channel.sh"), *args],
                                      text=True, capture_output=True, check=False,
                                      env={**os.environ, "PATH": str(root / "bin") + os.pathsep + os.environ["PATH"],
                                           "MISTLIB_PUBLIC_DIR": str(root / "public"), **fixtures})

            for bump, target in (("minor", "0.8.0-beta.1"), ("patch", "0.7.1-beta.1"), ("major", "1.0.0-beta.1")):
                result = run("0.7.0", "beta", bump, "--dry-run")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn(target, result.stdout)
                self.assertNotIn("release:", result.stdout)
            result = run("0.8.0-beta.9", "beta", "--dry-run")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("0.8.0-beta.10", result.stdout)
            self.assertNotEqual(run("0.8.0-beta.1", "beta", "minor", "--dry-run").returncode, 0)
            good = {"FIXTURE_TAG": "v0.8.0-beta.1", "FIXTURE_COMMIT": "fixture\tchore(release): v0.8.0-beta.1"}
            for fixtures in ({}, {"FIXTURE_TAG": "v0.8.0-beta.1"},
                             {"FIXTURE_COMMIT": "fixture\tchore(release): v0.8.0-beta.1"}, {**good, "FIXTURE_FILES": "mistlib-core/src/lib.rs"},
                             {**good, "FIXTURE_FILES": "mistlib-core/Cargo.toml"}):
                with self.subTest(fixtures=fixtures):
                    result = run("0.8.0-beta.1", "stable", "--dry-run", **fixtures)
                    self.assertNotEqual(result.returncode, 0, result.stdout)
                    result = run("0.8.0-beta.1", "stable", "--allow-changes", "--dry-run", **fixtures)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn("warning:", result.stderr)
            result = run("0.8.0-beta.1", "stable", "--push", **good, FIXTURE_FILES="docs/guide.md\nREADME.md")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("release:0.8.0\nrelease:--push", result.stdout)
            result = run("0.8.0-beta.1", "stable", "--allow-changes")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("release:0.8.0", result.stdout)
            self.assertNotIn("release:--allow-changes", result.stdout)

    def test_release_ordering_fragment_without_running_release(self):
        source = (ROOT / "scripts/publish/release.sh").read_text()
        fragment = source[source.index("version_core() "):source.index("# Run the shared gate once")]
        for candidate, tags, accepted in (
            ("0.8.0-beta.3", "v0.8.0", False),
            ("0.8.0-beta.3", "v0.8.0-beta.10", False),
            ("0.8.0-beta.10", "v0.8.0-beta.9", True),
            ("0.8.0-beta.3", "v0.8.0-beta.2\nv0.8.0-rc.1", False),
            ("0.8.0", "v0.8.0-beta.10", True),
            ("0.7.9", "v0.8.0-beta.1", False),
            ("0.8.0-beta.1", "v0.1\nv0.7.0", True),
        ):
            with self.subTest(candidate=candidate, tags=tags):
                result = subprocess.run(["bash", "-c", """
set -euo pipefail
TAG="v$VERSION"
pub() { printf '%s\\n' "$FIXTURE_TAGS"; }
info() { :; }
die() { echo "$*" >&2; exit 1; }
""" + fragment], cwd=ROOT, text=True, capture_output=True,
                    env={**os.environ, "VERSION": candidate, "FIXTURE_TAGS": tags})
                self.assertEqual(result.returncode == 0, accepted, result.stderr)
