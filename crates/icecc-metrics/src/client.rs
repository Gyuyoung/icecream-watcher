//! Minimal HTTP client for fetching one agent snapshot.
//!
//! Hand-rolled rather than pulling in a full HTTP stack, because the surface is
//! a single `GET` with no redirects, no TLS, no keep-alive and no chunked
//! encoding: we send `Connection: close` and read to EOF, which removes any
//! need to parse `Content-Length` or handle framing at all.
//!
//! HTTP is used instead of a private binary protocol for one reason: an
//! operator can debug a node with `curl node:9765/metrics`.

use std::io;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::{Snapshot, METRICS_PATH};

/// Hard cap on a response. A snapshot for a 256-core host is a few tens of KiB;
/// this only exists so a misbehaving or hostile endpoint cannot exhaust memory.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Cap on the header block, so a response that never sends a blank line fails
/// instead of being read forever.
const MAX_HEADER_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub enum FetchError {
    /// Could not connect, or the connection died mid-response. Usually means
    /// no agent is installed on that node.
    Io(io::Error),
    /// The whole exchange exceeded the caller's timeout.
    Timeout(Duration),
    /// Reached something that is not an agent, or an agent that is unhappy.
    Status(u16),
    /// Response was not a snapshot we can read.
    Malformed(String),
    /// Agent speaks a schema this build does not understand.
    UnsupportedSchema(u32),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Timeout(d) => write!(f, "no response within {d:?}"),
            Self::Status(code) => write!(f, "agent returned HTTP {code}"),
            Self::Malformed(why) => write!(f, "malformed snapshot: {why}"),
            Self::UnsupportedSchema(v) => {
                write!(
                    f,
                    "agent speaks metrics schema {v}, this build reads {}",
                    crate::SCHEMA_VERSION
                )
            }
        }
    }
}

impl std::error::Error for FetchError {}

impl From<io::Error> for FetchError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Fetch one snapshot from `host:port`, giving up after `timeout`.
///
/// The timeout covers connect, request and response together: from the
/// monitor's point of view a node that is slow to accept and a node that is
/// slow to answer are the same problem, and both must be bounded well below the
/// poll interval so one bad node cannot delay the next round.
pub async fn fetch(host: &str, port: u16, timeout: Duration) -> Result<Snapshot, FetchError> {
    match tokio::time::timeout(timeout, fetch_inner(host, port)).await {
        Ok(result) => result,
        Err(_) => Err(FetchError::Timeout(timeout)),
    }
}

async fn fetch_inner(host: &str, port: u16) -> Result<Snapshot, FetchError> {
    let mut stream = TcpStream::connect((host, port)).await?;
    stream.set_nodelay(true)?;

    let request = format!(
        "GET {METRICS_PATH} HTTP/1.1\r\n\
         Host: {host}:{port}\r\n\
         User-Agent: icecream-watcher/{}\r\n\
         Accept: application/json\r\n\
         Connection: close\r\n\r\n",
        env!("CARGO_PKG_VERSION")
    );
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;

    let mut buf = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_RESPONSE_BYTES {
            return Err(FetchError::Malformed(format!(
                "response exceeded {MAX_RESPONSE_BYTES} bytes"
            )));
        }
    }

    parse_response(&buf)
}

/// Split an HTTP response and decode the body. Separate from the I/O so the
/// parsing rules are testable without a socket.
pub fn parse_response(raw: &[u8]) -> Result<Snapshot, FetchError> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .filter(|&at| at <= MAX_HEADER_BYTES)
        .ok_or_else(|| FetchError::Malformed("no header terminator".into()))?;

    let head = std::str::from_utf8(&raw[..split])
        .map_err(|_| FetchError::Malformed("non-UTF-8 headers".into()))?;
    let status_line = head
        .lines()
        .next()
        .ok_or_else(|| FetchError::Malformed("empty response".into()))?;

    // "HTTP/1.1 200 OK" — we only care about the code.
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| FetchError::Malformed(format!("bad status line {status_line:?}")))?;
    if code != 200 {
        return Err(FetchError::Status(code));
    }

    let body = &raw[split + 4..];
    let snapshot: Snapshot =
        serde_json::from_slice(body).map_err(|e| FetchError::Malformed(e.to_string()))?;

    if !snapshot.schema_supported() {
        return Err(FetchError::UnsupportedSchema(snapshot.schema));
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cpu, Interface, Load, Mem, Net, Sensor, Thermal, SCHEMA_VERSION};

    fn snapshot(schema: u32) -> Snapshot {
        Snapshot {
            schema,
            agent_version: "0.1.0".into(),
            hostname: "build01".into(),
            addresses: vec!["10.0.0.11".into()],
            uptime_secs: 1,
            sampled_unix_ms: 1,
            sample_interval_ms: 1000,
            cpu: Cpu {
                cores: 2,
                total_busy_pct: 50.0,
                per_core_busy_pct: vec![40.0, 60.0],
                freq_mhz: vec![2000, 2100],
            },
            mem: Mem {
                total_kib: 100,
                available_kib: 40,
                free_kib: 20,
                buffers_kib: 10,
                cached_kib: 10,
                swap_total_kib: 0,
                swap_free_kib: 0,
            },
            load: Load {
                one: 1.0,
                five: 2.0,
                fifteen: 3.0,
                runnable: 1,
                total_procs: 100,
            },
            thermal: Thermal {
                cpu_celsius: Some(70.0),
                cpu_source: Some("coretemp/Package id 0".into()),
                sensors: vec![Sensor {
                    label: "coretemp/Package id 0".into(),
                    celsius: 70.0,
                }],
            },
            net: Net {
                rx_bytes_per_sec: 1,
                tx_bytes_per_sec: 2,
                interfaces: vec![Interface {
                    name: "eth0".into(),
                    rx_bytes_per_sec: 1,
                    tx_bytes_per_sec: 2,
                }],
            },
        }
    }

    fn response(status: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    #[test]
    fn parses_a_good_response() {
        let body = serde_json::to_string(&snapshot(SCHEMA_VERSION)).unwrap();
        let got = parse_response(&response("200 OK", &body)).unwrap();
        assert_eq!(got.hostname, "build01");
        assert_eq!(got.cpu.per_core_busy_pct, vec![40.0, 60.0]);
    }

    #[test]
    fn non_200_is_reported_as_a_status_not_a_parse_failure() {
        let err = parse_response(&response("503 Service Unavailable", "warming up")).unwrap_err();
        assert!(matches!(err, FetchError::Status(503)), "{err}");
        let err = parse_response(&response("404 Not Found", "")).unwrap_err();
        assert!(matches!(err, FetchError::Status(404)), "{err}");
    }

    #[test]
    fn a_future_schema_is_refused_explicitly() {
        let body = serde_json::to_string(&snapshot(SCHEMA_VERSION + 7)).unwrap();
        let err = parse_response(&response("200 OK", &body)).unwrap_err();
        assert!(
            matches!(err, FetchError::UnsupportedSchema(v) if v == SCHEMA_VERSION + 7),
            "{err}"
        );
    }

    #[test]
    fn reaching_something_that_is_not_an_agent_is_malformed_not_a_panic() {
        // e.g. someone pointed the port at a web server, or at iceccd.
        assert!(matches!(
            parse_response(&response("200 OK", "<html>hello</html>")).unwrap_err(),
            FetchError::Malformed(_)
        ));
        assert!(matches!(
            parse_response(b"not http at all").unwrap_err(),
            FetchError::Malformed(_)
        ));
        assert!(matches!(
            parse_response(b"\r\n\r\n").unwrap_err(),
            FetchError::Malformed(_)
        ));
    }

    #[test]
    fn a_truncated_body_fails_cleanly() {
        let body = serde_json::to_string(&snapshot(SCHEMA_VERSION)).unwrap();
        let cut = &body[..body.len() / 2];
        assert!(matches!(
            parse_response(&response("200 OK", cut)).unwrap_err(),
            FetchError::Malformed(_)
        ));
    }

    #[tokio::test]
    async fn connecting_to_a_dead_port_is_an_io_error_quickly() {
        // Port 1 on loopback: refused immediately, which is what a node without
        // an agent looks like.
        let err = fetch("127.0.0.1", 1, Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(matches!(err, FetchError::Io(_)), "{err}");
    }

    #[tokio::test]
    async fn a_silent_endpoint_hits_the_timeout_rather_than_hanging() {
        // A listener that accepts and then says nothing is the "slow node"
        // case that must never stall the poll loop.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let started = std::time::Instant::now();
        let err = fetch("127.0.0.1", addr.port(), Duration::from_millis(300))
            .await
            .unwrap_err();
        assert!(matches!(err, FetchError::Timeout(_)), "{err}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
