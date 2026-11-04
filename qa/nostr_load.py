#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""SPEC-23 Phase A: prebuilt native consumers + Go relay in qa/testbed netns.

Build separately with `just nostr load-build`. Runs never build. Default is three
serial repetitions; --smoke requires exactly one. No public network is reachable.
"""
import argparse
import asyncio
import json
import math
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time

import testbed

ROOT = Path(__file__).resolve().parents[1]
TAGS = ("NostrSend", "NostrOK", "NostrStats", "NostrReset", "NostrReceive",
        "NostrEOSE", "NostrBinding", "NostrIdentity", "IsolationRecovery", "IceCandidate",
        "IceState", "IceWatchdog", "ConnTiming", "PairBackoff", "IdentityRecovery",
        "DiscoveryRetired", "NostrGovernor")


def udp_drop_commands(name):
    # Local ICE candidates route through lo in testbed. tc drops UDP silently,
    # unlike OUTPUT firewall DROP, which may return EPERM to the sender.
    if not name.startswith(testbed.PREFIX):
        raise ValueError("UDP filtering is restricted to testbed namespaces")
    prefix = ("ip", "netns", "exec", name, "tc")
    return [(*prefix, "qdisc", "add", "dev", "lo", "clsact"),
            *[(*prefix, "filter", "add", "dev", "lo", "egress", "protocol", family,
               "flower", "ip_proto", "udp", "action", "drop") for family in ("ip", "ipv6")]]



def udp_restore_commands(name):
    # Delete only the qdisc that this harness created, in its private netns.
    udp_drop_commands(name)  # validates the namespace restriction
    return [("ip", "netns", "exec", name, "tc", "qdisc", "del", "dev", "lo", "clsact")]


def all_connected(clients):
    return bool(clients) and all(client.connected_peers for client in clients)


async def wait_connected(clients, timeout):
    deadline = time.monotonic() + timeout
    while not all_connected(clients):
        if time.monotonic() >= deadline:
            raise RuntimeError("not all nodes connected before deadline")
        if any(c.process.returncode is not None for c in clients):
            raise RuntimeError("native consumer exited while waiting for connectivity")
        await asyncio.sleep(0.05)


class Recorder:
    def __init__(self, directory):
        self.directory = directory
        directory.mkdir(parents=True, exist_ok=False)
        self.stream = (directory / "events.jsonl").open("w")
        self.raw = (directory / "measurements.log").open("w")
        self.started = time.monotonic()

    def emit(self, source, record_type, **data):
        self.stream.write(json.dumps({"t": round(time.monotonic() - self.started, 6),
                                      "source": source, "record": record_type, **data}) + "\n")
        self.stream.flush()

    def log(self, source, line):
        # Preserve only measurement logs, which never contain SDP, credentials,
        # or candidate addresses. Native's other diagnostics are intentionally
        # outside the measurement artifact.
        if any(f"[{tag}]" in line for tag in TAGS):
            self.raw.write(f"{time.monotonic() - self.started:.6f} {source} {line}\n")
            self.raw.flush()
            self.emit(source, "log", line=line)

    def close(self):
        self.stream.close()
        self.raw.close()


class Native:
    def __init__(self, name, recorder):
        self.name, self.recorder = name, recorder
        self.pending, self.tasks = {}, []
        self.serial = 0
        self.connected_peers = set()
        self.stopped = False
        self.process = None
        self.ready = asyncio.get_running_loop().create_future()

    async def start(self, library, directory):
        directory.mkdir()
        env = {**os.environ, "TMPDIR": str(directory), "TEMP": str(directory),
               "TMP": str(directory),
               "RUST_LOG": "off,mistlib_core::signaling::courtesy=debug,mistlib_core::signaling::nostr=debug,mistlib::signaling::nostr=debug,mistlib::transports::webrtc=debug"}
        self.process = await asyncio.create_subprocess_exec(
            sys.executable, str(ROOT / "qa/native.py"), str(library),
            cwd=directory, env=env, stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        self.tasks = [asyncio.create_task(self.output()), asyncio.create_task(self.logs())]
        await asyncio.wait_for(asyncio.shield(self.ready), 20)

    async def output(self):
        try:
            async for raw in self.process.stdout:
                value = json.loads(raw)
                if "ready" in value:
                    self.recorder.emit(self.name, "ready", build=value["ready"])
                    self.ready.set_result(value["ready"])
                elif "event" in value:
                    event = value["event"]
                    if event["kind"] == "connected":
                        self.connected_peers.add(event["sender"])
                    elif event["kind"] == "disconnected":
                        self.connected_peers.discard(event["sender"])
                    self.recorder.emit(self.name, "event", kind=event["kind"],
                                       peer=event["sender"], room=event["room"])
                elif value.get("id") in self.pending:
                    future = self.pending.pop(value["id"])
                    if not future.done():
                        future.set_result(value)
        finally:
            error = RuntimeError(f"{self.name} stdout closed")
            if not self.ready.done():
                self.ready.set_exception(error)
            for future in self.pending.values():
                if not future.done():
                    future.set_exception(error)

    async def logs(self):
        async for raw in self.process.stderr:
            self.recorder.log(self.name, raw.decode(errors="replace").rstrip())

    async def request(self, op, **data):
        self.serial += 1
        future = asyncio.get_running_loop().create_future()
        self.pending[self.serial] = future
        self.process.stdin.write((json.dumps({"id": self.serial, "op": op, **data}) + "\n").encode())
        await self.process.stdin.drain()
        result = await asyncio.wait_for(future, 20)
        if "error" in result:
            raise RuntimeError(f"{self.name}: {result['error']}")
        return result.get("value")

    async def stop(self):
        if self.process is None or self.stopped:
            return
        self.stopped = True
        if self.process.returncode is None:
            try:
                await self.request("leave-all")
            except (RuntimeError, asyncio.TimeoutError, BrokenPipeError, ConnectionError):
                pass
            self.process.stdin.close()
            try:
                await asyncio.wait_for(self.process.wait(), 5)
            except asyncio.TimeoutError:
                self.process.kill()
                await self.process.wait()
        await asyncio.gather(*self.tasks)
        self.recorder.emit(self.name, "exit", code=self.process.returncode)


async def stop_relay(process):
    if process and process.returncode is None:
        process.send_signal(signal.SIGTERM)
        try:
            await asyncio.wait_for(process.wait(), 5)
        except asyncio.TimeoutError:
            process.kill()
            await process.wait()


async def relay_output(process, recorder):
    async for raw in process.stdout:
        line = raw.decode(errors="replace").rstrip()
        if line.startswith('{"type":"NostrRelayStats"'):
            recorder.emit("relay", "relay_stats", stats=json.loads(line))
        # Other relay output is drained without retaining frame/tag contents.


async def wait_relay(process, port):
    for _ in range(100):
        if process.returncode is not None:
            raise RuntimeError(f"relay exited: {process.returncode}")
        try:
            _, writer = await asyncio.open_connection("127.0.0.1", port)
            writer.close()
            await writer.wait_closed()
            return
        except OSError:
            await asyncio.sleep(0.05)
    raise RuntimeError("relay did not start")


async def trial(args, directory, repetition):
    recorder = Recorder(directory)
    clients, relay, relay_task = [], None, None
    error = None
    namespace = Path("/var/run/netns") / f"{testbed.PREFIX}{args.slot}"
    # Reject accidental direct execution outside the namespace.
    if not namespace.exists() or os.stat("/proc/self/ns/net").st_ino != os.stat(namespace).st_ino:
        raise RuntimeError("worker must execute inside qa/testbed.py's namespace")
    recorder.emit("harness", "manifest", scenario=args.scenario, nodes=args.nodes,
                  seconds=args.seconds, repetition=repetition, rate=args.rate,
                  burst=args.burst, rate_req=args.rate_req,
                  ice="udp_drop_netns" if args.scenario in ("S2", "S3", "S4") else "normal_netns",
                  history_seconds=args.history_seconds, udp_down_seconds=args.udp_down_seconds, smoke=args.smoke)
    try:
        # Reserve an ephemeral local port, then immediately hand it to the relay.
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        command = [str(args.relay), "--host", "127.0.0.1", "--port", str(port),
                   "--rate-limit-events", str(args.rate), "--rate-limit-burst", str(args.burst),
                   "--stats-interval", "1s"]
        if args.rate_req:
            command.append("--rate-limit-req")
        if args.scenario == "S4":
            command.append("--store-ephemeral")
        relay = await asyncio.create_subprocess_exec(*command, stdout=asyncio.subprocess.PIPE,
                                                      stderr=asyncio.subprocess.STDOUT)
        relay_task = asyncio.create_task(relay_output(relay, recorder))
        await wait_relay(relay, port)
        config = {"iceServers": [], "aoiRange": 100,
                  "signaling": {"mode": "nostr", "nostr": {
                      "relays": [f"ws://127.0.0.1:{port}"], "inviteSalt": "spec22-local",
                      "inviteCode": "spec22-measurement"}}}
        room = "spec22-local"
        with tempfile.TemporaryDirectory(prefix="mistlib-nostr-load-") as temporary:
            async def make(index):
                client = Native(f"node-{index:02}", recorder)
                clients.append(client)
                await client.start(args.library, Path(temporary) / client.name)
                await client.request("init", node=client.name, config=config)
                return client

            await asyncio.gather(*(make(index) for index in range(args.nodes)))
            async def join(client):
                recorder.emit(client.name, "join")
                await client.request("join", room=room)
            await asyncio.gather(*(join(client) for client in clients))
            recovered_at = None
            restored_at = None
            if args.scenario == "S5":
                await wait_connected(clients, 60)
                recorder.emit("harness", "all_connected")
                for command in udp_drop_commands(f"{testbed.PREFIX}{args.slot}"):
                    testbed.root_command(*command)
                recorder.emit("harness", "udp_drop", seconds=args.udp_down_seconds)
                await asyncio.sleep(args.udp_down_seconds)
                for command in udp_restore_commands(f"{testbed.PREFIX}{args.slot}"):
                    testbed.root_command(*command)
                restored_at = time.monotonic()
                recorder.emit("harness", "udp_restore", disconnected_nodes=sum(not c.connected_peers for c in clients))
            deadline = time.monotonic() + args.seconds
            if args.scenario == "S4":
                await asyncio.sleep(args.history_seconds)
                await join(await make(args.nodes))
            while time.monotonic() < deadline:
                if restored_at is not None and recovered_at is None and all_connected(clients):
                    recovered_at = time.monotonic() - restored_at
                    recorder.emit("harness", "all_reconnected", recovery_seconds=recovered_at)
                if relay.returncode is not None or any(c.process.returncode is not None for c in clients):
                    raise RuntimeError("relay or native consumer exited during measurement")
                await asyncio.sleep(min(0.05 if restored_at is not None and recovered_at is None else 1,
                                        max(0, deadline - time.monotonic())))
            if args.scenario == "S5" and recovered_at is None:
                raise RuntimeError("S5 did not reconnect all nodes after UDP restoration")
            # End the measurement window before leave/close noise.
            recorder.emit("harness", "measurement_end")
            await asyncio.gather(*(c.stop() for c in clients))
    except (Exception, asyncio.CancelledError) as failure:
        error = f"{type(failure).__name__}: {failure}"
        recorder.emit("harness", "failure", error=error)
    finally:
        await asyncio.gather(*(c.stop() for c in clients), return_exceptions=True)
        await stop_relay(relay)
        if relay_task:
            await relay_task
        recorder.emit("harness", "result", success=error is None, error=error)
        recorder.close()
    from nostr_load_report import summarize, table
    rows = summarize(directory / "events.jsonl")
    (directory / "summary.md").write_text(table(rows))
    print(table(rows), flush=True)
    return int(error is not None)


async def run(args):
    if not args.library.is_file() or not args.relay.is_file():
        raise ValueError("build first with just nostr load-build")
    args.out.mkdir(parents=True, exist_ok=False)
    failures = 0
    for repetition in range(1, args.repeats + 1):
        async def setup(name):
            if args.scenario in ("S2", "S3", "S4"):
                for command in udp_drop_commands(name):
                    testbed.root_command(*command)
        command = [sys.executable, str(Path(__file__).resolve()), "worker",
                   "--scenario", args.scenario, "--nodes", str(args.nodes),
                   "--seconds", str(args.seconds), "--rate", str(args.rate),
                   "--burst", str(args.burst), "--slot", str(args.slot),
                   "--library", str(args.library), "--relay", str(args.relay),
                   "--history-seconds", str(args.history_seconds),
                   "--udp-down-seconds", str(args.udp_down_seconds),
                   "--out", str(args.out / f"run-{repetition:02}")]
        if args.smoke:
            command.append("--smoke")
        if args.rate_req:
            command.append("--rate-req")
        try:
            code = await testbed.run_slot(args.slot, args.cpus, command, setup=setup)
        except (OSError, RuntimeError, subprocess.CalledProcessError) as failure:
            code = 1
            directory = args.out / f"run-{repetition:02}"
            if not directory.exists():
                recorder = Recorder(directory)
                recorder.emit("harness", "manifest", scenario=args.scenario, repetition=repetition)
                recorder.emit("harness", "failure", error=str(failure))
                recorder.close()
        if code and not (args.out / f"run-{repetition:02}" / "events.jsonl").exists():
            # Infrastructure failures are results too.
            recorder = Recorder(args.out / f"run-{repetition:02}")
            recorder.emit("harness", "failure", error=f"worker exit {code}")
            recorder.close()
        failures += bool(code)
    from nostr_load_report import summarize, table
    rows = [row for path in sorted(args.out.glob("run-*/events.jsonl")) for row in summarize(path)]
    (args.out / "summary.md").write_text(table(rows))
    return int(bool(failures))


def arguments():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("run", "worker"))
    parser.add_argument("--scenario", choices=("S0", "S1", "S2", "S3", "S4", "S5"), required=True)
    parser.add_argument("--nodes", type=int, choices=(2, 6), default=2)
    parser.add_argument("--seconds", type=float, default=300)
    parser.add_argument("--udp-down-seconds", type=float, default=30,
                        help="S5: after every node has a peer, silently drop UDP for this many seconds, then restore; --seconds is the post-restore observation window (e.g. --scenario S5 --nodes 2 --udp-down-seconds 30 --seconds 120)")
    parser.add_argument("--history-seconds", type=float, default=120)
    parser.add_argument("--rate", type=float, default=None)
    parser.add_argument("--burst", type=int, default=10)
    parser.add_argument("--rate-req", action="store_true")
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--smoke", action="store_true")
    parser.add_argument("--slot", type=int, default=22)
    parser.add_argument("--cpus", type=int, default=2)
    parser.add_argument("--library", type=Path, default=ROOT / "target/release/libmistlib.so")
    parser.add_argument("--relay", type=Path, default=ROOT / "target/nostr-relay")
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    if args.rate is None:
        args.rate = 5 if args.scenario in ("S1", "S3") else 0
    if (args.seconds <= 0 or args.burst < 1 or args.rate < 0 or args.slot < 0 or args.cpus < 1
            or not math.isfinite(args.seconds) or not math.isfinite(args.rate)):
        parser.error("invalid duration/rate/burst/slot/cpus")
    if not math.isfinite(args.udp_down_seconds) or args.udp_down_seconds <= 0:
        parser.error("udp-down-seconds must be finite and positive")
    if args.scenario == "S4" and not 0 < args.history_seconds < args.seconds:
        parser.error("S4 requires 0 < history-seconds < seconds")
    if args.action == "run" and ((args.smoke and args.repeats != 1) or (not args.smoke and args.repeats < 3)):
        parser.error("use --smoke --repeats 1 for a single plumbing check; measurements require >=3")
    if args.scenario in ("S0", "S2", "S4", "S5") and args.rate != 0:
        parser.error("this scenario requires --rate 0")
    if args.scenario in ("S1", "S3") and args.rate <= 0:
        parser.error("this scenario requires a positive rate")
    for name in ("library", "relay", "out"):
        setattr(args, name, getattr(args, name).resolve())
    return args


def main():
    args = arguments()
    try:
        if args.action == "worker":
            return asyncio.run(trial(args, args.out, int(args.out.name.removeprefix("run-"))))
        return asyncio.run(run(args))
    except (OSError, RuntimeError, ValueError) as error:
        print(f"nostr-load: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
