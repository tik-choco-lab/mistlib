<div align="center">

# mistlib

**面向大型共享 3D 空间的点对点网络库**

[![npm](https://img.shields.io/npm/v/mistlib?logo=npm&logoColor=white&label=npm&color=cb3837)](https://www.npmjs.com/package/mistlib)
[![release](https://img.shields.io/github/v/release/tik-choco-lab/mistlib?label=release&color=2f6feb)](https://github.com/tik-choco-lab/mistlib/releases/latest)
[![CI](https://img.shields.io/github/actions/workflow/status/tik-choco-lab/mistlib/ci.yml?branch=main&label=CI)](https://github.com/tik-choco-lab/mistlib/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MPL--2.0-blue)](LICENSE)

[English](README.md) · [日本語](README.ja.md) · 简体中文

</div>

---

各节点通过 WebRTC 数据通道直接通信。让所有人互相连接无法扩展，因此 mistlib 会限制每个节点的
连接数上限，并根据三维空间中的距离和各个方向上的邻居密度来决定**保留哪些**连接。发往未直连
节点的消息，则在由此形成的覆盖网络上中继转发。

使用 Rust 编写。既可作为原生库供游戏引擎和桌面应用使用，也提供面向浏览器的 WebAssembly 构建。

## 特性

|  |  |
| --- | --- |
| **有界的连接数** | 无论世界多么拥挤，单个节点始终维持约 30 条连接（可配置） |
| **兴趣区域 (AOI)** | 你只会收到附近节点的信息，而不是所有人的 |
| **无服务器信令** | 节点通过 [Nostr](https://nostr.com) 中继互相发现，无需自建信令服务器 |
| **多房间** | 单个进程可同时加入多个房间，每个房间拥有独立的覆盖网络 |
| **共享存储** | 内容寻址的数据块在节点间复制，并持久化到 OPFS |
| **媒体** | 音视频轨道通过同一批 WebRTC 连接发布 |

## 安装

```sh
npm install mistlib
```

不想引入构建流程？固定版本号后直接从 CDN 引入：

```js
import init, { init_with_config } from "https://cdn.jsdelivr.net/npm/mistlib@0.6.0/mistlib_wasm.js";
```

面向游戏引擎和桌面应用，请从
[Releases](https://github.com/tik-choco-lab/mistlib/releases/latest) 下载原生构建。

| 平台 | 文件 | 库 |
| --- | --- | --- |
| Linux | `mistlib-native-linux-x86_64-<version>.zip` | `libmistlib.so` |
| Windows | `mistlib-native-windows-x86_64-<version>.zip` | `mistlib.dll` |
| macOS | `mistlib-native-macos-aarch64-<version>.zip` | `libmistlib.dylib` |
| Web | `mistlib-wasm-<version>.zip` | `pkg/` —— 同一份 wasm 构建，供随项目分发 |

> [!IMPORTANT]
> 请务必固定版本。覆盖网络的 wire 协议可能在版本之间发生变化，同一次部署中的所有节点
> 必须运行相同版本。

Unity、Python 和 JavaScript 的封装以及可运行的示例位于
[**mistlib-examples**](https://github.com/tik-choco-lab/mistlib-examples)。

## 用法

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
      inviteSalt: "my-app-2026",     // salt 与 code 相同的节点之间才能互相发现
      inviteCode: "a-shared-secret", // 请设置自己的值，内置默认值仅为占位
    },
  },
  aoiRange: 20.0,
}));

register_event_callback((eventType, fromId, payload, roomId) => {
  if (eventType === MistEvent.PeerConnected) console.log("已连接:", fromId);
});

join_room("lobby");
update_position(0.0, 1.5, 0.0);
send_message("", new TextEncoder().encode("hello"), Delivery.Reliable); // "" 为广播
```

角色移动时请调用 `update_position` —— 它驱动着拓扑结构与兴趣区域的计算。

## API

| 调用 | 作用 |
| --- | --- |
| `init_with_config(id, json)` | 创建节点。配置无效时返回 `false` |
| `join_room(id)` / `leave_room_id(id)` | 加入或离开房间 |
| `update_position(x, y, z)` | 上报自身位置 |
| `send_message(target, bytes, delivery)` | 发送给某个节点，或以 `""` 广播至整个房间 |
| `get_neighbors()` / `get_all_nodes()` | 以 JSON 获取当前房间的视图 |
| `storage_add(name, bytes)` / `storage_get(cid)` | 内容寻址存储 |
| `register_event_callback(fn)` | 节点、兴趣区域与房间事件 |

不指定房间的版本会作用于你加入的所有房间：`send_message("", …)` 会向全部房间广播，
`get_neighbors()` 会合并所有房间的视图。使用多个房间的应用请改用带房间 ID 的
`*_in_room` 变体。

原生库通过 C ABI 提供相同的操作（字符串以 `(指针, 长度)` 的形式传递），但有两点不同：
没有 `get_neighbors` / `get_all_nodes`（这些信息通过事件送达），且只有 `update_position`
和 `send_message` 提供 `*_in_room` 变体。详见 `mistlib-native/src/ffi.rs`。

**投递模式** —— `Delivery.Reliable`（`0`）必达且保持顺序；`Delivery.UnreliableOrdered`
（`1`）可能丢失但绝不乱序；`Delivery.Unreliable`（`2`）可能丢失或乱序，延迟最低。
事件 ID 同样可以从 `MistEvent` 取得。

## 配置

传给 `init_with_config`（或之后调用 `set_config`）的 JSON 对象。未指定的项保持默认值，
`get_config()` 会输出当前生效的配置，想确认有哪些键、各自是什么值，看它最快。

| 键 | 默认值 | 含义 |
| --- | --- | --- |
| `signaling.mode` | `"nostr"` | `"nostr"` 或 `"websocket"` |
| `signaling.nostr.relays` | *(空)* | 用于节点发现的中继。留空时会在启动阶段联网获取中继列表，若想自行掌控请显式指定 |
| `signaling.nostr.inviteSalt` / `inviteCode` | 占位值 | 将发现范围限定到你的应用的共享密钥 |
| `aoiRange` | `10.0` | 视为「附近」的半径（世界单位） |
| `maxConnectionCount` | `30` | 同时连接数的上限 |
| `hopCount` | `2` | 消息中继的最大跳数 |
| `maxMessageBytes` | `65536` | 单条消息的拒收阈值 |
| `storageMaxCapacityMb` | `8192` | 内容寻址存储的本地容量预算 |
| `iceServers` | 3 个公共 STUN | WebRTC 的 ICE 服务器。需要 TURN 时替换此项 |

### NAT 穿透

默认的 `iceServers` 是分属不同运营方的三个公共 STUN，即使其中一个被网络屏蔽，节点仍能
拿到 server-reflexive 候选。但 STUN **无法穿透对称型 NAT**，那需要 TURN 中继；中继会产生
实际的带宽成本，因此不作为默认值提供。需要时请自行配置：

```json
{
  "iceServers": [
    { "urls": ["stun:stun.l.google.com:19302"] },
    { "urls": ["turn:turn.example.com:3478"], "username": "user", "credential": "pass" }
  ]
}
```

没有 TURN 时，请预期会有一小部分节点之间无法建立连接。

## 从源码构建

工具链版本固定在 `rust-toolchain.toml` 中，`rustup` 会自动读取。

```sh
cargo build --release -p mistlib-native                        # .so / .dll / .dylib
cd mistlib-wasm && wasm-pack build --target web --release      # pkg/
```

## 使用 mistlib 的应用

一系列浏览器应用通过 mistlib 以点对点方式共享状态 —— [TC Space](https://tik-choco.github.io/tc-vrsns2/)、[TC Town](https://tik-choco.github.io/tc-town/)、[TC Chat](https://tik-choco.github.io/tc-chat/)、[TC Storage](https://tik-choco.github.io/tc-storage/) 等。可从 [TC Home](https://tik-choco.github.io/tc-home/) 浏览全部。

## 状态

仍在积极开发中，API 尚未稳定。每次发布都会打标签，但 wire 协议可能在版本之间发生变化。

## 许可

[Mozilla Public License 2.0](LICENSE)
