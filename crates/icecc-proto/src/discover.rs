//! Locating a scheduler, matching `DiscoverSched` in `icecream/services/comm.cpp`.
//!
//! Two modes, exactly as upstream:
//!
//! * **Explicit** — a host (and optionally a port) was named. Connect straight
//!   to it and ignore the netname entirely ("take whatever the machine is
//!   giving us", `comm.cpp:1573`).
//! * **Broadcast** — send a one-byte UDP probe to every interface's broadcast
//!   address, collect answers for a timeout, then pick the scheduler with the
//!   highest protocol version, breaking ties toward the longest-running one.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use tokio::net::UdpSocket;

use crate::wire::{DEFAULT_NETNAME, DEFAULT_SCHEDULER_PORT, PROTOCOL_VERSION};

const BROAD_BUFLEN: usize = 268;
const BROAD_BUFLEN_OLD_2: usize = 32;
const BROAD_BUFLEN_OLD_1: usize = 16;

/// Where to connect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerTarget {
    pub host: String,
    pub port: u16,
}

impl std::fmt::Display for SchedulerTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

/// How the scheduler should be found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discovery {
    Explicit(SchedulerTarget),
    Broadcast { netname: String, port: u16 },
}

/// Resolve CLI arguments and environment into a discovery plan.
///
/// Precedence follows upstream: an explicit argument wins, then
/// `$ICECC_SCHEDULER`, then `$USE_SCHEDULER`. A value of the form `host:port`
/// sets both; a bare `:port` (empty host) means "broadcast, but on this port",
/// which is how the icecream test suite drives a non-default scheduler.
pub fn resolve(scheduler: Option<&str>, netname: Option<&str>) -> Discovery {
    let from_env = || {
        std::env::var("ICECC_SCHEDULER")
            .or_else(|_| std::env::var("USE_SCHEDULER"))
            .ok()
    };
    let spec = scheduler.map(str::to_owned).or_else(from_env);

    let mut host = String::new();
    let mut port = 0u16;
    if let Some(spec) = spec.as_deref() {
        // rfind, so an IPv6 literal without a port is not split apart.
        match spec.rsplit_once(':') {
            Some((h, p)) => {
                host = h.to_owned();
                port = p.trim().parse().unwrap_or(0);
            }
            None => host = spec.to_owned(),
        }
    }
    if port == 0 {
        port = DEFAULT_SCHEDULER_PORT;
    }

    if host.is_empty() {
        let netname = netname
            .map(str::to_owned)
            .or_else(|| std::env::var("ICECC_NETNAME").ok())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| DEFAULT_NETNAME.to_owned());
        Discovery::Broadcast { netname, port }
    } else {
        Discovery::Explicit(SchedulerTarget { host, port })
    }
}

/// One scheduler that answered a discovery probe.
#[derive(Debug, Clone)]
pub struct Answer {
    pub target: SchedulerTarget,
    pub protocol: u32,
    pub start_time: u64,
    pub netname: String,
}

/// Broadcast for schedulers on `netname` and return the best answer.
///
/// Unlike upstream, loopback is probed too: a daemon has no reason to attach to
/// a scheduler only it can reach, but a monitor being run next to a scheduler
/// on the same box does.
pub async fn broadcast(
    netname: &str,
    port: u16,
    timeout: Duration,
) -> std::io::Result<Option<Answer>> {
    let socket = open_broadcast_socket()?;
    let probe = [PROTOCOL_VERSION as u8];

    let mut sent = 0usize;
    for addr in broadcast_addresses() {
        match socket
            .send_to(&probe, SocketAddr::V4(SocketAddrV4::new(addr, port)))
            .await
        {
            Ok(_) => sent += 1,
            Err(e) => tracing::debug!("broadcast to {addr}:{port} failed: {e}"),
        }
    }
    if sent == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "no usable broadcast interface",
        ));
    }

    let mut best: Option<Answer> = None;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = [0u8; BROAD_BUFLEN];

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let recv = tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await;
        let Ok(recv) = recv else { break };
        let (len, from) = recv?;

        let SocketAddr::V4(from) = from else { continue };
        let Some(answer) = parse_answer(&buf[..len], from) else {
            continue;
        };
        if !answer.netname.eq_ignore_ascii_case(netname) {
            tracing::debug!(
                "ignoring scheduler at {} because of netname {:?}",
                answer.target,
                answer.netname
            );
            continue;
        }
        tracing::info!(
            "scheduler at {} (protocol {})",
            answer.target,
            answer.protocol
        );
        // Highest protocol wins; among equals, the one running longest.
        let better = match &best {
            None => true,
            Some(b) => {
                answer.protocol > b.protocol
                    || (answer.protocol == b.protocol && answer.start_time < b.start_time)
            }
        };
        if better {
            best = Some(answer);
        }
    }

    Ok(best)
}

fn open_broadcast_socket() -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_broadcast(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)).into())?;
    UdpSocket::from_std(socket.into())
}

/// Per-interface broadcast addresses, plus the loopback broadcast.
fn broadcast_addresses() -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    match if_addrs::get_if_addrs() {
        Ok(ifaces) => {
            for iface in ifaces {
                if let if_addrs::IfAddr::V4(v4) = iface.addr {
                    if let Some(b) = v4.broadcast {
                        out.push(b);
                    }
                }
            }
        }
        Err(e) => tracing::warn!("cannot enumerate interfaces: {e}"),
    }
    // Loopback has no broadcast address in getifaddrs; add it explicitly so a
    // scheduler on this host is discoverable.
    out.push(Ipv4Addr::new(127, 255, 255, 255));
    out.sort();
    out.dedup();
    out
}

/// Decode a discovery reply. `prepareBroadcastReply` stamps `buf[0]` with our
/// probe version plus 1, 2 or 3 depending on how old the scheduler is, and the
/// layout of the rest follows from that mark (`get_broad_data`).
fn parse_answer(buf: &[u8], from: SocketAddrV4) -> Option<Answer> {
    let sent = PROTOCOL_VERSION as u8;
    let mark = *buf.first()?;

    let valid_len = matches!(
        buf.len(),
        BROAD_BUFLEN | BROAD_BUFLEN_OLD_2 | BROAD_BUFLEN_OLD_1
    );
    if !valid_len {
        return None;
    }

    let (protocol, start_time, name_off) = if mark == sent.wrapping_add(1) {
        // Protocol <= 32: no version or start time in the reply.
        (32u32, 0u64, 1usize)
    } else if mark == sent.wrapping_add(2) {
        // Protocol 33..37: native-endian words, an upstream bug kept for compat.
        let v = u32::from_ne_bytes(buf.get(1..5)?.try_into().ok()?);
        let t = u64::from_ne_bytes(buf.get(5..13)?.try_into().ok()?);
        (v, t, 13usize)
    } else if mark == sent.wrapping_add(3) {
        // Protocol >= 38: big-endian, with the start time split into two words.
        let v = u32::from_be_bytes(buf.get(1..5)?.try_into().ok()?);
        let hi = u32::from_be_bytes(buf.get(5..9)?.try_into().ok()?) as u64;
        let lo = u32::from_be_bytes(buf.get(9..13)?.try_into().ok()?) as u64;
        (v, (hi << 32) | lo, 13usize)
    } else {
        tracing::debug!("wrong discovery answer mark {mark} (size {})", buf.len());
        return None;
    };

    // Upstream rejects these outright rather than trusting a bogus reply.
    if protocol == 0 || protocol >= 128 {
        tracing::warn!("ignoring bogus protocol version {protocol} from {from}");
        return None;
    }

    let raw = buf.get(name_off..)?;
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    let netname = String::from_utf8_lossy(&raw[..end]).into_owned();

    Some(Answer {
        target: SchedulerTarget {
            host: from.ip().to_string(),
            // The reply comes from the scheduler's broadcast socket, which is
            // bound to its TCP port.
            port: from.port(),
        },
        protocol,
        start_time,
        netname,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn clear_env() {
        std::env::remove_var("ICECC_SCHEDULER");
        std::env::remove_var("USE_SCHEDULER");
        std::env::remove_var("ICECC_NETNAME");
    }

    #[test]
    fn explicit_host_and_port() {
        let _g = env_lock();
        clear_env();
        assert_eq!(
            resolve(Some("build-master:8765"), None),
            Discovery::Explicit(SchedulerTarget {
                host: "build-master".into(),
                port: 8765
            })
        );
    }

    #[test]
    fn explicit_host_defaults_the_port() {
        let _g = env_lock();
        clear_env();
        assert_eq!(
            resolve(Some("build-master"), None),
            Discovery::Explicit(SchedulerTarget {
                host: "build-master".into(),
                port: DEFAULT_SCHEDULER_PORT
            })
        );
    }

    #[test]
    fn no_argument_broadcasts_on_the_default_netname() {
        let _g = env_lock();
        clear_env();
        assert_eq!(
            resolve(None, None),
            Discovery::Broadcast {
                netname: "ICECREAM".into(),
                port: DEFAULT_SCHEDULER_PORT
            }
        );
    }

    #[test]
    fn bare_port_broadcasts_on_that_port() {
        let _g = env_lock();
        clear_env();
        assert_eq!(
            resolve(Some(":18765"), Some("LAB")),
            Discovery::Broadcast {
                netname: "LAB".into(),
                port: 18765
            }
        );
    }

    #[test]
    fn env_is_used_when_no_argument_is_given() {
        let _g = env_lock();
        clear_env();
        std::env::set_var("ICECC_SCHEDULER", "sched-a:9000");
        assert_eq!(
            resolve(None, None),
            Discovery::Explicit(SchedulerTarget {
                host: "sched-a".into(),
                port: 9000
            })
        );
        // The argument must win over the environment.
        assert_eq!(
            resolve(Some("sched-b"), None),
            Discovery::Explicit(SchedulerTarget {
                host: "sched-b".into(),
                port: DEFAULT_SCHEDULER_PORT
            })
        );
        clear_env();
    }

    #[test]
    fn use_scheduler_is_the_fallback_env_var() {
        let _g = env_lock();
        clear_env();
        std::env::set_var("USE_SCHEDULER", "legacy:1234");
        assert_eq!(
            resolve(None, None),
            Discovery::Explicit(SchedulerTarget {
                host: "legacy".into(),
                port: 1234
            })
        );
        clear_env();
    }

    #[test]
    fn icecc_netname_env_is_honoured_for_broadcast() {
        let _g = env_lock();
        clear_env();
        std::env::set_var("ICECC_NETNAME", "FARM");
        assert_eq!(
            resolve(None, None),
            Discovery::Broadcast {
                netname: "FARM".into(),
                port: DEFAULT_SCHEDULER_PORT
            }
        );
        clear_env();
    }

    /// Build a reply the way `prepareBroadcastReply` does for protocol >= 38.
    fn modern_reply(netname: &str, protocol: u32, start: u64) -> Vec<u8> {
        let mut buf = vec![0u8; BROAD_BUFLEN];
        buf[0] = (PROTOCOL_VERSION as u8).wrapping_add(3);
        buf[1..5].copy_from_slice(&protocol.to_be_bytes());
        buf[5..9].copy_from_slice(&((start >> 32) as u32).to_be_bytes());
        buf[9..13].copy_from_slice(&(start as u32).to_be_bytes());
        buf[13..13 + netname.len()].copy_from_slice(netname.as_bytes());
        buf
    }

    #[test]
    fn parses_a_modern_reply() {
        let from = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 5), 8765);
        let a = parse_answer(&modern_reply("ICECREAM", 43, 0x1_0000_0002), from).unwrap();
        assert_eq!(a.protocol, 43);
        assert_eq!(a.start_time, 0x1_0000_0002);
        assert_eq!(a.netname, "ICECREAM");
        assert_eq!(a.target.to_string(), "10.0.0.5:8765");
    }

    #[test]
    fn parses_a_protocol_33_reply_with_native_endian_words() {
        let mut buf = vec![0u8; BROAD_BUFLEN_OLD_2];
        buf[0] = (PROTOCOL_VERSION as u8).wrapping_add(2);
        buf[1..5].copy_from_slice(&35u32.to_ne_bytes());
        buf[5..13].copy_from_slice(&1234u64.to_ne_bytes());
        buf[13..21].copy_from_slice(b"ICECREAM");
        let from = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8765);
        let a = parse_answer(&buf, from).unwrap();
        assert_eq!(a.protocol, 35);
        assert_eq!(a.start_time, 1234);
        assert_eq!(a.netname, "ICECREAM");
    }

    #[test]
    fn parses_an_ancient_reply_as_version_32() {
        let mut buf = vec![0u8; BROAD_BUFLEN_OLD_1];
        buf[0] = (PROTOCOL_VERSION as u8).wrapping_add(1);
        buf[1..9].copy_from_slice(b"ICECREAM");
        let from = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8765);
        let a = parse_answer(&buf, from).unwrap();
        assert_eq!(a.protocol, 32);
        assert_eq!(a.start_time, 0);
        assert_eq!(a.netname, "ICECREAM");
    }

    #[test]
    fn rejects_wrong_mark_wrong_size_and_bogus_version() {
        let from = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8765);

        let mut bad_mark = modern_reply("ICECREAM", 43, 0);
        bad_mark[0] = 7;
        assert!(parse_answer(&bad_mark, from).is_none());

        assert!(parse_answer(&[0u8; 5], from).is_none());

        let bogus = modern_reply("ICECREAM", 200, 0);
        assert!(parse_answer(&bogus, from).is_none());
    }
}
