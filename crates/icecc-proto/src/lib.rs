//! The Icecream (icecc) scheduler *monitor* protocol.
//!
//! This is a native implementation, not a binding to `libicecc`. It speaks the
//! read-only half of the protocol that `icemon` and `icecream-sundae` use: log
//! in as a monitor and consume the cluster event stream.
//!
//! Layouts were read from `icecream/services/comm.cpp` and
//! `icecream/scheduler/scheduler.cpp`, then confirmed on the wire against a
//! live `icecc-scheduler` 1.4 (protocol 43). See `ARCHITECTURE.md` §2 for the
//! full derivation, including the parts that are easy to get wrong:
//!
//! * the version handshake is little-endian while payload scalars are big-endian,
//! * `MON_STATS` records are *partial* and must be merged, not replaced,
//! * `Load` is a composite scheduling weight, not CPU utilisation,
//! * stats arrive only when a node's load moves by ≥10 %, not on a timer,
//! * an idle scheduler can take ~36 s to answer a handshake.
//!
//! ```no_run
//! use icecc_proto::{conn, discover};
//!
//! # async fn demo() {
//! let discovery = discover::resolve(Some("build-master:8765"), None);
//! let mut rx = conn::spawn(conn::Source::Live(discovery), conn::Options::default());
//! while let Some(update) = rx.recv().await {
//!     println!("{update:?}");
//! }
//! # }
//! ```

pub mod conn;
pub mod discover;
pub mod msg;
pub mod stats;
pub mod wire;

pub use conn::{Options, Source, Update};
pub use discover::{Discovery, SchedulerTarget};
pub use msg::{Event, JobDone};
pub use stats::StatsRecord;
pub use wire::{MsgType, ProtoError, PROTOCOL_VERSION};
