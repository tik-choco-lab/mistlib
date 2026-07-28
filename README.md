<div align="center">

# mistlib

**Peer-to-peer networking for large shared 3D spaces.**

[![npm](https://img.shields.io/npm/v/mistlib?logo=npm&logoColor=white&label=npm&color=cb3837)](https://www.npmjs.com/package/mistlib)
[![release](https://img.shields.io/github/v/release/tik-choco-lab/mistlib?label=release&color=2f6feb)](https://github.com/tik-choco-lab/mistlib/releases/latest)
[![CI](https://img.shields.io/github/actions/workflow/status/tik-choco-lab/mistlib/ci.yml?branch=main&label=CI)](https://github.com/tik-choco-lab/mistlib/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MPL--2.0-blue)](LICENSE)

English · [日本語](README.ja.md) · [简体中文](README.zh.md)

</div>

---

Peers talk directly over WebRTC data channels. Connecting everyone to everyone does not scale,
so mistlib bounds each node's connection count and picks *which* peers to hold from 3D proximity
and neighbour density per direction. Messages for peers outside that set are relayed across the
resulting overlay.

Written in Rust. Ships as a native library for engines and desktop apps, and as WebAssembly for
the browser.

## Features

|  |  |
| --- | --- |
| **Bounded fan-out** | ~30 connections per node (configurable), however crowded the world gets |
| **Area of interest** | You hear about the nodes near you, not about everyone |
| **Serverless signaling** | Peers meet over [Nostr](https://nostr.com) relays — no signaling server to run |
| **Multiple rooms** | Several rooms per process, each with its own overlay |
| **Shared storage** | Content-addressed blobs replicated between peers, persisted to OPFS |
| **Media** | Audio and video tracks over the same WebRTC connections |

## Install

```sh
npm install mistlib
```

No build step? Import a pinned version straight from a CDN:

```js
import init, { init_with_config } from "https://cdn.jsdelivr.net/npm/mistlib@0.6.0/mistlib_wasm.js";
```

For engines and desktop apps, grab a native build from
[Releases](https://github.com/tik-choco-lab/mistlib/releases/latest):

| Platform | Asset | Library |
| --- | --- | --- |
| Linux | `mistlib-native-linux-x86_64-<version>.zip` | `libmistlib.so` |
| Windows | `mistlib-native-windows-x86_64-<version>.zip` | `mistlib.dll` |
| macOS | `mistlib-native-macos-aarch64-<version>.zip` | `libmistlib.dylib` |
| Web | `mistlib-wasm-<version>.zip` | `pkg/` — the same wasm build, to vendor |

> [!IMPORTANT]
> Pin a version. The overlay wire protocol may change between releases, and every peer in a
> deployment must run the same one.

Unity, Python and JavaScript wrappers with runnable samples live in
[**mistlib-examples**](https://github.com/tik-choco-lab/mistlib-examples).

## Usage

```js
import init, {
  init_with_config, join_room, update_position, send_message,
  register_event_callback, MistEvent, Delivery,
} from "mistlib";

await init();

init_with_config("alice", JSON.stringify({
  signaling: {
    mode: "nostr",
    nostr: {
      relays: ["wss://relay.example.com"],
      inviteSalt: "my-app-2026",     // peers sharing a salt and code find each other
      inviteCode: "a-shared-secret", // pick your own — the built-in values are placeholders
    },
  },
  aoiRange: 20.0,
}));

register_event_callback((eventType, fromId, payload, roomId) => {
  if (eventType === MistEvent.PeerConnected) console.log("peer joined:", fromId);
});

join_room("lobby");
update_position(0.0, 1.5, 0.0);
send_message("", new TextEncoder().encode("hello"), Delivery.Reliable); // "" = broadcast
```

Call `update_position` as your avatar moves — it is what drives topology and area of interest.

## API

| Call | Does |
| --- | --- |
| `init_with_config(id, json)` | Create the node. Returns `false` if the config is invalid |
| `join_room(id)` / `leave_room_id(id)` | Enter or leave a room |
| `update_position(x, y, z)` | Report where you are |
| `send_message(target, bytes, delivery)` | Send to one peer, or to the room with `""` |
| `get_neighbors()` / `get_all_nodes()` | Current view of the room, as JSON |
| `storage_add(name, bytes)` / `storage_get(cid)` | Content-addressed storage |
| `register_event_callback(fn)` | Peer, area-of-interest and room events |

Without an explicit room, these act across every room you have joined: `send_message("", …)`
broadcasts to all of them and `get_neighbors()` merges their views. Multi-room applications
should use the `*_in_room` variants, which take a room id.

The native library offers the same operations over a C ABI with `(pointer, length)` pairs, with
two differences: there is no `get_neighbors` / `get_all_nodes` (that data arrives as events
instead), and only `update_position` and `send_message` have `*_in_room` variants. See
`mistlib-native/src/ffi.rs`.

**Delivery modes** — `Delivery.Reliable` (`0`) arrives in order; `Delivery.UnreliableOrdered`
(`1`) may drop but never reorders; `Delivery.Unreliable` (`2`) may drop or reorder, for the
lowest latency. Event ids come from the `MistEvent` enum the same way.

## Configuration

A JSON object passed to `init_with_config`, or to `set_config` later. Anything you omit keeps its
default, and `get_config()` dumps the configuration currently in effect, which is the quickest way
to see every key and what it is set to.

| Key | Default | Meaning |
| --- | --- | --- |
| `signaling.mode` | `"nostr"` | `"nostr"` or `"websocket"` |
| `signaling.nostr.relays` | *(empty)* | Relays used for peer discovery. Left empty, mistlib fetches a relay list over the network at startup — set this to keep discovery under your control |
| `signaling.nostr.inviteSalt` / `inviteCode` | placeholders | Shared secret scoping discovery to your app |
| `aoiRange` | `10.0` | Radius, in world units, that counts as near |
| `maxConnectionCount` | `30` | Upper bound on simultaneous peer connections |
| `hopCount` | `2` | Maximum relay hops for a message |
| `maxMessageBytes` | `65536` | Rejection threshold for a single message |
| `storageMaxCapacityMb` | `8192` | Local budget for content-addressed storage |
| `iceServers` | 3 public STUN servers | WebRTC ICE servers. Replace to add your own TURN |

### NAT traversal

The default `iceServers` are three public STUN servers, spread across operators so a peer
behind a network that blocks one still gets a reflexive candidate. STUN cannot get through
symmetric NAT, though — that needs a TURN relay, and relaying costs real bandwidth, so none
ships as a default. Supply your own to cover it:

```json
{
  "iceServers": [
    { "urls": ["stun:stun.l.google.com:19302"] },
    { "urls": ["turn:turn.example.com:3478"], "username": "user", "credential": "pass" }
  ]
}
```

Without TURN, expect a small share of peer pairs to fail to connect.

## Building from source

The toolchain version is pinned in `rust-toolchain.toml`, so `rustup` picks it up for you.

```sh
cargo build --release -p mistlib-native                        # .so / .dll / .dylib
cd mistlib-wasm && wasm-pack build --target web --release      # pkg/
```

## Built with mistlib

A family of browser apps shares state peer-to-peer using mistlib — [TC Space](https://tik-choco.github.io/tc-vrsns2/), [TC Town](https://tik-choco.github.io/tc-town/), [TC Chat](https://tik-choco.github.io/tc-chat/), [TC Storage](https://tik-choco.github.io/tc-storage/) and more. Browse them from [TC Home](https://tik-choco.github.io/tc-home/).

## Status

Under active development; the API is not yet stable. Releases are tagged, and the wire protocol
may change between them.

## License

[Mozilla Public License 2.0](LICENSE)
