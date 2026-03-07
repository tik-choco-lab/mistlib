import ctypes
import os
import sys
import json
try:
    from dotenv import load_dotenv
    env_path = os.path.join(os.path.dirname(__file__), "..", ".env")
    load_dotenv(env_path)
except ImportError:
    pass

EVENT_RAW = 0
EVENT_OVERLAY = 1
EVENT_JOIN = 2
EVENT_LEAVE = 3
EVENT_NEIGHBORS = 4
EVENT_AOI_ENTERED = 5
EVENT_AOI_LEFT = 6
EVENT_NODE_POSITION_UPDATED = 7

DELIVERY_RELIABLE = 0
DELIVERY_UNRELIABLE_ORDERED = 1
DELIVERY_UNRELIABLE = 2

if sys.platform == "win32":
    lib_name = "mistlib.dll"
elif sys.platform == "darwin":
    lib_name = "libmistlib.dylib"
else:
    lib_name = "libmistlib.so"

_search_paths = [
    os.path.join(os.path.dirname(__file__), lib_name),
    os.path.join(os.path.dirname(__file__), "..", lib_name),
    os.path.join(os.path.dirname(__file__), "../../../target/release", lib_name),
    os.path.join(os.path.dirname(__file__), "../../../target/debug", lib_name),
]

DLL_PATH = next((os.path.abspath(p) for p in _search_paths if os.path.exists(p)), os.path.abspath(_search_paths[0]))

DEFAULT_SIGNALING_URL = b"wss://rtc.tik-choco.com/signaling"
SIGNALING_URL_ENV = os.getenv("MIST_SIGNALING_URL")
if SIGNALING_URL_ENV and "your-private-signaling" not in SIGNALING_URL_ENV:
    SIGNALING_URL = SIGNALING_URL_ENV.encode('utf-8')
else:
    SIGNALING_URL = DEFAULT_SIGNALING_URL

EVENT_CALLBACK = ctypes.CFUNCTYPE(None, ctypes.c_uint32, ctypes.POINTER(ctypes.c_uint8), ctypes.c_size_t, ctypes.POINTER(ctypes.c_uint8), ctypes.c_size_t)
LOG_CALLBACK = ctypes.CFUNCTYPE(None, ctypes.c_uint32, ctypes.POINTER(ctypes.c_uint8), ctypes.c_size_t)

class MistNode:
    def __init__(self, node_id, signaling_url=SIGNALING_URL):
        if not os.path.exists(DLL_PATH):
            raise FileNotFoundError(f"DLL not found: {DLL_PATH}")

        self.lib = ctypes.CDLL(DLL_PATH)
        self.node_id = node_id
        self.signaling_url = signaling_url

        self.lib.init.argtypes = [ctypes.c_char_p, ctypes.c_size_t, ctypes.c_char_p, ctypes.c_size_t]
        self.lib.join_room.argtypes = [ctypes.c_char_p, ctypes.c_size_t]
        self.lib.register_log_callback.argtypes = [LOG_CALLBACK]
        self.lib.register_event_callback.argtypes = [EVENT_CALLBACK]
        self.lib.update_position.argtypes = [ctypes.c_float, ctypes.c_float, ctypes.c_float]
        self.lib.send_message.argtypes = [ctypes.c_char_p, ctypes.c_size_t, ctypes.c_char_p, ctypes.c_size_t, ctypes.c_uint32]
        self.lib.leave_room.argtypes = []
        self.lib.get_stats.argtypes = [ctypes.POINTER(ctypes.c_uint8), ctypes.c_size_t]
        self.lib.get_stats.restype = ctypes.c_uint32
        self.lib.get_config.argtypes = [ctypes.POINTER(ctypes.c_uint8), ctypes.c_size_t]
        self.lib.get_config.restype = ctypes.c_uint32
        self.lib.set_config.argtypes = [ctypes.c_char_p, ctypes.c_size_t]

    def register_callbacks(self, log_cb, event_cb):
        self._log_cb_ref = LOG_CALLBACK(log_cb)
        self._event_cb_ref = EVENT_CALLBACK(event_cb)
        self.lib.register_log_callback(self._log_cb_ref)
        self.lib.register_event_callback(self._event_cb_ref)

    def init(self):
        id_bytes = self.node_id.encode('utf-8')
        self.lib.init(id_bytes, len(id_bytes), self.signaling_url, len(self.signaling_url))

    def join_room(self, room_id):
        room_bytes = room_id.encode('utf-8')
        self.lib.join_room(room_bytes, len(room_bytes))

    def update_position(self, x, y, z=0.0):
        self.lib.update_position(float(x), float(y), float(z))

    def send_message(self, to_id, payload, delivery=DELIVERY_UNRELIABLE):
        to_id_bytes = to_id.encode('utf-8') if to_id else b""
        self.lib.send_message(to_id_bytes, len(to_id_bytes), payload, len(payload), delivery)

    def leave_room(self):
        self.lib.leave_room()

    def get_stats(self):
        buffer = (ctypes.c_uint8 * 65536)()
        written = self.lib.get_stats(buffer, len(buffer))
        if written > 0:
            return ctypes.string_at(buffer, written).decode('utf-8')
        return "{}"

    def get_config(self):
        buffer = (ctypes.c_uint8 * 4096)()
        written = self.lib.get_config(buffer, len(buffer))
        if written > 0:
            return ctypes.string_at(buffer, written).decode('utf-8')
        return "{}"

    def set_config(self, config):
        if isinstance(config, dict):
            data = json.dumps(config).encode('utf-8')
        elif isinstance(config, str):
            data = config.encode('utf-8')
        elif isinstance(config, (bytes, bytearray)):
            data = bytes(config)
        else:
            raise TypeError("config must be dict, str, bytes, or bytearray")

        self.lib.set_config(data, len(data))
