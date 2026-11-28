# MistLib quinn-proto patches

Vendored from the crates.io source of **quinn-proto 0.11.16** (used with quinn 0.11.11).
The normalized Cargo.toml (plus an empty `[workspace]`, see below), the upstream
Cargo.lock and both licenses are retained; registry bookkeeping and Cargo.toml.orig
are omitted. The root workspace (`mistlib-native`'s ICE+QUIC link, SPEC-25) and
`tests/quicprobe` select this copy through `[patch.crates-io]`. The root lockfile
pins quinn 0.11.11 / quinn-udp 0.5.15, whose quinn-proto requirement (>= 0.11.15)
this version satisfies; quinn 0.11.12 requires quinn-proto >= 0.11.18 and would
silently bypass the patch.

The changes add an explicit pacing rate and per-packet send/ack/loss callbacks to
the congestion-control interface, which the BBRv3 controller
(`mistlib-native/src/transports/ice_quic/bbr3`, ported from `tests/quicprobe/src/bbr3`)
needs. Cubic, NewReno and quinn's BBR keep the default (no-op) implementations.

| registry 版から変更したファイル | 目的と差分 | upstream 提案の範囲 |
| --- | --- | --- |
| `src/congestion.rs` | `Controller::pacing_rate() -> Option<u64>` (B/s、既定 None)、既定が空の `on_data_sent/acked/lost` を追加。既存の callback と qlog の `metrics().pacing_rate` (bit/s) を保持 | 明示レートは独立に提案可能。パケット通知は番号空間、経路変更、破棄時の契約を一般化してから提案したい |
| `src/connection/packet_builder.rs` | ACK-eliciting な 1-RTT packet の組み立て終了時に時刻、packet number、QUIC wire bytes、追加前の path inflight bytes、直前の app-limited 判定を通知。MTU probe を除外。GSO の集約前に個別サイズを取得 | 上記のパケット通知と一体 |
| `src/connection/mod.rs` | 送信判定に controller の明示レートを渡す。Data 番号空間、現行 path generation の ACK-eliciting packet について、初回 ACK と個別損失を通知。従来の ACK バッチ終了、RTT 更新、損失検出の呼び出し順を保持 | pacer と packet observer の二つの差分として切り出せる |
| `src/connection/pacing.rs` | レート選択の入口と明示レート用の token 補充を追加。None のときは従来の `delay` 本体をそのまま呼ぶ。None の送信トレース比較と Some の補充量テストを追加 | レート API と併せて提案可能。burst と timer overshoot の設計は他のランタイムでも要検証 |
| `Cargo.toml` | 空の `[workspace]` を追加。vendor の元の lockfile でライブラリテストを単独実行できる | プローブの配置に固有 |

明示レート側は `q = clamp(rate / 500, 2 * MTU, 64 * MTU)` bytes を一度の起床の
目標量とする。bucket 容量は `2q`。2 ms 相当の送出と、ユーザー空間タイマーの
遅延 1 quantum 分を両立するためである。送信ごとに既存の `on_transmit` が
消費量を引く。前回からの経過時間には前回のレートを適用し、1 byte 未満の
補充量を持ち越す。0 B/s は 1 B/s として扱う。ACK-only、PTO probe など
Quinn 本来の pacing 検査の適用条件に従う。

パケットごとの delivered/lost 履歴は `src/bbr3` が所有する。Quinn の
`SentPacket` を拡張しない。ACK 側には送信サンプルの対象外も届き得るので、
対応する番号のない通知は無視する。probe は 0-RTT と migration を使用しない。

Cubic、NewReno、内蔵 BBR は新しい trait メソッドをすべて既定実装のまま使う。
None が継続する場合、bucket 初期化・容量・予約時刻・credit の演算は registry
版のコードを通る。回帰テストは MTU、cwnd、送信時刻を変えた 200 回の列について、
元の `delay` と新しい入口で各回の期限・利用可能トークンを比較する。

```bash
cargo test --locked --manifest-path third_party/quinn-proto/Cargo.toml \
  --no-default-features --features rustls-ring --lib 
```

## Testing and removal

The crate is excluded from the root workspace; run its library tests with
`cargo test --locked --manifest-path third_party/quinn-proto/Cargo.toml --no-default-features --features rustls-ring --lib`
(`just test` and the quality workflow run it). Drop this directory and the crates.io
patch once a quinn release exposes an explicit pacing rate and per-packet congestion
callbacks (or BBRv3 itself); re-measure the controller when validating that upgrade.
