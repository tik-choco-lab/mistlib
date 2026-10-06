use mistlib_core::signaling::nostr::DiscoveryExchanges;
use web_time::{Duration, Instant};

#[test]
fn queued_live_provider_keeps_a_retry_opportunity() {
    let now = Instant::now();
    let mut state = DiscoveryExchanges::default();
    for i in 0..12 {
        let key = format!("dead-{i}");
        assert!(state.request(&key, now));
        state.sent(&key, now);
    }
    assert!(state.request("live", now));
    state.queued("live");
    state.observe_advertisement("live", 1000, Some(900_000));
    state.observe_activity("live", 1001);
    let dead_retries = state.poll(now + Duration::from_secs(2));
    assert!(!dead_retries.contains(&"live".to_owned()));
    state.sent("live", now + Duration::from_secs(3));
    assert!(state.pending("live"));
    assert!(
        state.request("live", now + Duration::from_secs(5)),
        "dead identities spent all {} reservations while the best live candidate was still queued",
        dead_retries.len()
    );
}

#[test]
fn current_activity_beats_old_refresh_evidence() {
    let now = Instant::now();
    let mut state = DiscoveryExchanges::default();
    for i in 0..12 {
        let key = format!("dead-{i}");
        assert!(state.request(&key, now));
        state.observe_advertisement(&key, 800, Some(700_000));
    }
    assert!(state.request("live", now));
    state.observe_advertisement("live", 1000, Some(1_000_000));
    state.observe_activity("live", 1001);
    let due = state.poll(now + Duration::from_secs(2));
    assert!(
        due.contains(&"live".to_owned()),
        "old refreshed identities outranked current signed provider activity: {due:?}"
    );
}

#[test]
fn higher_ranked_peers_keep_allowance_while_queued_or_not_due() {
    let now = Instant::now();
    let mut state = DiscoveryExchanges::default();
    for i in 0..12 {
        let key = format!("dead-{i}");
        assert!(state.request(&key, now));
        state.sent(&key, now);
    }
    for key in ["queued", "not-due"] {
        assert!(state.request(key, now));
        state.observe_activity(key, 1000);
    }
    state.queued("queued");
    state.sent("not-due", now + Duration::from_secs(2));
    let due = state.poll(now + Duration::from_secs(2));
    assert_eq!(
        due.len(),
        10,
        "two higher-ranked repair chances are protected"
    );
    // Repeated maintenance must not spend the protected allowance either.
    assert!(state.poll(now + Duration::from_secs(3)).is_empty());
    state.sent("queued", now + Duration::from_secs(3));
    assert_eq!(
        state.poll(now + Duration::from_secs(5)),
        vec!["not-due", "queued"]
    );
    assert!(state.poll(now + Duration::from_secs(100)).is_empty());
}

#[test]
fn verified_activity_without_advertisement_ages_and_old_hints_cannot_replace_it() {
    let now = Instant::now();
    let mut state = DiscoveryExchanges::default();
    for key in ["inbound", "advertisement", "mailbox"] {
        assert!(state.request(key, now));
    }
    state.observe_activity("inbound", 1001);
    state.observe_advertisement("advertisement", 1002, None);
    state.observe_advertisement("mailbox", 800, Some(700_000));
    state.observe_activity("mailbox", 1003);
    state.observe_evidence("mailbox", (true, 900));
    state.observe_activity("mailbox", 700);
    assert_eq!(
        state.poll(now + Duration::from_secs(2)),
        vec!["mailbox", "advertisement", "inbound"]
    );
}
