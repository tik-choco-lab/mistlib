"""Summarise storage JSONL attempts; failed attempts remain in success/goodput counts."""

import argparse
from collections import defaultdict
import json
from pathlib import Path
import statistics


GROUP = ("label", "pair", "direction", "profile", "size")


def load_rows(paths):
    rows = []
    for path in paths:
        with open(path, encoding="utf-8") as stream:
            for number, line in enumerate(stream, 1):
                if line.strip():
                    try:
                        row = json.loads(line)
                        if row.get("schema") != 1 or "success" not in row:
                            raise ValueError("not a storage benchmark row")
                        rows.append(row)
                    except (ValueError, AttributeError) as error:
                        raise ValueError(f"{path}:{number}: {error}") from error
    return rows


def median(values):
    values = [value for value in values if value is not None]
    return statistics.median(values) if values else None


def summarise(rows):
    groups = defaultdict(list)
    for row in rows:
        groups[tuple(row.get(key) for key in GROUP)].append(row)
    result = []
    for key, attempts in sorted(groups.items()):
        result.append({**dict(zip(GROUP, key)), "attempts": len(attempts),
                       "successes": sum(bool(row["success"]) for row in attempts),
                       "medianCompletionMs": median(row.get("completionMs") for row in attempts if row["success"]),
                       "medianGoodputBps": median(row.get("goodputBps") for row in attempts),
                       "medianControlRttP95Ms": median(row.get("controlRttP95Ms") for row in attempts),
                       "medianMaxNoProgressMs": median(row.get("maxNoProgressMs") for row in attempts),
                       "medianDuplicateBytes": median(row.get("duplicateBytes") for row in attempts),
                       "medianSteadyGoodputBps": median(row.get("steadyGoodputBps") for row in attempts),
                       "medianPartialBytes": median(row.get("partialBytes") for row in attempts),
                       "metricsComplete": sum(bool(row.get("metricsComplete")) for row in attempts),
                       "controlSamples": sum(row.get("controlSamples") or 0 for row in attempts),
                       "controlTimeouts": sum(row.get("controlTimeouts") or 0 for row in attempts),
                       "controlMissingAttempts": sum(row.get("controlSamples") is None for row in attempts)})
    return result


def print_table(summary):
    columns = ["label", "pair", "direction", "profile", "size", "success", "median ms", "median Mbit/s", "RTT p95 ms", "stall ms", "dup bytes", "steady Mbit/s", "partial bytes", "metrics"]
    def number(value, scale=1):
        return "n/a" if value is None else f"{value / scale:.2f}"
    lines = [columns]
    for row in summary:
        lines.append([str(row[key]) for key in GROUP] + [f'{row["successes"]}/{row["attempts"]}',
                     number(row["medianCompletionMs"]), number(row["medianGoodputBps"], 1e6),
                     number(row["medianControlRttP95Ms"]), number(row["medianMaxNoProgressMs"]),
                     number(row["medianDuplicateBytes"]), number(row["medianSteadyGoodputBps"], 1e6),
                     number(row["medianPartialBytes"]), f'{row["metricsComplete"]}/{row["attempts"]}'])
    widths = [max(len(line[i]) for line in lines) for i in range(len(columns))]
    for line in lines:
        print("  ".join(value.ljust(width) for value, width in zip(line, widths)))
    print("Completion: successful attempts only; goodput includes failures (0). RTT: median of attempt p95s.")


def print_series(row):
    if not row.get("seriesPath"):
        raise ValueError("row has no seriesPath")
    series = json.loads(Path(row["seriesPath"]).read_text(encoding="utf-8"))["series"]
    print(f'Run {row["id"]} ({row.get("direction", "?")})')
    print("    t (s)     rx bytes/s     tx buffered (B)    RTT (ms)  rx rate")
    previous = None
    rates = []
    for point in series:
        current = point.get("rx", {}).get("storage", {}).get("acceptedBytes")
        rate = None
        if previous and current is not None and previous[1] is not None:
            elapsed = point["t"] - previous[0]
            if elapsed > 0:
                rate = max(0, current - previous[1]) * 1000 / elapsed
        rates.append(rate)
        previous = (point["t"], current)
    peak = max((rate or 0 for rate in rates), default=0)
    def number(value):
        return "n/a" if value is None else f"{value:.1f}"
    for point, rate in zip(series, rates):
        nodes = point.get("tx", {}).get("nodes", [])
        buffered = [node.get("transport", {}).get(key) for node in nodes
                    for key in ("reliableBufferedAmount", "reliableUnorderedBufferedAmount")]
        buffered = [value for value in buffered if value is not None]
        rtts = [stat["currentRoundTripTime"] * 1000 for end in ("rx", "tx")
                for stat in point.get(end, {}).get("rtc", []) if "currentRoundTripTime" in stat]
        bar = "#" * round(32 * rate / peak) if peak and rate is not None else ""
        print(f'{point["t"] / 1000:9.3f}  {number(rate):>13}  '
              f'{number(sum(buffered) if buffered else None):>18}  '
              f'{number(max(rtts) if rtts else None):>10}  {bar}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("files", nargs="+")
    parser.add_argument("--out", help="optional summary JSON path")
    parser.add_argument("--series", metavar="ROW_ID", help="print one run's 1 Hz ASCII timeline")
    args = parser.parse_args()
    rows = load_rows(args.files)
    if args.series:
        matches = [row for row in rows if row.get("id") == args.series]
        if len(matches) != 1:
            parser.error("--series must identify exactly one row by id")
        print_series(matches[0])
        return
    summary = summarise(rows)
    print_table(summary)
    if args.out:
        Path(args.out).write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
