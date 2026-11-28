#[cfg(test)]
mod util_test;

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use stun::agent::*;
use stun::attributes::*;
use stun::integrity::*;
use stun::message::*;
use stun::textattrs::*;
use stun::xoraddr::*;
use tokio::time::Duration;
use util::vnet::net::*;
use util::Conn;

use crate::agent::agent_config::{InterfaceFilterFn, IpFilterFn};
use crate::error::*;
use crate::network_type::*;

pub fn create_addr(_network: NetworkType, ip: IpAddr, port: u16) -> SocketAddr {
    /*if network.is_tcp(){
        return &net.TCPAddr{IP: ip, Port: port}
    default:
        return &net.UDPAddr{IP: ip, Port: port}
    }*/
    SocketAddr::new(ip, port)
}

pub fn assert_inbound_username(m: &Message, expected_username: &str) -> Result<()> {
    let mut username = Username::new(ATTR_USERNAME, String::new());
    username.get_from(m)?;

    if username.to_string() != expected_username {
        return Err(Error::Other(format!(
            "{:?} expected({}) actual({})",
            Error::ErrMismatchUsername,
            expected_username,
            username,
        )));
    }

    Ok(())
}

pub fn assert_inbound_message_integrity(m: &mut Message, key: &[u8]) -> Result<()> {
    let message_integrity_attr = MessageIntegrity(key.to_vec());
    Ok(message_integrity_attr.check(m)?)
}

/// Initiates a stun requests to `server_addr` using conn, reads the response and returns the
/// `XORMappedAddress` returned by the stun server.
/// Adapted from stun v0.2.
pub async fn get_xormapped_addr(
    conn: &Arc<dyn Conn + Send + Sync>,
    server_addr: SocketAddr,
    deadline: Duration,
) -> Result<XorMappedAddress> {
    let resp = stun_request(conn, server_addr, deadline).await?;
    let mut addr = XorMappedAddress::default();
    addr.get_from(&resp)?;
    Ok(addr)
}

const MAX_MESSAGE_SIZE: usize = 1280;

/// RFC 8489 section 6.2.1: initial retransmission timeout for STUN over UDP.
/// MistLib patch: each retransmission doubles it until `deadline` expires.
const STUN_INITIAL_RTO: Duration = Duration::from_millis(500);

/// Sends a Binding request and waits for the response with the same
/// transaction ID. With a positive `deadline`, the identical request is
/// retransmitted at 0, RTO, 3*RTO, 7*RTO, ... (RFC 8489 section 6.2.1) until
/// the deadline, so one lost request or response no longer loses the
/// server-reflexive candidate. Packets that are not a STUN message for this
/// transaction are ignored. A zero `deadline` keeps upstream behavior: one
/// request and an unbounded wait for the next packet.
pub async fn stun_request(
    conn: &Arc<dyn Conn + Send + Sync>,
    server_addr: SocketAddr,
    deadline: Duration,
) -> Result<Message> {
    let mut request = Message::new();
    request.build(&[Box::new(BINDING_REQUEST), Box::new(TransactionId::new())])?;
    let mut bs = vec![0_u8; MAX_MESSAGE_SIZE];

    if deadline == Duration::from_secs(0) {
        conn.send_to(&request.raw, server_addr).await?;
        let (n, _) = conn.recv_from(&mut bs).await?;
        let mut res = Message::new();
        res.raw = bs[..n].to_vec();
        res.decode()?;
        return Ok(res);
    }

    let give_up = tokio::time::Instant::now() + deadline;
    let mut next_send = tokio::time::Instant::now();
    let mut rto = STUN_INITIAL_RTO;
    loop {
        let now = tokio::time::Instant::now();
        if now >= give_up {
            return Err(Error::Other(format!(
                "STUN binding request to {server_addr} timed out after {deadline:?}"
            )));
        }
        if now >= next_send {
            conn.send_to(&request.raw, server_addr).await?;
            next_send = now + rto;
            rto = rto.saturating_mul(2);
        }
        match tokio::time::timeout_at(next_send.min(give_up), conn.recv_from(&mut bs)).await {
            // Retransmission or overall deadline reached; the loop decides.
            Err(_) => continue,
            Ok(Err(err)) => return Err(Error::Other(err.to_string())),
            Ok(Ok((n, _))) => {
                let mut res = Message::new();
                res.raw = bs[..n].to_vec();
                if res.decode().is_ok() && res.transaction_id == request.transaction_id {
                    return Ok(res);
                }
            }
        }
    }
}

pub async fn local_interfaces(
    vnet: &Arc<Net>,
    interface_filter: &Option<InterfaceFilterFn>,
    ip_filter: &Option<IpFilterFn>,
    network_types: &[NetworkType],
    include_loopback: bool,
) -> HashSet<IpAddr> {
    let mut ips = HashSet::new();
    let interfaces = vnet.get_interfaces().await;

    let (mut ipv4requested, mut ipv6requested) = (false, false);
    for typ in network_types {
        if typ.is_ipv4() {
            ipv4requested = true;
        }
        if typ.is_ipv6() {
            ipv6requested = true;
        }
    }

    for iface in interfaces {
        if let Some(filter) = interface_filter {
            if !filter(iface.name()) {
                continue;
            }
        }

        for ipnet in iface.addrs() {
            let ipaddr = ipnet.addr();

            if (!ipaddr.is_loopback() || include_loopback)
                && ((ipv4requested && ipaddr.is_ipv4()) || (ipv6requested && ipaddr.is_ipv6()))
                && ip_filter
                    .as_ref()
                    .map(|filter| filter(ipaddr))
                    .unwrap_or(true)
            {
                ips.insert(ipaddr);
            }
        }
    }

    ips
}

pub async fn listen_udp_in_port_range(
    vnet: &Arc<Net>,
    port_max: u16,
    port_min: u16,
    laddr: SocketAddr,
) -> Result<Arc<dyn Conn + Send + Sync>> {
    if laddr.port() != 0 || (port_min == 0 && port_max == 0) {
        return Ok(vnet.bind(laddr).await?);
    }
    let i = if port_min == 0 { 1 } else { port_min };
    let j = if port_max == 0 { 0xFFFF } else { port_max };
    if i > j {
        return Err(Error::ErrPort);
    }

    let port_start = rand::random::<u16>() % (j - i + 1) + i;
    let mut port_current = port_start;
    loop {
        let laddr = SocketAddr::new(laddr.ip(), port_current);
        match vnet.bind(laddr).await {
            Ok(c) => return Ok(c),
            Err(err) => log::debug!("failed to listen {}: {}", laddr, err),
        };

        port_current += 1;
        if port_current > j {
            port_current = i;
        }
        if port_current == port_start {
            break;
        }
    }

    Err(Error::ErrPort)
}
