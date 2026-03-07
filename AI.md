# AI Agent Context for mistlib

## プロジェクト概要
**mistlib** は、マルチプラットフォーム（Web, Native, Unity）対応の分散P2Pネットワークライブラリです。
WebRTCおよびWebSocketを併用し、サーバー負荷を抑えた低遅延な状態同期を実現します。

## アーキテクチャ
- **コアエンジン**: Rust (`mistlib-core`) によるP2P制御。
- **空間同期 (AOI)**: 3次元座標に基づき、近接ノード間での通信を最適化。

## 主要API (WASM/JavaScript)
`wrappers/web/index.js` の `MistNode` クラスを使用します。

- `new MistNode(nodeId, signalingUrl)`: インスタンス化（シグナリングサーバーのURLを指定）。
- `await node.init()`: WASMロード・初期化（必須）。
- `node.joinRoom(roomId)` / `node.leaveRoom()`: ルーム参加・退出。
- `node.updatePosition(x, y, z)`: 座標更新。
- `node.sendMessage(toId, payload, delivery)`: メッセージ送受信。
  - `toId=""` で全員へ送信 (Broadcast)。
  - `delivery`: 0(Reliable), 1(UnreliableOrdered), 2(Unreliable)。
- `node.getNeighbors()`: 周囲（AOI内）のノード一覧取得。
- `node.getAllNodes()`: ルーム内の全ノード一覧取得。
- `node.onEvent(handler)`: イベント処理（定数 0:RAW, 1:OVERLAY, 2:NEIGHBORS, 3:AOI_ENTERED, 4:AOI_LEFT）。
- `node.onMediaEvent(handler)`: メディア処理（定数 100:TRACK_ADDED, 101:TRACK_REMOVED）。
- `node.getStats()`: 通信統計取得。
- `storage_add(path, data)` / `storage_get(path)`: データ保存・取得。

## 実装上の注意
- **非同期**: `init` は必ず `await` してください。
- **データ型**: `sendMessage` のペイロードは `Uint8Array`, `String`, またはオブジェクトが可能です。

## 指示例
- 「`MistNode` を初期化し、座標を動的に更新する処理を書いてください」
- 「受信メッセージを画面に表示するコンポーネントを作成してください」
- 「マイク入力を取得して、メディアトラックとして配信してください」
