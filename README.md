# icecc-top

A terminal monitor for [Icecream](https://github.com/icecc/icecream) (`icecc`)
distributed compile clusters — the cluster equivalent of `btop`, where each
build node reads like a process.

**Status: Phase 3.** It attaches to a scheduler, shows the cluster's nodes and
jobs, and — where `icecc-top-agent` is installed — real CPU, memory, load and
temperature per node. Bars, graphs, sorting and a detail view are Phase 4–5. See
[ARCHITECTURE.md](ARCHITECTURE.md) for the design and the roadmap.

```
icecc-top  build-master:8765  proto 43   nodes 8   slots 47/64 (73%)   agents 7/8
┌ nodes ─────────────────────────────────────────────────────────────────────────────┐
│NODE                    CPU    MEM    LOAD   JOBS      SPEED   TEMP   IP            │
│build01                 82%    71%    14.2   8/8       3200    71°    10.0.0.11     │
│build02                 91%    88%    15.8   7/8       2900    78°    10.0.0.12     │
│build03                 23%    31%    3.1    2/8       3100    52°    10.0.0.13     │
│build04                 12%    —      9.4    6/8       940     —      10.0.0.14     │
│laptop [local only]     —      —      —      0/0       —       —      10.0.0.40     │
└────────────────────────────────────────────────────────────────────────────────────┘
[q] quit  [r] redraw   jobs 17 active · 3 pending · 1 local   done 1284 remote / 12 local (since connect)
```

## Build

Needs a Rust toolchain (1.75+). No `libicecc` and no C++ build dependencies —
the scheduler protocol is implemented natively.

```sh
cargo build --release
./target/release/icecc-top
```

For the agent, a static build is easiest to copy onto build nodes:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl -p icecc-agent
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
| `--agent-port PORT` | where node agents listen (default 9765) |
| `--poll-interval MS` | how often to poll agents (default 1000) |
| `--poll-timeout MS` | per-node deadline (default 750) |
| `--stale-after MS` | when to call metrics stale (default 5000) |
| `--no-agents` | scheduler data only; do not poll agents at all |

## Node metrics

CPU, memory, temperature, frequency, swap, uptime and network throughput are not
in the Icecream protocol at all, and what *is* there only updates when a node's
load shifts by 10 %. They come from `icecc-top-agent`, one small read-only
service per build node:

```sh
# on each build node
icecc-top-agent            # serves http://0.0.0.0:9765/metrics, samples once a second
curl -s localhost:9765/metrics | head -c 200
icecc-top-agent --once     # print one snapshot and exit
```

See [contrib/systemd/](contrib/systemd/) for a hardened unit file and deployment
notes. `icecc-top` finds agents by itself — it polls each node at the address the
scheduler reports, so there is nothing to configure per node.

Nodes without an agent still appear, with their resource cells reading `—`; the
header shows coverage as `agents 7/8`, so a half-finished rollout is visible
rather than looking like a cluster of idle machines. `--no-agents` turns polling
off entirely.

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
- **CPU**, **MEM** and **TEMP** need an agent on that node; `—` means nobody
  measured, which is not the same as 0. **LOAD** prefers the agent's 1 Hz
  reading and falls back to the scheduler's slower one.
- Node labels: `[offline]`, `[local only]` (the node refuses remote jobs),
  `[no reply]` (the scheduler has pinged it and is still waiting),
  `[stale]` (its agent answered before and has gone quiet), `[agent error]`
  (something answered on the agent port but was unusable), and
  `[wrong host?]` (the agent there calls itself something else, so this row's
  metrics may belong to another machine).
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
| `crates/icecc-metrics` | the node metrics wire format, plus a minimal client |
| `crates/icecc-model` | cluster state, job accounting, agent metrics |
| `crates/icecc-agent` | `icecc-top-agent`: `/proc` and `/sys` sampling and serving |
| `crates/icecc-top` | the TUI |

`icecc-proto` is deliberately independent of the rest and is the piece most
likely to be useful to other tools.

## Testing

```sh
cargo test
```

Protocol tests run against bytes captured from a real scheduler
(`contrib/capture/`), so they need no cluster — see
[contrib/capture/README.md](contrib/capture/README.md). The `/proc` and `/sys`
parsers are tested against captured file contents and the sampler against
fixture directories, so neither depends on the machine running the tests.

## Licence

GPL-2.0-or-later. The protocol and the job-accounting model are derived from
Icecream, `icemon` and `icecream-sundae`, all GPL-2.0-or-later.
