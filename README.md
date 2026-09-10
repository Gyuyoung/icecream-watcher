# icecc-top

A terminal monitor for [Icecream](https://github.com/icecc/icecream) (`icecc`)
distributed compile clusters — the cluster equivalent of `btop`, where each
build node reads like a process.

**Status: Phase 5.** Cluster band, bars, history graphs, sorting, keyboard
navigation and a per-node detail view. See [ARCHITECTURE.md](ARCHITECTURE.md)
for the design and the roadmap.

```
icecc-top  build-master:8765  proto 43  up 04:12:07   sort name   [?] help
┌ CLUSTER ───────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│SLOTS  38/88     █████████████████▎░░░░░░░░░░░░░░░░░░░░░░  43%  7 online · 1 down · 1 no agent                      │
│QUEUE  7 wait    ▁▂▂▃▅▇█▇▅▃▂▁▂▃▄▅▆▇█▇▆▅▄▃▂▁▂▃▄▅▆▇▆▅▄▃▂▁▂▃ ↑ rising    peak 12  38 remote · 0 local                  │
│RATE   38/s      ▃▄▅▆▇█▇▆▅▄▃▄▅▆▇█▇▆▅▄▃▂▃▄▅▆▇█▇▆▅▄▃▄▅▆▇█▇▆  peak 52/s  12843 done since connect                      │
└────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
┌ 8 nodes ───────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│NODE                        CPU               MEM            SLOTS      LOAD  SPEED  TEMP                           │
│build01              ███████████▌  96%! ███████▎░░  72%  ███████▌ 15/16 14.2   3200   78°                           │
│build02              ███████▍░░░░  61%  █████████▌  96%! █████░░░ 10/16 12.8   2900   71°                           │
│build03              █████▊░░░░░░  48%  ████▏░░░░░  41%  ████░░░░  8/16  7.1   3100   63°                           │
│build04              ██▋░░░░░░░░░  22%  ██▋░░░░░░░  26%  ██░░░░░░  4/16  3.4    940!  55°                           │
│build05              ▌░░░░░░░░░░░   4%  █▊░░░░░░░░  18%  ░░░░░░░░  0/8   0.4   3050   41°                           │
│build07              ············    —  ··········    —  ░░░░░░░░  0/16    —   3000     —                           │
│laptop local         █▌░░░░░░░░░░  12%  ██████▎░░░  62%  ▋░░░░░░░  1/12  1.9      —   52°                           │
│build06 down         ············    —  ··········    —               —    —      —     —                           │
└────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
q quit  ↑↓/jk select  s sort  c m l i by cpu/mem/load/jobs  ? help   selected build02
```

Reading it: `build01` is CPU-bound and `build02` memory-bound — the `!` says
which, without comparing numbers. `build04` is a slow outlier. `build07` has no
agent, so its resource cells claim nothing. `build06` has dropped out and has
sunk to the bottom. The queue is growing, and the band says so in a word.

At a wide terminal each row also carries a two-minute CPU sparkline; narrower
terminals drop whole columns rather than squeezing every bar into uselessness.

`Enter` opens the node the overview points at — everything the main screen
deliberately leaves out:

```
┌ build02  10.0.0.2  x86_64  proto 43 ───────────────────────────────────────────────────────────────────────────────┐
│  healthy                                                                                                           │
│                                                                                                                    │
│CPU                                                                                                                 │
│  ████████████▎░░░░░░░  61%   8 cores  @ 3100 MHz avg                                                               │
│                                                                                                                    │
│  C0  ██████░░  75%  C1  ████████ 100%  C2  ██████▍░  80%  C3  ██████▍░  80%  C4  █████▉░░  73%  C5  ██████▎░  78%  │
│  C6  ██████▏░  76%  C7  ██████▏░  77%                                                                              │
│                                                                                                                    │
│    load average      14.20  12.00  9.00                                                                            │
│    per core          1.77   (8 runnable of 500 processes)                                                          │
│                                                                                                                    │
│MEMORY                                                                                                              │
│  ███████████████████▏  96%   30.1 GiB used of 31.3 GiB                                                             │
│    available         1.2 GiB   (free 0.4 GiB, buffers 0.1 GiB, cached 0.7 GiB)                                     │
│    swap              0 KiB of 8.0 GiB   (0%)                                                                       │
│                                                                                                                    │
│ICECREAM                                                                                                            │
│    compile slots     10 of 16 in use                                                                               │
│    queued from here  0                                                                                             │
│    speed             2900 output bytes per user-second                                                             │
│    jobs in           10 compiled here for others                                                                   │
│    jobs out          0 submitted and compiled elsewhere                                                            │
└────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
Esc back  ↑↓/jk scroll  q quit   build02
```

Below the fold it continues with network throughput per interface, uptime, every
thermal sensor the node exposes, and which agent answered. Anything wrong with
the node — offline, no agent, a hostname that does not match — is stated at the
top, before the numbers it would explain.
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
| `q`, `Ctrl-C` | quit |
| `Esc` | close the help overlay, leave the detail view, or quit |
| `↑` / `k`, `↓` / `j` | move the selection, or scroll the detail view |
| `PgUp` / `PgDn` | move or scroll ten rows |
| `s` | cycle the sort key |
| `c`, `m`, `l`, `i` | sort by CPU, memory, load, Icecream jobs |
| `r` | redraw |
| `?` | help |
| `Enter` | open or close the node detail view |

Metric sorts put the busiest node first, since the reason to sort by CPU is to
see what is hot. Nodes with no measurement sort last rather than being flipped
to the top, and offline nodes always sink below live ones. The selection follows
its node through a re-sort.

## Reading the screen

- `—` and `····` mean **not measured**, never zero. An idle node reads `0%`; a
  node with no agent reads `—`. The two must not look the same.
- `!` marks the metric that makes a node a bottleneck — memory pressure first,
  then CPU saturation, then every slot taken on a machine that is otherwise
  idle — or, on SPEED, a node well below the cluster median.
- **Dim rows are idle.** Offline rows strike through and sink to the bottom.
- **SLOTS** is remote compiles running / slots offered. Local compiles occupy no
  scheduler slot and are counted separately.
- **SPEED** is `output bytes / user-second`, and is **unknown until a node has
  actually compiled something** — a fresh cluster shows `—` everywhere. It is
  not zero-because-slow.
- **LOAD** prefers the agent's 1 Hz reading and falls back to the scheduler's,
  which only updates when load shifts by 10 %. It is coloured by load *per
  core*, so a 4-core and a 64-core node can be compared.
- The **QUEUE** and **RATE** graphs are scaled to their own peak — a queue has
  no natural maximum — and the peak is printed next to them so a flat graph at
  full height cannot be mistaken for a queue at some limit.
- Badges after a node name: `down`, `local` (refuses remote jobs), `no ack` (the
  scheduler has pinged it and is still waiting), `stale` (its agent answered
  before and has gone quiet), `agent?` (something answered on the agent port but
  was unusable), `host?` (the agent there calls itself something else, so this
  row's metrics may belong to another machine).
- Cumulative counters are **since connect**. Job ids only mean anything within
  one scheduler session, so a reconnect necessarily resets them.

The scheduler's own `Load` field — a composite 0–1000 scheduling weight,
`max(1000 - idle, memory pressure)`, forced to 1000 when a node is low on disk —
is deliberately **not** shown as a bar. It is not CPU utilisation, and drawing it
as one would be a confidently wrong picture.

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

The screenshot above is real output from the renderer, not a mock-up; regenerate
it with `cargo test -p icecc-top screenshot -- --ignored --nocapture`.

Protocol tests run against bytes captured from a real scheduler
(`contrib/capture/`), so they need no cluster — see
[contrib/capture/README.md](contrib/capture/README.md). The `/proc` and `/sys`
parsers are tested against captured file contents and the sampler against
fixture directories, so neither depends on the machine running the tests.

## Licence

GPL-2.0-or-later. The protocol and the job-accounting model are derived from
Icecream, `icemon` and `icecream-sundae`, all GPL-2.0-or-later.
