# icecream-watcher

A terminal monitor for [Icecream](https://github.com/icecc/icecream) (`icecc`)
distributed compile clusters — the cluster equivalent of `btop`, where each
build node reads like a process.

**Status: Phase 6.** Cluster band, dot bars and graphs, sorting, keyboard
navigation, a per-node detail view, and failure handling that keeps the screen
honest when the cluster or the network misbehaves. See
[ARCHITECTURE.md](ARCHITECTURE.md) for the design and the roadmap.

```
icecream-watcher  build-master:8765  proto 43  up 00:00:00   sort name   [?] help
┌ CLUSTER ───────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│SLOTS  38/88      43%                     ⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀│
│       7 online · 1 down · 1 no agent     ⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶│
│QUEUE  7 wait    → steady                 ⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿│
│       peak 7 · 38 remote · 0 local       ⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿│
│RATE   0/s                                ⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀│
│       peak 1/s · 0 done since connect    ⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀│
└────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
┌ 8 nodes ───────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│NODE                                    CUR  MAX SLOTS                IN    OUT LOAD  SPEED  SLOTS 2min             │
│build01 cpu!                             15   16 ⣇⣇⣇⣇⣇⣇⣇⣇⣇⣇⣇⣇⣇⣇⣇⣀     15      8 14.2   3200  ⠀⠀⠀⠀⠀⠀⠀⠀⢸⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿  │
│build02 mem!                             10   16 ⣇⣇⣇⣇⣇⣇⣇⣇⣇⣇⣀⣀⣀⣀⣀⣀     10      0 14.2   2900  ⠀⠀⠀⠀⠀⠀⠀⠀⢰⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶⣶  │
│build03                                   8   16 ⣇⣇⣇⣇⣇⣇⣇⣇⣀⣀⣀⣀⣀⣀⣀⣀      8      0 14.2   3100  ⠀⠀⠀⠀⠀⠀⠀⠀⢠⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤⣤  │
│build04                                   4   16 ⣇⣇⣇⣇⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀      4      9 14.2    940! ⠀⠀⠀⠀⠀⠀⠀⠀⢀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀  │
│build05                                   0    8 ⣀⣀⣀⣀⣀⣀⣀⣀              0      0 14.2   3050  ⠀⠀⠀⠀⠀⠀⠀⠀⢀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀  │
│build07                                   0   16 ⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀      0     10    —   3000  ⠀⠀⠀⠀⠀⠀⠀⠀⢀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀  │
│laptop local                              1   12 ⣇⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀          1     11 14.2      —  ⠀⠀⠀⠀⠀⠀⠀⠀⢀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀⣀  │
│build06 down 0s                           —    —                       —      —    —      —                         │
│                                                                                                                    │
│                                                                                                                    │
│                                                                                                                    │
│                                                                                                                    │
│                                                                                                                    │
│                                                                                                                    │
│                                                                                                                    │
│                                                                                                                    │
│                                                                                                                    │
└────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
q quit  ↑↓/jk select  Enter detail  s sort  c m l i by cpu/mem/load/jobs  ? help   selected build02
```

Reading it: every column is an **Icecream** figure — compile slots, jobs
compiled here and submitted from here, the scheduler's load, and compile speed.
A machine's CPU, memory and temperature are its own business and live one
keypress away in the detail view; what belongs here is the conclusion, so
`build01` is badged `cpu!` and `build02` `mem!` — the answer to "why is this node
not taking more work" without the gauges behind it. `build04` is a slow outlier
and the `!` on its speed says so. `build07` has no agent, so only its load is
unknown; everything else came from the scheduler. `build06` has dropped out, says
how long it has been gone, and has sunk to the bottom.

`CUR` and `MAX` carry the slot counts as plain numbers, and the meter beside
them gives **one cell per slot**, so slots can be counted rather than estimated.
A busy slot is a filled dot-column with the baseline carrying on to its right —
a bar with a gap built in — so a run of them stays countable instead of merging
into one block, and each is coloured by the node that *submitted* the job, the
way `icecream-sundae` attributes work. A glance says "eight of these are
`build02`'s". Jobs already running when the monitor attached have no known
submitter and take the compiling node's own colour. A node with more slots than
the column has cells falls back to a proportional bar, and the figures carry the
count.

**Each node is drawn in its own colour**, keyed by hostname so it follows the
machine through a re-sort, through other nodes coming and going, and between
sessions. The palette is blues, greens, cyans and purples only: red, orange and
yellow mean *state* here — a problem badge, a saturated metric, a hot sensor — and
a healthy node that happened to hash into that range would read as a node in
trouble. State still wins over identity, so an offline row is grey whatever
colour it would otherwise have had.

The history strip gets one character row, so four levels rather than the eight a
block sparkline gives, but twice the horizontal resolution — for "has this node
been busy, and is it busier now than a minute ago" that is the better trade, and
the exact figure is one column to the left. Narrower terminals drop whole columns rather than
squeezing every one into uselessness; spare width goes to hostnames first,
because an elided name costs more than a shorter graph.

`Enter` opens the node the overview points at — everything the main screen
deliberately leaves out:

```
icecream-watcher  build-master:8765  proto 43  up 00:00:00   sort name   [?] help
┌ build02  10.0.0.2  ?  proto — ─────────────────────────────────────────────────────────────────────────────────────┐
│  healthy                                                                                                           │
│                                                                                                                    │
│CPU                                                                                                                 │
│  ████████████▎░░░░░░░  61%   8 cores                                                                               │
│                                                                                                                    │
│  C0  ████▉░░░  61%  C1  ████▉░░░  61%  C2  ████▉░░░  61%  C3  ████▉░░░  61%  C4  ████▉░░░  61%  C5  ████▉░░░  61%  │
│  C6  ████▉░░░  61%  C7  ████▉░░░  61%                                                                              │
│                                                                                                                    │
│    load average      14.20  12.00  9.00                                                                            │
│    per core          1.77   (8 runnable of 500 processes)                                                          │
│                                                                                                                    │
│MEMORY                                                                                                              │
│  ███████████████████▏  96%   955 KiB used of 1000 KiB                                                              │
│    available         45 KiB   (free 45 KiB, buffers 0 KiB, cached 0 KiB)                                           │
│    swap              none configured                                                                               │
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
./target/release/icecream-watcher
```

For the agent, a static build is easiest to copy onto build nodes:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl -p icecream-watcher-agent
```

## Use

```sh
icecream-watcher                                   # discover via UDP broadcast
icecream-watcher --scheduler build-master:8765     # connect directly
icecream-watcher --netname MYFARM                  # discover on a named network
ICECC_SCHEDULER=build-master:8765 icecream-watcher # same as --scheduler
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
| `--job-timeout SECS` | forget a job whose completion never arrives (default 1800; 0 disables) |
| `--forget-offline SECS` | drop a node after this long offline (default 0, meaning never) |
| `--reconnect-max-delay SECS` | ceiling on the reconnect backoff (default 30) |

## Node metrics

CPU, memory, temperature, frequency, swap, uptime and network throughput are not
in the Icecream protocol at all, and what *is* there only updates when a node's
load shifts by 10 %. They come from `icecream-watcher-agent`, one small read-only
service per build node:

```sh
# on each build node
icecream-watcher-agent            # serves http://0.0.0.0:9765/metrics, samples once a second
curl -s localhost:9765/metrics | head -c 200
icecream-watcher-agent --once     # print one snapshot and exit
```

See [contrib/systemd/](contrib/systemd/) for a hardened unit file and deployment
notes. `icecream-watcher` finds agents by itself — it polls each node at the address the
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
| `r` | redraw; while disconnected, retry the connection now |
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
daemons are connected the handshake is immediate. `icecream-watcher` shows
`handshaking…` while it waits and defaults `--handshake-timeout` to 45 s;
lowering it will make idle clusters look dead.

## When things go wrong

The screen is meant to stay honest, which mostly means never showing a
confident number it has not checked.

**A severed link.** Scheduler stats are change-driven, so an idle cluster and a
pulled cable both send nothing. TCP keepalive settles it: a vanished scheduler
becomes a visible `disconnected` in about 35 seconds instead of leaving a
plausible, frozen cluster on screen. Silence that is merely silence is labelled
`quiet 5m` in the header once it passes a minute.

**A scheduler that is gone.** Reconnection backs off 1, 2, 4 … up to
`--reconnect-max-delay`, so a monitor left running overnight does not spend the
night broadcasting. A session that reached login resets the backoff, so a
*restarted* scheduler is picked up within a second or two; only a genuinely
absent one is backed off. The header shows the countdown and the attempt number,
and `r` cuts the wait short.

**A different scheduler.** If discovery lands somewhere new, the header says
`⇄ moved from <old>`: every host id, node and counter now belongs to another
cluster, which is not something to discover by noticing the numbers changed.

**A node that leaves.** Its row stays, marked `down 12m`, because which node
just died is usually the thing you opened the monitor for. `--forget-offline`
drops them for sessions that run for days.

**A job that never finishes.** The protocol does not guarantee that every job
you are told about is one you are told the end of, and a stuck job would inflate
the queue depth for the rest of the session. Jobs expire after
`--job-timeout` — generously, because expiring early understates the queue just
as badly — and the count is logged rather than hidden.

## Layout

| Crate | Contents |
|---|---|
| `crates/icecc-proto` | the scheduler monitor protocol: framing, handshake, message decoders, discovery. No UI. |
| `crates/icecc-metrics` | the node metrics wire format, plus a minimal client |
| `crates/icecc-model` | cluster state, job accounting, agent metrics |
| `crates/icecream-watcher-agent` | `icecream-watcher-agent`: `/proc` and `/sys` sampling and serving |
| `crates/icecream-watcher` | the TUI |

`icecc-proto` is deliberately independent of the rest and is the piece most
likely to be useful to other tools.

## Testing

```sh
cargo test
```

The screenshot above is real output from the renderer, not a mock-up; regenerate
it with `cargo test -p icecream-watcher screenshot -- --ignored --nocapture`.

Protocol tests run against bytes captured from a real scheduler
(`contrib/capture/`), so they need no cluster — see
[contrib/capture/README.md](contrib/capture/README.md). The `/proc` and `/sys`
parsers are tested against captured file contents and the sampler against
fixture directories, so neither depends on the machine running the tests.

## Licence

GPL-2.0-or-later. The protocol and the job-accounting model are derived from
Icecream, `icemon` and `icecream-sundae`, all GPL-2.0-or-later.
