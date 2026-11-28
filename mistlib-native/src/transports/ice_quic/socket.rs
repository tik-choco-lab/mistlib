//! The nominated ICE connection as a quinn [`AsyncUdpSocket`].
//!
//! A packet-preserving adapter, not another UDP socket: ICE owns the
//! candidate sockets and NAT mappings. webrtc-ice handles STUN itself and
//! hands only non-STUN datagrams from validated remote candidates to
//! `Conn::recv`, keeping packet boundaries, so there is no second demux here.
//! quinn sees the nominated pair's addresses as fixed logical endpoints.

use std::future::Future;
use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::Bytes;
use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use util::Conn;

const QUEUE_PACKETS: usize = 64;
/// webrtc-ice 0.13 receive MTU.
const RECEIVE_BUFFER: usize = 8192;

#[derive(Debug)]
pub(super) struct IceSocket {
    local: SocketAddr,
    remote: SocketAddr,
    tx: mpsc::Sender<Bytes>,
    rx: Mutex<mpsc::Receiver<io::Result<Bytes>>>,
    /// Set once the first non-STUN datagram (QUIC) arrives from the peer.
    heard: Arc<AtomicBool>,
    tasks: [JoinHandle<()>; 2],
}

impl IceSocket {
    /// `drop_outgoing` discards every datagram instead of sending it (tests
    /// only: a path that blocks QUIC while ICE still works).
    pub(super) fn new(
        conn: Arc<dyn Conn + Send + Sync>,
        drop_outgoing: bool,
    ) -> io::Result<Arc<Self>> {
        let local = conn.local_addr().map_err(io::Error::other)?;
        let remote = conn
            .remote_addr()
            .ok_or_else(|| io::Error::other("ICE has no nominated remote address"))?;
        let (tx, mut outgoing) = mpsc::channel::<Bytes>(QUEUE_PACKETS);
        let (incoming, rx) = mpsc::channel(QUEUE_PACKETS);
        let send_conn = conn.clone();
        let send_task = tokio::spawn(async move {
            while let Some(data) = outgoing.recv().await {
                if drop_outgoing {
                    continue;
                }
                match send_conn.send(&data).await {
                    Ok(n) if n == data.len() => {}
                    Ok(n) => {
                        tracing::debug!(n, len = data.len(), "ICE+QUIC: short ICE send");
                    }
                    Err(error) => {
                        // Closing the queue surfaces the failure to quinn on
                        // its next try_send.
                        tracing::debug!(%error, "ICE+QUIC: ICE send failed");
                        break;
                    }
                }
            }
        });
        let heard = Arc::new(AtomicBool::new(false));
        let heard_by_recv = heard.clone();
        let recv_task = tokio::spawn(async move {
            let mut buf = vec![0; RECEIVE_BUFFER];
            loop {
                let packet = conn
                    .recv(&mut buf)
                    .await
                    .map(|n| Bytes::copy_from_slice(&buf[..n]))
                    .map_err(io::Error::other);
                let failed = packet.is_err();
                if !failed {
                    heard_by_recv.store(true, Ordering::Relaxed);
                }
                if incoming.send(packet).await.is_err() || failed {
                    break;
                }
            }
        });
        Ok(Arc::new(Self {
            local,
            remote,
            tx,
            rx: Mutex::new(rx),
            heard,
            tasks: [send_task, recv_task],
        }))
    }

    /// Whether any QUIC datagram has arrived from the peer yet.
    pub(super) fn heard_from_peer(&self) -> Arc<AtomicBool> {
        self.heard.clone()
    }
}

impl Drop for IceSocket {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

type Writable = Pin<Box<dyn Future<Output = io::Result<()>> + Send>>;

struct QueuePoller {
    tx: mpsc::Sender<Bytes>,
    waiting: Mutex<Option<Writable>>,
}

impl std::fmt::Debug for QueuePoller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("QueuePoller")
    }
}

impl UdpPoller for QueuePoller {
    fn poll_writable(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut waiting = self.waiting.lock().expect("queue poller mutex poisoned");
        let future = waiting.get_or_insert_with(|| {
            let tx = self.tx.clone();
            Box::pin(async move {
                // Readiness is advisory; try_send may still race another
                // poller and report WouldBlock.
                let permit = tx
                    .reserve_owned()
                    .await
                    .map_err(|_| io::Error::other("ICE send queue closed"))?;
                drop(permit);
                Ok(())
            })
        });
        let result = future.as_mut().poll(cx);
        if result.is_ready() {
            *waiting = None;
        }
        result
    }
}

impl AsyncUdpSocket for IceSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(QueuePoller {
            tx: self.tx.clone(),
            waiting: Mutex::new(None),
        })
    }

    fn try_send(&self, transmit: &Transmit<'_>) -> io::Result<()> {
        if transmit.destination != self.remote || transmit.segment_size.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported ICE destination or segmentation",
            ));
        }
        self.tx
            .try_send(Bytes::copy_from_slice(transmit.contents))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => io::ErrorKind::WouldBlock.into(),
                mpsc::error::TrySendError::Closed(_) => io::Error::other("ICE send queue closed"),
            })
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let mut rx = self.rx.lock().expect("ICE receive queue mutex poisoned");
        match rx.poll_recv(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(Err(io::Error::other("ICE receive queue closed"))),
            Poll::Ready(Some(Err(error))) => Poll::Ready(Err(error)),
            Poll::Ready(Some(Ok(packet))) => {
                if packet.len() > bufs[0].len() {
                    return Poll::Ready(Err(io::Error::other(
                        "ICE datagram exceeds receive buffer",
                    )));
                }
                bufs[0][..packet.len()].copy_from_slice(&packet);
                // The ICE Conn is connected and does not keep per-packet
                // source addresses; use the nominated pair, consistently
                // with the destination check in try_send.
                meta[0] = RecvMeta {
                    addr: self.remote,
                    len: packet.len(),
                    stride: packet.len(),
                    dst_ip: Some(self.local.ip()),
                    ..RecvMeta::default()
                };
                Poll::Ready(Ok(1))
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local)
    }

    // No ECN, no offload, no DF: quinn keeps PLPMTUD off on this socket.
    fn may_fragment(&self) -> bool {
        true
    }
}
