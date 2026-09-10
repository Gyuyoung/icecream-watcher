# icecc-top

A terminal monitor for [Icecream](https://github.com/icecc/icecream) (`icecc`)
distributed compile clusters — the cluster equivalent of `btop`, where each
build node reads like a process.

**Status: Phase 2.** It attaches to a scheduler and shows the cluster's nodes and
jobs. Per-node CPU, memory and temperature are Phase 3; bars, graphs and a
detail view are Phase 4–5. See [ARCHITECTURE.md](ARCHITECTURE.md) for the design
and the roadmap.

```
icecc-top  build-master:8765  proto 43   nodes 8   slots 47/64 (73%)
┌ nodes ──────────────────────────────────────────────────────────────────────┐
│NODE                 IP                JOBS      LOAD   SPEED     PLATFORM   │
│build01              10.0.0.11         8/8       842    3200      x86_64     │
│build02              10.0.0.12         7/8       915    2900      x86_64     │
│build03              10.0.0.13         2/8       311    3100      x86_64     │
│laptop [local only]  10.0.0.40         0/0       —      —         x86_64     │
└─────────────────────────────────────────────────────────────────────────────┘
[q] quit  [r] redraw   jobs 17 active · 3 pending · 1 local   done 1284 remote / 12 local (since connect)
```

## Build

Needs a Rust toolchain (1.75+). No `libicecc` and no C++ build dependencies —
the scheduler protocol is implemented natively.

```sh
cargo build --release
./target/release/icecc-top
```

## Use

```sh
icecc-top                                   # discover via UDP broadcast
icecc-top --scheduler build-master:8765     # connect directly
icecc-top --netname MYFARM                  # discover on a named network
ICECC_SCHEDULER=build-master:8765 icecc-top # same as --scheduler
```

Discovery follows Icecream's own rules: an explicit `--scheduler` wins, then
`$ICECC_SCHEDULER`, then `$USE_SCHEDULER`, otherwise a broadcast for netname
`$ICECC_NETNAME` (default `ICECREAM`). Unlike `icecream-sundae`, the port is not
hardcoded — `host:port` works everywhere, including a bare `:8767` to broadcast
on a non-default port.

Other flags:

| Flag | Purpose |
|---|---|
| `--dump` | print events as text instead of drawing a TUI |
| `--record FILE` | save the raw protocol stream for offline replay |
| `--replay FILE` | replay a capture instead of connecting |
| `--log-file FILE` | write diagnostics (never to the terminal, which would corrupt the display) |

### Keys

| Key | Action |
|---|---|
| `q`, `Esc`, `Ctrl-C` | quit |
| `r` | redraw |

Navigation and sorting land in Phase 4, when there is data worth sorting by.

## Reading the columns

Two of these mean something other than what they look like, because of how the
scheduler reports:

- **LOAD** is Icecream's composite scheduling weight on a 0–1000 scale
  (`max(1000 - idle, memory pressure)`, forced to 1000 when the node is low on
  disk). It is **not** CPU utilisation. Real CPU percentages need the Phase 3
  agent.
- **SPEED** is `output bytes / user-second`, and is **unknown (`—`) until a node
  has actually compiled something** — a fresh cluster shows `—` everywhere. It
  is not zero-because-slow.
- **JOBS** is remote compiles running / slots offered. A `+2L` suffix counts
  local compiles, which occupy no scheduler slot.
- Node labels: `[offline]`, `[local only]` (the node refuses remote jobs), and
  `[no reply]` (the scheduler has pinged it and is still waiting).
- Cumulative counters are **since connect**. Job ids only mean anything within
  one scheduler session, so a reconnect necessarily resets them.

## Notes on connecting

The first connection to an **idle** scheduler can take up to ~36 seconds. That
is upstream behaviour, not a hang: the scheduler stops polling its listen socket
for a second after each accept, then blocks in `poll()` for up to
`MAX_SCHEDULER_PING`, and with no daemons attached nothing wakes it. Once
daemons are connected the handshake is immediate. `icecc-top` shows
`handshaking…` while it waits and defaults `--handshake-timeout` to 45 s;
lowering it will make idle clusters look dead.

## Layout

| Crate | Contents |
|---|---|
| `crates/icecc-proto` | the scheduler monitor protocol: framing, handshake, message decoders, discovery. No UI. |
| `crates/icecc-model` | cluster state and job accounting |
| `crates/icecc-top` | the TUI |

`icecc-proto` is deliberately independent of the rest and is the piece most
likely to be useful to other tools.

## Testing

```sh
cargo test
```

Protocol tests run against bytes captured from a real scheduler
(`contrib/capture/`), so they need no cluster — see
[contrib/capture/README.md](contrib/capture/README.md).

## Licence

GPL-2.0-or-later. The protocol and the job-accounting model are derived from
Icecream, `icemon` and `icecream-sundae`, all GPL-2.0-or-later.
