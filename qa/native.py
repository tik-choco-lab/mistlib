"""A real ctypes consumer. Each process owns one library instance and storage directory
(clients.mjs points the OS temp dir, where the library keeps blocks, at the process cwd)."""

import ctypes as C
import hashlib
import json
import math
import os
import queue
import sys
import threading
import time


def generated_bytes(seed, size):
    """xorshift32, matching the browser's unsigned 32-bit operations."""
    state = seed & 0xffffffff
    data = bytearray(size)
    for index in range(size):
        state ^= (state << 13) & 0xffffffff
        state ^= state >> 17
        state ^= (state << 5) & 0xffffffff
        data[index] = state >> 24
    return bytes(data)


def storage_bytes(lib, cid):
    # FFI returns the required length without copying when the buffer is too
    # small. The first call fetches/caches the data; the second copies it.
    probe = C.create_string_buffer(1)
    required = lib.storage_get(cid, len(cid), probe, 0)
    if required <= 0:
        raise ValueError("storage_get failed")
    buffer = C.create_string_buffer(required)
    length = lib.storage_get(cid, len(cid), buffer, len(buffer))
    if length != required:
        raise ValueError(f"storage_get returned unexpected length {length}, expected {required}")
    return buffer.raw[:length]


def digest(data):
    return {"sha256": hashlib.sha256(data).hexdigest(), "size": len(data)}


class ControlProbe:
    """Reliable overlay ping/echo. Never call back into FFI on its callback thread."""

    def __init__(self, send):
        self.send = send
        self.lock = threading.Lock()
        self.echoes = queue.Queue(maxsize=32)
        self.config = None
        self.active = False
        self.pending = {}
        self.samples = []
        self.timeouts = 0
        self.censored = 0
        self.serial = 0
        self.shutdown = threading.Event()
        self.thread = None

    def configure(self, command):
        self.config = (command["room"], command["target"],
                       f'mistlib-storage-control:{command["token"]}:')
        if self.thread is None:
            self.thread = threading.Thread(target=self._loop, daemon=True)
            self.thread.start()

    def receive(self, room, sender, data):
        config = self.config
        if not config or (room, sender) != config[:2]:
            return False
        text = data.decode(errors="replace")
        if not text.startswith(config[2]):
            return False
        message = text[len(config[2]):]
        if message.startswith("ping:"):
            try:
                self.echoes.put_nowait((room, sender, (config[2] + "pong:" + message[5:]).encode()))
            except queue.Full:
                pass  # Requester counts missing echoes as timeouts.
        elif message.startswith("pong:"):
            with self.lock:
                started = self.pending.pop(message[5:], None)
                if started is not None:
                    self.samples.append((time.monotonic() - started) * 1000)
        return True

    def start(self):
        if not self.config:
            raise ValueError("control probe is not configured")
        with self.lock:
            self.pending, self.samples = {}, []
            self.timeouts = self.censored = 0
            self.next_ping = time.monotonic()
            self.active = True

    def stop(self):
        with self.lock:
            if self.active:
                self.censored = len(self.pending)
                self.timeouts += self.censored
                self.pending.clear()
                self.active = False
            samples = sorted(self.samples)
            return {"controlSamples": len(samples), "controlTimeouts": self.timeouts,
                    "controlCensored": self.censored,
                    **{f"controlRttP{p}Ms": samples[max(0, math.ceil(len(samples) * p / 100) - 1)]
                       if samples else None for p in (50, 95)}}

    def close(self):
        self.shutdown.set()

    def _loop(self):
        while not self.shutdown.is_set():
            try:
                echo = self.echoes.get(timeout=0.01)
            except queue.Empty:
                echo = None
            if echo:
                self.send(*echo)
            outgoing = None
            with self.lock:
                now = time.monotonic()
                if self.active:
                    for serial, started in list(self.pending.items()):
                        if now - started >= 10:
                            self.pending.pop(serial)
                            self.timeouts += 1
                    if now >= self.next_ping:
                        self.serial += 1
                        serial = str(self.serial)
                        self.pending[serial] = now
                        self.next_ping = now + 1
                        room, target, prefix = self.config
                        outgoing = (room, target, (prefix + "ping:" + serial).encode())
            if outgoing:
                self.send(*outgoing)


class TelemetrySampler:
    """Observe the cached FFI stats independently of the blocking get worker."""

    def __init__(self, read):
        self.read = read
        self.series = []
        self.stopped = threading.Event()
        self.worker = None

    def sample(self):
        t = time.time() * 1000
        try:
            self.series.append({"t": t, **self.read()})
        except Exception as error:
            self.series.append({"t": t, "error": str(error)})

    def start(self):
        if self.worker is not None:
            raise ValueError("telemetry already started")
        self.series = []
        self.stopped.clear()
        self.sample()

        def loop():
            while not self.stopped.wait(1):
                self.sample()

        self.worker = threading.Thread(target=loop, daemon=True)
        self.worker.start()

    def stop(self):
        self.stopped.set()
        if self.worker is not None:
            self.worker.join()
            self.worker = None
            self.sample()
        return self.series


def get_result(lib, command, control):
    """Runs on the single get worker; ctypes releases the GIL during storage_get."""
    bench = command["op"] == "get-bench"
    started = time.monotonic()
    if bench:
        control.start()
    try:
        data = storage_bytes(lib, command["cid"].encode())
        fetched = time.monotonic()
        value = list(data) if command["op"] == "get" else digest(data)
        if bench:
            if value != command["expected"]:
                raise ValueError(f"size/SHA-256 mismatch: {value}")
            finished = time.monotonic()
            value.update(getMs=(fetched - started) * 1000,
                         verifyMs=(finished - fetched) * 1000,
                         completionMs=(finished - started) * 1000,
                         **control.stop())
        return value
    finally:
        if bench:
            control.stop()


def main():
    lib = C.CDLL(sys.argv[1])
    lock = threading.Lock()

    def emit(value):
        with lock:
            print(json.dumps(value, separators=(",", ":")), flush=True)

    pointer, size = C.c_void_p, C.c_size_t
    callback_type = C.CFUNCTYPE(None, C.c_uint32, pointer, size, pointer, size, pointer, size)

    def send_control(room, target, data):
        room, target = room.encode(), target.encode()
        lib.send_message_in_room(room, len(room), target, len(target), data, len(data), 0)

    control = ControlProbe(send_control)

    @callback_type
    def callback(kind, room, room_len, sender, sender_len, data, data_len):
        room_text = C.string_at(room, room_len).decode()
        sender_text = C.string_at(sender, sender_len).decode()
        payload = C.string_at(data, data_len)
        if kind == 0 and control.receive(room_text, sender_text, payload):
            return
        emit({"event": {"kind": {0: "raw", 2: "connected", 3: "disconnected", 8: "aoi"}.get(kind, str(kind)),
                        "room": room_text, "sender": sender_text, "data": list(payload)}})

    signatures = {
        "register_event_callback_v2": ([callback_type], None),
        "init_with_config": ([C.c_char_p, size, C.c_char_p, size], C.c_bool),
        "join_room": ([C.c_char_p, size], None),
        "leave_room_id": ([C.c_char_p, size], None),
        "leave_room": ([], None),
        "update_position_in_room": ([C.c_char_p, size, C.c_float, C.c_float, C.c_float], None),
        "send_message_in_room": ([C.c_char_p, size, C.c_char_p, size, C.c_char_p, size, C.c_uint32], None),
        "get_build_info": ([pointer, size], C.c_uint32),
        "get_version": ([pointer, size], C.c_uint32),
        "get_stats": ([pointer, size], C.c_uint32),
        "storage_add": ([C.c_char_p, size, C.c_char_p, size, pointer, size], C.c_uint32),
        "storage_get": ([C.c_char_p, size, pointer, size], C.c_uint32),
    }
    for name, (args, result) in signatures.items():
        fn = getattr(lib, name)
        fn.argtypes, fn.restype = args, result
    lib.register_event_callback_v2(callback)  # Keep callback alive for the process lifetime.
    log_callback = None
    if os.environ.get("RUST_LOG", "off") != "off":
        # Opt-in diagnostics (QA_RUST_LOG): library logs go to stderr, which the harness saves.
        log_type = C.CFUNCTYPE(None, C.c_uint32, C.c_void_p, C.c_size_t)
        log_callback = log_type(lambda level, ptr, length: sys.stderr.write(
            f"{time.monotonic():.3f} LOG{level} {C.string_at(ptr, length).decode(errors='replace')}\n"))
        lib.register_log_callback(log_callback)

    def read_json(name):
        buffer = C.create_string_buffer(1024 * 1024)
        length = getattr(lib, name)(buffer, len(buffer))
        if not 0 < length <= len(buffer):
            raise ValueError(f"{name} returned invalid length {length}")
        return json.loads(buffer.raw[:length])

    telemetry = TelemetrySampler(lambda: read_json("get_stats"))
    info = read_json("get_build_info")
    version_buffer = C.create_string_buffer(1024)
    version_length = lib.get_version(version_buffer, len(version_buffer))
    if not 0 < version_length <= len(version_buffer) or version_buffer.raw[:version_length].decode() != info["version"]:
        raise ValueError("native version exports disagree")
    emit({"ready": info})
    get_lock = threading.Lock()

    def fetch(command):
        try:
            emit({"id": command["id"], "value": get_result(lib, command, control)})
        except Exception as error:
            emit({"id": command["id"], "error": str(error)})

        finally:
            get_lock.release()

    for line in sys.stdin:
        command = json.loads(line)
        try:
            op = command["op"]
            room = command.get("room", "").encode()
            value = None
            if op == "init":
                node = command["node"].encode()
                config = json.dumps(command["config"]).encode()
                if not lib.init_with_config(node, len(node), config, len(config)):
                    raise ValueError("init_with_config rejected configuration")
            elif op in ("join", "leave"):
                getattr(lib, "join_room" if op == "join" else "leave_room_id")(room, len(room))
            elif op == "position":
                lib.update_position_in_room(room, len(room), *command["position"])
            elif op == "send":
                target, data = command["target"].encode(), bytes(command["data"])
                lib.send_message_in_room(room, len(room), target, len(target), data, len(data), 0)
            elif op == "stats":
                value = read_json("get_stats")
            elif op == "telemetry-start":
                telemetry.start()
            elif op == "telemetry-stop":
                value = telemetry.stop()
            elif op in ("store", "store-generated"):
                data = (generated_bytes(command["seed"], command["size"])
                        if op == "store-generated" else bytes(command["data"]))
                buffer = C.create_string_buffer(1024)
                length = lib.storage_add(b"qa.bin", 6, data, len(data), buffer, len(buffer))
                if not 0 < length <= len(buffer):
                    raise ValueError("storage_add failed")
                value = buffer.raw[:length].decode()
                if op == "store-generated":
                    value = {"cid": value, **digest(data)}
            elif op == "control-config":
                control.configure(command)
            elif op == "control-stop":
                value = control.stop()
            elif op in ("get", "get-digest", "get-bench"):
                if not get_lock.acquire(blocking=False):
                    raise ValueError("a storage_get is already running")
                threading.Thread(target=fetch, args=(command,), daemon=True).start()
                continue
            else:
                raise ValueError(f"unknown command {op}")
            emit({"id": command["id"], "value": value})
        except Exception as error:
            emit({"id": command["id"], "error": str(error)})

    telemetry.stop()
    control.close()


if __name__ == "__main__":
    main()
