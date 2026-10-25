"""Run paired storage cells with shared seeds, alternating A/B order per repetition."""

import argparse
from collections import defaultdict
import json
from pathlib import Path
import subprocess
import sys
import time

from storage_report import load_rows, median, print_table, summarise

ROOT = Path(__file__).resolve().parents[1]
RUNNER = ["node", str(ROOT / "qa/storage_bench.mjs")]


def paired_summary(rows, a_label, b_label):
    groups = defaultdict(dict)
    for row in rows:
        key = tuple(row[key] for key in ("pair", "direction", "order", "profile", "size", "seed", "repetition"))
        if row["label"] in groups[key]:
            raise ValueError(f"duplicate variant for paired cell {key}")
        groups[key][row["label"]] = row
    cells = defaultdict(list)
    for key, variants in groups.items():
        if set(variants) != {a_label, b_label}:
            continue
        a, b = variants[a_label], variants[b_label]
        cells[key[:5]].append((a, b))
    result = []
    for key, attempts in sorted(cells.items()):
        result.append({**dict(zip(("pair", "direction", "order", "profile", "size"), key)),
                       "pairs": len(attempts), "bothSuccessful": sum(a["success"] and b["success"] for a, b in attempts),
                       "medianCompletionDeltaMs_BminusA": median(
                           b["completionMs"] - a["completionMs"] for a, b in attempts if a["success"] and b["success"]),
                       "medianGoodputDeltaBps_BminusA": median(b["goodputBps"] - a["goodputBps"] for a, b in attempts),
                       "medianControlRttP95DeltaMs_BminusA": median(
                           b["controlRttP95Ms"] - a["controlRttP95Ms"] for a, b in attempts
                           if a.get("controlRttP95Ms") is not None and b.get("controlRttP95Ms") is not None)})
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for key, default in [("pair", "chromium-chromium"), ("profile", "clean"), ("size", "4MiB"),
                         ("repeat", "1"), ("browser", "chromium"), ("timeout", "auto"), ("seed", "430243841")]:
        parser.add_argument("--" + key, default=default)
    parser.add_argument("--a-config", default="{}")
    parser.add_argument("--b-config", default="{}")
    parser.add_argument("--a-artifacts")
    parser.add_argument("--b-artifacts")
    parser.add_argument("--a-label", default="A")
    parser.add_argument("--b-label", default="B")
    parser.add_argument("--no-build", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--out", help="JSONL path (new file); summary is written alongside as .summary.json")
    args = parser.parse_args()
    if args.a_label == args.b_label:
        parser.error("A and B labels must differ")
    if bool(args.a_artifacts) != bool(args.b_artifacts):
        parser.error("provide both --a-artifacts and --b-artifacts")
    try:
        configs = [json.loads(args.a_config), json.loads(args.b_config)]
        if not all(isinstance(config, dict) for config in configs):
            raise ValueError("config overrides must be JSON objects")
    except ValueError as error:
        parser.error(str(error))
    if args.a_artifacts and any(configs):
        parser.error("choose either config overrides or artifact directories")
    common = [item for key in ("pair", "profile", "size", "repeat", "browser", "timeout", "seed")
              for item in ("--" + key, getattr(args, key))]
    plan = json.loads(subprocess.check_output([*RUNNER, *common, "--plan"], cwd=ROOT, text=True))
    out = Path(args.out).resolve() if args.out else ROOT / "qa/results" / f"storage-ab-{time.time_ns()}.jsonl"
    variants = [(args.a_label, configs[0], args.a_artifacts), (args.b_label, configs[1], args.b_artifacts)]
    if not args.dry_run:
        out.parent.mkdir(parents=True, exist_ok=True)
        if out.exists():
            parser.error(f"refusing to append a new A/B experiment to existing file: {out}")
        # Build shared artifacts once. Explicit artifact directories are never rebuilt.
        for directory in ([args.a_artifacts, args.b_artifacts] if args.a_artifacts else [None]):
            command = [*RUNNER, "--prepare-only"]
            if directory:
                command += ["--artifacts", directory]
            elif args.no_build:
                command += ["--no-build"]
            subprocess.run(command, cwd=ROOT, check=True)
    errors = []
    try:
        for cell in plan:
            ordered = variants if cell["repetition"] % 2 else list(reversed(variants))
            for label, config, directory in ordered:
                command = [*RUNNER, "--no-build", "--pair", cell["pair"], "--profile", cell["profile"],
                           "--size", str(cell["size"]), "--seed", str(cell["seed"]),
                           "--repetition", str(cell["repetition"]), "--direction", cell["order"],
                           "--browser", args.browser, "--timeout", args.timeout,
                           "--label", label, "--config-override", json.dumps(config), "--out", str(out)]
                if directory:
                    command += ["--artifacts", directory]
                if args.dry_run:
                    command += ["--dry-run"]
                print(f'{label}: {cell["direction"]} {cell["profile"]} {cell["size"]}B repetition {cell["repetition"]}', file=sys.stderr)
                result = subprocess.run(command, cwd=ROOT, text=True, stdout=subprocess.PIPE)
                if args.dry_run:
                    print(result.stdout, end="")
                elif result.returncode and not result.stdout.strip():
                    errors.append({"label": label, "cell": cell, "error": "runner failed before emitting an attempt"})
                if result.returncode not in (0, 1):
                    raise RuntimeError(f"runner exited {result.returncode}")
    except KeyboardInterrupt:
        errors.append({"error": "interrupted"})
    finally:
        if not args.dry_run:
            rows = load_rows([out]) if out.exists() else []
            summary = summarise(rows)
            paired = paired_summary(rows, args.a_label, args.b_label)
            report = {"schema": 1, "rows": str(out), "summary": summary, "paired": paired, "errors": errors}
            path = out.with_suffix(".summary.json")
            path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
            print_table(summary)
            print("Paired deltas (B - A):")
            for cell in paired:
                delta = cell["medianCompletionDeltaMs_BminusA"]
                print(f'{cell["pair"]} {cell["direction"]} {cell["order"]} {cell["profile"]} {cell["size"]}B: '
                      f'{cell["bothSuccessful"]}/{cell["pairs"]} both successful; median completion delta '
                      + ("n/a" if delta is None else f"{delta:.2f} ms"))
            print(f"JSON: {path}")
    if errors or (not args.dry_run and any(not row["success"] for row in rows)):
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
