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
    Disconnected { reason: String },
}

#[derive(Debug, Clone)]
pub struct Options {
    /// Time allowed for UDP discovery.
    pub discover_timeout: Duration,
    /// Time allowed for TCP connect plus version handshake. Must comfortably
    /// exceed 36 s or idle clusters look dead.
    pub handshake_timeout: Duration,
    /// Delay before reconnecting.
    pub reconnect_delay: Duration,
    /// Append the raw post-handshake frame stream here, for offline replay.
    pub record: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            discover_timeout: Duration::from_secs(2),
            handshake_timeout: Duration::from_secs(45),
            reconnect_delay: Duration::from_secs(2),
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

/// Spawn the connection task. The receiver is the only thing the UI touches.
pub fn spawn(source: Source, opts: Options) -> mpsc::Receiver<Update> {
    let (tx, rx) = mpsc::channel(1024);
    tokio::spawn(async move {
        match source {
            Source::Live(d) => run_live(d, opts, tx).await,
            Source::Replay { path, realtime } => {
                if let Err(e) = run_replay(&path, realtime, &tx).await {
                    let _ = tx
                        .send(Update::Disconnected {
                            reason: format!("replay failed: {e}"),
                        })
                        .await;
                }
            }
        }
    });
    rx
}

async fn run_live(discovery: Discovery, opts: Options, tx: mpsc::Sender<Update>) {
    loop {
        match connect_once(&discovery, &opts, &tx).await {
            Ok(()) => {
                let _ = tx
                    .send(Update::Disconnected {
                        reason: "scheduler closed the connection".into(),
                    })
                    .await;
            }
            Err(e) => {
                if tx
                    .send(Update::Disconnected {
                        reason: e.to_string(),
                    })
                    .await
                    .is_err()
                {
                    return; // receiver gone: the UI has quit
                }
            }
        }
        tokio::time::sleep(opts.reconnect_delay).await;
        if tx.is_closed() {
            return;
        }
    }
}

/// One full attempt: locate, connect, handshake, log in, then pump frames until
/// the connection ends.
async fn connect_once(
    discovery: &Discovery,
    opts: &Options,
    tx: &mpsc::Sender<Update>,
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
