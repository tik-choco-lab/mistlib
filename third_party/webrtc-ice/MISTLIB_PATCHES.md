# MistLib ICE patches

Vendored from the crates.io source of **webrtc-ice 0.13.0**, used by webrtc-rs 0.13.
The normalized Cargo.toml (minus the upstream `ping_pong` example entry), the
upstream Cargo.lock and both licenses are retained. Registry bookkeeping,
Cargo.toml.orig, codecov.yml and `examples/` are omitted. The root workspace and
`tests/quicprobe` select this copy through `[patch.crates-io]`.

## STUN Binding retransmission for server-reflexive gathering (2026-11-26)

Evidence: SPEC-24 M4 (`docs/investigation/SPEC24_P3_M4_SETUP_2026-11-26.md`).
Upstream `util::stun_request` sent one Binding request and waited for one packet
until the 5 s `STUN_GATHER_TIMEOUT`. When the request or the response was lost,
the agent ended gathering after 5 s without a server-reflexive candidate. Between
two endpoint-independent-mapping NATs with address-and-port-dependent filtering,
that peer can then not be reached at all. On the NAT testbed this caused every
raw ICE setup failure and 16 of 19 mistlib connect-watchdog firings. Upstream
0.17.3 still has the single-request code.

- `stun_request` with a positive deadline retransmits the identical request
  (same transaction ID) at 0, 500 ms, 1.5 s, 3.5 s, ... (RFC 8489 §6.2.1, initial
  RTO 500 ms, doubling) until the deadline. The overall deadline is unchanged.
- Responses are accepted only for the request's transaction ID; other packets
  arriving on the gathering socket are ignored instead of failing the request.
- A zero deadline keeps upstream behavior (one request, unbounded wait).

Regressions in `src/util/util_test.rs`: a server that drops the first two
requests and sends a stray response is answered on the third transmission; with
no server, two requests are sent within a 1.2 s deadline and the call fails at it.

## Readdressed upstream test vectors

The repository's commit scan and the public-sync scan reject RFC 1918 literals in
any tracked or published file. Six upstream test files used them as virtual-network
and parsing test addresses: `agent/{agent_gather_test,agent_test,agent_vnet_test}.rs`,
`candidate/candidate_test.rs`, `external_ip_mapper/external_ip_mapper_test.rs` and
`network_type/network_type_test.rs`. They were rewritten mechanically: `10.a.b.c` →
`11.a.b.c`, `192.168.a.b` → `193.168.a.b`, `172.16-31.a.b` → `173.16-31.a.b`. No
library code depends on these addresses. All 99 upstream tests still pass. A
future re-vendor must apply the same rewrite.

## Testing and removal

The crate is excluded from the root workspace. Run
`cargo test --manifest-path third_party/webrtc-ice/Cargo.toml -p webrtc-ice --locked`
(`just test` and the quality workflow run it). Drop this directory and the
crates.io patch once a webrtc-ice release selected by MistLib's webrtc version
retransmits STUN Binding requests during gathering; keep an equivalent
regression when validating that upgrade.
