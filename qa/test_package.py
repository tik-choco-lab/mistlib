"""Pure release arithmetic and precedence shared by release entry points."""

import unittest

from package import next_version, npm_dist_tag, semver_key


class VersionTest(unittest.TestCase):
    def test_cycle(self):
        for bump, expected in (("minor", "0.8.0-beta.1"), ("patch", "0.7.1-beta.1"), ("major", "1.0.0-beta.1")):
            with self.subTest(bump=bump):
                self.assertEqual(next_version("0.7.0", "beta", bump), expected)
        self.assertEqual(next_version("0.7.0", "beta"), "0.8.0-beta.1")
        self.assertEqual(next_version("0.8.0-beta.0", "beta"), "0.8.0-beta.1")
        self.assertEqual(next_version("0.8.0-beta.9", "beta"), "0.8.0-beta.10")
        self.assertEqual(next_version("0.8.0-beta.10", "stable"), "0.8.0")
        with self.assertRaisesRegex(ValueError, "already stable; start a beta first"):
            next_version("0.8.0", "stable")

    def test_cycle_rejections(self):
        for channel in ("beta", "stable"):
            for bump in ("minor", "patch", "major"):
                with self.subTest(channel=channel, bump=bump), self.assertRaisesRegex(ValueError, "finish with stable first"):
                    next_version("0.8.0-beta.1", channel, bump)
            for value in ("0.8.0-rc.1", "0.8.0-beta", "0.8.0-beta.1.extra", "0.8.0-alpha.1"):
                with self.subTest(value=value, channel=channel), self.assertRaises(ValueError):
                    next_version(value, channel)
        for value in ("0.8", "v0.8.0", "01.8.0", "0.8.0-beta.01", "0.8.0-rc..1"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                semver_key(value)
        with self.assertRaises(ValueError):
            next_version("0.7.0", "rc")
        with self.assertRaises(ValueError):
            next_version("0.7.0", "beta", "other")

    def test_semver_precedence(self):
        ordered = ["0.8.0-alpha", "0.8.0-alpha.1", "0.8.0-beta.2", "0.8.0-beta.10",
                   "0.8.0-rc.1", "0.8.0", "0.8.1-beta.1", "1.0.0"]
        self.assertEqual(sorted(reversed(ordered), key=semver_key), ordered)

    def test_dist_tags(self):
        for value, expected in (("0.8.0", "latest"), ("0.8.0-beta.1", "beta"),
                                ("0.8.0-rc.1", "rc"), ("0.8.0-alpha", "alpha"),
                                ("0.8.0-Beta.1", "next"), ("0.8.0-123", "next"),
                                ("0.8.0-beta-test.1", "next")):
            with self.subTest(value=value):
                self.assertEqual(npm_dist_tag(value), expected)
