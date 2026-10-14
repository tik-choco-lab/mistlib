"""Reject empty suites, retries, failures, and any newly ignored Rust test."""

import argparse
from pathlib import Path
import xml.etree.ElementTree as ET

from gate import load, require


def check(path, allowlist):
    policy = load(allowlist)
    allowed = policy.get("tests", [])
    require(policy.get("schema") == 1 and type(allowed) is list and len(allowed) == len(set(allowed)), "invalid ignored-test policy")
    cases = ET.parse(path).getroot().findall(".//testcase")
    require(cases, "empty Rust test suite")
    identities, skipped = set(), set()
    for case in cases:
        require(case.get("classname") and case.get("name"), "missing Rust test identity")
        identity = f"{case.get('classname')}:{case.get('name')}"
        require(identity not in identities, f"duplicate test result: {identity}")
        identities.add(identity)
        require(not any(child.tag in {"failure", "error", "flakyFailure", "flakyError", "rerunFailure", "rerunError"} for child in case), f"failed or retried Rust test: {identity}")
        if case.find("skipped") is not None:
            skipped.add(identity)
    require(skipped == set(allowed), f"ignored tests changed; unexpected={sorted(skipped - set(allowed))}, missing={sorted(set(allowed) - skipped)}")
    require(len(identities) > len(skipped), "no Rust tests executed")
    return {"executed": len(identities) - len(skipped), "ignored": len(skipped)}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report")
    args = parser.parse_args()
    print(check(Path(args.report), Path(__file__).with_name("ignored-tests.json")))
