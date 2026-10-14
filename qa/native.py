"""A real ctypes consumer. Each process owns one library instance and storage directory."""

import ctypes as C
import json
import sys
import threading


def main():
    lib = C.CDLL(sys.argv[1])
    lock = threading.Lock()

    def emit(value):
        with lock:
            print(json.dumps(value, separators=(",", ":")), flush=True)

    pointer, size = C.c_void_p, C.c_size_t
    callback_type = C.CFUNCTYPE(None, C.c_uint32, pointer, size, pointer, size, pointer, size)

    @callback_type
    def callback(kind, room, room_len, sender, sender_len, data, data_len):
        emit({"event": {"kind": {0: "raw", 2: "connected", 3: "disconnected", 8: "aoi"}.get(kind, str(kind)),
                        "room": C.string_at(room, room_len).decode(),
                        "sender": C.string_at(sender, sender_len).decode(),
                        "data": list(C.string_at(data, data_len))}})

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

    def read_json(name):
        buffer = C.create_string_buffer(1024 * 1024)
        length = getattr(lib, name)(buffer, len(buffer))
        if not 0 < length <= len(buffer):
            raise ValueError(f"{name} returned invalid length {length}")
        return json.loads(buffer.raw[:length])

    info = read_json("get_build_info")
    version_buffer = C.create_string_buffer(1024)
    version_length = lib.get_version(version_buffer, len(version_buffer))
    if not 0 < version_length <= len(version_buffer) or version_buffer.raw[:version_length].decode() != info["version"]:
        raise ValueError("native version exports disagree")
    emit({"ready": info})
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
            elif op == "store":
                data = bytes(command["data"])
                buffer = C.create_string_buffer(1024)
                length = lib.storage_add(b"qa.bin", 6, data, len(data), buffer, len(buffer))
                if not 0 < length <= len(buffer):
                    raise ValueError("storage_add failed")
                value = buffer.raw[:length].decode()
            elif op == "get":
                cid = command["cid"].encode()
                buffer = C.create_string_buffer(1024 * 1024)
                length = lib.storage_get(cid, len(cid), buffer, len(buffer))
                if not 0 < length <= len(buffer):
                    raise ValueError("storage_get failed")
                value = list(buffer.raw[:length])
            else:
                raise ValueError(f"unknown command {op}")
            emit({"id": command["id"], "value": value})
        except Exception as error:
            emit({"id": command["id"], "error": str(error)})


if __name__ == "__main__":
    main()
