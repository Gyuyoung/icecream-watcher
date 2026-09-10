//! Decoders for the six messages a monitor receives.
//!
//! Layouts come from `fill_from_channel` in `icecream/services/comm.cpp`.
//! `MON_STATS` was additionally confirmed byte-for-byte against a live scheduler.

use crate::wire::{MsgType, ProtoError, Reader, Result, MIN_MON_GET_CS_VERSION};

/// A remote job's cost report, from `JobDoneMsg`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JobDone {
    pub job_id: u32,
    pub exit_code: u32,
    pub real_msec: u32,
    pub user_msec: u32,
    pub sys_msec: u32,
    pub page_faults: u32,
    pub in_compressed: u32,
    pub in_uncompressed: u32,
    pub out_compressed: u32,
    pub out_uncompressed: u32,
    pub flags: u32,
    /// Protocol >= 39 only.
    pub client_count: Option<u32>,
}

impl JobDone {
    /// `JobDoneMsg::FROM_SUBMITTER`. When unset the report came from the node
    /// that did the compiling.
    pub fn from_submitter(&self) -> bool {
        self.flags & 1 != 0
    }

    /// `JobDoneMsg::UnknownJobId` — the scheduler had already forgotten this job.
    pub fn unknown_job_id(&self) -> bool {
        self.flags & (1 << 1) != 0
    }
}

/// One decoded monitor event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Node record changed, joined, or went offline. `stats` is the raw
    /// `Key:Value` blob; parse it with [`crate::stats::StatsRecord`].
    Stats { host_id: u32, stats: String },
    /// A client asked for a node: the job exists but has no node yet (pending).
    GetCs {
        job_id: u32,
        client_id: u32,
        filename: String,
        lang: u32,
    },
    /// A remote job started on `host_id`.
    JobBegin {
        job_id: u32,
        start_time: u32,
        host_id: u32,
    },
    /// A remote job finished.
    JobDone(JobDone),
    /// A job compiled locally on `host_id`, never distributed.
    LocalJobBegin {
        job_id: u32,
        start_time: u32,
        host_id: u32,
        file: String,
    },
    /// A local job finished.
    LocalJobDone { job_id: u32 },
    /// Scheduler is closing the connection.
    End,
    /// Keepalive. Monitors are never actually pinged (the scheduler stops
    /// reading from us after `MON_LOGIN`), but decode it rather than warn.
    Ping,
    /// A message we do not model. Carried through so callers can log it once
    /// instead of treating the stream as corrupt.
    Other { ty: u32 },
}

/// Decode one frame body. `payload` excludes the length and type words;
/// `protocol` is the negotiated version, which gates optional trailing fields.
pub fn decode(ty: u32, payload: &[u8], protocol: u32) -> Result<Event> {
    let Some(ty) = MsgType::from_u32(ty) else {
        return Ok(Event::Other { ty });
    };
    let mut r = Reader::new(payload);

    Ok(match ty {
        MsgType::MonStats => Event::Stats {
            host_id: r.u32()?,
            stats: r.string()?,
        },

        MsgType::MonJobBegin => Event::JobBegin {
            job_id: r.u32()?,
            start_time: r.u32()?,
            host_id: r.u32()?,
        },

        MsgType::MonLocalJobBegin => Event::LocalJobBegin {
            job_id: r.u32()?,
            start_time: r.u32()?,
            host_id: r.u32()?,
            file: r.string()?,
        },

        MsgType::JobLocalDone => Event::LocalJobDone { job_id: r.u32()? },

        MsgType::MonJobDone => {
            let mut d = JobDone {
                job_id: r.u32()?,
                exit_code: r.u32()?,
                real_msec: r.u32()?,
                user_msec: r.u32()?,
                sys_msec: r.u32()?,
                page_faults: r.u32()?,
                in_compressed: r.u32()?,
                in_uncompressed: r.u32()?,
                out_compressed: r.u32()?,
                out_uncompressed: r.u32()?,
                flags: r.u32()?,
                client_count: None,
            };
            if protocol >= 39 {
                d.client_count = r.u32_opt();
            } else if d.exit_code == 200 {
                // Pre-39 overloaded exit code 200 to mean "unknown job id".
                d.flags |= 1 << 1;
            }
            Event::JobDone(d)
        }

        // Protocol >= 29 sends the short monitor form. Older schedulers send a
        // full GetCSMsg whose body varies across six protocol revisions; we
        // decline it instead of carrying that path (ARCHITECTURE.md §2.6).
        MsgType::MonGetCs => {
            if protocol < MIN_MON_GET_CS_VERSION {
                return Ok(Event::Other { ty: ty as u32 });
            }
            let filename = r.string()?;
            let lang = r.u32()?;
            Event::GetCs {
                job_id: r.u32()?,
                client_id: r.u32()?,
                filename,
                lang,
            }
        }

        MsgType::End => Event::End,
        MsgType::Ping => Event::Ping,
        MsgType::MonLogin | MsgType::Unknown => Event::Other { ty: ty as u32 },
    })
}

/// Language tag from `MON_GET_CS` (`CompileJob::Language`).
pub fn language_name(lang: u32) -> &'static str {
    match lang {
        0 => "C",
        1 => "C++",
        2 => "ObjC",
        3 => "ObjC++",
        _ => "?",
    }
}

impl From<ProtoError> for std::io::Error {
    fn from(e: ProtoError) -> Self {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be(v: u32) -> [u8; 4] {
        v.to_be_bytes()
    }

    fn cstr(s: &str) -> Vec<u8> {
        let mut v = be((s.len() + 1) as u32).to_vec();
        v.extend_from_slice(s.as_bytes());
        v.push(0);
        v
    }

    #[test]
    fn decodes_mon_stats() {
        // Shape of the frame captured live: u32 hostid then a NUL-terminated blob.
        let mut p = be(1).to_vec();
        p.extend_from_slice(&cstr("Name:build01\nMaxJobs:16\n"));
        let ev = decode(87, &p, 43).unwrap();
        assert_eq!(
            ev,
            Event::Stats {
                host_id: 1,
                stats: "Name:build01\nMaxJobs:16\n".into()
            }
        );
    }

    #[test]
    fn decodes_job_begin() {
        let mut p = Vec::new();
        p.extend_from_slice(&be(7));
        p.extend_from_slice(&be(1_700_000_000));
        p.extend_from_slice(&be(3));
        assert_eq!(
            decode(84, &p, 43).unwrap(),
            Event::JobBegin {
                job_id: 7,
                start_time: 1_700_000_000,
                host_id: 3
            }
        );
    }

    #[test]
    fn decodes_local_job_begin_with_filename() {
        let mut p = Vec::new();
        p.extend_from_slice(&be(9));
        p.extend_from_slice(&be(100));
        p.extend_from_slice(&be(2));
        p.extend_from_slice(&cstr("main.cpp"));
        assert_eq!(
            decode(86, &p, 43).unwrap(),
            Event::LocalJobBegin {
                job_id: 9,
                start_time: 100,
                host_id: 2,
                file: "main.cpp".into()
            }
        );
    }

    #[test]
    fn job_done_reads_client_count_only_from_protocol_39() {
        let mut p = Vec::new();
        for v in [5u32, 0, 1200, 900, 300, 42, 1000, 4000, 500, 2000, 1] {
            p.extend_from_slice(&be(v));
        }
        p.extend_from_slice(&be(3)); // client_count

        let Event::JobDone(d) = decode(85, &p, 43).unwrap() else {
            panic!("expected JobDone")
        };
        assert_eq!(d.job_id, 5);
        assert_eq!(d.real_msec, 1200);
        assert_eq!(d.client_count, Some(3));
        assert!(d.from_submitter());

        // On an older peer the same trailing word is not ours to read.
        let Event::JobDone(old) = decode(85, &p, 38).unwrap() else {
            panic!("expected JobDone")
        };
        assert_eq!(old.client_count, None);
    }

    #[test]
    fn job_done_translates_the_legacy_unknown_job_marker() {
        let mut p = Vec::new();
        for v in [5u32, 200, 0, 0, 0, 0, 0, 0, 0, 0, 0] {
            p.extend_from_slice(&be(v));
        }
        let Event::JobDone(d) = decode(85, &p, 38).unwrap() else {
            panic!("expected JobDone")
        };
        assert!(d.unknown_job_id());

        // Protocol 39+ uses the flag directly, so 200 stays a plain exit code.
        let Event::JobDone(d) = decode(85, &p, 39).unwrap() else {
            panic!("expected JobDone")
        };
        assert!(!d.unknown_job_id());
        assert_eq!(d.exit_code, 200);
    }

    #[test]
    fn decodes_mon_get_cs_short_form() {
        let mut p = cstr("widget.cpp");
        p.extend_from_slice(&be(1)); // lang C++
        p.extend_from_slice(&be(11)); // job id
        p.extend_from_slice(&be(4)); // client id
        assert_eq!(
            decode(83, &p, 43).unwrap(),
            Event::GetCs {
                job_id: 11,
                client_id: 4,
                filename: "widget.cpp".into(),
                lang: 1
            }
        );
    }

    #[test]
    fn declines_mon_get_cs_below_protocol_29() {
        assert_eq!(decode(83, &[], 28).unwrap(), Event::Other { ty: 83 });
    }

    #[test]
    fn unknown_types_are_carried_not_fatal() {
        assert_eq!(decode(200, &[], 43).unwrap(), Event::Other { ty: 200 });
    }

    #[test]
    fn truncated_frames_are_errors() {
        assert!(decode(84, &be(1), 43).is_err());
    }
}
