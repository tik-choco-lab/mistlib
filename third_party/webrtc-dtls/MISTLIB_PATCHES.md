# MistLib DTLS patches

Vendored from the crates.io source of **webrtc-dtls 0.12.0**, used by webrtc-rs 0.13.
The normalized Cargo.toml (minus the upstream `[[example]]` entries), the upstream
Cargo.lock and both licenses are retained. Registry bookkeeping, Cargo.toml.orig and
`examples/` are omitted. The root workspace and `tests/quicprobe` select this copy
through `[patch.crates-io]`.

## Omitted upstream test: `src/crypto/crypto_test.rs`

The upstream file embeds a test RSA private key in PEM form. The repository's commit
scan (`scripts/secret-patterns.conf`) and the public-sync scan reject private-key
blocks in any tracked or published file, so the file and its `mod crypto_test;`
declaration in `src/crypto/mod.rs` were removed. It is a test-only module; no
library code depends on it. The remaining upstream tests still pass.
A future re-vendor must drop it the same way (or regenerate the key at test time).

## Omitted upstream test: `src/prf/prf_test.rs`

The upstream PRF known-answer test embeds an X25519 private key and pre-master
secret test vectors. The publication review treats key material in published files
as credentials, so the file and its `mod prf_test;` declaration in `src/prf/mod.rs`
were removed, like `crypto_test.rs`. No MistLib patch touches the PRF; upstream
keeps the test.

## Retransmit the final flight after handshake completion (2026-11-26)

Evidence: on the NAT testbed (75 ms one-way delay, 5 % bursty loss) one peer reached
PeerConnection Connected while the other stayed Connecting with ICE Connected for
13 s or longer. Raw webrtc-rs showed the same asymmetric pattern.

Cause: `DTLSConn::handshake()` returned as soon as it first reached
`HandshakeState::Finished`, so the post-completion `finish()` state never ran, and
the reader stopped routing handshake records once `handshake_done_rx` closed. In a
full DTLS 1.2 handshake the server completes right after sending flight 6
(ChangeCipherSpec + Finished). When flight 6 was lost, the client retransmitted
flight 5, but the completed server ignored it. The client waited until its own
handshake timeout. RFC 6347 §4.2.4 requires the sender of the final flight to
retransmit it when the peer retransmits its final flight.

Patch:

- `handshaker.rs`: when the handshake completes and the current flight is the last
  send flight (`Flight6`), a copy of its prepared packets is stored in a shared
  `FinalFlightRetransmit` slot *before* completion is signalled. `handshake()` and
  `DTLSConn::new` still return immediately, so readiness is not delayed.
- `conn/mod.rs` reader: after completion, any handshake record received from the
  peer is a retransmission of its final flight. The reader answers by re-sending the
  stored flight through the normal outgoing path. This re-encrypts the epoch-1
  Finished and gives it fresh record sequence numbers, the same as an in-handshake
  retransmission. It does not route the record to the finished handshaker and does
  not raise an error.
  - Responses are rate-limited to one per half `flight_interval`, because one peer
    flight spans several datagrams.
  - The slot is cleared as soon as authenticated application data arrives from the
    peer, which proves the peer completed. Otherwise it expires after
    240 s (`FINAL_FLIGHT_RETRANSMIT_LIFETIME`, 2 × the default TCP MSL from
    RFC 6347 §4.2.4).
  - The side that completes on receiving the final flight (the client) stores
    nothing and stays silent. This matches pion/dtls, where only the sender of the
    last flight retransmits in the finished state.
- `fragment_buffer/mod.rs`: fragments whose message sequence is older than the
  next expected one are skipped, as in pion/dtls. Before this change, such
  retransmissions were buffered and never popped. After completion they would grow
  without bound up to the 2 MB overflow error. They are still reported as handshake
  records, which is what triggers the retransmission above.

Regressions:

- `src/conn/conn_test.rs`
  `test_server_retransmits_lost_final_flight_after_completion`: a conn wrapper drops
  the server's first datagram carrying ChangeCipherSpec, which is flight 6. The test
  then checks that the client completes within 10 retransmit intervals (100 ms each)
  and that application data flows both ways afterwards. Before the patch the client
  did not complete within the 2 s test bound.
- `src/fragment_buffer/fragment_buffer_test.rs`
  `test_fragment_buffer_skips_retransmitted_message`.

Not changed: the unused `finish()` state in `handshaker.rs`, and session-resumption
flows. webrtc-dtls has no abbreviated handshake, so the server always sends the
final flight.

## Testing and removal

The crate is excluded from the root workspace. Run
`cargo test --manifest-path third_party/webrtc-dtls/Cargo.toml -p webrtc-dtls --locked`
(`just test` and the quality workflow run it). Drop this directory and the
crates.io patch once a webrtc-dtls release selected by MistLib's webrtc version
answers retransmissions of the peer's final flight after completion. Keep an
equivalent lost-flight-6 regression when validating that upgrade.
