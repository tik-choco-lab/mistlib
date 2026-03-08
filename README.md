# mistlib

**mistlib** は、Webブラウザ、ネイティブ、およびゲームエンジン間で動作する分散P2Pネットワークライブラリです。
サーバーを介さずユーザー間で直接通信を行うことで、低遅延な状態同期を実現します。

---

## 機能

- **マルチプラットフォーム & マルチ言語**: Rust製の共通コアにより、デスクトップ(Native)およびブラウザ(WASM)の両方に対応。Unity (C#)、Python、JavaScript/TypeScript から利用可能です。
- **通信・ネットワーク**:
  - WebRTC (P2P) と WebSocket を併用し、環境に応じた最適な接続（NAT越え等）を構築。
  - 接続状況に応じてネットワークトポロジーを動的に更新・最適化。
  - ルームへの参加・退出およびノードの状態管理。
- **空間同期 (AOI)**: 3次元座標に基づき、近接ノード間での通信を自動最適化。周囲（AOI内）のノード情報の取得が可能。
- **メッセージング**: バイナリ・テキスト・JSONデータの送受信。
  - ユニキャストおよびブロードキャスト（`toId` を空に指定）に対応。
  - 配送品質（Reliable / UnreliableOrdered / Unreliable）を選択可能。
- **メディア同期**: WebRTCによる音声・ビデオトラックのリアルタイム公開と受信。
- **ストレージ**: OPFS (Origin Private File System) 等を利用したデータの永続化。

## プロジェクト構成

- **mistlib-core**: P2Pアルゴリズムおよび通信制御ロジックの基盤。
- **mistlib-native**: PC・サーバー向け実装。
- **mistlib-wasm**: WebAssembly環境向け実装。
- **wrappers**: 各開発環境向けのインターフェース。

## 始め方

GitHubの [Releases](https://github.com/tik-choco-lab/mistlib/releases/latest) から、環境に合ったZIPをダウンロードしてご利用ください。

| ターゲット | 配布ファイル名 | 同梱されているもの |
| --- | --- | --- |
| **Web / WASM** | `mistlib-wasm-pkg.zip` | WASM本体 (`pkg/`) + JSラッパー (`wrappers/web/`) |
| **Windows** | `mistlib-native-windows.zip` | `.dll` + Python/Unity用ラッパー |
| **Linux** | `mistlib-native-linux.zip` | `.so` + Python/Unity用ラッパー |
| **macOS** | `mistlib-native-macos.zip` | `.dylib` + Python/Unity用ラッパー |


## AIエージェントを利用した開発

各種AIエージェントがプロジェクト構造を把握しやすくするため、API定義や開発ルールをまとめたファイルを用意しています。
最新のLLMの場合は不要かもしれませんが、小規模なLLMをご利用の際は、ご活用ください。

- **[AI.md](AI.md)**

## 主要API

`MistNode` クラス

- `node.joinRoom(roomId)` / `node.leaveRoom()`: ルームへの参加と退出。
- `node.updatePosition(x, y, z)`: 自身の座標を更新。
- `node.sendMessage(toId, data, method)`: メッセージ送受信。
- `node.getNeighbors()`: 周囲（AOI内）のノード一覧を取得。
- `node.getAllNodes()`: ルーム内の全ノード一覧を取得。
- `node.setConfig(config)`: 設定の更新（`{ "aoiRange": 100 }` 等の部分更新も可能）。
- `node.getStats()`: 通信統計の取得。
- `node.onEvent(handler)`: 以下の定数に基づくイベント処理。
  - 0: RAW, 1: OVERLAY, 2: NEIGHBORS, 3: AOI_ENTERED, 4: AOI_LEFT
- `node.onMediaEvent(handler)`: メディア関連イベント。
  - 100: TRACK_ADDED, 101: TRACK_REMOVED
- `storage_add(path, data)` / `storage_get(path)`: データ保存と取得。

## 利用例 (Web)

```javascript
import { MistNode } from '../wrappers/web/index.js';

const node = new MistNode("user-123");
await node.init();

node.joinRoom("mistlib-room-id");
node.updatePosition(10.5, 0, -5.2);

node.onEvent((type, fromId, payload) => {
    // イベント処理
});

node.sendMessage("target-id", "Hello P2P!");
```

---

## 開発状況

現在**テスト版**です。仕様変更が頻繁に行われる可能性があるため、現時点では評価・テスト目的での利用を推奨します。正式公開は後日を予定しています。

---

## ライセンス

[MPL-2.0](LICENSE)

