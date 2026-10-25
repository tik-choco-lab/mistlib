# MistLib SCTP patches

Vendored from the crates.io source of **webrtc-sctp 0.12.0**, used by
webrtc-rs 0.13. The normalized Cargo.toml and both upstream licenses are retained.
Registry bookkeeping, Cargo.toml.orig, codecov.yml and the upstream examples
(ping/pong/throughput, which need tokio features this crate does not enable) are
omitted, along with their `[[example]]` manifest entries. `fuzz/artifacts/` is copied
from the upstream git tag v0.13.0 (`sctp/fuzz/artifacts`) because the crates.io
package lacks it and `src/fuzz_artifact_test.rs` reads those crash inputs. The upstream Cargo.lock is
retained for standalone tests; the root lockfile selects the patched dependency.

## Evidence and patches

Firefox/usrsctp under packet loss can bundle FORWARD-TSN with SACK/DATA when
abandoning partially reliable messages (including MistLib's unreliable PING/PONG).
The native peer logged `unable to parse SCTP packet chunk too short` and storage
fetches stalled. Packet::unmarshal supplies the remaining packet to each parser;
the old FORWARD-TSN loop consumed beyond its own value, rejected the packet and
prevented both cumulative TSN advancement and reliable DATA delivery.

- FORWARD-TSN: bound parsing by the declared value length, require a complete TSN
  and whole stream entries, and reject truncated or malformed values with errors.
- ABORT/ERROR: bound cause iteration by the chunk value and skip inter-cause
  alignment padding. Short or truncated causes return errors.
- INIT: bound optional parameter iteration by the chunk value. INIT bundling is
  still rejected by packet validation; malformed suffixes cannot extend parsing.
- SHUTDOWN: validate its declared TSN value length rather than the remaining packet
  length, accepting bundled control chunks and preventing a short-value read panic.
- Packet and RECONFIG cursors: advance by wire header lengths rather than lengths
  reconstructed from parsed objects. This preserves alignment across padded error
  causes and reconfiguration responses with optional TSN fields, which the upstream
  response object does not retain. Optional TSN semantics remain upstream behavior.

Regression tests use SCTP wire layouts and Packet::marshal for padding/CRC32c,
cover bundled and final FORWARD-TSN, malformed lengths, padded causes, INIT parser
boundaries, bundled SHUTDOWN, and reconfiguration responses with optional fields.

## Testing and removal

The crate is excluded from the root workspace. Cargo cannot test a nonmember with
dev-dependencies by root package selection alone; use
`cargo test --manifest-path third_party/webrtc-sctp/Cargo.toml -p webrtc-sctp --locked`.
The local test recipe and quality workflow run it separately. Workspace formatting
and strict core/native clippy remain scoped to the application workspace.

Drop this vendor directory and crates.io patch once an upstream release selected
by MistLib's webrtc version contains these fixes; retain equivalent regressions
when validating that upgrade.

## SPEC-20 receive buffer default

`association::set_default_receive_buffer_size(bytes)` supplies the receive-buffer
default for new associations whose `Config::max_receive_buffer_size` is zero.
Zero preserves upstream's 1 MiB default; an explicit association setting wins.
Native calls this from shared storage-transfer configuration before negotiating
room peers. Existing associations retain their receive window. The override is
process-wide: webrtc-rs 0.13 hardcodes zero and exposes no per-PC setting or
association getter. Independent per-transport values require an upstream
SettingEngine hook (or a further webrtc-rs patch); last configuration wins for
subsequently created associations. No packet encoding or sender limits change.

## SPEC-20 F3 pending-queue fairness

Upstream `PendingQueue::peek` always preferred unordered DATA. A continuously
refilled reliable-unordered bulk stream could indefinitely starve an already
submitted ordered application ping. Alternate ordered/unordered selection at
whole-message boundaries when both classes have work. Keep a fragmented message
selected through its ending fragment; no fragment interleaving or wire change.
The association-wide append mutex and 128 KiB pending-byte semaphore remain;
independent native writers can still briefly wait on shared admission and cwnd/rwnd.
F2 pacing bounds bulk submissions when enabled. This addresses queue starvation,
not a hard control-latency guarantee under network congestion or loss.

Socket-free regressions cover alternation, repeated bulk refill, stable peeks,
and fragment contiguity. Run `cargo test --manifest-path
third_party/webrtc-sctp/Cargo.toml -p webrtc-sctp --locked queue::queue_test::test_pending_queue`.
