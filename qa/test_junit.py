import json
from pathlib import Path
import tempfile
import unittest

from check_junit import check


class JunitTest(unittest.TestCase):
    def test_failed_empty_retried_or_newly_ignored_tests_cannot_pass(self):
        allowed = '<testcase classname="crate" name="diagnostic"><skipped/></testcase>'
        passed = '<testcase classname="crate" name="required"/>'
        variants = {
            "valid": allowed + passed,
            "empty": "",
            "newly ignored": allowed + passed.replace('/>', '><skipped/></testcase>'),
            "failure": allowed + passed.replace('/>', '><failure/></testcase>'),
            "hidden retry": allowed + passed.replace('/>', '><flakyFailure/></testcase>'),
            "duplicate": allowed + passed + passed,
            "no executed tests": allowed,
            "missing identity": allowed + '<testcase/>',
        }
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            policy = root / "ignored.json"
            policy.write_text(json.dumps({"schema": 1, "tests": ["crate:diagnostic"]}))
            report = root / "junit.xml"
            for name, body in variants.items():
                with self.subTest(name=name):
                    report.write_text(f'<testsuites><testsuite>{body}</testsuite></testsuites>')
                    if name == "valid":
                        self.assertEqual(check(report, policy), {"executed": 1, "ignored": 1})
                    else:
                        with self.assertRaises(ValueError):
                            check(report, policy)
