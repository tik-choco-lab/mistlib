# Offer transport-role failure regression

The reported trace rejects an inbound Offer with `Failed to set SSL role for
the transport`, then fails rollback too. The fallback retries that same offer
once on a fresh peer connection, without waiting for the negotiation watchdog
or rotating the room signaling identity. Ordinary SDP errors and successful
rollback retain the existing peer. A concurrent replacement is never removed.

Run host guards with `cargo test -p mistlib-wasm`.

Browser tests live in `src/transport/webrtc/offer_failure_tests.rs`. Build with:

```sh
cargo test -p mistlib-wasm --target wasm32-unknown-unknown --lib --no-run
```

Pass the emitted `.wasm` test executable to a matching
`wasm-bindgen-test-runner`, with filter `offer_failure_tests`. For interactive
execution set `NO_HEADLESS=1` and open the loopback URL printed by the runner.
Stop the runner after testing. The tests use local peer connections, no relay.

The five tests cover the specific double failure, successful rollback,
unrelated errors, normal crossed offers, and concurrent peer replacement.
The double-failure test injects browser promise rejections into the old PC;
the replacement uses real Chromium SDP operations and its answer is applied
by the sender. This verifies recovery logic, not the natural cause of the SSL
role error or end-to-end data delivery in the original multi-room scenario.

Manual follow-up: rebuild the playground WASM using the project's normal
workflow, repeat the original leave/rejoin scenario, and check for
`Rebuilding peer ... after offer transport-role failure and unsuccessful rollback`.
Correlate it with a new PC, Answer, and data-channel open on both endpoints.
Verify other peers/rooms and published tracks remain usable. A fresh-attempt
failure must remain bounded by the existing cleanup/watchdog policy.
