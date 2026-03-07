# mistlib

**mistlib** は、Webブラウザ、ネイティブ、およびゲームエンジン間で動作する分散P2Pネットワークライブラリです。
サーバーを介さずユーザー間で直接通信を行うことで、低遅延な状態同期を実現します。

---

## 主な機能

- **マルチプラットフォーム**: Rust製の共通コアにより、デスクトップおよびブラウザ（WASM）に対応。
- **言語ラッパー**: Unity (C#)、Python、JavaScript/TypeScript から利用可能。
- **通信方式**: WebRTC (P2P) と WebSocket を併用し、環境に応じた接続を構築。
- **空間同期 (AOI)**: 3次元座標に基づき、近接ノード間での通信を最適化。
- **トポロジー制御**: 接続状況に応じてネットワーク構造を動的に更新。

## プロジェクト構成

- **mistlib-core**: P2Pアルゴリズムおよび通信制御ロジックの基盤。
- **mistlib-native**: PC・サーバー向け実装。
- **mistlib-wasm**: WebAssembly環境向け実装。
- **wrappers**: 各開発環境向けのインターフェース。

## ビルド済みバイナリの利用

Rust のビルド環境がない場合は、原則として GitHub の **Releases** から配布済みバイナリを利用してください。

- **Releases**: 利用者向けの正式な配布物。
- **Actions Artifacts**: CI の検証用に生成される一時的な成果物。

### Releases から取得する

1. GitHub リポジトリの **Releases** ページを開く。
2. 対象バージョンの **Assets** から必要なファイルをダウンロードする。
3. 利用環境に応じて以下を選択する。
  - `mistlib-wasm-pkg`: Web/WASM 用。
  - `mistlib-native-windows`: Windows 用 (`.dll`)。
  - `mistlib-native-linux`: Linux 用 (`.so`)。
  - `mistlib-native-macos`: macOS 用 (`.dylib`)。

各 Release 資産には、利用しやすいように対応する wrapper も同梱されます。

- `mistlib-wasm-pkg`: `mistlib-wasm/pkg` と `wrappers/web` を同梱。
- `mistlib-native-*`: ネイティブライブラリ本体に加えて `wrappers/python` と `wrappers/unity/MistLib` を同梱。

また、`v*` 形式のタグを push すると、CI がそのまま Release 資産を生成し、GitHub Release を自動作成します。

## Web/WASM の開発セットアップ

現状の Web 向けラッパーは、npm 公開済みパッケージではなく、**このリポジトリを clone して開発する前提の repo-local wrapper** です。

### 前提ツール

- Rust stable
- `wasm32-unknown-unknown` ターゲット
- `wasm-pack`

セットアップ例:

1. 依存確認として `cargo test --workspace` を実行する。
2. [mistlib-wasm](mistlib-wasm) で `wasm-pack build --target web` を実行する。
3. 生成された [mistlib-wasm/pkg](mistlib-wasm/pkg) を使って、[wrappers/web/index.js](wrappers/web/index.js) から `MistNode` を読み込む。

> `wrappers/web` は TypeScript 定義付きの ESM ラッパーですが、現時点では repo 内利用を前提にしています。

### Release 資産をそのまま使う場合

Releases から `mistlib-wasm-pkg` を取得した場合は、展開した `pkg` ディレクトリ内の `mistlib_wasm.js` を直接読み込めます。

## 機能詳細 (WASM/Web版)

`MistNode` クラスを通じて以下の機能を提供します。

- **ノード/ルーム管理**: 初期化、ルームへの参加・退出。
- **座標同期**: 3次元位置の更新と、周囲（AOI内）のノード情報の取得。
- **メッセージング**: バイナリ・テキスト・JSONデータの送受信。
  - `toId` を空にすると全ノードへの放送（Broadcast）になります。
  - `delivery`: 0(Reliable), 1(UnreliableOrdered), 2(Unreliable)。
- **メディア同期**: WebRTCによる音声・ビデオトラックの公開と受信。
- **ストレージ**: OPFS (Origin Private File System) を利用したデータの永続化。

## 主要API (WASM/Web)

`MistNode` クラスを通じて提供されます。

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

## 利用例 (Web版)

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

上記は、たとえば `examples/web` のような**同一リポジトリ内のサンプルやアプリ**から利用する想定です。

---

## AIエージェントを利用した開発

各種AIエージェントがプロジェクト構造を把握しやすくするため、以下の構成を推奨します。

- **[AI.md](AI.md)**: API定義や開発ルールをまとめたコンテキストファイル。

---

## 開発状況

現在**テスト版**です。仕様変更が頻繁に行われる可能性があるため、現時点では評価・テスト目的での利用を推奨します。正式公開は後日を予定しています。

---

## ライセンス

[MPL-2.0](LICENSE)

