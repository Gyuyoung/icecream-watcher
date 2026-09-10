//! A single-endpoint HTTP/1.1 server.
//!
//! Hand-rolled on purpose: the agent has to be a small static binary deployed
//! on every build node, and the surface here is one `GET` returning a
//! pre-serialised string. HTTP is worth keeping (rather than a private binary
//! protocol) only because it makes a node debuggable with `curl`.
//!
//! The served body is prepared by the sampler and handed over through a
//! `watch` channel, so a request costs no sampling, no serialisation and no
//! lock contention — a hundred monitors cost the same as one.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use icecc_metrics::{HEALTH_PATH, METRICS_PATH};

/// Cap on a request head. Anything longer is not a request we serve.
const MAX_REQUEST_BYTES: usize = 8 * 1024;

/// A client that connects and then says nothing must not hold a task open.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// The body to serve, or `None` while the first two samples are still being
/// taken.
pub type Body = watch::Receiver<Option<Arc<String>>>;

pub async fn serve(listener: TcpListener, body: Body) -> io::Result<()> {
    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(v) => v,
            // Per-connection errors (a client vanishing between the SYN and
            // accept) must not take the agent down.
            Err(e) => {
                tracing::debug!("accept failed: {e}");
                continue;
            }
        };
        let body = body.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(socket, body).await {
                tracing::debug!("connection from {peer} ended: {e}");
            }
        });
    }
}

async fn handle(mut socket: TcpStream, body: Body) -> io::Result<()> {
    socket.set_nodelay(true)?;

    let head = match tokio::time::timeout(READ_TIMEOUT, read_head(&mut socket)).await {
        Ok(head) => head?,
        Err(_) => return respond(&mut socket, 408, "text/plain", b"request timeout").await,
    };

    let Some(request) = Request::parse(&head) else {
        return respond(&mut socket, 400, "text/plain", b"bad request").await;
    };

    if request.method != "GET" {
        return respond(&mut socket, 405, "text/plain", b"only GET is supported").await;
    }

    // Clone the body out before awaiting: a `watch` borrow guard is not Send,
    // so holding it across the write would make this task non-spawnable.
    let snapshot = body.borrow().clone();

    match request.path.as_str() {
        METRICS_PATH => match snapshot {
            Some(json) => respond(&mut socket, 200, "application/json", json.as_bytes()).await,
            // Honest 503 rather than a snapshot of zeroes: the agent needs two
            // samples before any rate is meaningful.
            None => {
                respond(
                    &mut socket,
                    503,
                    "text/plain",
                    b"warming up: no complete sample yet",
                )
                .await
            }
        },
        HEALTH_PATH => respond(&mut socket, 200, "text/plain", b"ok").await,
        _ => {
            respond(
                &mut socket,
                404,
                "text/plain",
                format!("try {METRICS_PATH}\n").as_bytes(),
            )
            .await
        }
    }
}

/// Read until the blank line that ends the request head.
async fn read_head(socket: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];
    loop {
        let n = socket.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > MAX_REQUEST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request head too large",
            ));
        }
    }
    Ok(buf)
}

#[derive(Debug, PartialEq, Eq)]
struct Request {
    method: String,
    path: String,
}

impl Request {
    /// Parse just the request line. Headers are irrelevant to us, and ignoring
    /// them is what keeps this small enough to be obviously correct.
    fn parse(raw: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(raw).ok()?;
        let line = text.lines().next()?;
        let mut parts = line.split_whitespace();
        let method = parts.next()?.to_owned();
        let target = parts.next()?;
        if method.is_empty() || target.is_empty() {
            return None;
        }
        // A query string is not used, but must not turn /metrics into a 404.
        let path = target.split('?').next().unwrap_or(target).to_owned();
        Some(Self { method, path })
    }
}

async fn respond(
    socket: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    socket.write_all(head.as_bytes()).await?;
    socket.write_all(body).await?;
    socket.flush().await?;
    // Clients read to EOF, so the shutdown is what ends their read.
    socket.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use icecc_metrics::client;

    #[test]
    fn parses_a_normal_request_line() {
        let r = Request::parse(b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!(r.method, "GET");
        assert_eq!(r.path, "/metrics");
    }

    #[test]
    fn a_query_string_does_not_change_the_path() {
        let r = Request::parse(b"GET /metrics?x=1 HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(r.path, "/metrics");
    }

    #[test]
    fn rejects_garbage_and_empty_requests() {
        assert!(Request::parse(b"").is_none());
        assert!(Request::parse(b"GET\r\n\r\n").is_none());
        assert!(Request::parse(&[0xff, 0xfe]).is_none());
    }

    /// Start a server on an ephemeral port with the given body.
    async fn start(body: Option<Arc<String>>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (_tx, rx) = watch::channel(body);
        // The sender is dropped, which is fine: `borrow` keeps working.
        tokio::spawn(async move {
            let _ = serve(listener, rx).await;
        });
        port
    }

    fn a_snapshot_json() -> String {
        let snap = icecc_metrics::Snapshot {
            schema: icecc_metrics::SCHEMA_VERSION,
            agent_version: "0.1.0".into(),
            hostname: "build01".into(),
            addresses: vec!["10.0.0.11".into()],
            uptime_secs: 10,
            sampled_unix_ms: 1,
            sample_interval_ms: 1000,
            cpu: icecc_metrics::Cpu {
                cores: 2,
                total_busy_pct: 75.0,
                per_core_busy_pct: vec![70.0, 80.0],
                freq_mhz: vec![],
            },
            mem: icecc_metrics::Mem {
                total_kib: 100,
                available_kib: 25,
                free_kib: 25,
                buffers_kib: 0,
                cached_kib: 0,
                swap_total_kib: 0,
                swap_free_kib: 0,
            },
            load: icecc_metrics::Load {
                one: 1.5,
                five: 1.0,
                fifteen: 0.5,
                runnable: 2,
                total_procs: 300,
            },
            thermal: icecc_metrics::Thermal {
                cpu_celsius: Some(87.0),
                cpu_source: Some("coretemp/Package id 0".into()),
                sensors: vec![],
            },
            net: icecc_metrics::Net {
                rx_bytes_per_sec: 10,
                tx_bytes_per_sec: 20,
                interfaces: vec![],
            },
        };
        serde_json::to_string(&snap).unwrap()
    }

    #[tokio::test]
    async fn serves_a_snapshot_the_real_client_can_read() {
        let port = start(Some(Arc::new(a_snapshot_json()))).await;
        let snap = client::fetch("127.0.0.1", port, Duration::from_secs(5))
            .await
            .expect("fetch");
        assert_eq!(snap.hostname, "build01");
        assert_eq!(snap.cpu.total_busy_pct, 75.0);
        assert_eq!(snap.thermal.cpu_celsius, Some(87.0));
    }

    #[tokio::test]
    async fn says_503_while_warming_up_instead_of_reporting_zeroes() {
        let port = start(None).await;
        let err = client::fetch("127.0.0.1", port, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(matches!(err, client::FetchError::Status(503)), "{err}");
    }

    /// Raw exchange helper, for the paths the typed client cannot express.
    async fn raw(port: u16, request: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(request.as_bytes()).await.unwrap();
        s.flush().await.unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        String::from_utf8_lossy(&out).into_owned()
    }

    #[tokio::test]
    async fn healthz_is_cheap_and_always_available() {
        let port = start(None).await;
        let response = raw(port, "GET /healthz HTTP/1.1\r\nConnection: close\r\n\r\n").await;
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.ends_with("ok"), "{response}");
    }

    #[tokio::test]
    async fn unknown_paths_and_methods_are_refused_clearly() {
        let port = start(Some(Arc::new(a_snapshot_json()))).await;

        let r = raw(port, "GET /nope HTTP/1.1\r\nConnection: close\r\n\r\n").await;
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");
        assert!(
            r.contains("/metrics"),
            "404 should point at the endpoint: {r}"
        );

        // Read-only by construction: there is no write path to reach.
        let r = raw(port, "POST /metrics HTTP/1.1\r\nConnection: close\r\n\r\n").await;
        assert!(r.starts_with("HTTP/1.1 405"), "{r}");

        let r = raw(port, "nonsense\r\n\r\n").await;
        assert!(r.starts_with("HTTP/1.1 400"), "{r}");
    }

    #[tokio::test]
    async fn one_stalled_client_does_not_block_another() {
        let port = start(Some(Arc::new(a_snapshot_json()))).await;

        // Connect and send nothing; this connection sits in read_head.
        let _stalled = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

        // A well-behaved client must still be served promptly.
        let started = std::time::Instant::now();
        let snap = client::fetch("127.0.0.1", port, Duration::from_secs(3))
            .await
            .expect("second client served");
        assert_eq!(snap.hostname, "build01");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn many_concurrent_requests_all_succeed() {
        let port = start(Some(Arc::new(a_snapshot_json()))).await;
        let mut tasks = Vec::new();
        for _ in 0..32 {
            tasks.push(tokio::spawn(async move {
                client::fetch("127.0.0.1", port, Duration::from_secs(5)).await
            }));
        }
        for t in tasks {
            assert!(t.await.unwrap().is_ok());
        }
    }
}
