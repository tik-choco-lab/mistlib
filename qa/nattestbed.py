#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""Isolated IPv4 NAT experiments with unprivileged peer pairs and local STUN.

All veth ends, routes, nft rules, netem/clsact qdiscs and tc actions live
inside the slot's namespaces. Legacy netem shapes all four WAN ends; directional paths shape
only the source router's WAN egress.
Peer commands are argv, not shell scripts. Batch jobs are JSON Lines.
"""

import argparse
import asyncio
import dataclasses
from dataclasses import dataclass
import errno
import hashlib
import ipaddress
import json
import math
import os
from pathlib import Path
import random
import re
import select
import shlex
import signal
import socket
import statistics
import struct
import subprocess
import sys
import tempfile
import time

try:
    from . import testbed
except ImportError:
    import testbed


PREFIX = "mistnat-"
ROLES = ("a", "na", "wan", "nb", "b")
NAT_TYPES = ("none", "eim", "symmetric", "forward")
STUN_IPS = ("198.18.0.1", "198.18.1.1")
STUN_PORT = 3478
FORWARD_PORT = 40000
DEFAULT_TIMEOUT = 60
COOKIE = 0x2112A442
SCRIPT = str(Path(__file__).resolve())
PROFILES = Path(__file__).with_name("nattestbed-profiles.json")
# Both ends of each WAN link are measured and shaped independently.
WAN_INTERFACES = (("na", "na-wan"), ("wan", "wan-na"),
                  ("wan", "wan-nb"), ("nb", "nb-wan"))
COUNTERS = ("rxBytes", "txBytes", "rxPackets", "txPackets")
PATH_FIELDS = ("pathAB", "pathBA", "bottleneckAB", "bottleneckBA",
               "policerAB", "policerBA")
LOSS_TIME_FIELDS = {"ab": "lossTimeAB", "ba": "lossTimeBA"}
IMPAIRMENT_FIELDS = (set(PATH_FIELDS) | set(LOSS_TIME_FIELDS.values())
                     | {"jitterNoReorder", "schedule", "cross"})
DIRECTION_FIELDS = {
    "ab": ("pathAB", "bottleneckAB", "policerAB"),
    "ba": ("pathBA", "bottleneckBA", "policerBA"),
}
PATH_DEVICES = {"ab": ("na", "na-wan"), "ba": ("nb", "nb-wan")}
# Time-domain loss: clsact egress match-all u32 -> shared gact action. clsact
# egress runs in __dev_queue_xmit before the root qdisc enqueue, so toggling
# the action only decides the fate of packets not yet handed to netem.
# (u32 rather than matchall: cls_u32 is the classifier most commonly resident.)
LOSS_TIME_MODULES = ("sch_ingress", "cls_u32", "act_gact")
GACT_INDEX = 1
DEFAULT_LIMIT = 1000
QUEUE_PACKET_BYTES = 1500
ORDERED_JITTER_RATE = "100gbit"
CROSS_PORT_START = 41000
JOB_FIELDS = {"wan", "wanReady", "pairs", "pairStartOffsetsS", "blockQuic"}
# blockQuic: the WAN drops every forwarded UDP datagram whose first payload
# byte has both high bits set (a QUIC long header: Initial / 0-RTT /
# Handshake / Retry). STUN (00), DTLS (00/01: 20..63), RTP/RTCP (10) and
# QUIC short headers (01) pass, so the QUIC handshake never completes while
# WebRTC is untouched -- a middlebox that blocks QUIC by DPI (SPEC-25 KR 2.4).
QUIC_BLOCK_RULES = """table ip mistquicblock {
    chain forward {
        type filter hook forward priority filter - 10; policy accept;
        meta l4proto udp @th,64,2 3 counter drop
    }
}
"""


def positive_int(value, field):
    if type(value) is not int or value <= 0:
        raise ValueError(f"{field} must be a positive integer")
    return value


def finite_number(value, field, *, zero=False):
    if (type(value) not in (int, float) or not math.isfinite(value)
            or value < 0 or (not zero and value == 0)):
        raise ValueError(f"{field} must be finite and {'nonnegative' if zero else 'positive'}")
    return value


def argv_array(value):
    return (isinstance(value, (list, tuple)) and bool(value) and bool(value[0])
            and all(isinstance(arg, str) and '\0' not in arg for arg in value))


def command_arrays(value, field, *, count=None, optional=False):
    if argv_array(value):
        return (tuple(value),) * (count if count is not None else 1)
    if (not isinstance(value, (list, tuple)) or (not value and not optional)
            or not all(argv_array(command) for command in value)
            or (count is not None and len(value) != count)):
        raise ValueError(f"{field} must be argv or an array of argv arrays"
                         + (f" of length {count}" if count is not None else ""))
    return tuple(tuple(command) for command in value)


def netem_tokens(value, field):
    if value is None:
        return []
    if not isinstance(value, str) or not (tokens := shlex.split(value)):
        raise ValueError(f"{field} must be a nonempty argument string or null")
    if any("\0" in token for token in tokens):
        raise ValueError(f"{field} contains a NUL")
    return tokens


def validate_bottleneck(value, field):
    if value is None:
        return
    if (not isinstance(value, dict) or "rateKbit" not in value
            or set(value) - {"rateKbit", "queueMs", "limitPackets"}
            or ("queueMs" in value and "limitPackets" in value)):
        raise ValueError(f"{field} needs rateKbit and at most one of queueMs or limitPackets")
    positive_int(value["rateKbit"], f"{field}.rateKbit")
    if "queueMs" in value:
        finite_number(value["queueMs"], f"{field}.queueMs")
    if "limitPackets" in value:
        positive_int(value["limitPackets"], f"{field}.limitPackets")


def validate_policer(value, field):
    if value is None:
        return
    if not isinstance(value, dict) or set(value) != {"rateKbit", "burstBytes"}:
        raise ValueError(f"{field} needs rateKbit and burstBytes")
    positive_int(value["rateKbit"], f"{field}.rateKbit")
    positive_int(value["burstBytes"], f"{field}.burstBytes")


def validate_loss_time(value, field):
    """{"rate": 0<r<1, "meanBurstMs": >=1, "seed": optional uint64}."""
    if value is None:
        return
    if (not isinstance(value, dict) or not {"rate", "meanBurstMs"} <= set(value)
            or set(value) - {"rate", "meanBurstMs", "seed"}):
        raise ValueError(f"{field} needs rate and meanBurstMs (optional seed)")
    rate = value["rate"]
    if type(rate) not in (int, float) or not math.isfinite(rate) or not 0 < rate < 1:
        raise ValueError(f"{field}.rate must be a mean loss fraction strictly between 0 and 1")
    if finite_number(value["meanBurstMs"], f"{field}.meanBurstMs") < 1:
        raise ValueError(f"{field}.meanBurstMs must be at least 1 ms")
    if "seed" in value and (type(value["seed"]) is not int or not 0 <= value["seed"] < 2 ** 64):
        raise ValueError(f"{field}.seed must be an integer in 0..2^64-1")


def loss_time_periods(config, seed):
    """Endless alternating (bad, durationMs) periods of a time-domain
    Gilbert-Elliott process: bad ~ Exp(meanBurstMs) with 100 % loss, good ~
    Exp(meanBurstMs * (1 - rate) / rate) with 0 % loss. The first state is
    drawn from the stationary distribution (memorylessness makes the first,
    residual period exponential too)."""
    validate_loss_time(config, "lossTime")
    rng = random.Random(seed)
    rate, bad_mean = config["rate"], config["meanBurstMs"]
    means = {True: bad_mean, False: bad_mean * (1 - rate) / rate}
    bad = rng.random() < rate
    while True:
        yield bad, rng.expovariate(1 / means[bad])
        bad = not bad


def loss_time_schedule(config, seed, duration_ms):
    """Bad intervals [(startMs, endMs)] in [0, duration_ms), as the controller
    schedules them (before toggle latency)."""
    intervals, at = [], 0.0
    for bad, length in loss_time_periods(config, seed):
        if at >= duration_ms:
            break
        if bad:
            intervals.append((at, min(at + length, duration_ms)))
        at += length
    return intervals


def interval_stats(intervals, start, end):
    """Bad-period count/total/fraction/mean of intervals clipped to [start, end)."""
    clipped = [(max(s, start), min(e, end)) for s, e in intervals if e > start and s < end]
    total = sum(e - s for s, e in clipped)
    return {"badPeriods": len(clipped), "badTotal": total,
            "badFraction": total / (end - start) if end > start else None,
            "meanBad": total / len(clipped) if clipped else None}


def representative_seed(config, duration_s, first=1, offsets_s=(0, 0.5, 1, 1.5, 2),
                        rate_pp=0.005, mean_fraction=0.15):
    """First seed >= first whose scheduled window [offset, offset+duration)
    has bad fraction within rate_pp of rate and mean bad period within
    mean_fraction of meanBurstMs, for every candidate start offset.

    A short window holds few bad periods (10 s at 5 %/25 ms: ~20), so a raw
    seed can legitimately sample 2-3 % or 7-8 %. The selftest checks the
    emulator against nominal rate/burst, so it uses a schedule that is itself
    nominal; the rule is deterministic, not a hand-picked constant."""
    horizon = (max(offsets_s) + duration_s) * 1000
    for seed in range(first, first + 100000):
        intervals = loss_time_schedule(config, seed, horizon)
        windows = [interval_stats(intervals, offset * 1000, (offset + duration_s) * 1000)
                   for offset in offsets_s]
        if all(w["meanBad"] is not None and abs(w["badFraction"] - config["rate"]) <= rate_pp
               and abs(w["meanBad"] - config["meanBurstMs"]) <= mean_fraction * config["meanBurstMs"]
               for w in windows):
            return seed
    raise RuntimeError("no representative lossTime seed found")


def loss_time_seed(job, direction, job_id=0):
    """The explicit seed, or a stable hash of (job id, job definition, direction)."""
    config = getattr(job, LOSS_TIME_FIELDS[direction])
    if "seed" in config:
        return config["seed"]
    canonical = json.dumps({"jobId": job_id, "direction": direction, "job": dataclasses.asdict(job)},
                           sort_keys=True, separators=(",", ":"))
    return int.from_bytes(hashlib.sha256(canonical.encode()).digest()[:8], "big")


def queue_limit(bottleneck):
    if "limitPackets" in bottleneck:
        return bottleneck["limitPackets"]
    if "queueMs" in bottleneck:
        # netem's limit includes packets held for propagation delay as well
        # as serialization. This is a total queue budget in nominal packets.
        return max(1, math.ceil(bottleneck["rateKbit"] * bottleneck["queueMs"]
                                / (8 * QUEUE_PACKET_BYTES)))
    return DEFAULT_LIMIT


def canonical_path_tokens(tokens):
    # tc's netem parser accepts unambiguous/first-matching keyword prefixes
    # and the latency/drop aliases. Normalize its option names before
    # detecting rate/limit conflicts or resetting scheduled attributes.
    keywords = ("limit", "latency", "delay", "loss", "drop", "ecn", "reorder",
                "corrupt", "gap", "duplicate", "distribution", "rate", "slot")
    aliases = {"latency": "delay", "drop": "loss"}
    result = []
    distribution_name = False
    for token in tokens:
        if distribution_name:
            # Distribution table names are values, not option names.
            result.append(token)
            distribution_name = False
            continue
        if token and token.isalpha():
            token = next((key for key in keywords if key.startswith(token)), token)
            token = aliases.get(token, token)
        result.append(token)
        distribution_name = token == "distribution"
    return result


def path_arguments(path, bottleneck, *, no_reorder=False):
    tokens = canonical_path_tokens(netem_tokens(path, "path"))
    if bottleneck is not None:
        if "rate" in tokens or "limit" in tokens:
            raise ValueError("path rate/limit and bottleneck must not be combined in the same direction")
        tokens += ["rate", f"{bottleneck['rateKbit']}kbit", "limit", str(queue_limit(bottleneck))]
    if no_reorder:
        if any(token in tokens for token in ("reorder", "gap")):
            raise ValueError("jitterNoReorder cannot be combined with explicit reorder/gap")
        # Linux 6.8 netem uses the tail's scheduled departure as a floor
        # when rate is set. Use a very high rate when capacity is unspecified,
        # so the only substantial effect is the nondecreasing departure time.
        # A child pfifo receives packets *after* netem has time-sorted them.
        if "rate" not in tokens:
            tokens += ["rate", ORDERED_JITTER_RATE]
        else:
            idx = tokens.index("rate")
            if idx + 1 == len(tokens) or re.fullmatch(r"0+(?:\.0+)?(?:[a-zA-Z]+|%)?", tokens[idx + 1]):
                raise ValueError("jitterNoReorder requires a nonzero netem rate")
    return tokens


def validate_paths(state, no_reorder):
    for path, bottleneck, policer in DIRECTION_FIELDS.values():
        netem_tokens(state.get(path), path)
        validate_bottleneck(state.get(bottleneck), bottleneck)
        validate_policer(state.get(policer), policer)
        path_arguments(state.get(path), state.get(bottleneck), no_reorder=no_reorder)


def validate_schedule(schedule, initial, no_reorder):
    if not isinstance(schedule, (list, tuple)):
        raise ValueError("schedule must be an array")
    previous = -1
    state = dict(initial)
    for item in schedule:
        if (not isinstance(item, dict) or "atS" not in item
                or not set(item).intersection(PATH_FIELDS)
                or set(item) - ({"atS"} | set(PATH_FIELDS))):
            raise ValueError("schedule entries need atS and one or more path/bottleneck/policer fields")
        at = finite_number(item["atS"], "schedule.atS", zero=True)
        if at <= previous:
            raise ValueError("schedule.atS must be strictly increasing")
        previous = at
        new = {**state, **{key: item[key] for key in PATH_FIELDS if key in item}}
        validate_paths(new, no_reorder)
        for path, bottleneck, _ in DIRECTION_FIELDS.values():
            old_tokens = path_arguments(state[path], state[bottleneck])
            new_tokens = path_arguments(new[path], new[bottleneck])
            if old_tokens != new_tokens and any(t in old_tokens and t not in new_tokens
                                               for t in ("distribution", "slot", "ecn")):
                # These optional netlink attributes can persist on change;
                # refuse ambiguous removal rather than silently retaining it.
                raise ValueError("schedule cannot remove netem distribution, slot or ecn in-place")
        state = new


def validate_cross(cross):
    if not isinstance(cross, (list, tuple)):
        raise ValueError("cross must be an array")
    for entry in cross:
        if (not isinstance(entry, dict)
                or set(entry) != {"dir", "proto", "cc", "flows", "startS", "durationS"}):
            raise ValueError("cross entries need dir, proto, cc, flows, startS and durationS")
        if entry["dir"] not in ("ab", "ba") or entry["proto"] != "tcp" or entry["cc"] not in ("cubic", "bbr"):
            raise ValueError("cross needs dir ab/ba, proto tcp and cc cubic/bbr")
        positive_int(entry["flows"], "cross.flows")
        finite_number(entry["startS"], "cross.startS", zero=True)
        finite_number(entry["durationS"], "cross.durationS")
    if len(cross) > 65536 - CROSS_PORT_START:
        raise ValueError("too many cross entries for distinct TCP sink ports")


def load_profiles(path=PROFILES):
    profiles = json.loads(Path(path).read_text())
    if not isinstance(profiles, dict):
        raise ValueError("profiles file must be a JSON object")
    for name, profile in profiles.items():
        if name.startswith("_"):
            continue
        if not isinstance(profile, dict) or set(profile) - IMPAIRMENT_FIELDS - {"_comment"}:
            raise ValueError(f"invalid impairment profile: {name}")
    return profiles


def expand_profile(data, profiles=None):
    if "profile" not in data:
        return data
    name = data["profile"]
    if not isinstance(name, str) or name.startswith("_"):
        raise ValueError("profile must name an impairment profile")
    profiles = load_profiles() if profiles is None else profiles
    if name not in profiles:
        raise ValueError(f"unknown profile: {name}")
    return {**{k: v for k, v in profiles[name].items() if k != "_comment"}, **data}

def root_command(*args, check=True, input=None):
    command = list(args)
    if os.geteuid() != 0:
        command = ["sudo", "-n", *command]
    # Terminal-generated signals must not interrupt an in-flight ip/nft
    # command (especially during teardown). The asyncio supervisor handles
    # those signals and then finishes cleanup under the slot lock.
    return subprocess.run(command, check=check, input=input, text=True,
                          capture_output=True, start_new_session=True)


@dataclass(frozen=True)
class Side:
    role: str
    nat: str

    @property
    def router(self):
        return "n" + self.role

    @property
    def index(self):
        return int(self.role == "b")

    @property
    def lan_prefix(self):
        return (f"198.18.{2 + self.index}" if self.nat == "none"
                else f"198.19.{1 + self.index}")

    @property
    def peer_ip(self):
        return self.lan_prefix + ".2"

    @property
    def gateway(self):
        return self.lan_prefix + ".1"

    @property
    def wan_ip(self):
        return f"198.18.{self.index}.2"

    @property
    def wan_if(self):
        return self.router + "-wan"

    @property
    def lan_if(self):
        return self.router + "-lan"

    @property
    def mapped_ip(self):
        return self.peer_ip if self.nat == "none" else self.wan_ip


def nft_rules(role, nat, forward_port=FORWARD_PORT):
    """Render IPv4 rules for one router; none uses plain IP forwarding."""
    if role not in ("a", "b") or nat not in NAT_TYPES:
        raise ValueError("invalid router role or NAT type")
    if type(forward_port) is not int or not 1 <= forward_port <= 65535:
        raise ValueError("forward port must be in 1..65535")
    if nat == "none":
        return ""
    side = Side(role, nat)
    # nft spells iptables' --random-fully flag 'fully-random'. The default
    # preserves the original port if the translated tuple is available.
    # Drop unsolicited WAN UDP addressed to the router itself before INPUT
    # conntrack confirmation. Without this, an early punch may create a local
    # (non-NAT) flow and occupy the tuple needed by the peer's outgoing punch.
    flag = " fully-random" if nat == "symmetric" else ""
    dnat = (f'        iifname "{side.wan_if}" ip daddr {side.wan_ip} '
            f'udp dport {forward_port} dnat to {side.peer_ip}:{forward_port}\n'
            if nat == "forward" else "")
    return f'''table ip mistnat {{
    chain prerouting {{
        type nat hook prerouting priority dstnat; policy accept;
{dnat}    }}
    chain postrouting {{
        type nat hook postrouting priority srcnat; policy accept;
        oifname "{side.wan_if}" ip saddr {side.peer_ip} masquerade{flag}
    }}
    chain input {{
        type filter hook input priority filter; policy accept;
        iifname "{side.wan_if}" ip daddr {side.wan_ip} ip protocol udp drop
    }}
    chain forward {{
        type filter hook forward priority filter; policy drop;
        iifname "{side.lan_if}" oifname "{side.wan_if}" accept
        iifname "{side.wan_if}" oifname "{side.lan_if}" ct state established,related accept
        iifname "{side.wan_if}" oifname "{side.lan_if}" ct status dnat accept
    }}
}}
'''


@dataclass(frozen=True)
class Job:
    nat_a: str
    nat_b: str
    a: tuple
    b: tuple
    netem: str | None = None
    cpus: int = 2
    timeout: float = DEFAULT_TIMEOUT
    forward_port: int = FORWARD_PORT
    pathAB: str | None = None
    pathBA: str | None = None
    bottleneckAB: dict | None = None
    bottleneckBA: dict | None = None
    policerAB: dict | None = None
    policerBA: dict | None = None
    jitterNoReorder: bool = False
    schedule: tuple = ()
    cross: tuple = ()
    profile: str | None = None
    wan: tuple = ()
    wanReady: dict | None = None
    pairs: int = 1
    pairStartOffsetsS: tuple | None = None
    lossTimeAB: dict | None = None
    lossTimeBA: dict | None = None
    blockQuic: bool = False

    def __post_init__(self):
        if self.nat_a not in NAT_TYPES or self.nat_b not in NAT_TYPES:
            raise ValueError(f"NAT type must be one of {', '.join(NAT_TYPES)}")
        positive_int(self.pairs, "pairs")
        command_arrays(self.a, "a", count=self.pairs)
        command_arrays(self.b, "b", count=self.pairs)
        services = command_arrays(self.wan, "wan", optional=True)
        if self.wanReady is not None:
            ready = self.wanReady
            if (not services or not isinstance(ready, dict) or "tcpPort" not in ready
                    or set(ready) - {"tcpPort", "timeoutS"}):
                raise ValueError("wanReady requires wan services and tcpPort (optional timeoutS)")
            if type(ready["tcpPort"]) is not int or not 1 <= ready["tcpPort"] <= 65535:
                raise ValueError("wanReady.tcpPort must be in 1..65535")
            finite_number(ready.get("timeoutS", 10), "wanReady.timeoutS")
        if self.pairStartOffsetsS is not None:
            if (not isinstance(self.pairStartOffsetsS, (list, tuple))
                    or len(self.pairStartOffsetsS) != self.pairs):
                raise ValueError("pairStartOffsetsS must be an array of length pairs")
            for offset in self.pairStartOffsetsS:
                finite_number(offset, "pairStartOffsetsS", zero=True)
        netem_tokens(self.netem, "netem")
        if type(self.cpus) is not int or self.cpus < 1:
            raise ValueError("cpus must be a positive integer")
        if (type(self.timeout) not in (int, float) or not math.isfinite(self.timeout)
                or self.timeout <= 0):
            raise ValueError("timeout must be finite and positive")
        if type(self.forward_port) is not int or not 1 <= self.forward_port <= 65535:
            raise ValueError("forwardPort must be in 1..65535")
        if type(self.jitterNoReorder) is not bool:
            raise ValueError("jitterNoReorder must be boolean")
        if type(self.blockQuic) is not bool:
            raise ValueError("blockQuic must be boolean")
        state = self.path_state()
        validate_paths(state, self.jitterNoReorder)
        validate_schedule(self.schedule, state, self.jitterNoReorder)
        validate_cross(self.cross)
        for field in LOSS_TIME_FIELDS.values():
            validate_loss_time(getattr(self, field), field)
        if self.netem is not None and (any(state[k] is not None for k in PATH_FIELDS[:4])
                or self.jitterNoReorder or any(set(item).intersection(PATH_FIELDS[:4]) for item in self.schedule)
                or self.loss_time_directions()):
            raise ValueError("netem cannot be combined with directional paths/bottlenecks, "
                             "lossTime or jitterNoReorder")

    def loss_time_directions(self):
        return [direction for direction, field in LOSS_TIME_FIELDS.items()
                if getattr(self, field) is not None]

    def path_state(self):
        return {key: getattr(self, key) for key in PATH_FIELDS}

    def peer_commands(self, role):
        return command_arrays(getattr(self, role), role, count=self.pairs)

    def wan_commands(self):
        return command_arrays(self.wan, "wan", optional=True)

    def managed_paths(self):
        return {direction for direction, (path, bottleneck, _) in DIRECTION_FIELDS.items()
                if getattr(self, path) is not None or getattr(self, bottleneck) is not None
                or any(path in item or bottleneck in item for item in self.schedule)}


def parse_job(line, cpus=2, profiles=None):
    if not line.strip():
        return None
    data = json.loads(line)
    if not isinstance(data, dict):
        raise ValueError("each job must be a JSON object")
    required = {"natA", "natB", "a", "b"}
    allowed = required | {"netem", "cpus", "timeout", "forwardPort", "profile"} | IMPAIRMENT_FIELDS | JOB_FIELDS
    if missing := required - data.keys():
        raise ValueError(f"missing job fields: {', '.join(sorted(missing))}")
    if unknown := data.keys() - allowed:
        raise ValueError(f"unknown job fields: {', '.join(sorted(unknown))}")
    if not isinstance(data["a"], list) or not isinstance(data["b"], list):
        raise ValueError("a and b must be argv arrays")
    data = expand_profile(data, profiles)
    extras = {key: data[key] for key in IMPAIRMENT_FIELDS | {"profile"} | JOB_FIELDS if key in data}
    return Job(data["natA"], data["natB"], tuple(data["a"]), tuple(data["b"]),
               data.get("netem"), data.get("cpus", cpus), data.get("timeout", DEFAULT_TIMEOUT),
               data.get("forwardPort", FORWARD_PORT), **extras)


def link_counters(links, interface):
    link, = (link for link in links if link["ifname"] == interface)
    stats = link.get("stats64") or link["stats"]
    return {"rxBytes": stats["rx"]["bytes"], "txBytes": stats["tx"]["bytes"],
            "rxPackets": stats["rx"]["packets"], "txPackets": stats["tx"]["packets"]}


def make_summary(slot, job, exits, before, after, duration, schedule=(), cross=(), error=None,
                 wan_services=(), loss_time=None, quic_block_drops=None):
    wan = {interface: {key: after[interface][key] - counts[key] for key in COUNTERS}
           for interface, counts in before.items()}
    if any(value < 0 for counts in wan.values() for value in counts.values()):
        raise ValueError("WAN interface counters decreased during the experiment")
    # Accept the original single-pair exit tuple as well.
    if len(exits) == 2 and all(code is None or type(code) is int for code in exits):
        exits = [exits]
    by_pair = [{"pair": pair, "a": a, "b": b} for pair, (a, b) in enumerate(exits)]
    return {"type": "nattestbed", "slot": slot, "natA": job.nat_a, "natB": job.nat_b,
            "netem": job.netem, "exitA": by_pair[0]["a"], "exitB": by_pair[0]["b"], "wan": wan,
            "exits": by_pair, "wanServices": list(wan_services),
            "durationS": round(duration, 6), "profile": job.profile, **job.path_state(),
            "jitterNoReorder": job.jitterNoReorder, "schedule": list(schedule),
            "cross": list(cross), "lossTimeAB": job.lossTimeAB, "lossTimeBA": job.lossTimeBA,
            "lossTime": dict(loss_time or {}), "blockQuic": job.blockQuic,
            "quicBlockDrops": quic_block_drops, "error": error}


def names_for(slot):
    if type(slot) is not int or slot < 0:
        raise ValueError("slot must be nonnegative")
    return {role: f"{PREFIX}{slot}-{role}" for role in ROLES}


def namespaces():
    return [line.split()[0] for line in root_command("ip", "netns", "list").stdout.splitlines()
            if line.split() and line.split()[0].startswith(PREFIX)]


def namespace_pids(name):
    return root_command("ip", "netns", "pids", name).stdout.split()


def ip(ns, *args):
    return root_command("ip", "-n", ns, *args)


def ns_command(ns, *args):
    return root_command("ip", "netns", "exec", ns, *args)


def install_nft(ns, rules):
    root_command("ip", "netns", "exec", ns, "nft", "-f", "-", input=rules)


def policer_rule(direction, config):
    if direction not in DIRECTION_FIELDS:
        raise ValueError("policer direction must be ab or ba")
    if config is None:
        return ""
    validate_policer(config, "policer")
    source, dest = ("wan-na", "wan-nb") if direction == "ab" else ("wan-nb", "wan-na")
    # nft counts bytes (including the IP/UDP headers) rather than payload.
    # Decimal kbit/s -> integer bytes/s.
    return (f'iifname "{source}" oifname "{dest}" ip protocol udp '
            f'limit rate over {config["rateKbit"] * 125} bytes/second '
            f'burst {config["burstBytes"]} bytes counter drop')


def policer_rules(state, *, initial=False, directions=("ab", "ba")):
    # A separate chain per direction lets schedule update one bucket without
    # resetting the other one's tokens. A single nft -f is atomic.
    lines = []
    if initial:
        lines += ["add table ip mistnat_policer",
                  "add chain ip mistnat_policer forward { type filter hook forward priority filter; policy accept; }"]
        for direction in ("ab", "ba"):
            lines += [f"add chain ip mistnat_policer {direction}",
                      f"add rule ip mistnat_policer forward jump {direction}"]
    for direction in directions:
        if not initial:
            lines.append(f"flush chain ip mistnat_policer {direction}")
        rule = policer_rule(direction, state[DIRECTION_FIELDS[direction][2]])
        if rule:
            lines.append(f"add rule ip mistnat_policer {direction} {rule}")
    return "\n".join(lines) + "\n"


def configure_path(names, direction, state, no_reorder, *, old=None):
    path, bottleneck, _ = DIRECTION_FIELDS[direction]
    role, interface = PATH_DEVICES[direction]
    ns = names[role]
    tokens = path_arguments(state[path], state[bottleneck], no_reorder=no_reorder)
    operation = "add" if old is None else "change"
    if old is not None:
        previous = path_arguments(old[path], old[bottleneck], no_reorder=no_reorder)
        if tokens == previous:
            return
        # qdisc change retains optional rate/correlation/corruption/reorder
        # attributes if not sent. iproute2 6.1 sends TCA_NETEM_CORR (all
        # delay/loss/duplicate correlations) when corrupt's optional
        # correlation argument is present, even if it is zero.
        if "rate" in previous and "rate" not in tokens:
            tokens += ["rate", "0bit"]
        if "corrupt" not in tokens:
            tokens += ["corrupt", "0%", "0%"]
        else:
            index = tokens.index("corrupt") + 2
            if index <= len(tokens) and (index == len(tokens) or not re.match(r"^[0-9]", tokens[index])):
                tokens.insert(index, "0%")
        if "reorder" in previous and "reorder" not in tokens:
            # netem's normal time-sorted FIFO is itself able to reorder
            # jittered packets; 0% disables only explicit reorder.
            tokens += ["reorder", "0%"]
    ns_command(ns, "tc", "qdisc", operation, "dev", interface, "root",
               "handle", "1:", "netem", *(tokens or ["delay", "0ms"]))


def needs_policer(job):
    return (job.policerAB is not None or job.policerBA is not None
            or any(item.get(key) is not None for item in job.schedule for key in ("policerAB", "policerBA")))


def module_resident(name):
    """Check loaded/built-in modules without requesting kernel autoload."""
    if Path("/sys/module", name).exists():
        return True
    try:
        builtin = Path("/lib/modules", os.uname().release, "modules.builtin").read_text()
    except OSError:
        return False
    return any(Path(line).stem.replace("-", "_") == name for line in builtin.splitlines())


def connect(ns, interface, address, peer_ns, peer_interface, peer_address):
    # ip -n creates the first end directly in ns; the peer is created in
    # peer_ns. Neither end ever exists in the host's network namespace.
    ip(ns, "link", "add", interface, "type", "veth", "peer", "name", peer_interface,
       "netns", peer_ns)
    for name, dev, addr in ((ns, interface, address), (peer_ns, peer_interface, peer_address)):
        ip(name, "addr", "add", addr + "/24", "dev", dev)
        ip(name, "link", "set", dev, "up")


def loss_time_stage_commands(direction):
    """tc commands (run inside the NAT namespace) for the time-loss drop stage.

    The drop stage lives on the clsact egress hook of the same WAN device as
    pathAB/BA. That hook runs before the root qdisc enqueue, so the netem
    delay/rate queue never contains a packet the stage could still drop, and
    changing the gact action never touches netem's queue. Only IPv4 is
    matched: ARP still resolves during bad periods.
    """
    _, interface = PATH_DEVICES[direction]
    return (("tc", "qdisc", "add", "dev", interface, "clsact"),
            ("tc", "filter", "add", "dev", interface, "egress", "protocol", "ip", "pref", "1",
             "u32", "match", "u32", "0", "0", "action", "gact", "pass", "index", str(GACT_INDEX)))


def missing_loss_time_modules():
    return [name for name in LOSS_TIME_MODULES if not module_resident(name)]


def setup_topology(names, job, created):
    if needs_policer(job) and not module_resident("nft_limit"):
        raise RuntimeError("UDP policer needs nft_limit already loaded/built in; "
                           "refusing to trigger host kernel module autoload")
    if job.loss_time_directions() and (missing := missing_loss_time_modules()):
        raise RuntimeError(f"lossTime needs {', '.join(missing)} already loaded/built in; "
                           "refusing to trigger host kernel module autoload")
    for role in ROLES:
        name = names[role]
        root_command("ip", "netns", "add", name)
        created.append(name)
        # The simulated WAN is IPv4-only; eliminate asynchronous IPv6 DAD and
        # router/multicast discovery traffic from the measurement interfaces.
        ns_command(name, "sysctl", "-q", "-w", "net.ipv6.conf.all.disable_ipv6=1",
                   "net.ipv6.conf.default.disable_ipv6=1")
        ip(name, "link", "set", "lo", "up")
    ns_command(names["wan"], "sysctl", "-q", "-w", "net.ipv4.ip_forward=1")
    for role, nat in (("a", job.nat_a), ("b", job.nat_b)):
        side = Side(role, nat)
        router, peer, wan = names[side.router], names[role], names["wan"]
        connect(peer, "lan", side.peer_ip, router, side.lan_if, side.gateway)
        connect(router, side.wan_if, side.wan_ip, wan, "wan-" + side.router,
                STUN_IPS[side.index])
        ns_command(router, "sysctl", "-q", "-w", "net.ipv4.ip_forward=1")
        ip(peer, "route", "add", "default", "via", side.gateway)
        ip(router, "route", "add", "default", "via", STUN_IPS[side.index])
        if nat == "none":
            ip(wan, "route", "add", side.lan_prefix + ".0/24", "via", side.wan_ip)
        else:
            install_nft(router, nft_rules(role, nat, job.forward_port))
    # No default route in wan, no route to the NAT-private LANs, and no
    # interface connected to the host or external network.
    if job.netem:
        for role, interface in WAN_INTERFACES:
            ns_command(names[role], "tc", "qdisc", "add", "dev", interface,
                       "root", "netem", *shlex.split(job.netem))
    state = job.path_state()
    for direction in sorted(job.managed_paths()):
        configure_path(names, direction, state, job.jitterNoReorder)
    for direction in job.loss_time_directions():
        for command in loss_time_stage_commands(direction):
            ns_command(names[PATH_DEVICES[direction][0]], *command)
    if needs_policer(job):
        install_nft(names["wan"], policer_rules(state, initial=True))
    if job.blockQuic:
        install_nft(names["wan"], QUIC_BLOCK_RULES)
    for direction, cc in sorted({(entry["dir"], entry["cc"]) for entry in job.cross}):
        ns = names[direction[0]]
        available = ns_command(ns, "cat", "/proc/sys/net/ipv4/tcp_available_congestion_control").stdout.split()
        allowed = ns_command(ns, "cat", "/proc/sys/net/ipv4/tcp_allowed_congestion_control").stdout.split()
        if cc not in available or cc not in allowed:
            hint = ("; tcp_bbr must already be loaded/built in and allowed for unprivileged sockets"
                    if cc == "bbr" else "")
            raise RuntimeError(f"TCP congestion control {cc} unavailable or not allowed in {ns}{hint}")


def quic_block_drops(names):
    """Packets the blockQuic rule dropped in the WAN namespace."""
    listing = json.loads(ns_command(names["wan"], "nft", "-j", "list", "table", "ip",
                                    "mistquicblock").stdout)
    return sum(expr["counter"]["packets"] for item in listing["nftables"]
               for expr in item.get("rule", {}).get("expr", []) if "counter" in expr)


def wan_counters(names):
    links = {role: json.loads(ip(names[role], "-s", "-j", "link", "show").stdout)
             for role in ("na", "wan", "nb")}
    return {interface: link_counters(links[role], interface) for role, interface in WAN_INTERFACES}


async def protected(coro):
    """Finish a critical operation even if the supervisor is cancelled."""
    task = asyncio.create_task(coro)
    try:
        return await asyncio.shield(task)
    except asyncio.CancelledError:
        await task
        raise


async def teardown(names, processes=(), snapshot=None):
    # TERM/KILL every remaining namespace process, including descendants that
    # have left the original process group. Continue cleanup after any error.
    errors = []
    for sig in ("-TERM", "-KILL"):
        found = False
        for name in names:
            try:
                if pids := namespace_pids(name):
                    found = True
                    root_command("kill", sig, "--", *pids, check=False)
            except (OSError, subprocess.CalledProcessError) as error:
                errors.append(error)
        if found:
            await asyncio.sleep(0.5 if sig == "-TERM" else 0.1)
    try:
        if snapshot:
            snapshot()
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        errors.append(error)
    for name in reversed(names):
        try:
            root_command("ip", "netns", "delete", name)
        except (OSError, subprocess.CalledProcessError) as error:
            errors.append(error)
    # Reap the sudo wrappers too, after their namespace children have exited.
    await asyncio.gather(*(process.wait() for process in processes))
    if errors:
        for error in errors:
            report(error)
        raise RuntimeError("namespace teardown failed")


async def launch(name, command, cpulist, processes, env=None, on_start=None,
                 stdout=asyncio.subprocess.PIPE):
    if env:
        command = ["env", *(f"{key}={value}" for key, value in env.items()), *command]
    args = testbed.user_command(name, command, cpulist)
    if os.geteuid() != 0:
        args = ["sudo", "-n", *args]

    async def start():
        process = await asyncio.create_subprocess_exec(*args, start_new_session=True,
                                                       stdout=stdout)
        processes.append(process)
        if on_start:
            on_start(process)
        return process

    # Do not lose a process handle if cancelled during subprocess creation.
    return await protected(start())


def tag_peer_line(line, pair, role):
    try:
        record = json.loads(line)
        if not isinstance(record, dict):
            return line
        record.update(testbedPair=pair, testbedRole=role)
        return (json.dumps(record) + "\n").encode()
    except (ValueError, RecursionError):
        return line


async def collect(process, output, pair, role):
    try:
        pending = b""
        while chunk := await process.stdout.read(65536):
            pending += chunk
            while (end := pending.find(b"\n")) >= 0:
                testbed.write_line(output, tag_peer_line(pending[:end + 1], pair, role))
                pending = pending[end + 1:]
        if pending:
            testbed.write_line(output, tag_peer_line(pending + b"\n", pair, role))
    except OSError:
        # Keep draining on an output error so teardown cannot stall waiting
        # for a subprocess whose stdout transport is paused on a full buffer.
        while await process.stdout.read(65536):
            pass
        raise
    return await process.wait()


async def apply_schedule(names, job, start, records):
    state = job.path_state()
    for item, record in zip(job.schedule, records):
        await asyncio.sleep(max(0, start + item["atS"] - time.monotonic()))
        record["startedAtS"] = round(time.monotonic() - start, 6)
        new = {**state, **{key: item[key] for key in PATH_FIELDS if key in item}}
        for direction, (path, bottleneck, _) in DIRECTION_FIELDS.items():
            if path in item or bottleneck in item:
                configure_path(names, direction, new, job.jitterNoReorder, old=state)
        changed = [direction for direction, (_, _, policer) in DIRECTION_FIELDS.items()
                   if state[policer] != new[policer]]
        if changed:
            install_nft(names["wan"], policer_rules(new, directions=changed))
        record["appliedAtS"] = round(time.monotonic() - start, 6)
        state = new


async def read_cross(process, record, ready):
    try:
        async for line in process.stdout:
            data = json.loads(line)
            kind = data.pop("type")
            if kind == "ready":
                ready.set()
            elif kind == "sample":
                record["samples"].append(data)
            elif kind == "state":
                record.update(data)
            else:
                raise ValueError(f"unexpected cross record: {kind}")
    finally:
        # As with peer stdout, always drain before waiting for teardown.
        while await process.stdout.read(65536):
            pass
        record["exitCode"] = await process.wait()
    return record["exitCode"]


def new_cross_record(entry):
    return {**entry, "status": "notStarted",
            "sender": {"samples": [], "exitCode": None},
            "sink": {"samples": [], "exitCode": None}}


def cross_summary(record):
    result = dict(record)
    sender, sink = record["sender"], record["sink"]
    result["sentBytes"] = sum(s["bytes"] for s in sender["samples"])
    result["receivedBytes"] = sum(s["bytes"] for s in sink["samples"])
    series = {}
    for side, data in (("sent", sender), ("received", sink)):
        for sample in data["samples"]:
            second = sample["second"]
            row = series.setdefault(second, {"atS": round(record["startS"] + second, 6)})
            row[side + "Bytes"] = sample["bytes"]
            row[side + "BitsPerS"] = round(sample["bytes"] * 8 / sample["durationS"], 3)
            row[side + "DurationS"] = sample["durationS"]
    result["series"] = [series[second] for second in sorted(series)]
    return result


async def run_cross(names, entry, index, start, cpulist, processes, readers, record):
    epoch = start + entry["startS"]
    end = epoch + entry["durationS"]
    await asyncio.sleep(max(0, epoch - time.monotonic()))
    if time.monotonic() >= end:
        record["status"] = "missed"
        record["error"] = "cross TCP start window was missed"
        raise RuntimeError(record["error"])
    record["status"] = "partial"
    address = STUN_IPS[int(entry["dir"] == "ba")]
    port = CROSS_PORT_START + index
    common = [sys.executable, SCRIPT, "cross-traffic", "--address", address,
              "--port", str(port), "--epoch", str(epoch), "--end", str(end),
              "--flows", str(entry["flows"]), "--origin", str(start)]

    async def child(role, mode):
        ready = asyncio.Event()
        tasks = []

        def track(process):
            task = asyncio.create_task(read_cross(process, record[mode], ready))
            readers.append(task)
            tasks.append(task)

        await launch(names[role], [*common, "--mode", mode, "--cc", entry["cc"]],
                     cpulist, processes, on_start=track)
        return tasks[0], ready

    try:
        sink, ready = await child("wan", "sink")
        waiter = asyncio.create_task(ready.wait())
        try:
            await asyncio.wait((sink, waiter), timeout=max(0, end - time.monotonic()),
                               return_when=asyncio.FIRST_COMPLETED)
            if not ready.is_set():
                if sink.done():
                    sink.result()
                raise RuntimeError("cross TCP sink did not become ready")
        finally:
            waiter.cancel()
            await asyncio.gather(waiter, return_exceptions=True)
        sender, _ = await child(entry["dir"][0], "sender")
        pending = {sender, sink}
        while pending:
            done, pending = await asyncio.wait(pending, return_when=asyncio.FIRST_COMPLETED)
            if any(task.result() != 0 for task in done):
                raise RuntimeError(f"cross TCP {entry['dir']} {entry['cc']} failed")
        record["status"] = "completed"
    except asyncio.CancelledError:
        raise
    except (OSError, ValueError, RuntimeError) as error:
        record["status"] = "failed"
        record["error"] = str(error)
        raise


def new_loss_time_record(job, direction, job_id=0):
    config = getattr(job, LOSS_TIME_FIELDS[direction])
    return {"rate": config["rate"], "meanBurstMs": config["meanBurstMs"],
            "seed": loss_time_seed(job, direction, job_id), "device": PATH_DEVICES[direction][1],
            "status": "notStarted", "intervals": [], "exitCode": None}


async def launch_root(ns, command, processes, on_start):
    """Start a privileged helper inside a slot namespace, with a stdin pipe."""
    args = ["ip", "netns", "exec", ns, *command]
    if os.geteuid() != 0:
        args = ["sudo", "-n", *args]

    async def start():
        process = await asyncio.create_subprocess_exec(
            *args, stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
            start_new_session=True)
        processes.append(process)
        on_start(process)
        return process

    return await protected(start())


async def read_loss_time(process, record, ready):
    try:
        async for line in process.stdout:
            data = json.loads(line)
            kind = data.pop("type")
            if kind == "ready":
                record.update(status="running", startedAt=data["at"])
                ready.set()
            elif kind == "bad":
                record["intervals"].append((data["start"], data["end"]))
            elif kind == "summary":
                record.update(data, status="failed" if data.get("error") else "completed")
            else:
                raise ValueError(f"unexpected lossTime record: {kind}")
    finally:
        while await process.stdout.read(65536):
            pass
        record["exitCode"] = await process.wait()
        if record["status"] == "running":
            record["status"] = "killed"
    return record["exitCode"]


async def start_loss_time(names, job, records, processes, controllers, readers):
    """Start one toggling controller per lossTime direction and wait until each
    has verified its netlink control of the gact action."""
    for direction, record in records.items():
        role = PATH_DEVICES[direction][0]
        command = [sys.executable, SCRIPT, "loss-time", "--rate", repr(record["rate"]),
                   "--mean-burst-ms", repr(record["meanBurstMs"]), "--seed", str(record["seed"]),
                   "--device", record["device"], "--index", str(GACT_INDEX)]
        ready = asyncio.Event()
        tracked = []

        def track(process, record=record, ready=ready, tracked=tracked):
            controllers.append(process)
            task = asyncio.create_task(read_loss_time(process, record, ready))
            readers.append(task)
            tracked.append(task)

        await launch_root(names[role], command, processes, track)
        waiter = asyncio.create_task(ready.wait())
        try:
            await asyncio.wait((tracked[0], waiter), timeout=10, return_when=asyncio.FIRST_COMPLETED)
        finally:
            waiter.cancel()
            await asyncio.gather(waiter, return_exceptions=True)
        if not ready.is_set():
            if tracked[0].done():
                tracked[0].result()
            record["status"] = "failed"
            raise RuntimeError(f"lossTime controller for {direction} did not become ready")


async def stop_loss_time(controllers, readers, timeout=3):
    # EOF on stdin asks the controller to restore pass, read its counters and
    # emit a summary. Anything still alive afterwards is killed by teardown.
    for process in controllers:
        try:
            if process.stdin and not process.stdin.is_closing():
                process.stdin.close()
        except OSError:
            pass
    if readers:
        await asyncio.wait(readers, timeout=timeout)


def loss_time_result(record, origin):
    """Per-direction summary: realized (kernel-acknowledged) bad periods,
    relative to the job start."""
    result = {key: value for key, value in record.items()
              if key not in ("intervals", "startedAt", "stoppedAt")}
    intervals = record["intervals"]
    started, stopped = record.get("startedAt"), record.get("stoppedAt")
    if stopped is None and intervals:
        stopped = intervals[-1][1]
    base = origin if origin is not None else started
    stats = (interval_stats(intervals, started, stopped)
             if started is not None and stopped is not None else None)
    result.update(
        badPeriods=len(intervals), badMs=round(sum(e - s for s, e in intervals) * 1000, 3),
        activeMs=round((stopped - started) * 1000, 3) if stats else None,
        realizedBadFraction=round(stats["badFraction"], 6) if stats and stats["badFraction"] is not None else None,
        startedAtS=round(started - base, 6) if started is not None else None,
        stoppedAtS=round(stopped - base, 6) if stopped is not None else None,
        badIntervalsS=[[round(s - base, 6), round(e - base, 6)] for s, e in intervals])
    return result


async def run_slot(slot, job, output, active_check=None, job_id=0):
    names = names_for(slot)
    cpulist = testbed.slot_cpus(slot, job.cpus)
    user = testbed.invoking_user()
    created, processes, services, pair_tasks = [], [], {}, []
    peers, tasks = {}, {}
    before, after = {}, {}
    start, duration = None, 0.0
    timed_out = set()
    events, cross_readers = [], []
    schedule = [{**item, "startedAtS": None, "appliedAtS": None} for item in job.schedule]
    cross = [new_cross_record(entry) for entry in job.cross]
    loss_records = {direction: new_loss_time_record(job, direction, job_id)
                    for direction in job.loss_time_directions()}
    loss_controllers, loss_readers = [], []
    failure = None
    # Reuse the existing CPU slot lock, also avoiding overlap with testbed.py.
    with testbed.slot_lock(slot), tempfile.TemporaryDirectory(prefix="mistnat-") as directory:
        if os.geteuid() == 0:
            os.chown(directory, user.pw_uid, user.pw_gid)
        existing = [name for name in namespaces() if name in names.values()]
        if any(namespace_pids(name) for name in existing):
            raise RuntimeError(f"slot {slot} still has processes; use cleanup before reusing it")
        if existing:
            await protected(teardown(existing))
        try:
            setup_topology(names, job, created)
            server = await launch(names["wan"], [sys.executable, SCRIPT, "stun-server",
                                  "--bind", *STUN_IPS], cpulist, processes)
            try:
                ready = await asyncio.wait_for(server.stdout.readline(), timeout=10)
            except asyncio.TimeoutError as error:
                raise RuntimeError("STUN server readiness timed out") from error
            if ready != b"READY\n":
                raise RuntimeError("STUN server did not start")
            for index, command in enumerate(job.wan_commands()):
                await launch(names["wan"], command, cpulist, processes,
                             on_start=lambda process, i=index: services.__setitem__(i, process),
                             stdout=sys.stderr)
            if job.wanReady is not None:
                port = job.wanReady["tcpPort"]
                timeout = job.wanReady.get("timeoutS", 10)
                check = await launch(names["wan"], [sys.executable, SCRIPT, "wan-ready",
                                     "--port", str(port), "--timeout", str(timeout)],
                                     cpulist, processes, stdout=sys.stderr)
                try:
                    code = await asyncio.wait_for(check.wait(), timeout=timeout)
                except asyncio.TimeoutError as error:
                    raise RuntimeError(f"WAN TCP readiness timed out (port {port})") from error
                if code != 0:
                    raise RuntimeError(f"WAN TCP readiness timed out (port {port})")
            if active_check:
                active_check(names)
            # After topology/services, before peers: the drop stage is live
            # (and stats are recorded) for the whole peer lifetime.
            await start_loss_time(names, job, loss_records, processes, loss_controllers, loss_readers)
            before = wan_counters(names)
            start = time.monotonic()
            if schedule:
                events.append(asyncio.create_task(apply_schedule(names, job, start, schedule)))
            for index, (entry, record) in enumerate(zip(job.cross, cross)):
                events.append(asyncio.create_task(run_cross(names, entry, index, start, cpulist,
                                                            processes, cross_readers, record)))

            async def run_pair(pair, a, b, offset):
                await asyncio.sleep(max(0, start + offset - time.monotonic()))
                rendezvous = Path(directory) / str(pair)
                rendezvous.mkdir()
                if os.geteuid() == 0:
                    os.chown(rendezvous, user.pw_uid, user.pw_gid)
                for role, command in (("a", a), ("b", b)):
                    key = (pair, role)

                    def track_peer(peer, key=key):
                        # Register inside protected launch, even on cancellation.
                        peers[key] = peer
                        tasks[key] = asyncio.create_task(collect(peer, output, *key))

                    env = {"MIST_TESTBED_STUN": f"{STUN_IPS[0]}:{STUN_PORT}",
                           "MIST_TESTBED_STUN_ALT": f"{STUN_IPS[1]}:{STUN_PORT}",
                           "MIST_TESTBED_WAN_ADDRS": ",".join(STUN_IPS),
                           "MIST_TESTBED_PAIR": str(pair), "MIST_TESTBED_ROLE": role,
                           "MIST_TESTBED_RENDEZVOUS": str(rendezvous),
                           "MIST_TESTBED_START_MONOTONIC": str(start),
                           "MIST_TESTBED_PEER_IP": Side("b" if role == "a" else "a",
                                                        job.nat_b if role == "a" else job.nat_a).peer_ip}
                    await launch(names[role], command, cpulist, processes, env, track_peer)
                # asyncio.wait does not cancel the readers when this task is cancelled.
                pending = {tasks[pair, "a"], tasks[pair, "b"]}
                while pending:
                    done, pending = await asyncio.wait(pending, return_when=asyncio.FIRST_COMPLETED)
                    for task in done:
                        task.result()

            offsets = job.pairStartOffsetsS or (0,) * job.pairs
            for pair, (a, b, offset) in enumerate(zip(job.peer_commands("a"), job.peer_commands("b"), offsets)):
                pair_tasks.append(asyncio.create_task(run_pair(pair, a, b, offset)))
            pending = set(pair_tasks + events)
            while any(not task.done() for task in pair_tasks):
                remaining = start + job.timeout - time.monotonic()
                if remaining <= 0:
                    break
                done, pending = await asyncio.wait(pending, timeout=remaining,
                                                   return_when=asyncio.FIRST_COMPLETED)
                for task in done:
                    task.result()
            timed_out = {(pair, role) for pair in range(job.pairs) for role in ("a", "b")
                         if (pair, role) not in tasks or not tasks[pair, role].done()}
            if timed_out:
                print(f"nattestbed: slot {slot} timed out", file=sys.stderr)
            for task in pair_tasks + events:
                if task.done():
                    task.result()
        except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError) as error:
            failure = str(error)
            raise
        except asyncio.CancelledError:
            failure = "cancelled"
            raise
        finally:
            if start is not None:
                duration = time.monotonic() - start

            quic_drops = []

            def snapshot():
                after.update(wan_counters(names))
                if job.blockQuic:
                    quic_drops.append(quic_block_drops(names))

            async def finish():
                service_states = {}
                try:
                    # Stop all scheduled launches/changes before namespace
                    # teardown, but keep every child stdout reader draining.
                    for event in events + pair_tasks:
                        if not event.done():
                            event.cancel()
                    await asyncio.gather(*events, *pair_tasks, return_exceptions=True)
                    service_states = {i: "exited" if p.returncode is not None else "stopped"
                                      for i, p in services.items()}
                    await stop_loss_time(loss_controllers, loss_readers)
                    await teardown(created, processes, snapshot if before else None)
                finally:
                    await asyncio.gather(*tasks.values(), *cross_readers, *loss_readers,
                                         return_exceptions=True)

                    def exit_code(key):
                        return 124 if key in timed_out else peers[key].returncode if key in peers else None

                    exits = [(exit_code((pair, "a")), exit_code((pair, "b"))) for pair in range(job.pairs)]
                    wan_services = [{"index": i, "status": service_states.get(i, "notStarted"),
                                     "exitCode": services[i].returncode if i in services else None}
                                    for i in range(len(job.wan_commands()))]
                    # A controller that died early leaves the drop stage in an
                    # unknown state for part of the run: fail the job.
                    broken = [direction for direction, record in loss_records.items()
                              if record["status"] != "completed"]
                    error = failure or (f"lossTime controller did not complete: {', '.join(broken)}"
                                        if broken else None)
                    # A setup failure has null exit codes and no measurement.
                    summary = make_summary(slot, job, exits, before if after else {}, after, duration,
                                           schedule, [cross_summary(record) for record in cross], error,
                                           wan_services,
                                           {direction: loss_time_result(record, start)
                                            for direction, record in loss_records.items()},
                                           quic_drops[0] if quic_drops else None)
                    testbed.write_line(output, (json.dumps(summary) + "\n").encode())
                return summary

            result = await protected(finish())
    return result


def succeeded(summary):
    exits = summary.get("exits")
    if exits is None:
        exits = [{"a": summary["exitA"], "b": summary["exitB"]}]
    return bool(exits) and all(row["a"] == row["b"] == 0 for row in exits) and summary.get("error") is None


async def batch(args, output):
    queue = asyncio.Queue()
    profiles = load_profiles(args.profiles)
    for number, line in enumerate(args.jobs.read_text().splitlines(), 1):
        try:
            job = parse_job(line, args.cpus, profiles)
        except ValueError as error:
            raise ValueError(f"{args.jobs}:{number}: {error}") from error
        if job:
            queue.put_nowait((number, job))
    failures = []

    async def worker(slot):
        while not queue.empty():
            number, job = queue.get_nowait()
            try:
                summary = await run_slot(slot, job, output, job_id=number)
                if not succeeded(summary):
                    failures.append(number)
                print(f"nattestbed: job {number} exits {summary['exits']}",
                      file=sys.stderr)
            except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError) as error:
                failures.append(number)
                report(error)

    workers = [asyncio.create_task(worker(slot)) for slot in range(min(args.parallel, queue.qsize()))]
    group = asyncio.gather(*workers, return_exceptions=True)
    try:
        # Shield from gather's cancellation cascade; cancel workers exactly once
        # below so a second cancellation cannot interrupt protected teardown.
        results = await asyncio.shield(group)
    finally:
        for worker_task in workers:
            if not worker_task.done():
                worker_task.cancel()
        await group
    for result in results:
        if isinstance(result, BaseException):
            raise result
    return int(bool(failures))


async def cleanup():
    slots = {}
    failed = False
    for name in namespaces():
        match = re.fullmatch(r"mistnat-(0|[1-9][0-9]*)-(a|na|wan|nb|b)", name)
        if match:
            slots.setdefault(int(match[1]), []).append(name)
        else:
            print(f"nattestbed: refusing unrecognized namespace {name}", file=sys.stderr)
            failed = True
    for slot, names in sorted(slots.items()):
        try:
            with testbed.slot_lock(slot):
                await protected(teardown(names))
                print(f"removed slot {slot}: {', '.join(names)}")
        except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
            report(error)
            failed = True
    return int(failed)


# RFC 5389: IPv4 Binding Request/Success Response, XOR-MAPPED-ADDRESS.
def stun_message(message_type, transaction, attributes=b""):
    if len(transaction) != 12 or len(attributes) % 4:
        raise ValueError("invalid STUN transaction or attributes")
    return struct.pack("!HHI12s", message_type, len(attributes), COOKIE, transaction) + attributes


def stun_header(data):
    if len(data) < 20:
        raise ValueError("short STUN header")
    kind, length, cookie, transaction = struct.unpack_from("!HHI12s", data)
    if kind & 0xC000 or cookie != COOKIE or length % 4 or length + 20 != len(data):
        raise ValueError("invalid STUN header")
    return kind, transaction, data[20:]


def stun_response(request, address):
    kind, transaction, _ = stun_header(request)
    if kind != 0x0001:
        raise ValueError("not a Binding Request")
    ip, port = address
    xor_ip = int(ipaddress.IPv4Address(ip)) ^ COOKIE
    attribute = struct.pack("!HHBBHI", 0x0020, 8, 0, 1, port ^ (COOKIE >> 16), xor_ip)
    return stun_message(0x0101, transaction, attribute)


def stun_mapped(data, transaction):
    kind, actual, attributes = stun_header(data)
    if kind != 0x0101 or actual != transaction:
        raise ValueError("unexpected Binding Response")
    while attributes:
        attr, length = struct.unpack_from("!HH", attributes)
        end = 4 + ((length + 3) & ~3)
        if end > len(attributes):
            raise ValueError("truncated STUN attribute")
        if attr == 0x0020:
            if length != 8 or attributes[5] != 1:
                raise ValueError("expected IPv4 XOR-MAPPED-ADDRESS")
            port, address = struct.unpack_from("!HI", attributes, 6)
            return str(ipaddress.IPv4Address(address ^ COOKIE)), port ^ (COOKIE >> 16)
        attributes = attributes[end:]
    raise ValueError("missing XOR-MAPPED-ADDRESS")


def stun_server(addresses):
    sockets = []
    try:
        for address in addresses:
            sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            sockets.append(sock)
            sock.bind((address, STUN_PORT))
        print("READY", flush=True)
        while True:
            readable, _, _ = select.select(sockets, [], [])
            for sock in readable:
                request, address = sock.recvfrom(65535)
                try:
                    response = stun_response(request, address)
                except ValueError:
                    continue
                sock.sendto(response, address)
    finally:
        for sock in sockets:
            sock.close()


def wan_ready(port, timeout):
    """Poll the simulated WAN's addresses from within its namespace."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        for address in STUN_IPS:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return 1
            try:
                with socket.create_connection((address, port), timeout=min(0.1, remaining)):
                    return 0
            except OSError:
                pass
        time.sleep(min(0.05, max(0, deadline - time.monotonic())))
    return 1


def wan_echo(port):
    """Internal selftest service, bound only to the simulated WAN addresses."""
    listeners = []
    try:
        for address in STUN_IPS:
            listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            listeners.append(listener)
            listener.bind((address, port))
            listener.listen()
        print("WAN echo ready", flush=True)
        while True:
            readable, _, _ = select.select(listeners, [], [])
            for listener in readable:
                conn, _ = listener.accept()
                with conn:
                    conn.settimeout(3)
                    try:
                        while data := conn.recv(4096):
                            conn.sendall(data)
                    except OSError:
                        pass
    finally:
        for listener in listeners:
            listener.close()


def wan_echo_probe(port):
    role = os.environ["MIST_TESTBED_ROLE"]
    addresses = os.environ["MIST_TESTBED_WAN_ADDRS"].split(",")
    if tuple(addresses) != STUN_IPS:
        raise RuntimeError("unexpected WAN addresses")
    payload = f"mistnat-echo:{role}".encode()
    for address in addresses:
        with socket.create_connection((address, port), timeout=3) as conn:
            conn.sendall(payload)
            conn.shutdown(socket.SHUT_WR)
            received = b""
            while data := conn.recv(4096):
                received += data
            if received != payload:
                raise RuntimeError("WAN echo response did not match")
    emit({"type": "wan-echo", "role": role, "received": True})
    return 0


def endpoint(variable):
    address, port = os.environ[variable].rsplit(":", 1)
    return address, int(port)


def stun_query(sock, server):
    transaction = os.urandom(12)
    request = stun_message(0x0001, transaction)
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline:
        sock.sendto(request, server)
        retry = min(deadline, time.monotonic() + 0.25)
        while (remaining := retry - time.monotonic()) > 0:
            sock.settimeout(remaining)
            try:
                data, source = sock.recvfrom(65535)
            except socket.timeout:
                break
            if source == server:
                try:
                    return stun_mapped(data, transaction)
                except ValueError:
                    continue
    raise RuntimeError("STUN Binding Request timed out")


def probe(mode, port):
    role = os.environ["MIST_TESTBED_ROLE"]
    other = "b" if role == "a" else "a"
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.bind(("0.0.0.0", port))
        mapped = stun_query(sock, endpoint("MIST_TESTBED_STUN"))
        if mode == "mapping":
            alt = stun_query(sock, endpoint("MIST_TESTBED_STUN_ALT"))
            print(json.dumps({"type": "mapping", "role": role, "localPort": sock.getsockname()[1],
                              "mapped": [mapped, alt]}), flush=True)
            return 0
        directory = Path(os.environ["MIST_TESTBED_RENDEZVOUS"])
        path = directory / role
        pending = path.with_suffix(".tmp")
        pending.write_text(json.dumps(mapped))
        pending.replace(path)
        deadline = time.monotonic() + 5
        while not (directory / other).exists():
            if time.monotonic() >= deadline:
                raise RuntimeError("peer rendezvous timed out")
            time.sleep(0.02)
        advertised = tuple(json.loads((directory / other).read_text()))
        destinations = {advertised}
        sources = set()
        marker = f"mistnat-punch:{role}".encode()
        expected = f"mistnat-punch:{other}".encode()
        # Keep sending for the entire window, even after receiving a packet.
        # A public/forwarded peer can learn a symmetric peer's actual source
        # tuple, as ICE can learn a peer-reflexive candidate.
        deadline = time.monotonic() + 1.5
        while time.monotonic() < deadline:
            for target in destinations:
                sock.sendto(marker, target)
            # Pace retransmissions even if both peers receive immediately.
            retry = min(deadline, time.monotonic() + 0.05)
            while (remaining := retry - time.monotonic()) > 0:
                sock.settimeout(remaining)
                try:
                    data, source = sock.recvfrom(65535)
                except socket.timeout:
                    break
                if data == expected:
                    sources.add(source)
                    destinations.add(source)
        print(json.dumps({"type": "punch", "role": role, "mapped": mapped,
                          "received": bool(sources), "sources": sorted(sources)}), flush=True)
        return int(not sources)


def emit(data):
    print(json.dumps(data), flush=True)


class TrafficSamples:
    """Byte counts in fixed one-second bins starting at the cross epoch."""

    def __init__(self, epoch):
        self.epoch = epoch
        self.second = 0
        self.count = 0

    def advance(self, now):
        while now >= self.epoch + self.second + 1:
            self.sample(1)
            self.second += 1
            self.count = 0

    def sample(self, duration):
        emit({"type": "sample", "second": self.second, "bytes": self.count,
              "durationS": round(max(duration, 0.000001), 6)})

    def add(self, count):
        self.advance(time.monotonic())
        self.count += count

    def finish(self, now):
        self.advance(now)
        remaining = now - self.epoch - self.second
        if remaining > 0 or self.count:
            self.sample(remaining)


def cross_traffic(args):
    """Internal TCP sender/sink, with graceful partial samples on teardown."""
    samples = TrafficSamples(args.epoch)
    stop = False
    connected = 0
    sockets = []

    def terminate(*_):
        nonlocal stop
        stop = True

    signal.signal(signal.SIGTERM, terminate)
    signal.signal(signal.SIGINT, terminate)
    emit({"type": "state", "startedAtS": round(time.monotonic() - args.origin, 6)})
    try:
        if args.mode == "sink":
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
                listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                listener.bind((args.address, args.port))
                listener.listen(args.flows)
                listener.setblocking(False)
                emit({"type": "ready"})
                # Allow already accepted TCP data to drain after the send
                # interval; the slot's own deadline still takes precedence.
                deadline = args.end + 1
                while not stop and time.monotonic() < deadline:
                    if connected == args.flows and not sockets:
                        break
                    read, _, _ = select.select([listener, *sockets], [], [], 0.05)
                    samples.advance(time.monotonic())
                    for sock in read:
                        if sock is listener:
                            conn, _ = listener.accept()
                            conn.setblocking(False)
                            sockets.append(conn)
                            connected += 1
                        else:
                            try:
                                data = sock.recv(65536)
                            except (BlockingIOError, InterruptedError):
                                continue
                            except ConnectionResetError:
                                data = b""
                            if data:
                                samples.add(len(data))
                            else:
                                sockets.remove(sock)
                                sock.close()
        else:
            # Do not let setsockopt trigger module autoload for an absent CC.
            available = Path("/proc/sys/net/ipv4/tcp_available_congestion_control").read_text().split()
            allowed = Path("/proc/sys/net/ipv4/tcp_allowed_congestion_control").read_text().split()
            if args.cc not in available or args.cc not in allowed:
                raise RuntimeError(f"TCP congestion control {args.cc} unavailable or not allowed; "
                                   "tcp_bbr must already be loaded/built in for bbr")
            pending = set()
            for _ in range(args.flows):
                sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
                sockets.append(sock)
                sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_CONGESTION, args.cc.encode())
                sock.setblocking(False)
                code = sock.connect_ex((args.address, args.port))
                if code == 0:
                    connected += 1
                elif code in (errno.EINPROGRESS, errno.EWOULDBLOCK, errno.EINTR):
                    pending.add(sock)
                else:
                    raise OSError(code, os.strerror(code))
            payload = bytes(65536)
            while not stop and time.monotonic() < args.end:
                _, writable, _ = select.select([], sockets, [], min(0.05, max(0, args.end - time.monotonic())))
                samples.advance(time.monotonic())
                for sock in writable:
                    if stop or time.monotonic() >= args.end:
                        break
                    if sock in pending:
                        code = sock.getsockopt(socket.SOL_SOCKET, socket.SO_ERROR)
                        if code:
                            raise OSError(code, os.strerror(code))
                        pending.remove(sock)
                        connected += 1
                    try:
                        samples.add(sock.send(payload))
                    except (BlockingIOError, InterruptedError):
                        pass
            if not stop and connected != args.flows:
                raise RuntimeError(f"only {connected}/{args.flows} cross TCP flows connected before deadline")
    finally:
        for sock in sockets:
            sock.close()
        now = time.monotonic()
        samples.finish(now)
        emit({"type": "state", "finishedAtS": round(now - args.origin, 6),
              "connectedFlows": connected, "interrupted": stop})
    return 0


UDP_HEADER = struct.Struct("!QQ")


def delay_statistics(values):
    if not values:
        return {"receivedPackets": 0, "delayMedianMs": None, "delayMinMs": None, "delayMaxMs": None}
    return {"receivedPackets": len(values), "delayMedianMs": round(statistics.median(values), 3),
            "delayMinMs": round(min(values), 3), "delayMaxMs": round(max(values), 3)}


def loss_runs(stamps, interval_ms):
    """Runs of consecutive missing sequence numbers below the highest received.

    A run's duration is the send-time gap between the received packets that
    bracket it minus one nominal interval: k lost packets at a fixed interval
    span k intervals, independently of small pacing errors elsewhere."""
    ordered = sorted(stamps)
    runs = [(after - before - 1, (stamps[after] - stamps[before]) / 1e6 - interval_ms)
            for before, after in zip(ordered, ordered[1:]) if after - before > 1]
    return {"lostBelowHighest": sum(count for count, _ in runs), "lossRunCount": len(runs),
            "lossRunMeanPackets": round(statistics.mean(c for c, _ in runs), 3) if runs else None,
            "lossRunMeanMs": round(statistics.mean(ms for _, ms in runs), 3) if runs else None}


def udp_measure(duration, pps, packet_bytes, port=FORWARD_PORT, drain=0.5):
    """Internal, bidirectional timestamped UDP probe for the none/none topology."""
    role = os.environ["MIST_TESTBED_ROLE"]
    other = "b" if role == "a" else "a"
    directory = Path(os.environ["MIST_TESTBED_RENDEZVOUS"])
    origin = float(os.environ["MIST_TESTBED_START_MONOTONIC"])
    target = (os.environ["MIST_TESTBED_PEER_IP"], port)
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.bind(("0.0.0.0", port))
        sock.setblocking(False)
        path = directory / f"udp-measure-{role}"
        ready = directory / f"udp-measure-{other}"
        path.touch()
        deadline = time.monotonic() + 5
        while not ready.exists():
            if time.monotonic() >= deadline:
                raise RuntimeError("UDP measurement peer not ready")
            time.sleep(0.005)
        # Prime ARP caches before introducing sequence numbers: initial
        # neighbour resolution can itself reorder packets on unshaped links.
        start = max(origin + 0.5, time.monotonic() + 0.3)
        while time.monotonic() < start:
            sock.sendto(b"\0", target)
            time.sleep(0.02)
        # Both peers start after launch; fixed job-relative bins are useful for
        # comparing a schedule before and after its recorded application time.
        start = time.monotonic()
        end = start + duration
        next_send = start
        interval = 1 / pps
        sent, reordered, duplicates, highest = 0, 0, 0, -1
        seen = set()
        stamps = {}
        delays = []
        seconds = {}
        payload = bytes(packet_bytes - UDP_HEADER.size)
        while time.monotonic() < end + drain:
            now = time.monotonic()
            if now >= next_send and now < end:
                try:
                    sock.sendto(UDP_HEADER.pack(sent, time.monotonic_ns()) + payload, target)
                    sent += 1
                except BlockingIOError:
                    pass
                # Do not catch up by sending a burst after CPU descheduling.
                next_send = max(next_send + interval, now + interval / 2)
            wait = (max(0, min(next_send, end) - time.monotonic())
                    if now < end else max(0, end + drain - now))
            readable, _, _ = select.select([sock], [], [], wait)
            if readable:
                while True:
                    try:
                        data, source = sock.recvfrom(65535)
                    except BlockingIOError:
                        break
                    received = time.monotonic_ns()
                    if source != target or len(data) != packet_bytes:
                        continue
                    seq, timestamp = UDP_HEADER.unpack_from(data)
                    if seq in seen:
                        duplicates += 1
                        continue
                    seen.add(seq)
                    stamps[seq] = timestamp
                    if seq < highest:
                        reordered += 1
                    highest = max(highest, seq)
                    delay = (received - timestamp) / 1e6
                    delays.append(delay)
                    second = math.floor(timestamp / 1e9 - origin)
                    seconds.setdefault(second, []).append(delay)
        emit({"type": "udp-measure", "role": role, "sentPackets": sent,
              "sendDurationS": duration, "targetPps": pps, "packetBytes": packet_bytes,
              "sendStartAtS": round(start - origin, 6), "drainS": drain,
              "reorderedPackets": reordered, "duplicatePackets": duplicates,
              "reorderRate": reordered / len(delays) if delays else None,
              "receivedBytes": len(delays) * packet_bytes, **delay_statistics(delays),
              **loss_runs(stamps, 1000 / pps),
              "series": [{"atS": second, **delay_statistics(values)}
                         for second, values in sorted(seconds.items())]})
    return 0


RTM_NEWACTION = 48
NLMSG_ERROR = 2
NLM_F_REQUEST, NLM_F_ACK, NLM_F_REPLACE = 0x1, 0x4, 0x100
TCA_ACT_TAB, TCA_ACT_KIND, TCA_ACT_OPTIONS, TCA_GACT_PARMS = 1, 1, 2, 2
TC_ACT_OK, TC_ACT_SHOT = 0, 2


def netlink_attribute(kind, payload):
    length = 4 + len(payload)
    return struct.pack("=HH", length, kind) + payload + b"\0" * (-length % 4)


def gact_replace_message(seq, index, drop):
    """RTM_NEWACTION|NLM_F_REPLACE for an existing gact action (as
    `tc actions replace action gact pass|drop index N`). The action object is
    shared with the u32 filter, so only its verdict changes."""
    # struct tc_gact (tc_gen): index, capab, action, refcnt, bindcnt.
    parms = struct.pack("=IIiii", index, 0, TC_ACT_SHOT if drop else TC_ACT_OK, 0, 0)
    action = (netlink_attribute(TCA_ACT_KIND, b"gact\0")
              + netlink_attribute(TCA_ACT_OPTIONS, netlink_attribute(TCA_GACT_PARMS, parms)))
    body = struct.pack("=BBH", socket.AF_UNSPEC, 0, 0) + netlink_attribute(
        TCA_ACT_TAB, netlink_attribute(1, action))
    flags = NLM_F_REQUEST | NLM_F_ACK | NLM_F_REPLACE
    return struct.pack("=IHHII", 16 + len(body), RTM_NEWACTION, flags, seq, 0) + body


class GactToggle:
    """Switch one gact action over rtnetlink and wait for the kernel ack."""

    def __init__(self, index):
        self.index = index
        self.seq = 0
        self.sock = socket.socket(socket.AF_NETLINK, socket.SOCK_RAW, socket.NETLINK_ROUTE)
        self.sock.bind((0, 0))

    def set(self, drop):
        self.seq += 1
        self.sock.send(gact_replace_message(self.seq, self.index, drop))
        while True:
            data = self.sock.recv(65536)
            offset = 0
            while offset + 16 <= len(data):
                length, kind, _, seq, _ = struct.unpack_from("=IHHII", data, offset)
                if kind == NLMSG_ERROR and seq == self.seq:
                    code, = struct.unpack_from("=i", data, offset + 16)
                    if code:
                        raise OSError(-code, f"gact replace: {os.strerror(-code)}")
                    return
                offset += max(16, (length + 3) & ~3)

    def close(self):
        self.sock.close()


def find_dicts(value, predicate):
    if isinstance(value, dict):
        if predicate(value):
            yield value
        children = list(value.values())
    elif isinstance(value, list):
        children = value
    else:
        return
    for item in children:
        yield from find_dicts(item, predicate)


def tc_drop_counters(device, index):
    """Best-effort gact/qdisc counters for the controller summary."""
    result = {"droppedPackets": None, "actionPackets": None, "qdiscDrops": None}
    try:
        actions = json.loads(subprocess.run(
            ["tc", "-s", "-j", "actions", "get", "action", "gact", "index", str(index)],
            check=True, capture_output=True, text=True).stdout)
        stats = next(find_dicts(actions, lambda d: "drops" in d and "packets" in d), None)
        if stats:
            result.update(droppedPackets=stats["drops"], actionPackets=stats["packets"])
        qdiscs = json.loads(subprocess.run(["tc", "-s", "-j", "qdisc", "show", "dev", device],
                                           check=True, capture_output=True, text=True).stdout)
        # Root qdisc only (netem when pathAB/BA is set): any drop here is a
        # netem loss/limit drop, never a time-loss drop.
        root = [q for q in qdiscs if q.get("kind") not in ("clsact", "ingress")]
        result["qdiscDrops"] = sum(q.get("drops", 0) for q in root) if root else None
    except (OSError, ValueError, subprocess.CalledProcessError):
        pass
    return result


def percentiles_us(values):
    if not values:
        return {"medianUs": None, "p99Us": None, "maxUs": None}
    ordered = sorted(values)
    return {"medianUs": round(statistics.median(ordered) * 1e6, 1),
            "p99Us": round(ordered[min(len(ordered) - 1, math.ceil(0.99 * len(ordered)) - 1)] * 1e6, 1),
            "maxUs": round(ordered[-1] * 1e6, 1)}


class StopController(Exception):
    pass


SPIN_S = 0.001


def loss_time_controller(args):
    """Internal privileged controller in a NAT namespace. Toggles the time-loss
    gact verdict on schedule until stdin EOF or SIGTERM/SIGINT, then restores
    pass and emits a summary. Bad periods are emitted as they end so that a
    killed controller still leaves a partial record."""
    config = {"rate": args.rate, "meanBurstMs": args.mean_burst_ms}
    validate_loss_time(config, "lossTime")

    def stop(*_):
        raise StopController

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    toggle = GactToggle(args.index)
    latency, late = [], []
    bad, bad_since, toggles = False, None, 0
    started, error = None, None

    def apply(want, scheduled):
        nonlocal bad, bad_since, toggles
        sent = time.monotonic()
        toggle.set(want)
        acked = time.monotonic()
        latency.append(acked - sent)
        if scheduled is not None:
            late.append(sent - scheduled)
        toggles += 1
        # The verdict changes between send and ack; using the ack for both
        # edges cancels in the bad-period duration.
        if want:
            bad_since = acked
        else:
            emit({"type": "bad", "start": bad_since, "end": acked})
        bad = want

    def wait_until(deadline):
        # Sleep (watching stdin for EOF) until 1 ms before the deadline, then
        # spin: wake-up latency of a sleeping process under load is
        # otherwise ~1-2 ms, comparable to the shortest bad periods.
        while (remaining := deadline - time.monotonic() - SPIN_S) > 0:
            readable, _, _ = select.select([0], [], [], remaining)
            if readable and not os.read(0, 4096):
                return False
        while time.monotonic() < deadline:
            pass
        return True

    try:
        toggle.set(False)  # Verify control before reporting readiness.
        started = time.monotonic()
        emit({"type": "ready", "at": started})
        next_at = started
        for want, length_ms in loss_time_periods(config, args.seed):
            if not wait_until(next_at):
                break
            if want != bad:
                apply(want, next_at)
            next_at += length_ms / 1000
    except StopController:
        pass
    except OSError as failure:
        error = str(failure)
    finally:
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        stopped = time.monotonic()
        if bad:
            try:
                apply(False, None)
            except OSError as failure:
                error = error or str(failure)
            stopped = time.monotonic()
        toggle.close()
        if started is not None:
            emit({"type": "summary", "stoppedAt": stopped, "toggles": toggles,
                  "toggleLatency": percentiles_us(latency), "toggleLateness": percentiles_us(late),
                  **tc_drop_counters(args.device, args.index), "error": error})
    if error:
        raise OSError(error)
    return 0


def host_state():
    # Read-only host observations. Stateless nft output ignores packet/byte
    # counters which may legitimately advance on an otherwise unchanged host.
    qdiscs = json.loads(root_command("tc", "-j", "qdisc", "show").stdout)
    links = json.loads(root_command("ip", "-j", "link", "show").stdout)
    return (sorted(json.dumps(qdisc, sort_keys=True) for qdisc in qdiscs),
            sorted((link["ifindex"], link["ifname"]) for link in links),
            root_command("nft", "-s", "list", "ruleset").stdout)


def isolation_check(names, baseline, cpus):
    if host_state() != baseline:
        raise RuntimeError("host interface/qdisc/nft state changed during the experiment")
    for role, interface in WAN_INTERFACES:
        qdiscs = json.loads(ns_command(names[role], "tc", "-j", "qdisc", "show", "dev", interface).stdout)
        if not any(q["kind"] == "netem" for q in qdiscs):
            raise RuntimeError(f"missing namespace netem on {interface}")
    if ip(names["wan"], "route", "show", "default").stdout.strip():
        raise RuntimeError("WAN has a default route")
    # Addresses are discovered at runtime and never recorded. Exclude ranges
    # used by the simulation, since those could name local simulated devices.
    excluded = tuple(map(ipaddress.ip_network, ("127.0.0.0/8", "198.18.0.0/15")))
    addresses = json.loads(root_command("ip", "-j", "-4", "addr", "show").stdout)
    targets = [addr["local"] for link in addresses for addr in link.get("addr_info", [])
               if not any(ipaddress.ip_address(addr["local"]) in network for network in excluded)]
    if not targets:
        raise RuntimeError("no non-overlapping host IPv4 address for isolation test")
    for target in targets:
        if root_command("ip", "-n", names["wan"], "route", "get", target, check=False).returncode == 0:
            raise RuntimeError("WAN can route to a host address")
        for role in ("a", "b"):
            command = testbed.user_command(names[role], ["ping", "-n", "-c", "1", "-W", "1", target], cpus)
            code = root_command(*command, check=False).returncode
            if code != 1:
                raise RuntimeError(f"host isolation ping from peer {role} returned {code}, expected no reply")


def expected_punch(a, b):
    return (a in ("none", "forward") or b in ("none", "forward") or a == b == "eim")


def measure_command(duration, pps, packet_bytes, port=FORWARD_PORT, drain=0.5):
    return (sys.executable, SCRIPT, "udp-measure", "--duration", str(duration),
            "--pps", str(pps), "--packet-bytes", str(packet_bytes),
            "--port", str(port), "--drain", str(drain))


async def impairment_selftest(cpus):
    async def trial(impairments, commands, timeout=12):
        job = Job("none", "none", *commands, cpus=cpus, timeout=timeout, **impairments)
        with tempfile.TemporaryFile() as output:
            summary = await run_slot(0, job, output)
            output.seek(0)
            records = [json.loads(line) for line in output]
        if not succeeded(summary):
            raise RuntimeError(f"impairment trial failed: {summary}")
        return summary, records[:-1]

    async def measure(impairments, duration, pps, packet_bytes=128):
        command = measure_command(duration, pps, packet_bytes)
        summary, records = await trial(impairments, (command, command), timeout=max(12, duration + 10))
        by_role = {record["role"]: record for record in records if record["type"] == "udp-measure"}
        if set(by_role) != {"a", "b"} or not all(r["receivedPackets"] for r in by_role.values()):
            raise RuntimeError("UDP measurement did not receive both directions")
        return summary, by_role["b"], by_role["a"]

    def require(condition, message):
        if not condition:
            raise RuntimeError(message)

    print("Directional WAN impairments (one-way UDP, CLOCK_MONOTONIC):", flush=True)
    _, ab, ba = await measure({"pathAB": "delay 60ms"}, 1, 100)
    require(45 < ab["delayMedianMs"] < 85 and ba["delayMedianMs"] < 20,
            "pathAB did not produce the expected one-way asymmetry")
    print(f"  pathAB only: A->B {ab['delayMedianMs']:.3f} ms, B->A {ba['delayMedianMs']:.3f} ms PASS",
          flush=True)

    policer = {"rateKbit": 2000, "burstBytes": 32768}
    policer_available = module_resident("nft_limit")
    if policer_available:
        duration = 2.5
        _, ab, ba = await measure({"policerAB": policer}, duration, 1000, 1000)
        # nft's initial byte bucket is one second of rate plus burstBytes;
        # measure after its initial tokens have been consumed.
        steady = [row for row in ab["series"] if row["atS"] >= ba["sendStartAtS"] + 0.5
                  and row["atS"] + 1 <= ba["sendStartAtS"] + duration]
        require(bool(steady), "policer has no full steady-state sample")
        received = sum(row["receivedPackets"] for row in steady) * 1000 * 8 / len(steady) / 1e6
        require(1.5 < received < 2.5 and ab["receivedPackets"] < ba["sentPackets"] * 0.6
                and ba["receivedPackets"] > ab["sentPackets"] * 0.9,
                "UDP policer did not drop near the directional configured rate")
        print(f"  UDP policer (2 Mbit/s): A->B {received:.3f} Mbit/s steady-state payload, "
              f"{ab['receivedPackets']}/{ba['sentPackets']} packets PASS", flush=True)
    else:
        print("  UDP policer: SKIP (nft_limit is not loaded/built in; no host module autoload)", flush=True)

    events = [{"atS": 1.5, "pathAB": "delay 75ms loss gemodel 0.202020202% 20% 100% 0%"},
              {"atS": 2.5, "pathAB": "delay 75ms"}]
    if policer_available:
        events[0]["policerAB"] = policer
        events[1]["policerAB"] = None
    summary, ab, _ = await measure({"pathAB": "delay 5ms", "schedule": events}, 3, 100)
    medians = {row["atS"]: row["delayMedianMs"] for row in ab["series"]}
    require(0 in medians and 2 in medians and medians[0] < 20 and 60 < medians[2] < 100
            and all(item["appliedAtS"] is not None and item["appliedAtS"] >= item["atS"]
                    for item in summary["schedule"]), "delay schedule did not switch")
    print(f"  schedule + gemodel{(' + nft updates' if policer_available else '')}: "
          f"{medians[0]:.3f} -> {medians[2]:.3f} ms; "
          f"appliedAtS={[item['appliedAtS'] for item in summary['schedule']]} PASS", flush=True)

    rates = []
    for ordered in (False, True):
        _, ab, _ = await measure({"pathAB": "delay 75ms 25ms", "jitterNoReorder": ordered}, 1.5, 500)
        rates.append(ab["reorderRate"])
        print(f"  jitter {('serialized' if ordered else 'tfifo')}: {ab['reorderedPackets']}/"
              f"{ab['receivedPackets']} reordered ({ab['reorderRate']:.2%}), "
              f"delay min/max {ab['delayMinMs']:.3f}/{ab['delayMaxMs']:.3f} ms", flush=True)
    require(rates[0] > 0.05 and rates[1] == 0, "rate serialization did not suppress jitter-induced reordering")
    print("  jitter reordering control PASS", flush=True)

    entries = [{"dir": direction, "proto": "tcp", "cc": "cubic", "flows": flows,
                "startS": 0.25, "durationS": 2}
               for direction, flows in (("ab", 2), ("ba", 1))]
    idle = (sys.executable, "-c", "import time; time.sleep(4)")
    summary, _ = await trial({"bottleneckAB": {"rateKbit": 10000, "queueMs": 200},
                              "bottleneckBA": {"rateKbit": 10000, "limitPackets": 167},
                              "cross": entries}, (idle, idle))
    for result in summary["cross"]:
        require(result["status"] == "completed" and result["receivedBytes"] > 100000
                and result["sentBytes"] >= result["receivedBytes"]
                and result["sender"]["connectedFlows"] == result["flows"]
                and result["sink"]["connectedFlows"] == result["flows"]
                and len(result["series"]) >= 2, f"cross TCP failed: {result}")
        print(f"  cross {result['dir']} cubic ({result['flows']} flows): "
              f"sent={result['sentBytes']} received={result['receivedBytes']} bytes PASS", flush=True)

    loss_time_available = not missing_loss_time_modules()
    if loss_time_available:
        await loss_time_selftest(measure, require)
    else:
        print(f"  time-domain loss: SKIP ({', '.join(missing_loss_time_modules())} not loaded/built in; "
              "no host module autoload)", flush=True)
    return policer_available, loss_time_available


async def loss_time_selftest(measure, require):
    """Raw-UDP check of lossTimeAB/BA composed with the intl-peak delay/rate/queue.

    A 10 s window holds only ~20 bad periods (sampling sigma of the loss rate
    ~1.5 pp, of the mean burst ~20 %), so seeds are chosen deterministically
    by representative_seed: the checks against nominal rate/meanBurstMs then
    test the emulator, not schedule luck. The tight check compares the
    receiver's loss with the controller's acknowledged bad fraction over the
    same send window, independently of the seed.
    """
    print("Time-domain Gilbert-Elliott loss (clsact egress gact before netem, intl-peak-tloss):", flush=True)
    profile = {key: value for key, value in load_profiles()["intl-peak-tloss"].items() if key != "_comment"}
    trials = []
    first = 1
    for label, pps, duration, tolerance in (("1 kHz", 1000, 10, 0.015), ("10 Hz", 10, 30, 0.03)):
        seeds = []
        for field in ("lossTimeAB", "lossTimeBA"):
            seed = representative_seed(profile[field], duration, first)
            seeds.append(seed)
            first = seed + 1
        trials.append((label, pps, duration, tuple(seeds), tolerance))
    for label, pps, duration, seeds, tolerance in trials:
        print(f"  {label}: seeds ab={seeds[0]} ba={seeds[1]} (representative_seed)", flush=True)
        impairments = {**profile,
                       "lossTimeAB": {**profile["lossTimeAB"], "seed": seeds[0]},
                       "lossTimeBA": {**profile["lossTimeBA"], "seed": seeds[1]}}
        summary, ab, ba = await measure(impairments, duration, pps)
        senders = {"ab": ba, "ba": ab}  # role a's record carries A's send stats
        for direction, received in (("ab", ab), ("ba", ba)):
            sender = senders[direction]
            result = summary["lossTime"][direction]
            config = impairments[LOSS_TIME_FIELDS[direction]]
            window = interval_stats(result["badIntervalsS"], sender["sendStartAtS"],
                                    sender["sendStartAtS"] + duration)
            lost = sender["sentPackets"] - received["receivedPackets"]
            loss = lost / sender["sentPackets"]
            mean_bad_ms = window["meanBad"] * 1000 if window["meanBad"] is not None else float("nan")
            print(f"  {label} {direction}: lost {lost}/{sender['sentPackets']} = {loss:.2%} "
                  f"(rate {config['rate']:.2%}, window bad fraction {window['badFraction']:.2%}, "
                  f"{window['badPeriods']} bad periods, mean {mean_bad_ms:.1f} ms); "
                  f"loss runs {received['lossRunCount']}, mean "
                  f"{received['lossRunMeanPackets']} pkt / {received['lossRunMeanMs']} ms; "
                  f"toggle latency median/p99/max {result['toggleLatency']['medianUs']}/"
                  f"{result['toggleLatency']['p99Us']}/{result['toggleLatency']['maxUs']} us, lateness max "
                  f"{result['toggleLateness']['maxUs']} us; gact drops {result['droppedPackets']}, "
                  f"netem drops {result['qdiscDrops']}, reordered {received['reorderedPackets']}", flush=True)
            require(result["status"] == "completed" and result["badPeriods"] > 0,
                    f"lossTime controller {direction} did not complete: {result}")
            require(abs(loss - config["rate"]) <= tolerance,
                    f"{label} {direction} loss {loss:.4f} not within {tolerance:.3f} of {config['rate']}")
            # Queue integrity: the drop stage precedes netem, so with ~100
            # packets in flight through the delay queue at 1 kHz netem still
            # never drops (limit far above the probe load) or reorders
            # (rate-serialized jitter), and loss tracks the toggle windows.
            require(result["qdiscDrops"] == 0 and received["reorderedPackets"] == 0
                    and received["duplicatePackets"] == 0,
                    f"{label} {direction}: netem queue disturbed (drops/reorder/duplicates)")
            # gact also counts ARP-warm-up UDP primers dropped before the probe.
            require(result["droppedPackets"] is not None and lost <= result["droppedPackets"],
                    f"{label} {direction}: {lost} lost packets but only {result['droppedPackets']} gact drops")
            # Kernel-acknowledged toggle (send -> ack) plus wake-up lateness.
            require(result["toggleLatency"]["p99Us"] + result["toggleLateness"]["p99Us"] <= 2000,
                    f"{label} {direction}: toggle latency/lateness p99 above 2 ms")
            if pps >= 1000:
                require(abs(loss - window["badFraction"]) <= 0.005,
                        f"{label} {direction} loss {loss:.4f} does not match acknowledged bad fraction "
                        f"{window['badFraction']:.4f}")
                require(received["lossRunMeanMs"] is not None
                        and abs(received["lossRunMeanMs"] - config["meanBurstMs"]) <= 0.4 * config["meanBurstMs"],
                        f"{label} {direction} mean loss burst {received['lossRunMeanMs']} ms not within 40% "
                        f"of {config['meanBurstMs']} ms")
            else:
                # Defining property: bursts are time-limited, so at 100 ms
                # spacing losses are almost always isolated packets (the
                # per-packet gemodel would average 5-packet runs here).
                require(received["lossRunCount"] == 0 or received["lossRunMeanPackets"] < 1.5,
                        f"{label} {direction} loss runs too long for time-domain bursts")
    print("  time-domain loss PASS", flush=True)


async def selftest(cpus):
    testbed.host_unshaped()
    baseline = host_state()
    if namespaces():
        raise RuntimeError("selftest requires no existing mistnat namespaces; use cleanup")
    mapping = (sys.executable, SCRIPT, "probe", "mapping")
    punch = (sys.executable, SCRIPT, "probe", "punch")
    idle = (sys.executable, "-c", "pass")

    async def trial(a, b, commands, netem=None, active_check=None, **fields):
        job = Job(a, b, *commands, netem=netem, cpus=cpus, timeout=12, **fields)
        with tempfile.TemporaryFile() as output:
            summary = await run_slot(0, job, output, active_check)
            output.seek(0)
            records = [json.loads(line) for line in output]
        interfaces = (("na-wan", "wan-na") if commands[1] == idle
                      else tuple(interface for _, interface in WAN_INTERFACES))
        if not all(summary["wan"][interface]["rxPackets"] > 0 and summary["wan"][interface]["txPackets"] > 0
                   for interface in interfaces):
            raise RuntimeError("WAN counters did not record traffic on the exercised links")
        return summary, records[:-1]

    failures = []
    print("Mapping (same local UDP port, two STUN destinations):", flush=True)
    try:
        for nat in NAT_TYPES:
            shaped = nat == "none"
            summary, records = await trial(nat, "none", (mapping, idle),
                                           "delay 5ms" if shaped else None,
                                           (lambda names: isolation_check(names, baseline, testbed.slot_cpus(0, cpus)))
                                           if shaped else None)
            # The primary and alternate STUN addresses are both on wan; mapping
            # from A only traverses the A link. Other tests cover both links.
            record, = records
            first, second = record["mapped"]
            same = first[1] == second[1]
            expected_ip = Side("a", nat).mapped_ip
            ok = (succeeded(summary) and record["localPort"] == FORWARD_PORT
                  and first[0] == second[0] == expected_ip and same == (nat != "symmetric"))
            print(f"  {nat:9} {first[0]}:{first[1]} -> {second[0]}:{second[1]} "
                  f"{'PASS' if ok else 'FAIL'}", flush=True)
            if not ok:
                failures.append(f"mapping {nat}")

        print("Hole punching (both peers received UDP; peer-reflexive replies allowed):", flush=True)
        print(f"{'A / B':>10}" + "".join(f"{nat:>12}" for nat in NAT_TYPES), flush=True)
        for a in NAT_TYPES:
            row = []
            for b in NAT_TYPES:
                summary, records = await trial(a, b, (punch, punch))
                by_role = {record["role"]: record for record in records if record["type"] == "punch"}
                if set(by_role) != {"a", "b"} or any(summary["exit" + role.upper()] != int(not by_role[role]["received"])
                                                     for role in ("a", "b")):
                    raise RuntimeError(f"probe failed for {a}/{b}: {summary}")
                actual = succeeded(summary)
                ok = actual == expected_punch(a, b)
                row.append(("yes" if actual else "no") + ("" if ok else "!"))
                if not ok:
                    failures.append(f"hole punching {a}/{b}")
            print(f"{a:>10}" + "".join(f"{cell:>12}" for cell in row), flush=True)
        echo = (sys.executable, SCRIPT, "wan-echo-probe", "--port", "7777")
        service = (sys.executable, SCRIPT, "wan-echo", "--port", "7777")
        summary, records = await trial("eim", "symmetric", (echo, echo), wan=service,
                                       wanReady={"tcpPort": 7777, "timeoutS": 5})
        if (not succeeded(summary) or summary["wanServices"][0]["status"] != "stopped"
                or {(r["testbedPair"], r["testbedRole"]) for r in records if r["received"]}
                != {(0, "a"), (0, "b")}):
            raise RuntimeError("WAN echo selftest failed")
        print("WAN TCP echo: both NAT peers reached both WAN addresses PASS", flush=True)

        # Ephemeral ports let identical probe commands coexist on each peer.
        multi = (*punch, "--port", "0")
        summary, records = await trial("eim", "eim", (multi, multi), pairs=2)
        if (not succeeded(summary) or len(records) != 4
                or {(r["testbedPair"], r["testbedRole"]) for r in records if r["received"]}
                != {(pair, role) for pair in range(2) for role in ("a", "b")}):
            raise RuntimeError("concurrent UDP pairs selftest failed")
        print("Concurrent UDP punch: two pairs, four tagged peer records PASS", flush=True)
        all_impairments = await impairment_selftest(cpus)
    finally:
        testbed.host_unshaped()
        if host_state() != baseline:
            raise RuntimeError("host interface/qdisc/nft state changed")
        if namespaces():
            raise RuntimeError("mistnat namespaces remain after selftest")
        print("Host interfaces/qdiscs/nft unchanged; no mistnat namespaces remain", flush=True)
    if failures:
        raise RuntimeError("unexpected results: " + ", ".join(failures))
    policer_available, loss_time_available = all_impairments
    if not policer_available or not loss_time_available:
        missing = ([] if policer_available else ["UDP policer requires resident nft_limit"]) + (
            [] if loss_time_available else ["time-domain loss requires resident " + ", ".join(LOSS_TIME_MODULES)])
        print("INCOMPLETE: available checks passed; " + "; ".join(missing), flush=True)
        return 1
    print("PASS: STUN mapping, UDP hole punching, WAN services, concurrent pairs, directional impairments, "
          "time-domain loss, cross TCP and host isolation", flush=True)
    return 0


def report(error):
    print(f"nattestbed: {error}", file=sys.stderr)
    if isinstance(error, subprocess.CalledProcessError) and error.stderr:
        print(error.stderr.rstrip(), file=sys.stderr)


async def dispatch(args):
    if args.action == "cleanup":
        return await cleanup()
    if args.action == "selftest":
        return await selftest(args.cpus)
    output = sys.stdout.buffer
    try:
        if args.out:
            args.out.parent.mkdir(parents=True, exist_ok=True)
            output = args.out.open("ab")
        if args.action == "batch":
            return await batch(args, output)
        if args.action == "measure":
            validate_udp_measure(args)
            command = measure_command(args.duration, args.pps, args.packet_bytes, args.forward_port, args.drain)
            a = b = list(command)
            nat_a = nat_b = "none"
        else:
            nat_a, nat_b = args.nat_a, args.nat_b
            a, b = shlex.split(args.a), shlex.split(args.b)
        data = {"natA": nat_a, "natB": nat_b, "a": a, "b": b, "cpus": args.cpus,
                "timeout": args.timeout, "forwardPort": args.forward_port, "netem": args.netem}
        data.update({key: getattr(args, key) for key in IMPAIRMENT_FIELDS | {"profile"}
                     if hasattr(args, key)})
        if getattr(args, "wan", None):
            data["wan"] = [shlex.split(command) for command in args.wan]
        if getattr(args, "wan_ready_tcp", None) is not None:
            data["wanReady"] = {"tcpPort": args.wan_ready_tcp}
        for key in ("pairs", "pairStartOffsetsS"):
            if hasattr(args, key):
                data[key] = getattr(args, key)
        job = parse_job(json.dumps(data), profiles=load_profiles(args.profiles))
        return int(not succeeded(await run_slot(args.slot, job, output)))
    finally:
        if args.out and output is not sys.stdout.buffer:
            output.close()


async def supervised(args):
    task = asyncio.create_task(dispatch(args))
    loop = asyncio.get_running_loop()
    interrupted = None

    def stop(sig):
        nonlocal interrupted
        if interrupted is None:
            interrupted = sig
            task.cancel()

    for sig in testbed.SIGNALS:
        loop.add_signal_handler(sig, stop, sig)
    try:
        return await task
    except asyncio.CancelledError:
        return 128 + interrupted if interrupted else 1
    finally:
        for sig in testbed.SIGNALS:
            loop.remove_signal_handler(sig)


def impairment_arguments(parser):
    parser.add_argument("--netem", help="legacy netem arguments, at both ends of both WAN links")
    parser.add_argument("--path-ab", dest="pathAB", default=argparse.SUPPRESS,
                        help="netem arguments on NAT A WAN egress only")
    parser.add_argument("--path-ba", dest="pathBA", default=argparse.SUPPRESS,
                        help="netem arguments on NAT B WAN egress only")
    for direction in ("ab", "ba"):
        parser.add_argument(f"--bottleneck-{direction}", dest=f"bottleneck{direction.upper()}",
                            type=json.loads, default=argparse.SUPPRESS, help="JSON rate/queue object")
        parser.add_argument(f"--policer-{direction}", dest=f"policer{direction.upper()}",
                            type=json.loads, default=argparse.SUPPRESS, help="JSON UDP policer object")
        parser.add_argument(f"--loss-time-{direction}", dest=LOSS_TIME_FIELDS[direction],
                            type=json.loads, default=argparse.SUPPRESS,
                            help='JSON time-domain burst loss: {"rate":0.05,"meanBurstMs":25,"seed":1}')
    parser.add_argument("--jitter-no-reorder", dest="jitterNoReorder",
                        action=argparse.BooleanOptionalAction, default=argparse.SUPPRESS,
                        help="serialize netem departures with configured rate (100gbit if unset)")
    parser.add_argument("--schedule", type=json.loads, default=argparse.SUPPRESS, help="JSON schedule array")
    parser.add_argument("--cross", type=json.loads, default=argparse.SUPPRESS, help="JSON cross traffic array")
    parser.add_argument("--profile", default=argparse.SUPPRESS, help="impairment profile name")
    parser.add_argument("--profiles", type=Path, default=PROFILES, help="profile definition JSON file")


def run_arguments(parser, *, measurement=False):
    parser.add_argument("--slot", type=int, required=not measurement, default=0)
    impairment_arguments(parser)
    parser.add_argument("--cpus", type=testbed.positive, default=2)
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT)
    parser.add_argument("--forward-port", type=int, default=FORWARD_PORT)
    parser.add_argument("--out", type=Path, help="append stdout and summary JSON to this file")


def measure_arguments(parser):
    parser.add_argument("--duration", type=float, default=3, help="UDP send duration in seconds")
    parser.add_argument("--pps", type=float, default=500, help="UDP packets per second per direction")
    parser.add_argument("--packet-bytes", type=int, default=128, help="UDP payload bytes, including timestamp")
    parser.add_argument("--drain", type=float, default=0.5, help="receive window after sending ends (seconds)")


def validate_udp_measure(args):
    finite_number(args.duration, "duration")
    finite_number(args.pps, "pps")
    finite_number(args.drain, "drain", zero=True)
    if not UDP_HEADER.size <= args.packet_bytes <= 1472:
        raise ValueError("packet-bytes must be in 16..1472 (IPv4 UDP payload)")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    actions = parser.add_subparsers(dest="action", required=True)
    run = actions.add_parser("run", help="run peer pairs on an isolated NAT topology")
    run_arguments(run)
    run.add_argument("--nat-a", choices=NAT_TYPES, required=True)
    run.add_argument("--nat-b", choices=NAT_TYPES, required=True)
    run.add_argument("--a", required=True)
    run.add_argument("--b", required=True)
    run.add_argument("--wan", action="append", help="WAN service argv string (repeatable)")
    run.add_argument("--wan-ready-tcp", type=int, metavar="PORT", help="wait for a WAN TCP listener (10s)")
    run.add_argument("--pairs", type=testbed.positive, default=1, help="concurrent peer pairs")
    run.add_argument("--pair-start-offsets-s", dest="pairStartOffsetsS", type=json.loads,
                     default=argparse.SUPPRESS, help="JSON array of pair start offsets")
    measurement = actions.add_parser("measure", help="measure one-way delay, loss and sequence reordering with UDP")
    run_arguments(measurement, measurement=True)
    measure_arguments(measurement)
    jobs = actions.add_parser("batch", help="run JSONL jobs on parallel slots")
    jobs.add_argument("--parallel", type=testbed.positive, required=True)
    jobs.add_argument("--jobs", type=Path, required=True)
    jobs.add_argument("--cpus", type=testbed.positive, default=2)
    jobs.add_argument("--profiles", type=Path, default=PROFILES)
    jobs.add_argument("--out", type=Path, help="append all stdout and summaries to a shared file")
    actions.add_parser("cleanup", help="remove unlocked mistnat slots, including abandoned processes")
    test = actions.add_parser("selftest", help="verify STUN mapping, UDP hole punching and isolation")
    test.add_argument("--cpus", type=testbed.positive, default=2)
    server = actions.add_parser("stun-server", help="internal IPv4 Binding Request server")
    server.add_argument("--bind", nargs="+", required=True)
    ready = actions.add_parser("wan-ready", help="internal WAN TCP readiness probe")
    ready.add_argument("--port", type=int, required=True)
    ready.add_argument("--timeout", type=float, required=True)
    for name in ("wan-echo", "wan-echo-probe"):
        echo = actions.add_parser(name, help="internal WAN TCP echo selftest")
        echo.add_argument("--port", type=int, required=True)
    peer = actions.add_parser("probe", help="internal UDP selftest peer")
    peer.add_argument("mode", choices=("mapping", "punch"))
    peer.add_argument("--port", type=int, default=FORWARD_PORT)
    udp = actions.add_parser("udp-measure", help="internal timestamped UDP measurement peer")
    measure_arguments(udp)
    udp.add_argument("--port", type=int, default=FORWARD_PORT)
    cross = actions.add_parser("cross-traffic", help="internal TCP traffic generator/sink")
    cross.add_argument("--mode", choices=("sender", "sink"), required=True)
    cross.add_argument("--address", choices=STUN_IPS, required=True)
    cross.add_argument("--port", type=int, required=True)
    cross.add_argument("--cc", choices=("cubic", "bbr"), required=True)
    cross.add_argument("--flows", type=testbed.positive, required=True)
    cross.add_argument("--epoch", type=float, required=True)
    cross.add_argument("--end", type=float, required=True)
    cross.add_argument("--origin", type=float, required=True)
    loss = actions.add_parser("loss-time", help="internal privileged time-domain loss controller")
    loss.add_argument("--rate", type=float, required=True)
    loss.add_argument("--mean-burst-ms", type=float, required=True)
    loss.add_argument("--seed", type=int, required=True)
    loss.add_argument("--device", required=True)
    loss.add_argument("--index", type=int, default=GACT_INDEX)
    args = parser.parse_args()
    try:
        if args.action == "stun-server":
            return stun_server(args.bind)
        if args.action == "wan-ready":
            return wan_ready(args.port, args.timeout)
        if args.action == "wan-echo":
            return wan_echo(args.port)
        if args.action == "wan-echo-probe":
            return wan_echo_probe(args.port)
        if args.action == "probe":
            return probe(args.mode, args.port)
        if args.action == "udp-measure":
            validate_udp_measure(args)
            return udp_measure(args.duration, args.pps, args.packet_bytes, args.port, args.drain)
        if args.action == "cross-traffic":
            return cross_traffic(args)
        if args.action == "loss-time":
            return loss_time_controller(args)
        return asyncio.run(supervised(args))
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError) as error:
        report(error)
        return 1


if __name__ == "__main__":
    sys.exit(main())
