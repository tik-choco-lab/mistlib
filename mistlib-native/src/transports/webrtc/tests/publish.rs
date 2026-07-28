//! Unit coverage for the native-side track PUBLISH API
//! (`transports::webrtc::publish`): the `published_tracks`/`published_senders`
//! bookkeeping, `attach_published_tracks_to_peer`'s idempotency, and the
//! new-peer hook wired into `connection::create_pc`. These don't need real
//! ICE connectivity -- `create_offer`/`set_local_description`/`add_track` are
//! all local SDP/state operations -- so, like the rest of this module's
//! tests, they run against `MockSignaler` without a live network. The
//! renegotiation-over-a-real-connection path (reaching `Stable` with a second
//! real peer) is covered by the `#[ignore]`d test in
//! `mistlib-native/tests/loopback_media.rs`, matching how
//! `add_track_and_renegotiate_rejects_while_first_offer_is_still_pending`
//! (`tests/basic.rs`) already documents that split.

use super::make_transport;
use mistlib_core::types::NodeId;
use std::sync::Arc;
use webrtc::api::media_engine::MIME_TYPE_H264;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

fn make_track(id: &str) -> Arc<TrackLocalStaticRTP> {
    Arc::new(TrackLocalStaticRTP::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            ..Default::default()
        },
        id.to_string(),
        "stream".to_string(),
    ))
}

#[tokio::test]
async fn publish_local_track_records_state_with_no_peers_connected() {
    let t = make_transport();
    let track = make_track("video");

    t.publish_local_track(track.clone())
        .await
        .expect("publishing with no peers should succeed trivially");

    assert!(t.published_tracks.read().unwrap().contains_key("video"));
}

#[tokio::test]
async fn unpublish_local_track_with_no_peers_clears_state() {
    let t = make_transport();
    let track = make_track("video");

    t.publish_local_track(track.clone()).await.unwrap();
    t.unpublish_local_track(track.clone())
        .await
        .expect("unpublishing with no peers should succeed trivially");

    assert!(!t.published_tracks.read().unwrap().contains_key("video"));
}

#[tokio::test]
async fn create_pc_attaches_already_published_tracks_to_a_new_peer() {
    // The new-peer hook (mirroring mistlib-wasm's `create_pc`): a track
    // published *before* a peer connection exists must still end up attached
    // to it automatically, with no separate app-level call needed.
    let t = make_transport();
    let track = make_track("video");
    t.publish_local_track(track).await.unwrap();

    let node = NodeId("late-joiner".to_string());
    t.create_pc(node.clone())
        .await
        .expect("create_pc should succeed");

    let senders = t.published_senders.read().await;
    let peer_senders = senders
        .get(&node)
        .expect("create_pc should have attached published tracks for this node");
    assert!(
        peer_senders.contains_key("video"),
        "published track should be attached to the new peer without any extra app call"
    );
}

#[tokio::test]
async fn create_pc_does_not_touch_published_senders_when_nothing_is_published() {
    let t = make_transport();
    let node = NodeId("peer".to_string());
    t.create_pc(node.clone())
        .await
        .expect("create_pc should succeed");

    assert!(
        !t.published_senders.read().await.contains_key(&node),
        "no bookkeeping entry should be created when there is nothing to attach"
    );
}

#[tokio::test]
async fn attach_published_tracks_to_peer_is_idempotent() {
    let t = make_transport();
    let track = make_track("video");
    {
        let mut lock = t.published_tracks.write().unwrap();
        lock.insert("video".to_string(), track);
    }

    let node = NodeId("peer".to_string());
    let peer = t
        .create_pc(node.clone())
        .await
        .expect("create_pc should succeed");

    // create_pc's own new-peer hook already attached it once; calling again
    // directly must be a no-op (already attached), not a duplicate sender.
    let changed = t
        .attach_published_tracks_to_peer(&node, &peer)
        .await
        .expect("attach should not fail");
    assert!(
        !changed,
        "re-attaching an already-attached track should report no change"
    );

    let senders = t.published_senders.read().await;
    assert_eq!(senders.get(&node).map(|m| m.len()), Some(1));
}

#[tokio::test]
async fn create_pc_clears_stale_published_senders_on_reconnect() {
    // `create_pc` always builds a brand-new `RTCPeerConnection`, so any
    // sender bookkeeping recorded against this NodeId from a *previous*
    // connection (which is being replaced) is stale and must not survive --
    // otherwise `attach_published_tracks_to_peer` would wrongly skip
    // re-attaching a still-published track to the new connection, or leave a
    // now-irrelevant track id lying around for one that was unpublished in
    // between.
    let t = make_transport();
    let node = NodeId("reconnecting-peer".to_string());

    t.publish_local_track(make_track("a")).await.unwrap();
    t.create_pc(node.clone())
        .await
        .expect("first create_pc should succeed");
    assert!(t
        .published_senders
        .read()
        .await
        .get(&node)
        .is_some_and(|m| m.contains_key("a")));

    // Simulate a reconnect where "a" is no longer published but "b" is.
    t.unpublish_local_track(make_track("a")).await.unwrap();
    t.publish_local_track(make_track("b")).await.unwrap();

    t.create_pc(node.clone())
        .await
        .expect("second create_pc (reconnect) should succeed");

    let senders = t.published_senders.read().await;
    let peer_senders = senders
        .get(&node)
        .expect("reconnect should re-populate senders");
    assert!(
        !peer_senders.contains_key("a"),
        "stale sender for an unpublished track must not survive a reconnect"
    );
    assert!(
        peer_senders.contains_key("b"),
        "currently-published track must be attached to the replacement connection"
    );
}

#[tokio::test]
async fn unpublish_local_track_removes_bookkeeping_for_attached_peers() {
    // The peer here was never `connect()`-ed (no ICE offer/answer round
    // trip), so the renegotiation `unpublish_local_track` attempts afterward
    // has no valid connection state to renegotiate against and is rejected
    // by `can_send_offer` -- expected without a real second peer (see the
    // module doc comment). What this test checks is that the
    // published-track bookkeeping is already cleared by the time that
    // happens, matching mistlib-wasm's `unpublish_local_track` (which also
    // mutates its maps before attempting renegotiation).
    let t = make_transport();
    let track = make_track("video");
    t.publish_local_track(track.clone()).await.unwrap();

    let node = NodeId("peer".to_string());
    t.create_pc(node.clone())
        .await
        .expect("create_pc should succeed");
    assert!(t
        .published_senders
        .read()
        .await
        .get(&node)
        .is_some_and(|m| m.contains_key("video")));

    let _ = t.unpublish_local_track(track).await;

    assert!(!t.published_tracks.read().unwrap().contains_key("video"));
    assert!(t
        .published_senders
        .read()
        .await
        .get(&node)
        .is_none_or(|m| !m.contains_key("video")));
}

#[tokio::test]
async fn has_published_tracks_reflects_current_publish_state() {
    let t = make_transport();
    assert!(!t.has_published_tracks());

    let track = make_track("video");
    t.publish_local_track(track.clone()).await.unwrap();
    assert!(t.has_published_tracks());

    t.unpublish_local_track(track).await.unwrap();
    assert!(!t.has_published_tracks());
}
