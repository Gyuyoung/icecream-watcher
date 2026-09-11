//! The monitor connection: discovery, handshake, `MON_LOGIN`, then a stream of
//! decoded events, with reconnection.
//!
//! Runs as its own task and reports through a channel, so no caller ever does
//! network I/O on a render path.
//!
//! **On timeouts:** `icecc-scheduler` stops polling its listening socket for a
//! second after each accept and then blocks in `poll()` for up to
//! `MAX_SCHEDULER_PING` (36 s). With no daemons attached there is nothing to
//! wake it, so the TCP connect completes from the kernel backlog while the
//! version handshake sits unanswered. Measured: 34 s on an idle scheduler,
//! 0.0 s once a daemon is attached. The handshake timeout therefore has to be
//! generous, and a slow first connect is normal rather than a failure.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::discover::{self, Discovery, SchedulerTarget};
use crate::msg::{self, Event};
use crate::wire::{self, Handshake, MsgType, PROTOCOL_VERSION};

/// Idle time before the kernel starts probing a quiet monitor connection.
/// Deliberately longer than a normal lull but far shorter than the minutes a
/// cluster can legitimately stay silent.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(20);
/// Gap between probes once probing starts.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
/// Unanswered probes before the connection is declared dead. With the values
/// above, a vanished scheduler is noticed in roughly 35 s.
#[cfg(target_os = "linux")]
const KEEPALIVE_RETRIES: u32 = 3;

/// Capture file magic. Bump the digit if the layout ever changes.
const CAPTURE_MAGIC: &[u8; 8] = b"ICWCAP01";

/// What the connection task reports upward.
#[derive(Debug, Clone)]
pub enum Update {
    /// Looking for, or handshaking with, a scheduler. May last ~36 s; see the
    /// module note.
    Connecting { what: String },
    /// Logged in as a monitor. The node list arrives immediately after as a
    /// burst of `Stats` events.
    Connected {
        target: SchedulerTarget,
        protocol: u32,
    },
    /// A decoded monitor event.
    Event(Event),
    /// Connection lost or never established. The task will retry.
    Disconnected {
        reason: String,
        /// Consecutive failed attempts. Reset to 0 by a successful login, so a
        /// large number means "down for a while", not "flapping".
        attempt: u32,
        /// When the next attempt starts, so the UI can count down rather than
        /// leaving the user wondering whether anything is still happening.
        retry_at: std::time::Instant,
    },
}

#[derive(Debug, Clone)]
pub struct Options {
    /// Time allowed for UDP discovery.
    pub discover_timeout: Duration,
    /// Time allowed for TCP connect plus version handshake. Must comfortably
    /// exceed 36 s or idle clusters look dead.
    pub handshake_timeout: Duration,
    /// First delay before reconnecting. Doubles per consecutive failure up to
    /// [`Self::reconnect_max_delay`].
    pub reconnect_min_delay: Duration,
    /// Ceiling on the reconnect delay. A monitor left running overnight against
    /// a dead scheduler must not spend the night broadcasting.
    pub reconnect_max_delay: Duration,
    /// Append the raw post-handshake frame stream here, for offline replay.
    pub record: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            discover_timeout: Duration::from_secs(2),
            handshake_timeout: Duration::from_secs(45),
            reconnect_min_delay: Duration::from_secs(1),
            reconnect_max_delay: Duration::from_secs(30),
            record: None,
        }
    }
}

/// Where events come from.
#[derive(Debug, Clone)]
pub enum Source {
    Live(Discovery),
    /// Replay a capture file, pacing events by their recorded timestamps.
    Replay {
        path: PathBuf,
        realtime: bool,
    },
}

/// A handle for cutting the current reconnect wait short.
///
/// Backoff is what stops a dead scheduler being probed all night, but it also
/// means a scheduler that comes back can go unnoticed for as long as the cap.
/// Letting the user say "try now" is what makes an aggressive cap safe.
#[derive(Clone, Debug, Default)]
pub struct Retry(std::sync::Arc<tokio::sync::Notify>);

impl Retry {
    /// Wake the connection task if it is waiting between attempts. Harmless at
    /// any other time: a connected session has nothing to interrupt.
    pub fn now(&self) {
        self.0.notify_one();
    }
}

/// Spawn the connection task. The receiver is the only thing the UI touches.
pub fn spawn(source: Source, opts: Options) -> (mpsc::Receiver<Update>, Retry) {
    let (tx, rx) = mpsc::channel(1024);
    let retry = Retry::default();
    let signal = retry.clone();
    tokio::spawn(async move {
        match source {
            Source::Live(d) => run_live(d, opts, signal, tx).await,
            Source::Replay { path, realtime } => {
                if let Err(e) = run_replay(&path, realtime, &tx).await {
                    let _ = tx
                        .send(Update::Disconnected {
                            reason: format!("replay failed: {e}"),
                            attempt: 0,
                            retry_at: std::time::Instant::now(),
                        })
                        .await;
                }
            }
        }
    });
    (rx, retry)
}

async fn run_live(
    discovery: Discovery,
    opts: Options,
    retry: Retry,
    tx: mpsc::Sender<Update>,
) {
    // Counts *consecutive* failures. A connection that got as far as logging in
    // resets it, so a scheduler that flaps once an hour keeps reconnecting
    // promptly while one that is simply gone is backed off.
    let mut attempt: u32 = 0;

    loop {
        let mut established = false;
        let outcome = connect_once(&discovery, &opts, &tx, &mut established).await;

        if established {
            attempt = 0;
        } else {
            attempt = attempt.saturating_add(1);
        }
        let delay = backoff(attempt, &opts);

        let reason = match outcome {
            Ok(()) => "scheduler closed the connection".to_owned(),
            Err(e) => e.to_string(),
        };
        if tx
            .send(Update::Disconnected {
                reason,
                attempt,
                retry_at: std::time::Instant::now() + delay,
            })
            .await
            .is_err()
        {
            return; // receiver gone: the UI has quit
        }

        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = retry.0.notified() => {
                tracing::info!("reconnecting now at the user's request");
            }
        }
        if tx.is_closed() {
            return;
        }
    }
}

/// Exponential backoff, capped. `attempt` 0 means the last try succeeded, so
/// reconnect immediately rather than sitting out a delay after a scheduler
/// restart.
fn backoff(attempt: u32, opts: &Options) -> Duration {
    if attempt == 0 {
        return Duration::ZERO;
    }
    let shift = (attempt - 1).min(16);
    opts.reconnect_min_delay
        .saturating_mul(1u32 << shift)
        .min(opts.reconnect_max_delay)
}

/// Ask the kernel to probe an idle connection, so a peer that vanished without
/// a FIN is noticed.
///
/// This is the only thing standing between the user and a frozen screen: the
/// scheduler sends nothing on an idle cluster (§2.3), so application silence is
/// indistinguishable from a severed link. Without keepalive, a pulled cable or
/// a dropped VPN leaves the monitor displaying a "connected" cluster whose
/// figures stopped being true, indefinitely.
fn enable_keepalive(stream: &TcpStream) -> io::Result<()> {
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(KEEPALIVE_IDLE)
        .with_interval(KEEPALIVE_INTERVAL);
    #[cfg(target_os = "linux")]
    let keepalive = keepalive.with_retries(KEEPALIVE_RETRIES);
    socket2::SockRef::from(stream).set_tcp_keepalive(&keepalive)
}

/// One full attempt: locate, connect, handshake, log in, then pump frames until
/// the connection ends.
async fn connect_once(
    discovery: &Discovery,
    opts: &Options,
    tx: &mpsc::Sender<Update>,
    established: &mut bool,
) -> io::Result<()> {
    let target = match discovery {
        Discovery::Explicit(t) => {
            let _ = tx
                .send(Update::Connecting {
                    what: format!("connecting to {t}"),
                })
                .await;
            t.clone()
        }
        Discovery::Broadcast { netname, port } => {
            let _ = tx
                .send(Update::Connecting {
                    what: format!("searching for netname {netname:?} on port {port}"),
                })
                .await;
            match discover::broadcast(netname, *port, opts.discover_timeout).await? {
                Some(a) => a.target,
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("no scheduler answered for netname {netname:?}"),
                    ))
                }
            }
        }
    };

    let _ = tx
        .send(Update::Connecting {
            what: format!("handshaking with {target}"),
        })
        .await;

    // One timeout covers connect and handshake together: both are part of "can
    // we talk to this scheduler at all", and the scheduler's accept latency
    // makes them indistinguishable from outside.
    let mut stream = tokio::time::timeout(opts.handshake_timeout, async {
        let stream = TcpStream::connect((target.host.as_str(), target.port)).await?;
        stream.set_nodelay(true)?;
        // Not fatal: a kernel that refuses the option still gives a usable
        // session, just one that cannot notice a severed link on its own.
        if let Err(e) = enable_keepalive(&stream) {
            tracing::warn!("could not enable TCP keepalive: {e}");
        }
        Ok::<_, io::Error>(stream)
    })
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("timed out connecting to {target}"),
        )
    })??;

    let protocol = tokio::time::timeout(opts.handshake_timeout, handshake(&mut stream))
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "{target} accepted the connection but did not complete the version \
                     handshake within {:?} (an idle scheduler can take ~36 s)",
                    opts.handshake_timeout
                ),
            )
        })??;

    stream.write_all(&wire::mon_login_frame()).await?;
    stream.flush().await?;

    *established = true;
    let _ = tx
        .send(Update::Connected {
            target: target.clone(),
            protocol,
        })
        .await;

    let mut recorder = match &opts.record {
        Some(path) => Some(Recorder::create(path, protocol).await?),
        None => None,
    };

    // Pump frames. The scheduler never reads from a monitor again, so there is
    // nothing to send from here on.
    loop {
        let (ty, payload) = match read_frame(&mut stream).await {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };

        if let Some(r) = recorder.as_mut() {
            r.write(ty, &payload).await?;
        }

        match msg::decode(ty, &payload, protocol) {
            Ok(Event::End) => return Ok(()),
            Ok(ev) => {
                if tx.send(Update::Event(ev)).await.is_err() {
                    return Ok(()); // UI quit
                }
            }
            // A field we misread must not kill the session; the next frame is
            // independent because framing is length-prefixed.
            Err(e) => tracing::warn!("undecodable message type {ty}: {e}"),
        }
    }
}

/// The four-step version exchange. Returns the negotiated protocol.
async fn handshake(stream: &mut TcpStream) -> io::Result<u32> {
    let mut hs = Handshake::new(PROTOCOL_VERSION);
    stream.write_all(&hs.greeting()).await?;
    stream.flush().await?;

    let mut peer = [0u8; 4];
    stream.read_exact(&mut peer).await?;
    let echo = hs.accept_peer(peer)?;
    stream.write_all(&echo).await?;
    stream.flush().await?;

    let mut confirm = [0u8; 4];
    stream.read_exact(&mut confirm).await?;
    let protocol = hs.confirm(confirm)?;
    tracing::info!("negotiated icecream protocol {protocol}");
    Ok(protocol)
}

/// Read one `[u32 len][u32 type][payload]` frame.
async fn read_frame(stream: &mut TcpStream) -> io::Result<(u32, Vec<u8>)> {
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    let len = u32::from_be_bytes(head);
    let payload_len = wire::payload_len(len)?;

    let mut ty = [0u8; 4];
    stream.read_exact(&mut ty).await?;

    let mut payload = vec![0u8; payload_len];
    stream.read_exact(&mut payload).await?;
    Ok((u32::from_be_bytes(ty), payload))
}

/// Writes the frame stream to disk so protocol work does not need a cluster.
struct Recorder {
    file: tokio::fs::File,
    start: std::time::Instant,
}

impl Recorder {
    async fn create(path: &Path, protocol: u32) -> io::Result<Self> {
        let mut file = tokio::fs::File::create(path).await?;
        file.write_all(CAPTURE_MAGIC).await?;
        file.write_all(&protocol.to_be_bytes()).await?;
        Ok(Self {
            file,
            start: std::time::Instant::now(),
        })
    }

    async fn write(&mut self, ty: u32, payload: &[u8]) -> io::Result<()> {
        let millis = self.start.elapsed().as_millis() as u64;
        self.file.write_all(&millis.to_be_bytes()).await?;
        let len = 4 + payload.len() as u32;
        self.file.write_all(&len.to_be_bytes()).await?;
        self.file.write_all(&ty.to_be_bytes()).await?;
        self.file.write_all(payload).await?;
        self.file.flush().await
    }
}

async fn run_replay(path: &Path, realtime: bool, tx: &mpsc::Sender<Update>) -> io::Result<()> {
    let data = tokio::fs::read(path).await?;
    if data.len() < 12 || &data[..8] != CAPTURE_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not an icecream-watcher capture file",
        ));
    }
    let protocol = u32::from_be_bytes(data[8..12].try_into().unwrap());

    let _ = tx
        .send(Update::Connected {
            target: SchedulerTarget {
                host: format!("replay:{}", path.display()),
                port: 0,
            },
            protocol,
        })
        .await;

    let mut pos = 12usize;
    let mut last_millis = 0u64;
    while pos + 12 <= data.len() {
        let millis = u64::from_be_bytes(data[pos..pos + 8].try_into().unwrap());
        let len = u32::from_be_bytes(data[pos + 8..pos + 12].try_into().unwrap());
        let payload_len = wire::payload_len(len)?;
        let ty_at = pos + 12;
        if ty_at + 4 + payload_len > data.len() {
            break; // truncated capture: replay what we have
        }
        let ty = u32::from_be_bytes(data[ty_at..ty_at + 4].try_into().unwrap());
        let payload = &data[ty_at + 4..ty_at + 4 + payload_len];
        pos = ty_at + 4 + payload_len;

        if realtime {
            let gap = millis.saturating_sub(last_millis);
            if gap > 0 {
                tokio::time::sleep(Duration::from_millis(gap)).await;
            }
        }
        last_millis = millis;

        match msg::decode(ty, payload, protocol) {
            Ok(Event::End) => break,
            Ok(ev) => {
                if tx.send(Update::Event(ev)).await.is_err() {
                    return Ok(());
                }
            }
            Err(e) => tracing::warn!("undecodable message type {ty}: {e}"),
        }
    }

    let _ = tx
        .send(Update::Disconnected {
            reason: "end of capture".into(),
            attempt: 0,
            retry_at: std::time::Instant::now(),
        })
        .await;
    Ok(())
}

/// Encode a frame the way a capture file stores it. Used by the test fixtures.
pub fn capture_bytes(protocol: u32, frames: &[(MsgType, Vec<u8>)]) -> Vec<u8> {
    let mut out = CAPTURE_MAGIC.to_vec();
    out.extend_from_slice(&protocol.to_be_bytes());
    for (i, (ty, payload)) in frames.iter().enumerate() {
        out.extend_from_slice(&(i as u64).to_be_bytes());
        out.extend_from_slice(&wire::encode_frame(*ty, payload));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Options {
        Options::default()
    }

    #[test]
    fn a_successful_session_reconnects_immediately() {
        // attempt 0 means the last try worked, so a scheduler restart is picked
        // up at once rather than after a penalty it did not earn.
        assert_eq!(backoff(0, &opts()), Duration::ZERO);
    }

    #[test]
    fn consecutive_failures_back_off_and_then_stop_growing() {
        let o = opts();
        let delays: Vec<Duration> = (1..=8).map(|a| backoff(a, &o)).collect();
        assert_eq!(delays[0], Duration::from_secs(1));
        assert_eq!(delays[1], Duration::from_secs(2));
        assert_eq!(delays[2], Duration::from_secs(4));

        for pair in delays.windows(2) {
            assert!(pair[1] >= pair[0], "backoff must not shrink: {delays:?}");
        }
        assert_eq!(
            *delays.last().unwrap(),
            o.reconnect_max_delay,
            "backoff must settle at the cap"
        );
    }

    #[test]
    fn backoff_cannot_overflow_on_a_long_outage() {
        // A monitor left running for days against a dead scheduler reaches very
        // large attempt counts; the shift must not wrap or the multiply panic.
        let o = opts();
        for attempt in [16u32, 64, 1000, u32::MAX] {
            assert_eq!(backoff(attempt, &o), o.reconnect_max_delay);
        }
    }

    #[test]
    fn an_hour_of_outage_costs_a_bounded_number_of_attempts() {
        // The point of backoff: a dead scheduler must not be probed all night.
        let o = opts();
        let mut elapsed = Duration::ZERO;
        let mut attempts = 0u32;
        while elapsed < Duration::from_secs(3600) {
            attempts += 1;
            elapsed += backoff(attempts, &o);
        }
        assert!(
            attempts <= 125,
            "an hour down should cost ~2 attempts a minute, not {attempts}"
        );
    }

    #[tokio::test]
    async fn keepalive_is_actually_set_on_the_socket() {
        // Asserted against the kernel rather than trusting the call: without
        // this option a severed link leaves a frozen "connected" screen for as
        // long as the user leaves it open.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accept = tokio::spawn(async move { listener.accept().await });
        let stream = TcpStream::connect(addr).await.unwrap();
        let _server = accept.await.unwrap().unwrap();

        enable_keepalive(&stream).expect("set keepalive");

        let sock = socket2::SockRef::from(&stream);
        assert!(sock.keepalive().unwrap(), "SO_KEEPALIVE not enabled");
        assert_eq!(sock.keepalive_time().unwrap(), KEEPALIVE_IDLE);
        #[cfg(target_os = "linux")]
        {
            assert_eq!(sock.keepalive_interval().unwrap(), KEEPALIVE_INTERVAL);
            assert_eq!(sock.keepalive_retries().unwrap(), KEEPALIVE_RETRIES);
        }
    }

    #[tokio::test]
    async fn a_scheduler_that_never_answers_is_reported_with_a_retry_plan() {
        // A listener that accepts and says nothing is the shape of a hung or
        // filtered scheduler. It must produce a bounded failure carrying enough
        // for the UI to show that something is still being tried.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((s, _)) = listener.accept().await {
                held.push(s);
            }
        });

        let (mut rx, _retry) = spawn(
            Source::Live(Discovery::Explicit(SchedulerTarget {
                host: addr.ip().to_string(),
                port: addr.port(),
            })),
            Options {
                handshake_timeout: Duration::from_millis(150),
                reconnect_min_delay: Duration::from_millis(10),
                reconnect_max_delay: Duration::from_millis(50),
                ..Options::default()
            },
        );

        let mut disconnects = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while disconnects.len() < 2 {
            let Ok(Some(update)) = tokio::time::timeout_at(deadline, rx.recv()).await else {
                break;
            };
            if let Update::Disconnected {
                reason, attempt, ..
            } = update
            {
                disconnects.push((reason, attempt));
            }
        }

        assert_eq!(disconnects.len(), 2, "the task must keep retrying");
        assert!(
            disconnects[0].0.contains("handshake"),
            "the reason must name what failed: {:?}",
            disconnects[0].0
        );
        assert_eq!(disconnects[0].1, 1);
        assert_eq!(
            disconnects[1].1, 2,
            "consecutive failures must keep counting up"
        );
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;

    #[tokio::test]
    async fn a_retry_request_cuts_the_backoff_wait_short() {
        // Backoff is what keeps a dead scheduler from being probed all night,
        // but it also delays noticing one that came back. This is what makes an
        // aggressive cap safe to ship.
        let retry = Retry::default();
        let waiter = retry.clone();
        let started = tokio::time::Instant::now();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            retry.now();
        });

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(30)) => panic!("retry was ignored"),
            _ = waiter.0.notified() => {}
        }
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn a_retry_request_while_connected_is_harmless() {
        // notify_one stores one permit, so a request made with nothing waiting
        // is consumed by the next wait rather than lost or doubled.
        let retry = Retry::default();
        retry.now();
        retry.now();
        let first = tokio::time::timeout(Duration::from_millis(50), retry.0.notified()).await;
        assert!(first.is_ok(), "a stored request should be honoured");
        let second = tokio::time::timeout(Duration::from_millis(50), retry.0.notified()).await;
        assert!(second.is_err(), "two requests must not queue up as two wakeups");
    }
}
