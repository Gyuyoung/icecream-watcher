# Protocol captures

Recorded scheduler streams, so protocol work does not need a live cluster.

Record one:

    icecream-watcher --scheduler build-master:8765 --record session.icwcap

Replay it:

    icecream-watcher --replay session.icwcap              # as fast as possible
    icecream-watcher --replay session.icwcap --replay-realtime   # original pacing

## Format

Everything is big-endian. The stream stored is the post-handshake frame stream
exactly as it arrived, so a capture is a faithful record of the wire.

```
magic     8 bytes   "ICWCAP01"
protocol  u32       negotiated protocol version for the session
repeat:
  offset  u32 x2    milliseconds since the first recorded frame (u64)
  length  u32       frame length, covering the type word and payload
  type    u32       message type
  payload length-4 bytes
```

A capture truncated mid-frame (killed recorder, full disk) replays up to the
last complete frame rather than failing; `golden.rs` asserts that every
truncation point is survivable.

## Fixtures

| File | Recorded against | Contents |
|---|---|---|
| `lab-session.icwcap` | `icecc-scheduler` 1.4, protocol 43, one `iceccd`, isolated netname `ICWLAB` on port 18765 | login replay, then two live stats updates captured while CPU load was applied and released |

`lab-session.icwcap` is the fixture behind
`crates/icecc-proto/tests/golden.rs`. It is small on purpose and shows the two
cases the parser must not confuse: the login replay carries identity but **no**
`LoadAvg*`/`FreeMem`, while the later updates carry both. Keep it.
