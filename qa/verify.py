"""Recheck the gate receipt and shipping files immediately before publishing."""

import argparse
import json
from pathlib import Path
import tempfile

from gate import evaluate, load, require
from package import sha256


def verify(receipt, dist, commit, version):
    result = load(receipt)
    require(result.get("schema") == 1 and result.get("passed") is True, "missing successful quality gate")
    require(result.get("commit") == commit and result.get("version") == version, "quality receipt belongs to another release")
    require(result.get("policySha256") == sha256(Path(__file__).with_name("policy.json")), "quality policy changed")
    require(type(result.get("files")) is dict and len(result["files"]) > 0, "empty shipping inventory")
    require(type(result.get("reports")) is dict and len(result["reports"]) > 0, "missing quality evidence")
    # A boolean/manifest alone cannot authorize publishing. Re-evaluate every
    # required scenario and loaded identity, as well as the distribution bytes.
    with tempfile.TemporaryDirectory(prefix="mistlib-verify-") as temp:
        reports = Path(temp)
        for index, report in enumerate(result["reports"].values()):
            (reports / f"{index}.json").write_text(json.dumps(report))
        rebuilt = evaluate(dist, reports, commit, version, Path(__file__).with_name("policy.json"))
    require(rebuilt == result, "quality receipt changed after QA")
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("receipt", "dist", "commit", "version"):
        parser.add_argument(f"--{name}", required=True)
    args = parser.parse_args()
    verify(Path(args.receipt), Path(args.dist), args.commit, args.version)
