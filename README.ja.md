<div align="center">

# mistlib

**広い 3D 空間を大人数で共有するための P2P ネットワークライブラリ**

[![npm](https://img.shields.io/npm/v/mistlib?logo=npm&logoColor=white&label=npm&color=cb3837)](https://www.npmjs.com/package/mistlib)
[![release](https://img.shields.io/github/v/release/tik-choco-lab/mistlib?label=release&color=2f6feb)](https://github.com/tik-choco-lab/mistlib/releases/latest)
[![CI](https://img.shields.io/github/actions/workflow/status/tik-choco-lab/mistlib/ci.yml?branch=main&label=CI)](https://github.com/tik-choco-lab/mistlib/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MPL--2.0-blue)](LICENSE)

[English](README.md) · 日本語 · [简体中文](README.zh.md)

</div>

---

ピア同士は WebRTC データチャネルで直接通信する。全員が全員に接続する方式は規模が大きく
なると破綻するため、mistlib は各ノードの接続数に上限を設けたうえで、3D 空間上の近さと
方向ごとの近傍密度から「どのピアと繋いでおくか」を決める。直接繋がっていないピア宛の
メッセージは、こうしてできたオーバーレイ上を中継される。

Rust 製。ゲームエンジンやデスクトップアプリ向けのネイティブライブラリと、ブラウザ向けの
WebAssembly を提供する。

## 特徴

|  |  |
| --- | --- |
| **接続数の上限** | 世界がどれだけ混雑しても 1 ノードあたり約 30 接続（変更可） |
| **AOI (Area of Interest)** | 全員ではなく、自分の近くにいるノードだけが通知される |
| **サーバレスなシグナリング** | ピアの発見は [Nostr](https://nostr.com) リレー経由。専用サーバの運用が不要 |
| **マルチルーム** | 1 プロセスで複数のルームを持て、それぞれが独立したオーバーレイを張る |
| **共有ストレージ** | コンテンツアドレス指定の blob をピア間で複製し、OPFS に永続化 |
| **メディア** | 音声・映像トラックを同じ WebRTC 接続の上で配信 |

## 導入

```sh
npm install mistlib
```

ビルド工程を挟まないなら、バージョンを固定して CDN から直接 import してもよい。

```js
import init, { init_with_config } from "https://cdn.jsdelivr.net/npm/mistlib@0.6.0/mistlib_wasm.js";
```

ゲームエンジンやデスクトップアプリ向けには、
[Releases](https://github.com/tik-choco-lab/mistlib/releases/latest) からネイティブビルドを取得する。

| 環境 | アセット | ライブラリ |
| --- | --- | --- |
| Linux | `mistlib-native-linux-x86_64-<version>.zip` | `libmistlib.so` |
| Windows | `mistlib-native-windows-x86_64-<version>.zip` | `mistlib.dll` |
| macOS | `mistlib-native-macos-aarch64-<version>.zip` | `libmistlib.dylib` |
| Web | `mistlib-wasm-<version>.zip` | `pkg/` — 同じ wasm ビルド。自前で同梱する場合に |

> [!IMPORTANT]
> バージョンは必ず固定すること。オーバーレイの wire プロトコルはリリース間で変わりうるため、
> ひとつのデプロイに参加するピアは全員同じバージョンである必要がある。

Unity・Python・JavaScript のラッパーと動作するサンプルは
[**mistlib-examples**](https://github.com/tik-choco-lab/mistlib-examples) にある。

## 使い方

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
      inviteSalt: "my-app-2026",     // salt と code が一致するピア同士が発見し合う
      inviteCode: "a-shared-secret", // 必ず自前の値を。組み込みの既定値はプレースホルダ
    },
  },
  aoiRange: 20.0,
}));

register_event_callback((eventType, fromId, payload, roomId) => {
  if (eventType === MistEvent.PeerConnected) console.log("接続:", fromId);
});

join_room("lobby");
update_position(0.0, 1.5, 0.0);
send_message("", new TextEncoder().encode("hello"), Delivery.Reliable); // "" でブロードキャスト
```

アバターが動いたら `update_position` を呼ぶこと。これがトポロジーと AOI を駆動する。

## API

| 呼び出し | 役割 |
| --- | --- |
| `init_with_config(id, json)` | ノードを生成する。設定が不正なら `false` を返す |
| `join_room(id)` / `leave_room_id(id)` | ルームへの参加・退出 |
| `update_position(x, y, z)` | 自分の位置を報告する |
| `send_message(target, bytes, delivery)` | 特定のピアへ、または `""` でルーム全体へ送信 |
| `get_neighbors()` / `get_all_nodes()` | 現在のルームの見え方を JSON で取得 |
| `storage_add(name, bytes)` / `storage_get(cid)` | コンテンツアドレス指定ストレージ |
| `register_event_callback(fn)` | ピア・AOI・ルームのイベント |

ルームを指定しない版は、参加中の全ルームに対して働く。`send_message("", …)` は全ルームへ
ブロードキャストし、`get_neighbors()` は全ルームの見え方を統合して返す。複数ルームを扱う
アプリはルーム ID を渡す `*_in_room` 版を使うこと。

ネイティブライブラリは同じ操作を C ABI で提供する（文字列は `(ポインタ, 長さ)` の組）。ただし
2 点異なる: `get_neighbors` / `get_all_nodes` は存在せず（その情報はイベントで届く）、
`*_in_room` 版があるのは `update_position` と `send_message` だけ。詳細は
`mistlib-native/src/ffi.rs` を参照。

**配送モード** — `Delivery.Reliable`（`0`）は到達し順序も保つ。
`Delivery.UnreliableOrdered`（`1`）は欠落しうるが順序は乱れない。
`Delivery.Unreliable`（`2`）は欠落・順序入れ替わりを許す代わりに遅延が最小。
イベント ID も同様に `MistEvent` から取れる。

## 設定

`init_with_config`（または後から `set_config`）に渡す JSON オブジェクト。省略した項目は既定値の
まま。`get_config()` は現在有効な設定をそのまま吐き出すので、どのキーが何になっているかを
確かめるのが一番早い。

| キー | 既定値 | 意味 |
| --- | --- | --- |
| `signaling.mode` | `"nostr"` | `"nostr"` または `"websocket"` |
| `signaling.nostr.relays` | *(空)* | ピア発見に使うリレー。空のままだと起動時にネットワーク越しにリレー一覧を取得しに行くので、経路を自分で握りたければ明示すること |
| `signaling.nostr.inviteSalt` / `inviteCode` | プレースホルダ | 発見範囲をアプリ単位に区切る共有秘密 |
| `aoiRange` | `10.0` | 「近い」とみなす半径（ワールド単位） |
| `maxConnectionCount` | `30` | 同時接続数の上限 |
| `hopCount` | `2` | メッセージ中継の最大ホップ数 |
| `maxMessageBytes` | `65536` | 1 メッセージあたりの拒否閾値 |
| `storageMaxCapacityMb` | `8192` | コンテンツアドレス指定ストレージのローカル上限 |
| `iceServers` | 公開 STUN 3本 | WebRTC の ICE サーバ。TURN を足すならここを差し替える |

### NAT 越え

既定の `iceServers` は運用者の異なる公開 STUN 3本で、片方が塞がれた網でも
server-reflexive 候補が得られるようにしてある。ただし STUN では**対称 NAT を越えられない**。
それには TURN 中継が要るが、中継は実費で帯域を食うため既定には入れていない。必要なら
自前のものを渡すこと。

```json
{
  "iceServers": [
    { "urls": ["stun:stun.l.google.com:19302"] },
    { "urls": ["turn:turn.example.com:3478"], "username": "user", "credential": "pass" }
  ]
}
```

TURN 無しでは、一定割合のピア同士が接続できないことを見込んでおくこと。

## ソースからのビルド

ツールチェインのバージョンは `rust-toolchain.toml` で固定してあり、`rustup` が自動で読む。

```sh
cargo build --release -p mistlib-native                        # .so / .dll / .dylib
cd mistlib-wasm && wasm-pack build --target web --release      # pkg/
```

## mistlib を使っているもの

複数のブラウザアプリが mistlib で状態を P2P 共有している — [TC Space](https://tik-choco.github.io/tc-vrsns2/)、[TC Town](https://tik-choco.github.io/tc-town/)、[TC Chat](https://tik-choco.github.io/tc-chat/)、[TC Storage](https://tik-choco.github.io/tc-storage/) ほか。一覧は [TC Home](https://tik-choco.github.io/tc-home/) から。

## ステータス

開発途上であり、API はまだ安定していない。リリースにはタグを打っているが、wire プロトコルは
リリース間で変わりうる。

## ライセンス

[Mozilla Public License 2.0](LICENSE)
