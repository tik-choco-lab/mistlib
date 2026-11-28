use super::*;

#[tokio::test]
async fn test_local_interfaces() -> Result<()> {
    let vnet = Arc::new(Net::new(None));
    let interfaces = vnet.get_interfaces().await;
    let ips = local_interfaces(
        &vnet,
        &None,
        &None,
        &[NetworkType::Udp4, NetworkType::Udp6],
        false,
    )
    .await;

    let ips_with_loopback = local_interfaces(
        &vnet,
        &None,
        &None,
        &[NetworkType::Udp4, NetworkType::Udp6],
        true,
    )
    .await;
    assert!(ips_with_loopback.is_superset(&ips));
    assert!(!ips.iter().any(|ip| ip.is_loopback()));
    assert!(ips_with_loopback.iter().any(|ip| ip.is_loopback()));
    log::info!(
        "interfaces: {:?}, ips: {:?}, ips_with_loopback: {:?}",
        interfaces,
        ips,
        ips_with_loopback
    );
    Ok(())
}

/// MistLib patch: the srflx Binding request is retransmitted, so losing the
/// first requests (or their responses) no longer drops the candidate.
#[tokio::test]
async fn test_get_xormapped_addr_retransmits_lost_requests() -> Result<()> {
    let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
    let server_addr = server.local_addr()?;
    let client: Arc<dyn Conn + Send + Sync> =
        Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await?);
    let client_addr = client.local_addr()?;

    let responder = tokio::spawn(async move {
        let mut buf = vec![0u8; MAX_MESSAGE_SIZE];
        let mut transactions = Vec::new();
        loop {
            let (n, from) = server.recv_from(&mut buf).await.unwrap();
            let mut req = Message::new();
            req.raw = buf[..n].to_vec();
            req.decode().unwrap();
            transactions.push(req.transaction_id);
            if transactions.len() < 3 {
                continue; // drop the first two requests
            }
            // A stray non-matching response first: must be ignored.
            let mut stray = Message::new();
            stray
                .build(&[
                    Box::new(TransactionId::new()),
                    Box::new(BINDING_SUCCESS),
                    Box::new(XorMappedAddress {
                        ip: "203.0.113.9".parse().unwrap(),
                        port: 9,
                    }),
                ])
                .unwrap();
            server.send_to(&stray.raw, from).await.unwrap();
            let mut res = Message::new();
            res.build(&[
                Box::new(req.clone()),
                Box::new(BINDING_SUCCESS),
                Box::new(XorMappedAddress {
                    ip: from.ip(),
                    port: from.port(),
                }),
            ])
            .unwrap();
            server.send_to(&res.raw, from).await.unwrap();
            return transactions;
        }
    });

    let started = tokio::time::Instant::now();
    let addr = get_xormapped_addr(&client, server_addr, Duration::from_secs(5)).await?;
    let elapsed = started.elapsed();
    assert_eq!(addr.ip, client_addr.ip());
    assert_eq!(addr.port, client_addr.port());
    // Sent at 0, 0.5 and 1.5 s; well before the 5 s gathering deadline.
    assert!(elapsed >= Duration::from_millis(1400), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    let transactions = responder.await.unwrap();
    assert_eq!(transactions.len(), 3);
    assert!(transactions.iter().all(|t| *t == transactions[0]));
    Ok(())
}

#[tokio::test]
async fn test_get_xormapped_addr_times_out_at_deadline() -> Result<()> {
    let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
    let server_addr = server.local_addr()?;
    let client: Arc<dyn Conn + Send + Sync> =
        Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await?);
    let started = tokio::time::Instant::now();
    let result = get_xormapped_addr(&client, server_addr, Duration::from_millis(1200)).await;
    assert!(result.is_err());
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(1200), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(1700), "{elapsed:?}");
    // 0, 0.5 s: the next send (1.5 s) is past the deadline.
    let mut buf = vec![0u8; MAX_MESSAGE_SIZE];
    let mut count = 0;
    while let Ok(Ok(_)) =
        tokio::time::timeout(Duration::from_millis(50), server.recv_from(&mut buf)).await
    {
        count += 1;
    }
    assert_eq!(count, 2);
    Ok(())
}
