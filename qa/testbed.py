#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""Linux loopback-only benchmark slots; Python stdlib, no installation needed.

Jobs are argv command lines (shlex quoting), optionally ending in > FILE or
>> FILE. Shell pipelines/expansion are deliberately not evaluated. --out appends
whole output lines under flock, so jobs can share a JSONL destination safely.
"""

import argparse
import asyncio
from contextlib import contextmanager
from dataclasses import dataclass
import fcntl
import json
import os
from pathlib import Path
import pwd
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile


PREFIX = "mistlab-"
ICE_ADDRESS = "192.0.2.1"
SIGNALS = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)


def slot_cpus(slot, cpus=2, available=None):
    """Map logical CPU positions modulo the CPUs available to the caller.

    Slots are disjoint until they wrap; restricted/noncontiguous cpusets use
    their actual CPU IDs rather than assuming CPU zero is available.
    """
    if slot < 0 or cpus < 1:
        raise ValueError("slot must be nonnegative and cpus must be positive")
    available = sorted(os.sched_getaffinity(0) if available is None else available)
    if not available:
        raise ValueError("no CPUs available")
    return sorted({available[(slot * cpus + i) % len(available)] for i in range(cpus)})


@dataclass(frozen=True)
class Job:
    command: tuple
    output: Path | None = None
    append: bool = False


def parse_job(line):
    words = shlex.split(line, comments=True)
    if not words:
        return None
    output, append = None, False
    redirects = [i for i, word in enumerate(words) if word in (">", ">>")]
    if redirects:
        i = redirects[0]
        if len(redirects) != 1 or i == 0 or i != len(words) - 2:
            raise ValueError("use a single trailing > FILE or >> FILE")
        output, append = Path(words[-1]), words[i] == ">>"
        words = words[:i]
    if any(word in ("|", "||", "&&", ";", "&", "<", "2>", "2>>") for word in words):
        raise ValueError("jobs are argv commands, not shell scripts; use sh -c explicitly")
    return Job(tuple(words), output, append)


def root_command(*args, check=True, capture=True):
    command = list(args)
    if os.geteuid() != 0:
        command = ["sudo", "-n", *command]
    return subprocess.run(command, check=check, text=True,
                          stdout=subprocess.PIPE if capture else None,
                          stderr=subprocess.PIPE if capture else None)


def namespaces():
    return [line.split()[0] for line in root_command("ip", "netns", "list").stdout.splitlines()
            if line.split() and line.split()[0].startswith(PREFIX)]


@contextmanager
def slot_lock(slot):
    # Persistent lock files avoid an unlink/open race between competing runners.
    path = Path("/tmp") / f"mistlib-testbed-{slot}.lock"
    fd = os.open(path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise RuntimeError(f"slot {slot} is already running") from None
        yield
    finally:
        os.close(fd)


def namespace_pids(name):
    return root_command("ip", "netns", "pids", name).stdout.split()


async def teardown(name):
    # ip netns del alone leaves a namespace alive while a browser/relay holds it.
    # Kill all descendants, including processes that left the job's session.
    for sig in ("-TERM", "-KILL"):
        pids = namespace_pids(name)
        if not pids:
            break
        root_command("kill", sig, "--", *pids, check=False)
        await asyncio.sleep(0.5 if sig == "-TERM" else 0.1)
    root_command("ip", "netns", "exec", name, "tc", "qdisc", "del", "dev", "lo", "root", check=False)
    root_command("ip", "netns", "delete", name)


async def protected_teardown(name):
    # A first signal can arrive during normal-exit cleanup, too. Let cleanup
    # finish before propagating cancellation to the caller.
    cleanup = asyncio.create_task(teardown(name))
    try:
        await asyncio.shield(cleanup)
    except asyncio.CancelledError:
        await cleanup
        raise


def invoking_user():
    uid = int(os.environ.get("SUDO_UID", os.getuid())) if os.geteuid() == 0 else os.getuid()
    if uid == 0:
        raise RuntimeError("invoke as a non-root user with sudo access (or via sudo from that user)")
    return pwd.getpwuid(uid)


def user_command(name, command, cpulist):
    user = invoking_user()
    # Explicit env restores PATH and all other values that sudo might filter.
    # No shell is involved; cwd is inherited unchanged.
    return ["ip", "netns", "exec", name, "setpriv", "--reuid", str(user.pw_uid),
            "--regid", str(user.pw_gid), "--init-groups", "env", "-i",
            *(f"{key}={value}" for key, value in os.environ.items()),
            shutil.which("taskset"), "--cpu-list", ",".join(map(str, cpulist)), *command]


def write_line(stream, data):
    # flock also protects against another testbed batch appending to this file.
    if stream is not sys.stdout.buffer:
        fcntl.flock(stream, fcntl.LOCK_EX)
    try:
        stream.write(data)
        stream.flush()
    finally:
        if stream is not sys.stdout.buffer:
            fcntl.flock(stream, fcntl.LOCK_UN)


async def pump(reader, stream):
    pending = b""
    while chunk := await reader.read(65536):
        pending += chunk
        while (end := pending.find(b"\n")) >= 0:
            write_line(stream, pending[:end + 1])
            pending = pending[end + 1:]
    if pending:
        write_line(stream, pending + b"\n")


async def run_slot(slot, cpus, command, output=None, setup=None):
    name = f"{PREFIX}{slot}"
    cpulist = slot_cpus(slot, cpus)
    process = None
    with slot_lock(slot):
        exists = name in namespaces()
        if exists and namespace_pids(name):
            raise RuntimeError(f"{name} still has processes; use cleanup before reusing it")
        if not exists:
            root_command("ip", "netns", "add", name)
        try:
            # Reuse an idle namespace left by a crashed runner without inheriting
            # its shaping. The slot lock and pid check protect live experiments.
            root_command("ip", "netns", "exec", name, "tc", "qdisc", "del", "dev", "lo", "root", check=False)
            root_command("ip", "netns", "exec", name, "ip", "link", "set", "lo", "up")
            # Browser/native ICE implementations exclude loopback interfaces or
            # 127/8 by default. An unconnected dummy supplies a host candidate;
            # traffic to our own address is locally routed through shaped lo.
            root_command("ip", "netns", "exec", name, "ip", "link", "delete", "mistice0", check=False)
            root_command("ip", "netns", "exec", name, "ip", "link", "add", "mistice0", "type", "dummy")
            root_command("ip", "netns", "exec", name, "ip", "addr", "add", f"{ICE_ADDRESS}/32", "dev", "mistice0")
            root_command("ip", "netns", "exec", name, "ip", "link", "set", "mistice0", "up")
            # Browsers also require a default interface for ICE enumeration.
            # This route terminates on the unconnected dummy: no host/internet access.
            root_command("ip", "netns", "exec", name, "ip", "route", "replace", "default", "dev", "mistice0")
            route = root_command("ip", "netns", "exec", name, "ip", "route", "get", ICE_ADDRESS).stdout
            if "dev lo " not in route:
                raise RuntimeError(f"ICE address must route through lo: {route}")
            if setup:
                await setup(name)
            print(f"testbed: {name} CPUs {','.join(map(str, cpulist))}: {shlex.join(command)}", file=sys.stderr)
            args = user_command(name, command, cpulist)
            if os.geteuid() != 0:
                args = ["sudo", "-n", *args]
            process = await asyncio.create_subprocess_exec(*args, start_new_session=True,
                                                          stdout=asyncio.subprocess.PIPE if output else None)
            if output:
                await pump(process.stdout, output)
            return await process.wait()
        finally:
            await protected_teardown(name)
            if process:
                await process.wait()


async def batch(args):
    jobs = []
    destinations = {}
    for number, line in enumerate(args.jobs.read_text().splitlines(), 1):
        try:
            job = parse_job(line)
        except ValueError as error:
            raise ValueError(f"{args.jobs}:{number}: {error}") from error
        if job:
            if args.out and job.output:
                raise ValueError("--out cannot be combined with job output redirection")
            if job.output:
                path = job.output.resolve()
                if path in destinations and not (job.append and destinations[path]):
                    raise ValueError(f"shared output {path} requires >> on every job")
                destinations[path] = job.append
            jobs.append(job)
    queue = asyncio.Queue()
    for number, job in enumerate(jobs, 1):
        queue.put_nowait((number, job))
    failures = []

    async def worker(slot):
        while not queue.empty():
            number, job = queue.get_nowait()
            stream = None
            try:
                path = args.out or job.output
                if path:
                    path.parent.mkdir(parents=True, exist_ok=True)
                    stream = path.open("ab" if args.out or job.append else "wb")
                code = await run_slot(slot, args.cpus, job.command, stream or sys.stdout.buffer)
                if code:
                    failures.append(number)
                print(f"testbed: job {number} exit {code}", file=sys.stderr)
            except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
                failures.append(number)
                report(error)
            finally:
                if stream:
                    stream.close()

    workers = [asyncio.create_task(worker(slot)) for slot in range(min(args.parallel, len(jobs)))]
    try:
        await asyncio.gather(*workers)
    finally:
        for worker_task in workers:
            worker_task.cancel()
        await asyncio.gather(*workers, return_exceptions=True)
    return int(bool(failures))


def host_unshaped():
    result = json.loads(root_command("tc", "-j", "qdisc", "show", "dev", "lo").stdout)
    if any(qdisc["kind"] != "noqueue" for qdisc in result):
        raise RuntimeError(f"host lo is shaped: {result}")


async def selftest(cpus):
    host_unshaped()
    ready = asyncio.Event()
    count = 0

    async def probe(slot, delay):
        async def setup(name):
            nonlocal count
            root_command("ip", "netns", "exec", name, "tc", "qdisc", "add", "dev", "lo",
                         "root", "netem", "delay", f"{delay}ms")
            count += 1
            if count == 2:
                host_unshaped()
                ready.set()
            await ready.wait()

        with tempfile.TemporaryFile() as output:
            code = await run_slot(slot, cpus, ["ping", "-n", "-c", "4", "-i", "0.25", "127.0.0.1"], output, setup)
            output.seek(0)
            text = output.read().decode()
        match = re.search(r"= [\d.]+/([\d.]+)/", text)
        if code or not match:
            raise RuntimeError(f"slot {slot} ping failed: {text}")
        rtt = float(match[1])
        expected = 2 * delay  # Request and reply each cross the loopback qdisc.
        if abs(rtt - expected) > max(20, expected * 0.2):
            raise RuntimeError(f"slot {slot}: RTT {rtt}ms, expected {expected}ms")
        print(f"slot {slot}: delay={delay}ms RTT={rtt:.3f}ms expected={expected}ms")

    tasks = [asyncio.create_task(probe(0, 30)), asyncio.create_task(probe(1, 100))]
    try:
        await asyncio.gather(*tasks)
    finally:
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
    host_unshaped()
    print("PASS: parallel loopback delays isolated; host lo unshaped; slots removed")
    return 0


def report(error):
    print(f"testbed: {error}", file=sys.stderr)
    if isinstance(error, subprocess.CalledProcessError) and error.stderr:
        print(error.stderr.rstrip(), file=sys.stderr)


def positive(value):
    number = int(value)
    if number < 1:
        raise argparse.ArgumentTypeError("must be positive")
    return number


async def dispatch(args):
    if args.action == "run":
        command = args.command[1:] if args.command[:1] == ["--"] else args.command
        if args.slot < 0 or not command:
            raise ValueError("run requires a nonnegative --slot and a command after --")
        return await run_slot(args.slot, args.cpus, command)
    if args.action == "batch":
        return await batch(args)
    if args.action == "selftest":
        return await selftest(args.cpus)
    failed = False
    for name in namespaces():
        try:
            with slot_lock(name[len(PREFIX):]):
                await protected_teardown(name)
                print(f"removed {name}")
        except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
            report(error)
            failed = True
    return int(failed)


async def supervised(args):
    task = asyncio.create_task(dispatch(args))
    loop = asyncio.get_running_loop()
    interrupted = None

    def stop(sig):
        nonlocal interrupted
        if interrupted is None:
            interrupted = sig
            task.cancel()  # Further signals must not interrupt teardown.

    for sig in SIGNALS:
        loop.add_signal_handler(sig, stop, sig)
    try:
        return await task
    except asyncio.CancelledError:
        return 128 + interrupted if interrupted else 1
    finally:
        for sig in SIGNALS:
            loop.remove_signal_handler(sig)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    actions = parser.add_subparsers(dest="action", required=True)
    run = actions.add_parser("run", help="run an argv command in a loopback-only namespace")
    run.add_argument("--slot", type=int, required=True)
    run.add_argument("--cpus", type=positive, default=2)
    run.add_argument("command", nargs=argparse.REMAINDER)
    batch_parser = actions.add_parser("batch", help="run one quoted argv command per jobs-file line")
    batch_parser.add_argument("--parallel", type=positive, required=True)
    batch_parser.add_argument("--jobs", type=Path, required=True)
    batch_parser.add_argument("--cpus", type=positive, default=2)
    batch_parser.add_argument("--out", type=Path, help="append all stdout lines to a shared file")
    actions.add_parser("cleanup", help="remove idle mistlab-* namespaces (active slots are refused)")
    test = actions.add_parser("selftest", help="verify two simultaneous delays and host isolation")
    test.add_argument("--cpus", type=positive, default=2)
    args = parser.parse_args()
    try:
        return asyncio.run(supervised(args))
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError) as error:
        report(error)
        return 1


if __name__ == "__main__":
    sys.exit(main())
