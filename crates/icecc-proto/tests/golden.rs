//! Golden tests against bytes captured from a real scheduler.
//!
//! The fixture in `contrib/capture/` was recorded with
//! `icecc-top --scheduler localhost:18765 --record …` against
//! `icecc-scheduler` 1.4 (protocol 43) with one `iceccd` attached, while CPU
//! load was applied so that a live stats update landed in the stream.
//!
//! These tests are the reason the protocol layer can be changed safely without
//! a cluster to hand: if framing, endianness or the `statmsg` parse regress,
//! real bytes stop decoding.

use icecc_proto::msg::{self, Event};
use icecc_proto::stats::StatsRecord;
use icecc_proto::wire;

const CAPTURE: &[u8] = include_bytes!("../../../contrib/capture/lab-session.ictpcap");

/// The first bytes the live scheduler actually sent after `MON_LOGIN`, copied
/// out of the capture. Kept inline so this test stands on its own.
#[rustfmt::skip]
const REAL_MON_STATS_FRAME: &[u8] = &[
    // length = 152, covering the type word and payload
    0x00, 0x00, 0x00, 0x98,
    // type = 87 (MON_STATS)
    0x00, 0x00, 0x00, 0x57,
    // host id = 1
    0x00, 0x00, 0x00, 0x01,
    // string length = 140, including the NUL terminator
    0x00, 0x00, 0x00, 0x8c,
    // "Name:gyu..." — the rest is asserted through the decoder below
];

#[test]
fn real_frame_header_matches_our_framing_rules() {
    let len = u32::from_be_bytes(REAL_MON_STATS_FRAME[0..4].try_into().unwrap());
    let ty = u32::from_be_bytes(REAL_MON_STATS_FRAME[4..8].try_into().unwrap());
    let host_id = u32::from_be_bytes(REAL_MON_STATS_FRAME[8..12].try_into().unwrap());
    let str_len = u32::from_be_bytes(REAL_MON_STATS_FRAME[12..16].try_into().unwrap());

    assert_eq!(len, 152);
    assert_eq!(wire::payload_len(len).unwrap(), 148);
    assert_eq!(ty, wire::MsgType::MonStats as u32);
    assert_eq!(host_id, 1);
    // 148 payload bytes = 4 (host id) + 4 (string length) + 140 (string)
    assert_eq!(str_len as usize + 8, wire::payload_len(len).unwrap());
}

/// Decode the whole capture the way `conn::run_replay` does.
fn decode_capture() -> (u32, Vec<Event>) {
    assert_eq!(&CAPTURE[..8], b"ICTPCAP1", "fixture magic");
    let protocol = u32::from_be_bytes(CAPTURE[8..12].try_into().unwrap());

    let mut events = Vec::new();
    let mut pos = 12usize;
    while pos + 12 <= CAPTURE.len() {
        let len = u32::from_be_bytes(CAPTURE[pos + 8..pos + 12].try_into().unwrap());
        let payload_len = wire::payload_len(len).expect("valid frame length");
        let ty_at = pos + 12;
        let ty = u32::from_be_bytes(CAPTURE[ty_at..ty_at + 4].try_into().unwrap());
        let payload = &CAPTURE[ty_at + 4..ty_at + 4 + payload_len];
        events.push(msg::decode(ty, payload, protocol).expect("decodes"));
        pos = ty_at + 4 + payload_len;
    }
    assert_eq!(pos, CAPTURE.len(), "capture consumed exactly");
    (protocol, events)
}

#[test]
fn capture_decodes_to_the_expected_event_sequence() {
    let (protocol, events) = decode_capture();
    assert_eq!(protocol, 43);
    assert_eq!(events.len(), 3, "one login replay plus two stats updates");
    assert!(
        events
            .iter()
            .all(|e| matches!(e, Event::Stats { host_id: 1, .. })),
        "all three frames are MON_STATS for host 1"
    );
}

#[test]
fn login_replay_has_identity_but_no_resource_fields() {
    let (_, events) = decode_capture();
    let Event::Stats { stats, .. } = &events[0] else {
        panic!("expected Stats")
    };
    let rec = StatsRecord::parse(stats);

    assert_eq!(rec.name.as_deref(), Some("gyuyoung-ThinkPad-P1"));
    assert_eq!(rec.ip.as_deref(), Some("127.0.0.1"));
    assert_eq!(rec.max_jobs, Some(4));
    assert_eq!(rec.protocol, Some(43));
    assert_eq!(rec.no_remote, Some(true));
    assert_eq!(rec.platform.as_deref(), Some("x86_64"));
    assert_eq!(rec.features.as_deref(), Some("env_xz env_zstd"));
    assert_eq!(rec.speed, Some(0.0));
    assert!(rec.load.is_some());

    // This is the constraint the whole architecture turns on: the scheduler's
    // login replay carries no live resource data at all.
    assert!(
        !rec.has_resource_fields(),
        "login replay must not appear to carry CPU/memory data"
    );
    assert_eq!(rec.load_avg_1, None);
    assert_eq!(rec.free_mem_mib, None);
}

#[test]
fn a_loaded_update_carries_resource_fields_with_units_applied() {
    let (_, events) = decode_capture();
    let Event::Stats { stats, .. } = &events[1] else {
        panic!("expected Stats")
    };
    let rec = StatsRecord::parse(stats);

    assert!(rec.has_resource_fields());
    // Captured verbatim: Load:800 LoadAvg1:3940 LoadAvg5:10263 FreeMem:26521.
    assert_eq!(rec.load, Some(800));
    assert_eq!(rec.load_avg_1, Some(3.940));
    assert_eq!(rec.load_avg_5, Some(10.263));
    assert_eq!(rec.load_avg_10, Some(15.720));
    assert_eq!(rec.free_mem_mib, Some(26521));
}

#[test]
fn merging_the_capture_in_order_yields_one_complete_node() {
    let (_, events) = decode_capture();
    let mut node = StatsRecord::default();
    for ev in &events {
        let Event::Stats { stats, .. } = ev else {
            continue;
        };
        StatsRecord::parse(stats).merge_into(&mut node);
    }

    // Identity from the replay, resources from the later updates, last value wins.
    assert_eq!(node.name.as_deref(), Some("gyuyoung-ThinkPad-P1"));
    assert_eq!(node.max_jobs, Some(4));
    assert_eq!(node.load, Some(681));
    assert_eq!(node.load_avg_1, Some(5.314));
    assert_eq!(node.free_mem_mib, Some(26544));
    assert!(!node.offline);
}

#[test]
fn truncating_the_capture_anywhere_never_panics() {
    // A capture cut short mid-frame (killed recorder, full disk) must be
    // survivable, since replay is how protocol bugs get reproduced.
    for cut in 12..CAPTURE.len() {
        let data = &CAPTURE[..cut];
        let mut pos = 12usize;
        while pos + 12 <= data.len() {
            let len = u32::from_be_bytes(data[pos + 8..pos + 12].try_into().unwrap());
            let Ok(payload_len) = wire::payload_len(len) else {
                break;
            };
            let ty_at = pos + 12;
            if ty_at + 4 + payload_len > data.len() {
                break;
            }
            let ty = u32::from_be_bytes(data[ty_at..ty_at + 4].try_into().unwrap());
            let _ = msg::decode(ty, &data[ty_at + 4..ty_at + 4 + payload_len], 43);
            pos = ty_at + 4 + payload_len;
        }
    }
}
