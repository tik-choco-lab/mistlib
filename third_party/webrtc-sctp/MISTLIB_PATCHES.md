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

## SPEC-20 P4 delayed-ACK timer lifecycle (2026-10-25)

The delayed-ACK timer is one-shot, but its expiry callback retained `close_tx`.
After emitting a SACK, the association returned to `AckState::Idle`; the next
isolated DATA entered `Delay` while `AckTimer::start` rejected the stale handle.
Its SACK then depended on another packet or sender retransmission. Retire the
handle in `AssociationInternal::on_ack_timeout` under the existing association
lock before requesting the SACK. Keep the ACK interval, wire format, receive
window, reliability, and congestion policy unchanged.

Paused-time association regressions deliver isolated ordered/unordered messages,
await each write-loop wakeup without sending a second packet, inspect the emitted
SACK, verify its cumulative TSN, and read the delivered payload. Both fail on
the original implementation after the first expiry and pass with the fix.
Run the standalone crate tests above, or filter by `delayed_ack_rearms_after_expiry`.

This is an independently confirmed receiver defect, not a complete explanation
of Firefox/native WAN stalls: the timer-only timing cohort still exhibited ICE
restart and association replacement. See SPEC-20's P4 memo and the dated storage
evaluation for the separate browser repair investigation and acceptance results.

## SPEC-25 KR3.4 selectable congestion control (research, 2026-11-26)

What: `association::set_default_congestion_control(CongestionControl::{Reno, Bbr3})`
selects the sender controller for associations created afterwards (same
process-wide pattern and reason as the receive-buffer default above;
`default_congestion_control()` reads it). `Reno` is the default and is
upstream's RFC 4960 §7.2 code path, unchanged: every BBR hook is behind
`AssociationInternal::bbr == None`. No wire, SACK, receiver or Config change.

Why: SPEC-24 stage B measured QUIC+BBRv3 at 6–11× the goodput of webrtc-rs
SCTP under loss. To separate protocol from controller, `Bbr3` runs the same
research BBRv3 model as `tests/quicprobe/src/bbr3` under SCTP's loss recovery.

- `src/congestion/bbr3/`: draft-ietf-ccwg-bbr-06 model and
  draft-cheng-iccrg-delivery-rate-estimation-02 sampler, ported from the probe
  without quinn types (same model, gains, filters and loss responses).
- `src/congestion/sctp.rs`: TSN adapter and pacer. Units are DATA payload bytes
  (SCTP's own cwnd/rwnd unit); MSS = 1200. Each transmission gets a sample record
  (delivered, delivered time, first-sent time, app-limited, in-flight at send).
  Newly acked TSNs come from the cumulative TSN ack and from gap blocks (each TSN
  once). The third miss indication (HTNA rule, also during Fast Recovery) and
  T3-rtx declare chunks lost; lost chunks leave BBR in-flight until resent.
  SCTP resends under the same TSN, so a retransmission's SACK delivers bytes but
  never yields an RTT or rate sample (Karn); ACKs with only such deliveries still
  update the window, as BBRUpdateControlParameters runs on every ACK. A late SACK
  of a chunk already declared lost counts as delivered without undo.
- Admission: new DATA needs BBR in-flight + chunk ≤ BBR cwnd (and rwnd).
  T3 retransmissions get the free window (cwnd − in-flight). Fast retransmit
  keeps RFC 4960 §7.2.4 (one packet per SACK, cwnd ignored) but is charged to
  the pacer. Fast Recovery still gates miss indications, but the Reno ssthresh
  and cwnd cut are skipped.
- Pacer (BBR only): quinn's explicit-rate pacer as patched for the probe —
  quantum `clamp(rate/500, 2·MSS, 64·MSS)`, bucket 2 quanta (bounded burst),
  refill at the previous rate; when blocked, one tokio timer wakes the write loop.
- T3-rtx: BBROnEnterRTO (save cwnd, cwnd = in-flight + 1 MSS, model kept), and
  BBRRestoreCwnd once the cumulative ack reaches the highest TSN sent at the RTO.
  The RTO value and backoff remain SCTP's.

Not implemented or deliberately different: Fast Recovery packet conservation
and cwnd restore (the QUIC probe omits them too), spurious-loss undo, ECN.
PR-SCTP abandoned chunks get no miss indications, so unreliable channels report
no losses to BBR and FORWARD-TSN-skipped chunks count as delivered. Wrap-around
breaks one loss-range adjacency per 2^32 TSNs. RTT samples include the
receiver's delayed SACK (≤ 200 ms). SCTP cwnd/state are not exported through
webrtc-rs statistics.

Tests: model, sampler, adapter and pacer unit tests (`congestion::`); internal
tests driving `handle_sack` with gap blocks, fast retransmit and T3-rtx
(`test_bbr_`, `test_reno_default_has_no_bbr_state`); a 1 MiB ordered transfer
under deterministic DATA/SACK loss (`test_assoc_bbr3_transfer_completes_under_loss`).
Tests switch one association with a `cfg(test)` setter, never the global.
Removal: delete `src/congestion/`, the `bbr`/`pacer_wakeup` fields and their
hooks, and the selector in `association/mod.rs`; Reno needs no change.
