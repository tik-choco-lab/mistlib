//! End-to-end sanity checks for the `[ConnTiming]` bookkeeping maps
//! (`connect_started_at`/`disconnect_observed_at`) against a real (loopback)
//! WebRTC handshake -- the pure rate-limiter/formatting logic itself is
//! covered directly in `conn_timing`'s own unit tests.

use super::disconnect::{make_connected_pair, wait_for_state};
use mistlib_core::transport::Transport;
use mistlib_core::types::ConnectionState;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connect_started_at_is_consumed_once_established() {
    let (ta, _tb, _id_a, id_b) = make_connected_pair();

    ta.connect(&id_b).await.expect("connect should not fail");
    assert!(
        wait_for_state(&ta, &id_b, ConnectionState::Connected, 10_000).await,
        "A did not reach Connected state"
    );

    assert!(
        !ta.connect_started_at.read().unwrap().contains_key(&id_b),
        "connect_started_at must be consumed once the ReliableOrdered data channel opens"
    );
}

/// A full disconnect -> reconnect cycle between the same two peers: confirms
/// `disconnect_observed_at` is populated by a confirmed disconnect (on both
/// the side that initiated it and the side that merely observed the data
/// channel close) and is consumed again -- alongside a freshly-set
/// `connect_started_at` -- once the same peer re-establishes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disconnect_then_reconnect_populates_and_consumes_disconnect_observed_at() {
    let (ta, tb, id_a, id_b) = make_connected_pair();

    ta.connect(&id_b).await.expect("connect should not fail");
    assert!(
        wait_for_state(&ta, &id_b, ConnectionState::Connected, 10_000).await,
        "A did not reach Connected state"
    );
    assert!(
        wait_for_state(&tb, &id_a, ConnectionState::Connected, 10_000).await,
        "B did not reach Connected state"
    );

    ta.disconnect(&id_b)
        .await
        .expect("disconnect should not fail");
    assert!(
        wait_for_state(&tb, &id_a, ConnectionState::Disconnected, 2_000).await,
        "B did not detect A's disconnect"
    );

    assert!(
        ta.disconnect_observed_at
            .read()
            .unwrap()
            .contains_key(&id_b),
        "A (the side that initiated the disconnect) must record disconnect_observed_at for B"
    );
    assert!(
        tb.disconnect_observed_at
            .read()
            .unwrap()
            .contains_key(&id_a),
        "B (the side that merely observed the data channel close) must record \
         disconnect_observed_at for A"
    );

    // A re-initiates the connection to the same peer.
    ta.connect(&id_b).await.expect("reconnect should not fail");
    assert!(
        wait_for_state(&ta, &id_b, ConnectionState::Connected, 10_000).await,
        "A did not reach Connected state on reconnect"
    );
    assert!(
        wait_for_state(&tb, &id_a, ConnectionState::Connected, 10_000).await,
        "B did not reach Connected state on reconnect"
    );

    assert!(
        !ta.disconnect_observed_at
            .read()
            .unwrap()
            .contains_key(&id_b),
        "A's disconnect_observed_at entry must be consumed once A re-establishes with B"
    );
    assert!(
        !ta.connect_started_at.read().unwrap().contains_key(&id_b),
        "A's connect_started_at entry must be consumed once A re-establishes with B"
    );
    assert!(
        !tb.disconnect_observed_at
            .read()
            .unwrap()
            .contains_key(&id_a),
        "B's disconnect_observed_at entry must be consumed once B re-establishes with A"
    );
    assert!(
        !tb.connect_started_at.read().unwrap().contains_key(&id_a),
        "B's connect_started_at entry must be consumed once B re-establishes with A"
    );
}
