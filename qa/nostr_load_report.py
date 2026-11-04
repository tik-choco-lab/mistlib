#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""Summarize SPEC-23 events.jsonl; keep failed trials and 1-second demand peaks."""
import argparse
from collections import Counter, defaultdict
import json
from pathlib import Path
import re

PAIR = re.compile(r'([a-zA-Z_]+)=("(?:[^"\\]|\\.)*"|[^\s]+)')
TAG = re.compile(r'\[(' + '|'.join(("NostrSend", "NostrOK", "NostrStats", "NostrReset",
                                  "NostrReceive", "NostrEOSE", "NostrBinding", "NostrIdentity",
                                  "IceCandidate", "IceState", "IceWatchdog", "ConnTiming",
                                  "IsolationRecovery", "PairBackoff", "IdentityRecovery", "DiscoveryRetired", "NostrGovernor")) + r')\]')


def parse_log(line):
    match = TAG.search(line)
    if not match:
        return None, {}
    return match[1], dict(PAIR.findall(line[match.end():]))


def analyze(path):
    records = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
    manifest = next((r for r in records if r["record"] == "manifest"), {})
    end = next((r["t"] for r in records if r["record"] == "measurement_end"), None)
    failures = [r.get("error") for r in records if r["record"] == "failure"]
    if not any(r["record"] == "result" and r.get("success") for r in records):
        failures.append("missing successful result")
    recovery = next((r.get("recovery_seconds") for r in records if r["record"] == "all_reconnected"), None)
    transitions = defaultdict(Counter)
    nodes, bins, candidates = {}, defaultdict(Counter), defaultdict(Counter)
    identities, replay, subscriptions = {}, [], Counter()
    current_pc, identity_history = {}, defaultdict(list)
    relay_stats = None
    samples, history = {}, defaultdict(list)
    # Learn all identities first: an old anonymous discovery may be replayed
    # before a binding is observed by the newcomer.
    for record in records:
        tag, data = parse_log(record.get("line", ""))
        if tag in ("NostrBinding", "NostrIdentity"):
            identities[data["pubkey"]] = data["node"]
            if tag == "NostrIdentity":
                identity_history[data["node"]].append((record["t"], data["pubkey"]))
    for record in records:
        source, kind, t = record["source"], record["record"], record["t"]
        if kind == "relay_stats":
            relay_stats = record["stats"]
        if not source.startswith("node-"):
            continue
        node = nodes.setdefault(source, {"join": None, "first_connected": None,
                                        "publish": Counter(), "accepted": Counter(), "rejected": Counter(),
                                        "req": 0, "resets": 0, "attempts": 0, "connections": 0,
                                        "targets": None, "unanswered": None})
        if kind == "join":
            node["join"] = t
        if end is not None and t > end:
            continue
        if kind == "event" and record.get("kind") == "connected" and node["first_connected"] is None:
            node["first_connected"] = t - node["join"] if node["join"] is not None else None
        if kind != "log":
            continue
        tag, data = parse_log(record["line"])
        if tag in ("IsolationRecovery", "PairBackoff", "IdentityRecovery", "DiscoveryRetired", "NostrGovernor"):
            transitions[source][f"{tag}:{data.get('action', 'unknown')}"] += 1
        if tag == "NostrSend":
            metric = "req" if data["type"] == "req" else "publish"
            label = "" if metric == "req" else data["type"]
            history[(source, metric, label)].append(t)
        elif tag == "NostrOK":
            metric = "accepted" if data["accepted"] == "true" else "rejected"
            history[(source, metric, data["type"])].append(t)
        elif tag == "NostrReset":
            history[(source, "resets", "")].append(t)
        elif tag == "NostrStats":
            if "type" in data:
                for metric in ("publish", "accepted", "rejected"):
                    samples[(source, metric, data["type"])] = (t, int(data[metric]))
            elif "req" in data:
                for metric in ("req", "resets"):
                    samples[(source, metric, "")] = (t, int(data[metric]))
        bucket = bins[(source, int(t))]
        if tag == "NostrSend":
            label = data["type"]
            if label == "req":
                node["req"] += 1
                bucket["req"] += 1
                if "subscription" in data:
                    subscriptions[(source, data["relay"], data["subscription"])] = 0
                # One slot per REQ on each subscription; receive-side logs below
                # count all delivery copies (including duplicate/replayed IDs).
            else:
                node["publish"][label] += 1
                bucket[f"publish:{label}"] += 1
                if label in ("candidate", "candidates"):
                    owner = next((owner for pubkey, owner in identities.items()
                                  if pubkey.startswith(data.get("target", "unknown"))), None)
                    pc = current_pc.get((source, owner))
                    if pc:
                        candidates[(source, pc, owner)]["candidate_events"] += 1
        elif tag == "NostrOK":
            status = "accepted" if data["accepted"] == "true" else "rejected"
            node[status][data["type"]] += 1
            bucket[f"{status}:{data['type']}"] += 1
        elif tag == "NostrReset":
            node["resets"] += 1
            bucket["resets"] += 1
        elif tag == "NostrStats" and "targets" in data:
            node["targets"], node["unanswered"] = int(data["targets"]), int(data["unanswered"])
            bucket["targets"], bucket["unanswered"] = node["targets"], node["unanswered"]
        elif tag == "ConnTiming":
            if data.get("kind") == "attempt_start":
                node["attempts"] += 1
            elif data.get("kind") in ("connect", "reconnect"):
                node["connections"] += 1
                if node["first_connected"] is None and node["join"] is not None:
                    node["first_connected"] = t - node["join"]
        elif tag == "IceState" and data.get("kind") == "created":
            current_pc[(source, data["peer"])] = data["pc_id"]
            candidates[(source, data["pc_id"], data["peer"])]["created"] += 1
        elif tag == "IceCandidate" and "pc_id" in data:
            counts = candidates[(source, data["pc_id"], data["peer"])]
            if data.get("action") == "generated":
                counts[data["type"]] += 1
            elif data.get("action") in ("receive", "buffer", "apply"):
                counts[data["action"]] += 1
                if data.get("action") == "apply" and data.get("success") == "false":
                    counts["apply_failed"] += 1
        elif tag == "IceWatchdog":
            bucket["watchdogs"] += 1
            if "pc_id" in data:
                counts = candidates[(source, data["pc_id"], data["peer"])]
                for metric, field in (("receive", "received"), ("buffer", "buffered_total"),
                                      ("apply", "applied"), ("apply_failed", "apply_failed")):
                    counts[metric] = max(counts[metric], int(data.get(field, 0)))
                classified = sum(counts[kind] for kind in ("host", "srflx", "relay", "prflx", "unknown"))
                counts["unknown"] += max(0, int(data.get("generated", 0)) - classified)
        elif tag == "NostrReceive":
            key = (source, data["relay"], data["subscription"])
            subscriptions[key] += 1
            owner = identities.get(data["pubkey"])
            known = [key for at, key in identity_history.get(owner, []) if at <= t]
            identity_class = "unknown" if owner is None else (
                "old_identity" if known and data["pubkey"] != known[-1] else "known_identity")
            replay.append({"t": t, "node": source, "subscription": data["subscription"],
                           "identity_class": identity_class,
                           "event_id": data["event_id"], "pubkey": data["pubkey"],
                           "owner": identities.get(data["pubkey"]), "kind": int(data["kind"]),
                           "created_at": int(data["created_at"])})
        elif tag == "NostrEOSE":
            key = (source, data["relay"], data["subscription"])
            replay.append({"t": t, "node": source, "subscription": data["subscription"],
                           "delivery_count_since_eose": subscriptions.pop(key, 0),
                           "note": "includes live deliveries interleaved by relay"})
    # FFI's existing nonblocking callback can drop individual logs. Cumulative
    # counters recover totals through each sample; only the unobserved tail
    # after that sample remains dependent on event logs. Never invent times for
    # recovered events: one-second bins remain explicitly observed lower bounds.
    recovered = Counter()
    for (source, metric, label), (sample_time, count) in samples.items():
        before = len(history[(source, metric, label)])
        total = count + sum(t > sample_time for t in history[(source, metric, label)])
        if metric == "publish":
            recovered[source] += max(0, total - before)
        if metric in ("req", "resets"):
            nodes[source][metric] = max(nodes[source][metric], total)
        else:
            nodes[source][metric][label] = max(nodes[source][metric][label], total)
    rows = []
    for source, node in sorted(nodes.items()):
        duration = max(0.000001, (end if end is not None else records[-1]["t"]) - (node["join"] or 0))
        peak = max((sum(v for k, v in counts.items() if k.startswith("publish:"))
                    for (name, _), counts in bins.items() if name == source), default=0)
        rows.append({"run": str(path.parent), "scenario": manifest.get("scenario", "unknown"),
                     "node": source, "seconds": round(duration, 6), "failed": bool(failures),
                     "errors": failures, "events": sum(node["publish"].values()),
                     "events_per_s": round(sum(node["publish"].values()) / duration, 6),
                     "peak_events_per_s": peak, "publish_logs_missing": recovered[source],
                     "peak_is_lower_bound": True, "req": node["req"],
                     "req_per_s": round(node["req"] / duration, 6),
                     "rejected": sum(node["rejected"].values()), "publish_by_type": dict(node["publish"]),
                     "accepted_by_type": dict(node["accepted"]), "rejected_by_type": dict(node["rejected"]),
                     "resets": node["resets"], "connected": node["first_connected"] is not None,
                     "first_connected_s": round(node["first_connected"], 6) if node["first_connected"] is not None else None,
                     "attempts": node["attempts"], "connections": node["connections"],
                     "attempt_success_rate": node["connections"] / node["attempts"] if node["attempts"] else None,
                     "recovery_seconds": recovery, "transitions": dict(transitions[source]),
                     "targets": node["targets"], "unanswered": node["unanswered"]})
    if not rows:
        rows.append({"run": str(path.parent), "scenario": manifest.get("scenario", "unknown"),
                     "node": "none", "failed": True, "errors": failures})
    # Explicit zero rows preserve quiet periods rather than omitting demand.
    for source, node in nodes.items():
        if node["join"] is not None:
            finish = end if end is not None else records[-1]["t"]
            for second in range(int(node["join"]), int(finish) + 1):
                bins[(source, second)]
    series = [{"node": source, "second": second, **dict(counts)}
              for (source, second), counts in sorted(bins.items())]
    attempts = [{"node": source, "pc_id": pc, "peer": peer,
                 "total": sum(counts[kind] for kind in ("host", "srflx", "relay", "prflx", "unknown")),
                 "candidate_events": counts["candidate_events"], "receive": counts["receive"],
                 "buffer": counts["buffer"], "apply": counts["apply"], "apply_failed": counts["apply_failed"],
                 **{kind: counts[kind] for kind in ("host", "srflx", "relay", "prflx", "unknown")}}
                for (source, pc, peer), counts in sorted(candidates.items())]
    return rows, series, attempts, replay, relay_stats


def write_jsonl(path, rows):
    path.write_text("".join(json.dumps(row, ensure_ascii=False) + "\n" for row in rows))


def relay_series(path):
    previous, last_t, rows = {}, None, []
    for line in path.read_text().splitlines():
        record = json.loads(line)
        if record["record"] != "relay_stats":
            continue
        stats = record["stats"]
        # CLI emits at 1s cadence. Use the relay's own timestamps for subsequent
        # intervals, independent of Python stdout scheduling.
        now = stats["time_ms"] / 1000
        interval = now - last_t if last_t is not None else 1
        for connection, counts in (stats.get("connections") or {}).items():
            before = previous.get(connection, {})
            kinds = {}
            for kind, values in counts["kinds"].items():
                old = before.get("kinds", {}).get(kind, {})
                kinds[kind] = {key: values[key] - old.get(key, 0) for key in ("accepted", "rejected")}
            events = sum(sum(values.values()) for values in kinds.values())
            req = counts["req"] - before.get("req", 0)
            rows.append({"t": record["t"], "connection": connection,
                         "interval_s": round(interval, 6), "events": events,
                         "events_per_s": round(events / interval, 6) if interval > 0 else None,
                         "req": req, "kinds": kinds, "final": stats["final"]})
        previous, last_t = stats.get("connections") or {}, now
    return rows


def summarize(path):
    rows, series, attempts, replay, relay = analyze(path)
    write_jsonl(path.with_name("summary.jsonl"), rows)
    path.with_name("summary.md").write_text(table(rows))
    write_jsonl(path.with_name("series.jsonl"), series)
    write_jsonl(path.with_name("relay-series.jsonl"), relay_series(path))
    write_jsonl(path.with_name("candidate-attempts.jsonl"), attempts)
    write_jsonl(path.with_name("discovery-deliveries.jsonl"), replay)
    if relay:
        path.with_name("relay-final.json").write_text(json.dumps(relay, indent=2) + "\n")
        relay_events = sum(sum(values.values()) for conn in (relay["connections"] or {}).values()
                           for values in conn["kinds"].values())
        relay_req = sum(conn["req"] for conn in (relay["connections"] or {}).values())
        node_events = sum(row.get("events", 0) for row in rows)
        node_req = sum(row.get("req", 0) for row in rows)
        consistency = {"node_events_window": node_events, "relay_events_final": relay_events,
                       "event_difference": relay_events - node_events,
                       "node_req_window": node_req, "relay_req_final": relay_req,
                       "req_difference": relay_req - node_req,
                       "note": "relay final may include shutdown writes outside the measurement window"}
        path.with_name("consistency.json").write_text(json.dumps(consistency, indent=2) + "\n")
    return rows


def table(rows):
    columns = [("scenario", "scenario"), ("node", "node"), ("failed", "failed"),
               ("events", "EVENT"), ("events_per_s", "ev/s"), ("peak_events_per_s", "observed peak/s"),
               ("publish_logs_missing", "recovered sends"),
               ("req", "REQ"), ("rejected", "reject"), ("resets", "reset"),
               ("connected", "connected"), ("first_connected_s", "first(s)"),
               ("recovery_seconds", "S5 recovery(s)"),
               ("attempts", "attempts"), ("connections", "connections"),
               ("targets", "targets"), ("unanswered", "unanswered")]
    lines = ["| " + " | ".join(label for _, label in columns) + " |",
             "| " + " | ".join("---" for _ in columns) + " |"]
    for row in rows:
        lines.append("| " + " | ".join(str(row.get(key, "-")) for key, _ in columns) + " |")
    lines.extend(["", "| scenario | node | type | publish | ev/s | accepted | rejected |",
                  "| --- | --- | --- | --- | --- | --- | --- |"])
    for row in rows:
        published = row.get("publish_by_type", {})
        accepted = row.get("accepted_by_type", {})
        rejected = row.get("rejected_by_type", {})
        for label in sorted(published.keys() | accepted.keys() | rejected.keys()):
            rate = round(published.get(label, 0) / row["seconds"], 6)
            lines.append(f"| {row.get('scenario', 'unknown')} | {row['node']} | {label} | {published.get(label, 0)} | {rate} | {accepted.get(label, 0)} | {rejected.get(label, 0)} |")
    lines.extend(["", "| scenario | node | transition | count |", "| --- | --- | --- | --- |"])
    for row in rows:
        for transition, count in sorted(row.get("transitions", {}).items()):
            lines.append(f"| {row.get('scenario', 'unknown')} | {row['node']} | {transition} | {count} |")
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", type=Path, nargs="+")
    args = parser.parse_args()
    rows = []
    for path in args.paths:
        paths = sorted(path.rglob("events.jsonl")) if path.is_dir() else [path]
        group = []
        for candidate in paths:
            group.extend(summarize(candidate))
        if path.is_dir():
            (path / "summary.md").write_text(table(group))
        rows.extend(group)
    print(table(rows), end="")


if __name__ == "__main__":
    main()
