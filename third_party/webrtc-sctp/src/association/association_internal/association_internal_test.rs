use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use super::*;

type Result<T> = std::result::Result<T, util::Error>;

impl From<Error> for util::Error {
    fn from(e: Error) -> Self {
        util::Error::from_std(e)
    }
}

struct DumbConn;

#[async_trait]
impl Conn for DumbConn {
    async fn connect(&self, _addr: SocketAddr) -> Result<()> {
        Err(io::Error::new(io::ErrorKind::Other, "Not applicable").into())
    }

    async fn recv(&self, _b: &mut [u8]) -> Result<usize> {
        Ok(0)
    }

    async fn recv_from(&self, _buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        Err(io::Error::new(io::ErrorKind::Other, "Not applicable").into())
    }

    async fn send(&self, _b: &[u8]) -> Result<usize> {
        Ok(0)
    }

    async fn send_to(&self, _buf: &[u8], _target: SocketAddr) -> Result<usize> {
        Err(io::Error::new(io::ErrorKind::Other, "Not applicable").into())
    }

    fn local_addr(&self) -> Result<SocketAddr> {
        Err(io::Error::new(io::ErrorKind::AddrNotAvailable, "Addr Not Available").into())
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        None
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

fn create_association_internal(config: Config) -> AssociationInternal {
    let (close_loop_ch_tx, _close_loop_ch_rx) = broadcast::channel(1);
    let (accept_ch_tx, _accept_ch_rx) = mpsc::channel(1);
    let (handshake_completed_ch_tx, _handshake_completed_ch_rx) = mpsc::channel(1);
    let (awake_write_loop_ch_tx, _awake_write_loop_ch_rx) = mpsc::channel(1);
    AssociationInternal::new(
        config,
        close_loop_ch_tx,
        accept_ch_tx,
        handshake_completed_ch_tx,
        Arc::new(awake_write_loop_ch_tx),
    )
}

#[test]
fn test_create_forward_tsn_forward_one_abandoned() -> Result<()> {
    let mut a = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "client".to_owned(),
    });

    a.cumulative_tsn_ack_point = 9;
    a.advanced_peer_tsn_ack_point = 10;
    a.inflight_queue.push_no_check(ChunkPayloadData {
        beginning_fragment: true,
        ending_fragment: true,
        tsn: 10,
        stream_identifier: 1,
        stream_sequence_number: 2,
        user_data: Bytes::from_static(b"ABC"),
        nsent: 1,
        abandoned: Arc::new(AtomicBool::new(true)),
        ..Default::default()
    });

    let fwdtsn = a.create_forward_tsn();

    assert_eq!(fwdtsn.new_cumulative_tsn, 10, "should be able to serialize");
    assert_eq!(fwdtsn.streams.len(), 1, "there should be one stream");
    assert_eq!(fwdtsn.streams[0].identifier, 1, "si should be 1");
    assert_eq!(fwdtsn.streams[0].sequence, 2, "ssn should be 2");

    Ok(())
}

#[test]
fn test_create_forward_tsn_forward_two_abandoned_with_the_same_si() -> Result<()> {
    let mut a = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "client".to_owned(),
    });

    a.cumulative_tsn_ack_point = 9;
    a.advanced_peer_tsn_ack_point = 12;
    a.inflight_queue.push_no_check(ChunkPayloadData {
        beginning_fragment: true,
        ending_fragment: true,
        tsn: 10,
        stream_identifier: 1,
        stream_sequence_number: 2,
        user_data: Bytes::from_static(b"ABC"),
        nsent: 1,
        abandoned: Arc::new(AtomicBool::new(true)),
        ..Default::default()
    });
    a.inflight_queue.push_no_check(ChunkPayloadData {
        beginning_fragment: true,
        ending_fragment: true,
        tsn: 11,
        stream_identifier: 1,
        stream_sequence_number: 3,
        user_data: Bytes::from_static(b"DEF"),
        nsent: 1,
        abandoned: Arc::new(AtomicBool::new(true)),
        ..Default::default()
    });
    a.inflight_queue.push_no_check(ChunkPayloadData {
        beginning_fragment: true,
        ending_fragment: true,
        tsn: 12,
        stream_identifier: 2,
        stream_sequence_number: 1,
        user_data: Bytes::from_static(b"123"),
        nsent: 1,
        abandoned: Arc::new(AtomicBool::new(true)),
        ..Default::default()
    });

    let fwdtsn = a.create_forward_tsn();

    assert_eq!(fwdtsn.new_cumulative_tsn, 12, "should be able to serialize");
    assert_eq!(fwdtsn.streams.len(), 2, "there should be two stream");

    let mut si1ok = false;
    let mut si2ok = false;
    for s in &fwdtsn.streams {
        match s.identifier {
            1 => {
                assert_eq!(3, s.sequence, "ssn should be 3");
                si1ok = true;
            }
            2 => {
                assert_eq!(1, s.sequence, "ssn should be 1");
                si2ok = true;
            }
            _ => panic!("unexpected stream identifier"),
        }
    }
    assert!(si1ok, "si=1 should be present");
    assert!(si2ok, "si=2 should be present");

    Ok(())
}

#[tokio::test]
async fn test_handle_forward_tsn_forward_3unreceived_chunks() -> Result<()> {
    let mut a = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "client".to_owned(),
    });
    a.use_forward_tsn = true;

    let prev_tsn = a.peer_last_tsn;

    let fwdtsn = ChunkForwardTsn {
        new_cumulative_tsn: a.peer_last_tsn + 3,
        streams: vec![ChunkForwardTsnStream {
            identifier: 0,
            sequence: 0,
        }],
    };

    let p = a.handle_forward_tsn(&fwdtsn).await?;

    let delayed_ack_triggered = a.delayed_ack_triggered;
    let immediate_ack_triggered = a.immediate_ack_triggered;
    assert_eq!(
        a.peer_last_tsn,
        prev_tsn + 3,
        "peerLastTSN should advance by 3 "
    );
    assert!(delayed_ack_triggered, "delayed sack should be triggered");
    assert!(
        !immediate_ack_triggered,
        "immediate sack should NOT be triggered"
    );
    assert!(p.is_empty(), "should return empty");

    Ok(())
}

#[tokio::test]
async fn test_handle_forward_tsn_forward_1for1_missing() -> Result<()> {
    let mut a = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "client".to_owned(),
    });
    a.use_forward_tsn = true;

    let prev_tsn = a.peer_last_tsn;

    // this chunk is blocked by the missing chunk at tsn=1
    a.payload_queue.push(
        ChunkPayloadData {
            beginning_fragment: true,
            ending_fragment: true,
            tsn: a.peer_last_tsn + 2,
            stream_identifier: 0,
            stream_sequence_number: 1,
            user_data: Bytes::from_static(b"ABC"),
            ..Default::default()
        },
        a.peer_last_tsn,
    );

    let fwdtsn = ChunkForwardTsn {
        new_cumulative_tsn: a.peer_last_tsn + 1,
        streams: vec![ChunkForwardTsnStream {
            identifier: 0,
            sequence: 1,
        }],
    };

    let p = a.handle_forward_tsn(&fwdtsn).await?;

    let delayed_ack_triggered = a.delayed_ack_triggered;
    let immediate_ack_triggered = a.immediate_ack_triggered;
    assert_eq!(
        a.peer_last_tsn,
        prev_tsn + 2,
        "peerLastTSN should advance by 2"
    );
    assert!(delayed_ack_triggered, "delayed sack should be triggered");
    assert!(
        !immediate_ack_triggered,
        "immediate sack should NOT be triggered"
    );
    assert!(p.is_empty(), "should return empty");

    Ok(())
}

#[tokio::test]
async fn test_handle_forward_tsn_forward_1for2_missing() -> Result<()> {
    let mut a = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "client".to_owned(),
    });
    a.use_forward_tsn = true;

    let prev_tsn = a.peer_last_tsn;

    // this chunk is blocked by the missing chunk at tsn=1
    a.payload_queue.push(
        ChunkPayloadData {
            beginning_fragment: true,
            ending_fragment: true,
            tsn: a.peer_last_tsn + 3,
            stream_identifier: 0,
            stream_sequence_number: 1,
            user_data: Bytes::from_static(b"ABC"),
            ..Default::default()
        },
        a.peer_last_tsn,
    );

    let fwdtsn = ChunkForwardTsn {
        new_cumulative_tsn: a.peer_last_tsn + 1,
        streams: vec![ChunkForwardTsnStream {
            identifier: 0,
            sequence: 1,
        }],
    };

    let p = a.handle_forward_tsn(&fwdtsn).await?;

    let immediate_ack_triggered = a.immediate_ack_triggered;
    assert_eq!(
        a.peer_last_tsn,
        prev_tsn + 1,
        "peerLastTSN should advance by 1"
    );
    assert!(
        immediate_ack_triggered,
        "immediate sack should be triggered"
    );
    assert!(p.is_empty(), "should return empty");

    Ok(())
}

#[tokio::test]
async fn test_handle_forward_tsn_dup_forward_tsn_chunk_should_generate_sack() -> Result<()> {
    let mut a = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "client".to_owned(),
    });
    a.use_forward_tsn = true;

    let prev_tsn = a.peer_last_tsn;

    let fwdtsn = ChunkForwardTsn {
        new_cumulative_tsn: a.peer_last_tsn,
        streams: vec![ChunkForwardTsnStream {
            identifier: 0,
            sequence: 1,
        }],
    };

    let p = a.handle_forward_tsn(&fwdtsn).await?;

    assert_eq!(a.peer_last_tsn, prev_tsn, "peerLastTSN should not advance");
    assert_eq!(a.ack_state, AckState::Immediate, "sack should be requested");
    assert!(p.is_empty(), "should return empty");

    Ok(())
}

#[tokio::test]
async fn test_assoc_create_new_stream() -> Result<()> {
    let (close_loop_ch_tx, _close_loop_ch_rx) = broadcast::channel(1);
    let (accept_ch_tx, _accept_ch_rx) = mpsc::channel(ACCEPT_CH_SIZE);
    let (handshake_completed_ch_tx, _handshake_completed_ch_rx) = mpsc::channel(1);
    let (awake_write_loop_ch_tx, _awake_write_loop_ch_rx) = mpsc::channel(1);
    let mut a = AssociationInternal::new(
        Config {
            net_conn: Arc::new(DumbConn {}),
            max_receive_buffer_size: 0,
            max_message_size: 0,
            name: "client".to_owned(),
        },
        close_loop_ch_tx,
        accept_ch_tx,
        handshake_completed_ch_tx,
        Arc::new(awake_write_loop_ch_tx),
    );

    for i in 0..ACCEPT_CH_SIZE {
        let s = a.create_stream(i as u16, true);
        if let Some(s) = s {
            let result = a.streams.get(&s.stream_identifier);
            assert!(result.is_some(), "should be in a.streams map");
        } else {
            panic!("{i} should success");
        }
    }

    let new_si = ACCEPT_CH_SIZE as u16;
    let s = a.create_stream(new_si, true);
    assert!(s.is_none(), "should be none");
    let result = a.streams.get(&new_si);
    assert!(result.is_none(), "should NOT be in a.streams map");

    let to_be_ignored = ChunkPayloadData {
        beginning_fragment: true,
        ending_fragment: true,
        tsn: a.peer_last_tsn + 1,
        stream_identifier: new_si,
        user_data: Bytes::from_static(b"ABC"),
        ..Default::default()
    };

    let p = a.handle_data(&to_be_ignored).await?;
    assert!(p.is_empty(), "should return empty");

    Ok(())
}

async fn handle_init_test(name: &str, initial_state: AssociationState, expect_err: bool) {
    let mut a = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "client".to_owned(),
    });
    a.set_state(initial_state);
    let pkt = Packet {
        source_port: 5001,
        destination_port: 5002,
        ..Default::default()
    };
    let mut init = ChunkInit {
        initial_tsn: 1234,
        num_outbound_streams: 1001,
        num_inbound_streams: 1002,
        initiate_tag: 5678,
        advertised_receiver_window_credit: 512 * 1024,
        ..Default::default()
    };
    init.set_supported_extensions();

    let result = a.handle_init(&pkt, &init).await;
    if expect_err {
        assert!(result.is_err(), "{name} should fail");
        return;
    } else {
        assert!(result.is_ok(), "{name} should be ok");
    }
    assert_eq!(
        a.peer_last_tsn,
        if init.initial_tsn == 0 {
            u32::MAX
        } else {
            init.initial_tsn - 1
        },
        "{name} should match"
    );
    assert_eq!(a.my_max_num_outbound_streams, 1001, "{name} should match");
    assert_eq!(a.my_max_num_inbound_streams, 1002, "{name} should match");
    assert_eq!(a.peer_verification_tag, 5678, "{name} should match");
    assert_eq!(a.destination_port, pkt.source_port, "{name} should match");
    assert_eq!(a.source_port, pkt.destination_port, "{name} should match");
    assert!(a.use_forward_tsn, "{name} should be set to true");
}

#[tokio::test]
async fn test_assoc_handle_init() -> Result<()> {
    handle_init_test("normal", AssociationState::Closed, false).await;

    handle_init_test(
        "unexpected state established",
        AssociationState::Established,
        true,
    )
    .await;

    handle_init_test(
        "unexpected state shutdownAckSent",
        AssociationState::ShutdownAckSent,
        true,
    )
    .await;

    handle_init_test(
        "unexpected state shutdownPending",
        AssociationState::ShutdownPending,
        true,
    )
    .await;

    handle_init_test(
        "unexpected state shutdownReceived",
        AssociationState::ShutdownReceived,
        true,
    )
    .await;

    handle_init_test(
        "unexpected state shutdownSent",
        AssociationState::ShutdownSent,
        true,
    )
    .await;

    Ok(())
}

#[tokio::test]
async fn test_assoc_max_message_size_default() -> Result<()> {
    let mut a = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "client".to_owned(),
    });
    assert_eq!(
        a.max_message_size.load(Ordering::SeqCst),
        65536,
        "should match"
    );

    let stream = a.create_stream(1, false);
    assert!(stream.is_some(), "should succeed");

    if let Some(s) = stream {
        let p = Bytes::from(vec![0u8; 65537]);
        let ppi = PayloadProtocolIdentifier::from(s.default_payload_type.load(Ordering::SeqCst));

        if let Err(err) = s.write_sctp(&p.slice(..65536), ppi).await {
            assert_ne!(
                err,
                Error::ErrOutboundPacketTooLarge,
                "should be not Error::ErrOutboundPacketTooLarge"
            );
        } else {
            panic!("should be error");
        }

        if let Err(err) = s.write_sctp(&p.slice(..65537), ppi).await {
            assert_eq!(
                err,
                Error::ErrOutboundPacketTooLarge,
                "should be Error::ErrOutboundPacketTooLarge"
            );
        } else {
            panic!("should be error");
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_assoc_max_message_size_explicit() -> Result<()> {
    let mut a = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 30000,
        name: "client".to_owned(),
    });

    assert_eq!(
        a.max_message_size.load(Ordering::SeqCst),
        30000,
        "should match"
    );

    let stream = a.create_stream(1, false);
    assert!(stream.is_some(), "should succeed");

    if let Some(s) = stream {
        let p = Bytes::from(vec![0u8; 30001]);
        let ppi = PayloadProtocolIdentifier::from(s.default_payload_type.load(Ordering::SeqCst));

        if let Err(err) = s.write_sctp(&p.slice(..30000), ppi).await {
            assert_ne!(
                err,
                Error::ErrOutboundPacketTooLarge,
                "should be not Error::ErrOutboundPacketTooLarge"
            );
        } else {
            panic!("should be error");
        }

        if let Err(err) = s.write_sctp(&p.slice(..30001), ppi).await {
            assert_eq!(
                err,
                Error::ErrOutboundPacketTooLarge,
                "should be Error::ErrOutboundPacketTooLarge"
            );
        } else {
            panic!("should be error");
        }
    }

    Ok(())
}

// After an idle packet has used the delayed-ACK timer, the next idle packet
// must get its own ACK without needing another DATA packet or a sender RTO.
#[tokio::test(start_paused = true)]
async fn test_delayed_ack_rearms_after_expiry() -> Result<()> {
    delayed_ack_rearms_after_expiry(true).await
}

#[tokio::test(start_paused = true)]
async fn test_ordered_delayed_ack_rearms_after_expiry() -> Result<()> {
    delayed_ack_rearms_after_expiry(false).await
}

async fn delayed_ack_rearms_after_expiry(unordered: bool) -> Result<()> {
    let mut a = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "receiver".to_owned(),
    });
    let (wake_tx, mut wake_rx) = mpsc::channel(1);
    a.awake_write_loop_ch = Arc::new(wake_tx);
    a.peer_last_tsn = 100;
    a.set_state(AssociationState::Established);
    let stream = a.open_stream(1, PayloadProtocolIdentifier::Binary)?;
    let a = Arc::new(Mutex::new(a));
    a.lock().await.ack_timer = Some(AckTimer::new(Arc::downgrade(&a), ACK_INTERVAL));

    for tsn in 101..=103 {
        {
            let mut a = a.lock().await;
            a.handle_chunk_start();
            a.handle_data(&ChunkPayloadData {
                tsn,
                stream_identifier: 1,
                unordered,
                stream_sequence_number: (tsn - 101) as u16,
                beginning_fragment: true,
                ending_fragment: true,
                payload_type: PayloadProtocolIdentifier::Binary,
                user_data: Bytes::from_static(b"data"),
                ..Default::default()
            })
            .await?;
            a.handle_chunk_end();
            assert_eq!(a.ack_state, AckState::Delay);
        }
        // Auto-advance paused time to the delayed ACK, with no second DATA.
        tokio::time::timeout(
            ACK_INTERVAL + tokio::time::Duration::from_millis(10),
            wake_rx.recv(),
        )
        .await
        .unwrap_or_else(|_| panic!("isolated DATA TSN {tsn} must trigger its delayed ACK"))
        .expect("write loop must remain open");
        let mut a = a.lock().await;
        let packets = a.gather_outbound_sack_packets(vec![]).await;
        assert_eq!(packets.len(), 1);
        let sack = packets[0].chunks[0]
            .as_any()
            .downcast_ref::<ChunkSelectiveAck>()
            .unwrap();
        assert_eq!(sack.cumulative_tsn_ack, tsn);
        assert_eq!(a.stats.get_num_ack_timeouts(), u64::from(tsn - 100));
        assert_eq!(a.ack_state, AckState::Idle);
        let mut buffer = [0; 4];
        assert_eq!(stream.read_sctp(&mut buffer).await?.0, 4);
        assert_eq!(&buffer, b"data");
    }
    assert!(!a.lock().await.ack_timer.as_ref().unwrap().is_running());
    Ok(())
}

// --- MistLib research patch: CongestionControl::Bbr3 (see MISTLIB_PATCHES.md) ---

fn bbr_sender() -> AssociationInternal {
    let mut a = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "bbr".to_owned(),
    });
    assert!(a.bbr.is_none(), "Reno must remain the default controller");
    a.set_congestion_control(CongestionControl::Bbr3);
    a.set_state(AssociationState::Established);
    a.rwnd = 1024 * 1024;
    a
}

async fn queue_data(a: &AssociationInternal, n: u16, len: usize) {
    for ssn in 0..n {
        a.pending_queue
            .push(ChunkPayloadData {
                stream_identifier: 1,
                stream_sequence_number: ssn,
                beginning_fragment: true,
                ending_fragment: true,
                payload_type: PayloadProtocolIdentifier::Binary,
                user_data: Bytes::from(vec![0u8; len]),
                ..Default::default()
            })
            .await;
    }
}

fn data_tsns(packets: &[Packet]) -> Vec<u32> {
    packets
        .iter()
        .flat_map(|p| p.chunks.iter())
        .filter_map(|c| c.as_any().downcast_ref::<ChunkPayloadData>())
        .map(|d| d.tsn)
        .collect()
}

fn sack(cumulative_tsn_ack: u32, gaps: &[(u16, u16)]) -> ChunkSelectiveAck {
    ChunkSelectiveAck {
        cumulative_tsn_ack,
        advertised_receiver_window_credit: 1024 * 1024,
        gap_ack_blocks: gaps
            .iter()
            .map(|&(start, end)| crate::chunk::chunk_selective_ack::GapAckBlock { start, end })
            .collect(),
        duplicate_tsn: vec![],
    }
}

fn bbr(a: &AssociationInternal) -> &SctpBbr {
    a.bbr.as_ref().unwrap()
}

#[tokio::test(start_paused = true)]
async fn test_bbr_gap_blocks_miss_indications_and_fast_retransmit() -> Result<()> {
    let mut a = bbr_sender();
    queue_data(&a, 4, 1000).await;
    let (packets, _) = a.gather_outbound().await;
    let tsns = data_tsns(&packets);
    assert_eq!(tsns.len(), 4);
    let t0 = tsns[0];
    assert_eq!(bbr(&a).inflight(), 4000);

    tokio::time::advance(Duration::from_millis(50)).await;
    // t0 is missing; the gap block grows over three SACKs.
    a.handle_sack(&sack(t0 - 1, &[(2, 2)])).await?;
    assert_eq!((bbr(&a).inflight(), bbr(&a).bbr.delivered()), (3000, 1000));
    assert_eq!(bbr(&a).bbr.min_rtt(), Some(Duration::from_millis(50)));
    a.handle_sack(&sack(t0 - 1, &[(2, 3)])).await?;
    // t0+1 is reported again but counted once.
    assert_eq!((bbr(&a).inflight(), bbr(&a).bbr.delivered()), (2000, 2000));
    assert_eq!(bbr(&a).lost_pending(), 0);
    a.handle_sack(&sack(t0 - 1, &[(2, 4)])).await?;
    // Third miss indication: lost for BBR, Fast Recovery without a Reno cut.
    assert_eq!((bbr(&a).inflight(), bbr(&a).lost_pending()), (0, 1000));
    assert_eq!(bbr(&a).bbr.lost(), 1000);
    assert!(a.in_fast_recovery);
    assert_eq!(a.ssthresh, 0, "Reno state is not used in BBR mode");
    assert_eq!(u64::from(a.cwnd), bbr(&a).cwnd());

    let (packets, _) = a.gather_outbound().await;
    assert_eq!(data_tsns(&packets), vec![t0], "fast retransmission");
    assert_eq!(a.stats.get_num_fast_retrans(), 1);
    assert_eq!((bbr(&a).inflight(), bbr(&a).lost_pending()), (1000, 0));
    assert_eq!(bbr(&a).retransmissions, 1);

    // The retransmission's SACK delivers its bytes but is not an RTT sample:
    // 1 ms after the resend would otherwise become the minimum.
    tokio::time::advance(Duration::from_millis(1)).await;
    a.handle_sack(&sack(t0 + 3, &[])).await?;
    assert_eq!((bbr(&a).inflight(), bbr(&a).bbr.delivered()), (0, 4000));
    assert_eq!(bbr(&a).bbr.min_rtt(), Some(Duration::from_millis(50)));
    assert!(!a.in_fast_recovery);
    assert!(a.inflight_queue.is_empty());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn test_bbr_t3_rtx_uses_rto_recovery_instead_of_one_mtu_slow_start() -> Result<()> {
    let mut a = bbr_sender();
    queue_data(&a, 4, 1000).await;
    let (packets, _) = a.gather_outbound().await;
    let t0 = data_tsns(&packets)[0];
    tokio::time::advance(Duration::from_millis(50)).await;
    a.handle_sack(&sack(t0 - 1, &[(3, 3)])).await?; // only t0+2 arrives
    let before = bbr(&a).cwnd();

    a.on_retransmission_timeout(RtxTimerId::T3RTX, 1).await;
    assert_eq!(a.stats.get_num_t3timeouts(), 1);
    // t0, t0+1 and t0+3 are lost; cwnd = in-flight (0) + 1 MSS, model kept.
    assert_eq!((bbr(&a).inflight(), bbr(&a).lost_pending()), (0, 3000));
    assert_eq!(bbr(&a).cwnd(), u64::from(a.max_payload_size));
    assert_eq!(a.cwnd, a.max_payload_size);
    assert!(bbr(&a).bbr.in_recovery());
    assert_eq!(a.ssthresh, 0);

    let (packets, _) = a.gather_outbound().await;
    assert_eq!(data_tsns(&packets), vec![t0], "one MSS of retransmission");

    tokio::time::advance(Duration::from_millis(50)).await;
    a.handle_sack(&sack(t0, &[(2, 2)])).await?;
    // Only an ambiguous retransmission was acked, but the window still grows.
    assert!(bbr(&a).bbr.in_recovery());
    assert!(bbr(&a).cwnd() >= 4 * u64::from(a.max_payload_size));

    let (packets, _) = a.gather_outbound().await;
    assert_eq!(data_tsns(&packets), vec![t0 + 1, t0 + 3]);
    tokio::time::advance(Duration::from_millis(50)).await;
    a.handle_sack(&sack(t0 + 3, &[])).await?;
    assert!(!bbr(&a).bbr.in_recovery(), "recovery point reached");
    assert!(bbr(&a).cwnd() >= before, "saved cwnd restored");
    assert_eq!((bbr(&a).inflight(), bbr(&a).lost_pending()), (0, 0));
    assert_eq!(bbr(&a).bbr.delivered(), 4000);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn test_bbr_pacer_bounds_the_burst_and_wakes_the_write_loop() -> Result<()> {
    let mut a = bbr_sender();
    let (wake_tx, mut wake_rx) = mpsc::channel(1);
    a.awake_write_loop_ch = Arc::new(wake_tx);
    queue_data(&a, 8, 1200).await;
    let start = tokio::time::Instant::now();
    let (packets, _) = a.gather_outbound().await;
    // cwnd (12000 B) would admit 10 chunks; the bucket holds 2 quanta (4800 B).
    assert_eq!(data_tsns(&packets).len(), 4);
    assert!(bbr(&a).window_allows(1200));

    tokio::time::timeout(Duration::from_secs(1), wake_rx.recv())
        .await
        .expect("the pacer must wake the write loop")
        .expect("channel open");
    // One quantum (2400 B) at the initial ~98.8 kB/s Startup rate.
    let waited = start.elapsed();
    assert!(
        waited > Duration::from_millis(20) && waited < Duration::from_millis(30),
        "{waited:?}"
    );
    let (packets, _) = a.gather_outbound().await;
    assert_eq!(data_tsns(&packets).len(), 2);
    Ok(())
}

#[test]
fn test_reno_default_has_no_bbr_state() {
    let a = create_association_internal(Config {
        net_conn: Arc::new(DumbConn {}),
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "reno".to_owned(),
    });
    assert_eq!(default_congestion_control(), CongestionControl::Reno);
    assert!(a.bbr.is_none());
    assert!(a.pacer_wakeup.is_none());
}
