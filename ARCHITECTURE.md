# icecc-top — Architecture (Phase 1: Research)

Status: **Phase 1 research complete; Phases 2 and 3 implemented** (see §8).
Decisions approved 2026-09-10: **Rust + ratatui/crossterm/tokio**, **Tier 0+1** collection (§4, §5).
Target: a btop-class TUI for monitoring an Icecream (icecc) distributed compile cluster.

Everything in this document was derived from reading upstream source and, where marked
**[verified]**, confirmed empirically against a live `icecc-scheduler` 1.4 (protocol 43) with a
real `iceccd` attached, using a raw-socket probe.

Sources analysed:

| Project | Revision | License |
|---|---|---|
| `icecc/icecream` | `37cb407` (2026-03-04), protocol 44 in tree | GPL-2.0-or-later |
| `icecc/icemon` | current master | GPL-2.0-or-later |
| `JPEWdev/icecream-sundae` | v1.1.0 | GPL-2.0-or-later |

Local reference installs used for cross-checking: icecc 1.4, icemon 3.3, icecream-sundae 1.0.0.

---

## 1. How existing monitors get their data

Both `icemon` and `icecream-sundae` use **exactly one** data source: they log into the scheduler
as a *monitor* and then passively consume an event stream. Neither talks to `iceccd` directly.
Neither collects any host resource data.

```
icemon / icecream-sundae
        |
        |  TCP :8765  (MON_LOGIN, then read-only event stream)
        v
   icecc-scheduler  <----  iceccd (LOGIN + STATS)  ----  compile nodes
```

### 1.1 Connection and discovery

`services/comm.h` exposes `DiscoverSched`, which both monitors use:

```cpp
DiscoverSched(netname = "", timeout = 2, schedname = "", port = 0);
MsgChannel *try_get_scheduler();   // poll until non-NULL or timed_out()
```

- With an explicit `schedname`, it TCP-connects directly (`connect_fd()`).
- With no `schedname`, it UDP-broadcasts on port 8765 and waits for a scheduler to answer
  (`listen_fd()`), selecting the best responder by protocol version and start time.

`icemon` (`src/icecreammonitor.cc:129-160`) drives this from the Qt event loop and re-creates the
`DiscoverSched` object on every failure. `icecream-sundae` does the same from a GLib main loop.

**Limitation found in icecream-sundae:** `-s/--scheduler` takes a *hostname only*; the port is
hardcoded to 8765, so it cannot attach to a scheduler on a non-default port. **[verified]** —
`icecream-sundae -s localhost` against a scheduler on :18765 reports `Cannot get scheduler`.
`icecc-top` will accept `host:port`.

### 1.2 The monitor handshake

After MON_LOGIN, **the scheduler never reads from the monitor again** — `handle_mon_login()` does
`fd2cs.erase(cs->fd)` with the comment *"no expected data from them"*. Monitors are not pinged and
are not subject to `prune_servers()` timeouts; liveness relies on TCP keepalive only.
**[verified]** — a probe that sent nothing for 40 s stayed connected and kept receiving events.

On login the scheduler immediately replays one `MON_STATS` per known node
(`handle_mon_login` loops over `css` calling `handle_monitor_stats(*it)`), which is how a monitor
learns the current cluster topology. **Note this initial replay carries no live resource fields**
(see §2.3).

### 1.3 The event stream

`scheduler.cpp:notify_monitors()` broadcasts these to every monitor:

| Message | Emitted when | Payload |
|---|---|---|
| `MON_STATS` (87) | node stats change, node login, node offline | `u32 hostid`, `string statmsg` |
| `MON_GET_CS` (83) | a client requests a node (job becomes *pending*) | `string filename`, `u32 lang`, `u32 job_id`, `u32 clientid` |
| `MON_JOB_BEGIN` (84) | remote job starts (job becomes *active*) | `u32 job_id`, `u32 stime`, `u32 hostid` |
| `MON_JOB_DONE` (85) | remote job finishes | full `JobDoneMsg` (see below) |
| `MON_LOCAL_JOB_BEGIN` (86) | local (non-distributed) job starts | `u32 job_id`, `u32 stime`, `u32 hostid`, `string file` |
| `JOB_LOCAL_DONE` (79) | local job finishes | `u32 job_id` |

`MON_JOB_DONE` carries the only per-job cost data in the protocol:
`job_id, exitcode, real_msec, user_msec, sys_msec, pfaults, in_compressed, in_uncompressed,
out_compressed, out_uncompressed, flags`, plus `client_count` on protocol ≥ 39.

### 1.4 The data model both monitors build

Job state is **derived by the monitor**, not sent by the scheduler. `icecream-sundae`
(`src/main.hpp`, `src/main.cpp:125-178`) keeps five maps and this is the model worth reusing:

```
MON_GET_CS            -> pendingJobs[id]                (job queued, no node yet)
MON_JOB_BEGIN         -> activeJobs[id], remoteJobs[id] ; host.total_in++, client.total_out++
MON_LOCAL_JOB_BEGIN   -> localJobs[id]                  ; host.total_local++
MON_JOB_DONE          -> remove(id)
JOB_LOCAL_DONE        -> remove(id)
```

So *Pending / Active / IN / OUT / LOCAL* — the columns icemon and sundae display — are all
monitor-side accumulators over this stream. `icecc-top` must reimplement the same accounting;
there is no query API to ask the scheduler for them.

**Consequence:** these counters are only correct for the lifetime of the connection. A monitor
that reconnects starts from zero and cannot recover history. Cumulative totals must be labelled
"since connect", not "since boot".

---

## 2. The scheduler↔monitor protocol (authoritative, verified on the wire)

Do not guess any of this; it was read from `services/comm.cpp` and confirmed against a live
scheduler.

### 2.1 Framing

```
connect TCP to scheduler port (default 8765)
  ->  send  4 bytes:  PROTOCOL_VERSION, 0, 0, 0        (raw little-endian, NOT htonl)
  <-  recv  4 bytes:  peer version (little-endian)
  ->  send  4 bytes:  min(mine, peer)                  (little-endian)
  <-  recv  4 bytes:  confirmation, must equal agreed
then, repeating:
      u32 big-endian  length      (covers type + payload)
      u32 big-endian  type
      payload
```

Scalars inside the payload are **big-endian** (`htonl`/`ntohl`), while the version handshake is
**little-endian raw bytes**. Strings are `u32 length (including the NUL) + bytes`.

**[verified]** live handshake output: `remote_protocol=43 agreed=43 confirm=43`, then a well-formed
`MON_STATS`.

`MON_LOGIN` is the empty message `[len=4][type=82]`. Message type values are the ASCII-ordered enum
in `comm.h` starting at `UNKNOWN='A'` (65); the ones we need are `END=67`, `JOB_LOCAL_DONE=79`,
`MON_LOGIN=82`, `MON_GET_CS=83`, `MON_JOB_BEGIN=84`, `MON_JOB_DONE=85`,
`MON_LOCAL_JOB_BEGIN=86`, `MON_STATS=87`.

### 2.2 `MON_STATS.statmsg` — the node record

A newline-separated `Key:Value` blob built by `scheduler.cpp:handle_monitor_stats()`:

```
Name:build01            node name
IP:10.0.0.11            node address as the scheduler sees it
MaxJobs:16              compile slots
NoRemote:false          "true" => will not accept remote jobs
Platform:x86_64         host platform
Version:43              max remote protocol version
Features:env_xz env_zstd
Speed:1234.5            server_speed() = output bytes / user-seconds; 0 until the node compiles
Load:426                composite 0..1000 scheduling weight (see below)
LoadAvg1:2197           loadavg * 1000   } only present on stats updates,
LoadAvg5:1801           loadavg * 1000   } never in the login replay
LoadAvg10:1503          loadavg * 1000   }
FreeMem:36732           MiB available    }
```

A node going away is announced as a `MON_STATS` containing only `State:Offline`.

**[verified]** live capture, idle node:
`Name:gyuyoung-ThinkPad-P1 / IP:127.0.0.1 / MaxJobs:4 / NoRemote:true / Platform:x86_64 /
Version:43 / Features:env_xz env_zstd / Speed:0.000000 / Load:426` — and no `LoadAvg*`/`FreeMem`.

**[verified]** unit checks: with 8 spinner processes running, one update carried `Load=839,
LoadAvg1=2197, FreeMem=36732`; `free -m` on the same box reported `available = 36566` MiB.
So `FreeMem` is **MiB of *available* memory** (MemFree+Buffers+Cached, per `calculateMemLoad()`),
and `LoadAvg*` is loadavg×1000.

Two traps in this record:

- **`Load` is not CPU utilisation.** `daemon/main.cpp:maybe_stats()` computes
  `load = max(1000 - idle_average, memory_fillgrade)`, and forces `1000` when the build directory
  is low on disk. It is a *scheduling weight* that folds CPU, memory pressure and disk together.
  Rendering it as a CPU bar would be wrong.
- **`Speed` is 0 until the node has actually compiled something** — `server_speed()` returns 0
  while `lastCompiledJobs()` is empty. A fresh cluster shows every node at speed 0.

**There is no `MemTotal` anywhere in the protocol**, so memory *percentage* cannot be computed from
scheduler data at all. Nor are per-core CPU, temperature, frequency, swap, uptime, network or disk
present in any message.

### 2.3 Update cadence — the finding that drives the whole design

`iceccd` does **not** push stats on a timer. `daemon/main.cpp:902`:

```cpp
if (abs(int(msg.load) - current_load) >= 100
    || (msg.load == 1000 && current_load != 1000)
    || (msg.load != 1000 && current_load == 1000)) {
    send_scheduler(msg);
}
```

Stats are sent **only when the composite load moves by ≥ 10 %** (or crosses saturation). The
scheduler forwards each one to monitors immediately (`handle_stats -> handle_monitor_stats(c, m)`).

**[verified]**, single node, 45 s observation window:

| t | event |
|---|---|
| +0.2 s | login replay (no LoadAvg / FreeMem) |
| — | **40 s idle: zero updates** |
| +8.7 s | CPU load applied → `Load 421→839`, full field set appears |
| +29.7 s | load released → `Load 427` |

**A 1 Hz refresh of node resources from the scheduler is impossible.** An idle cluster emits
nothing for minutes. Any btop-like display fed only by the scheduler would show frozen bars. This
is the single most important constraint on the architecture, and it is why §4 concludes a node-side
collector is mandatory rather than optional.

### 2.4 Scheduler accept latency

`icecc-scheduler` sets `next_listen = now + 1` after each accept round and then blocks in `poll()`
for up to `prune_servers()` = `MAX_SCHEDULER_PING` = **36 s**. With no daemons connected there is
nothing to wake the loop, so a monitor's TCP connect completes (kernel backlog) but the handshake
stalls.

**[verified]** on an idle scheduler, four consecutive monitor connections each completed their
handshake in **34.0–34.4 s**; with a daemon attached (whose stats wake the loop) the same handshake
took **0.0 s**.

Design consequences: the connect/handshake timeout must be ≥ 40 s, the UI must show a
`connecting…` state rather than failing fast, and this must not be mistaken for a hung scheduler.

### 2.5 Secondary interface (considered, not adopted)

The scheduler opens a plain-text command port on `scheduler_port + 1` (default **8766**) with
`listcs`, `listjobs`, `listblocks`, `internals`, `blockcs`, `removecs`, `help`, `quit`.
`listcs` gives `nodename (ip:port) [platform] speed=… jobs=cur/max load=…` plus a job dump, and
`internals` returns each daemon's `dump_internals()` (current/max kids, cache size, cpu idle/nice,
loadavg, memory fillgrade, free memory).

**Rejected as a polling source**, for two reasons: the output is an unstable debug format with no
compatibility guarantee, and `internals` is implemented as a **blocking** `it->get_msg()` inside the
scheduler's main loop (`scheduler.cpp:1704-1734`) — polling it would stall scheduling for the whole
cluster whenever one daemon is slow to answer. It stays useful for `icecc-top --debug-dump` and for
one-shot diagnostics only.

### 2.6 Reimplement or link `libicecc`?

For a *monitor*, the required decoders are: `MON_STATS` (u32 + string), `MON_JOB_BEGIN` (3×u32),
`MON_LOCAL_JOB_BEGIN` (3×u32 + string), `JOB_LOCAL_DONE` (u32), `MON_JOB_DONE` (11×u32 + optional
u32), and `MON_GET_CS` — which on protocol ≥ 29 is just `string, u32, u32, u32`. The gnarly
version-dependent `GetCSMsg` path (environment lists, feature flags per protocol 22/31/34/39/42/43)
is only reached below protocol 29, which predates icecream 0.9.x.

That is roughly **200 lines**. No compression, no environment transfer, no job submission.

Against linking `libicecc`: it is C++ with no stable API or ABI, distros ship it effectively as a
static archive (Ubuntu's `icecream-sundae` has no `libicecc.so` in `ldd` — it statically links and
pulls in lzo2/zstd/cap-ng), `libicecc-dev` is a separate package that is often absent, its
`MsgChannel` imposes its own blocking/poll model, and it is GPL-2.0-or-later.

**Decision: implement the monitor protocol natively**, pinning `PROTOCOL_VERSION` we advertise and
negotiating down. Refuse to decode `MON_GET_CS` below protocol 29 (log and continue) rather than
carrying the legacy path.

**Licensing:** we derive the protocol and the job-accounting model from GPL-2.0-or-later sources.
`icecc-top` should therefore be **GPL-2.0-or-later**.

---

## 3. Where each piece of information must come from

| Field | Source | Notes |
|---|---|---|
| node id / name / IP | scheduler `MON_STATS` | authoritative |
| max compile slots | scheduler `MaxJobs` | |
| current / active / pending jobs | derived from job event stream | monitor-side accounting |
| jobs IN / OUT / LOCAL | derived, since connect | not recoverable after reconnect |
| compile speed | scheduler `Speed` | 0 until node compiles |
| remote-job availability | scheduler `NoRemote` | |
| platform, features, protocol | scheduler | |
| node online/offline | scheduler `State:Offline` + connection loss | |
| load average | scheduler (coarse) **or** agent (1 Hz) | scheduler value is event-driven |
| free memory | scheduler `FreeMem` (coarse) **or** agent | MiB available; **no MemTotal** |
| **CPU utilisation, per-core** | **agent only** | `/proc/stat`, delta between samples |
| **memory total / used / %** | **agent only** | `/proc/meminfo` |
| **swap** | **agent only** | `/proc/meminfo` |
| **CPU frequency** | **agent only** | `/proc/cpuinfo`, `/sys/devices/system/cpu/*/cpufreq` |
| **CPU temperature** | **agent only** | `/sys/class/hwmon/*`, `/sys/class/thermal/thermal_zone*/temp` |
| **uptime** | **agent only** | `/proc/uptime` |
| **network RX/TX** | **agent only** | `/proc/net/dev`, delta between samples |
| **disk I/O** (optional) | **agent only** | `/proc/diskstats` |

Nine of the fields required for the target UX exist in no Icecream message at all.

---

## 4. Node resource collection: Option A vs Option B

### The premise of Option A does not hold

Option A ("the monitor connects directly to each node, without SSH") requires *something* on the
node that serves resource data. There is nothing:

- `iceccd` accepts `GET_INTERNALS` **only on its scheduler channel** (`daemon/main.cpp:2055-2083`
  handles it inside the scheduler-fd branch) and sends the reply **to the scheduler**, not to the
  requester. A third party connecting to the daemon port cannot ask for it.
- Even if it could, `dump_internals()` has no per-core CPU, temperature, swap, or network data.
- The one path that does reach daemon internals is the scheduler's blocking `internals` text
  command (§2.5), which is unsafe to poll.

So Option A, as stated, collapses into Option B: a small agent has to exist. The real design
question is only **how the monitor reaches it**.

### Decision (approved): Option B, pull-based, Tier 0+1, with graceful degradation

Three tiers, so the tool is useful before any agent is deployed:

| Tier | Source | Gives | Deployment |
|---|---|---|---|
| **0 — always on** | scheduler MON stream | topology, jobs, slots, speed, coarse load/free-mem | none |
| **1 — recommended** | `icecc-top-agent` on each node | full 1 Hz `/proc` + `/sys` metrics | one static binary + socket unit |
| **2 — deferred** | existing Prometheus `node_exporter` | most Tier-1 metrics | none, where already deployed |

Tier 2 is **out of scope for now** and revisited only if a target cluster already runs
`node_exporter`. Nodes without an agent still appear, with resource cells rendered as `—` and a `no-agent` badge.
This matters: it means `icecc-top` is never worse than `icemon`, and gets better as agents roll out.

**Pull over push**, because: no per-node configuration (the agent never needs to know where the
monitor is), the monitor owns its own timeouts so one hung node cannot stall anything, adding a
monitor requires no node changes, and a node that stops answering is trivially marked stale rather
than silently missing.

Agent design constraints:
- Single static binary, no runtime dependencies, ~1 MiB, idle CPU well under 0.1 %.
- Read-only. Serves a fixed metrics snapshot; no shell, no arbitrary paths, no writes.
- Samples `/proc` on its own 1 Hz timer and serves the last snapshot, so an HTTP request never
  costs a `/proc` sweep and N monitors cost the same as one.
- Bind to a configurable address, default loopback-off; document that it should be firewalled to
  the build network. It exposes hostname, load, memory and thermal data — not secrets, but not for
  the open internet.
- Node identity is matched to the scheduler's `MON_STATS` record by IP first, then by hostname.

### Architecture

```
                        ┌──────────────────────────────────────────┐
                        │              icecc-top (TUI)             │
                        │                                          │
                        │   render thread ── 60 ms frame budget    │
                        │        ▲            reads snapshot       │
                        │        │                                 │
                        │   ┌────┴─────────────────────────────┐   │
                        │   │  ClusterState (single writer,    │   │
                        │   │  RwLock snapshot + ring buffers) │   │
                        │   └────▲──────────────────▲──────────┘   │
                        │        │                  │              │
                        │   scheduler task     collector pool      │
                        └────────┼──────────────────┼──────────────┘
                                 │                  │
                    MON_LOGIN +  │                  │  parallel pull, per-node
                    event stream │                  │  timeout, independent failure
                                 │                  │
                    ┌────────────▼─────┐   ┌────────▼────────┬─────────────┐
                    │ icecc-scheduler  │   │ agent @ build01 │ agent @ ... │
                    │      :8765       │   │     :8766+      │             │
                    └────────▲─────────┘   └────────▲────────┴─────────────┘
                             │                      │
                             │ LOGIN/STATS          │ /proc /sys
                       ┌─────┴──────┐               │
                       │  iceccd    ├───────────────┘
                       │ build01…N  │   (same host)
                       └────────────┘
```

Three independent failure domains: the scheduler connection, each agent, and the renderer. None
blocks another. The UI thread performs **no** network I/O and never blocks on a lock held during
I/O — collectors write into the shared state, the renderer takes a cheap snapshot per frame.

---

## 5. Technology choice

| Criterion | Rust + ratatui | C++ + FTXUI | Go + bubbletea |
|---|---|---|---|
| Icecream integration | native protocol, ~200 lines (§2.6) | could link libicecc, but we established that is a liability | native protocol |
| Protocol access | full control, version-negotiated | same once libicecc is dropped | same |
| Resource collection | `procfs`/`sysinfo` crates or direct parsing | manual | `gopsutil` |
| TUI rendering | sparklines, gauges, charts, flex layout built in | good, fewer dashboard widgets | weakest for dense dashboards |
| Concurrency | tokio: 100+ nodes, per-task timeouts, cancellation | manual threads/epoll | goroutines, easy |
| Agent deployment | **static musl binary, zero deps** | needs matching libstdc++ | static binary, ~3× larger |
| Idle CPU | no GC, predictable | predictable | GC wakeups — matters for a build-cluster tool |
| Maintainability | strong types over a binary protocol; single build tool | most familiar here | simple |

**Decision (approved): Rust, with `ratatui` + `crossterm`, and `tokio` for the network side.**

The deciding factors are the agent (a static musl binary is the difference between "copy one file"
and "manage a dependency on every node") and the concurrency model (100+ independently-timed pulls
plus an event stream, with a render loop that must never block).

Runner-up (not taken): **C++20 + FTXUI**, the right choice if avoiding a new toolchain mattered
more than the above.

**Prerequisite:** this machine has `g++ 13.3` but **no `rustc`/`cargo`** — Phase 2 is gated on
installing a Rust toolchain (`rustup`, plus the `x86_64-unknown-linux-musl` target for the agent).

Rejected: linking `libicecc` in any language (§2.6). Also note the `libicecc-dev` package is not
installed here, so an FTXUI build would need it or a vendored protocol layer anyway.

---

## 6. Repository structure

Present tense = exists today (Phase 2). Marked *(planned)* = later phases.

```
IceccTop/
├── ARCHITECTURE.md               this document; §2 is the protocol reference
├── README.md
├── LICENSE                       GPL-2.0-or-later (§2.6)
├── Cargo.toml                    workspace
├── crates/
│   ├── icecc-proto/              scheduler monitor protocol — no UI
│   │   ├── src/wire.rs           framing, handshake, endianness
│   │   ├── src/msg.rs            MON_* decoders, version gates
│   │   ├── src/stats.rs          statmsg Key:Value parser + unit conversion
│   │   ├── src/discover.rs       explicit target, env, UDP broadcast discovery
│   │   ├── src/conn.rs           connection task, reconnect, record/replay
│   │   └── tests/golden.rs       decodes bytes captured from a real scheduler
│   ├── icecc-metrics/            node metrics wire format + HTTP client
│   │   ├── src/lib.rs            Snapshot and friends; units in field names
│   │   └── src/client.rs         one GET, no HTTP stack pulled in
│   ├── icecc-model/              Cluster, Node, job accounting, agent metrics
│   │                             (history ring buffers: planned, Phase 4)
│   ├── icecc-agent/              icecc-top-agent
│   │   ├── src/parse.rs          pure /proc and /sys parsers
│   │   ├── src/sampler.rs        sampling, deltas, sensor selection
│   │   └── src/server.rs         single-endpoint HTTP/1.1
│   └── icecc-top/                the TUI binary
│       ├── src/main.rs           CLI, runtime wiring, terminal setup
│       ├── src/app.rs            state, key handling (sort modes: planned)
│       ├── src/collect.rs        parallel agent polling
│       └── src/ui.rs             rendering (splits into ui/ at Phase 4)
└── contrib/
    ├── capture/                  protocol captures + format docs
    │   ├── README.md
    │   └── lab-session.ictpcap   golden-test fixture from a live scheduler
    └── systemd/                  hardened agent unit + deployment notes
```

The planned `docs/protocol.md` was **dropped**: §2 of this document is already
the authoritative, wire-verified protocol reference, and maintaining a second
copy would only let the two drift apart. The capture file format — the one thing
§2 does not cover — is documented in `contrib/capture/README.md`.

`icecc-proto` and `icecc-agent` stay independently usable — the protocol crate is the piece most
likely to be valuable to other people, and keeping it UI-free keeps it honest.

---

## 7. UI design direction

The stated goal is that nine questions be answerable in 1–2 seconds (which nodes are busy / idle /
CPU-bound / memory-bound, slot occupancy, whether the queue is growing, whether one node is slow,
whether anything is unhealthy, whether the cluster is being used efficiently). That argues against
a uniform table where every metric gets equal weight.

Proposed overview, three bands:

```
 icecc-top   build-master:8765   proto 43   up 04:12:07                          1.0s  [?] help
╭─ CLUSTER ──────────────────────────────────────────────────────────────────────────────────╮
│  SLOTS  47/64  ███████████████████████████░░░░░░░  73%   nodes 8 online · 1 stale · 1 down │
│  QUEUE  pending  7  ▁▂▂▃▅▇█▇▅▃▂▁▂  rising        remote 42 · local 5 · done 12843          │
│  RATE   38 job/s ▃▄▅▆▇█▇▆▅▄▃▄▅▆▇   p50 1.2s  p95 6.8s                                      │
╰────────────────────────────────────────────────────────────────────────────────────────────╯
 NODE       CPU                    MEM             LOAD   SLOTS            SPEED   TEMP
 build01    ████████████████▏ 82%  ███████▏ 71%    14.2   ████████ 8/8     3200    71°
 build02    █████████████████ 91%  ████████▏ 88%!  15.8   ████████ 8/8     2900    78°
 build03    ████▏         23%      ███▏     31%     3.1   ██░░░░░░ 2/8     3100    52°
 build04    ██████████▏   58%      ██████▏  62%     9.4   ██████░░ 6/8      940!   61°
 build05    ·  no agent            ·               11.0   ███████░ 7/8     3050    ·
 build06    ── stale 42s ──────────────────────────────────────────────────────────────────
```

Hierarchy decisions:

- **Cluster band first.** Slot occupancy and queue trend answer questions 5, 6 and 9 without
  reading the table at all. The queue sparkline plus a `rising`/`draining`/`steady` word is what
  makes "is the scheduler queue growing?" a one-glance answer.
- **Four bars, not twelve columns.** CPU, MEM, SLOTS get proportional bars; LOAD, SPEED, TEMP are
  numbers. Everything else — per-core, swap, network, frequency, uptime, IN/OUT/LOCAL, platform,
  features, protocol — moves to the detail view.
- **Colour carries state, not decoration.** Bars ramp green→yellow→red on their own thresholds; a
  `!` marks the metric that makes a node a bottleneck, so "CPU-bound" vs "memory-bound" is
  distinguishable at a glance (questions 3 and 4).
- **Idle is visually quiet, unhealthy is loud.** Idle nodes render dim; stale nodes collapse to a
  single struck-through row; offline nodes drop to a footer line. Question 8 answers itself.
- **Outliers are marked, not left to be computed.** A node whose speed is far below the cluster
  median gets `!` on SPEED (question 7).
- **Sparklines on the cluster band, not per row** at 8 nodes; per-row history appears in the detail
  view and in a `[g]` graph mode. Per-row sparklines at 100 nodes are noise.

History: ring buffers of ~120 samples per node per metric (CPU, mem, slots) — 2 minutes at 1 Hz,
a few KiB per node, resampled for whatever width the terminal gives.

Scale: above ~40 nodes the table switches to a compact mode (one line per node, bars shortened,
cluster band unchanged); above ~100 it pages rather than scrolls, and only visible rows are
rendered.

---

## 8. Phase 2 — minimum first implementation — **done**

Scope, deliberately narrow, scheduler-only (no agent, no resource collection, no history):

1. `icecc-proto`: TCP connect, version handshake, `MON_LOGIN`, frame reader, decoders for
   `MON_STATS`, `MON_JOB_BEGIN`, `MON_JOB_DONE`, `MON_LOCAL_JOB_BEGIN`, `JOB_LOCAL_DONE`,
   `MON_GET_CS` (protocol ≥ 29), and the `statmsg` parser with unit conversion.
2. Discovery: `--scheduler host[:port]`, else `$ICECC_SCHEDULER`, else UDP broadcast with
   `--netname` / `$ICECC_NETNAME`. Handshake timeout ≥ 40 s with a `connecting…` state (§2.4).
3. `icecc-model`: node map keyed by hostid, job accounting per §1.4, `State:Offline` handling,
   reconnect that resets the derived counters and says so.
4. A static table: `NODE · IP · JOBS cur/max · LOAD · SPEED · PLATFORM`, plus a header line with
   scheduler address and node count. Redraw on event, capped at 10 fps. `q` quits, terminal resize
   works.
5. Capture/replay harness (`contrib/capture/`) so protocol tests do not need a live cluster, seeded
   with the frames already captured during this research.

Explicitly deferred: agent, CPU/mem/temperature, bars, graphs, sorting, detail view, colour.

Acceptance: attaches to a real scheduler, shows every node icemon shows, survives scheduler restart
and node disconnect, and idles at negligible CPU.

### What was verified, and how

| Claim | Evidence |
|---|---|
| framing and handshake are right | golden tests decode bytes captured from `icecc-scheduler` 1.4; live handshake negotiated protocol 43 |
| explicit-target connect works | `--scheduler localhost:18765` against an isolated lab scheduler |
| UDP broadcast discovery works | plain `icecc-top` found the real cluster; answers arrived from both loopback and the LAN interface and the netname filter rejected a wrong netname |
| partial `MON_STATS` merge is correct | login replay (no `LoadAvg`/`FreeMem`) followed by loaded updates (both present) merges into one complete node — asserted in `golden.rs` and in the model tests |
| reconnect is clean | killing the scheduler mid-session produced `Connection reset`, then repeated `Connection refused` retries, with no crash and no stuck UI |
| record/replay round-trips | a captured session replays to byte-identical events |
| the real terminal path works | ran in a pty, drew, and exited 0 on `q`; a panic hook restores the terminal |
| renders at any geometry | `TestBackend` snapshots at 10×3 through 200×60, and with 120 nodes |
| it does not disturb the machine | attached to the live cluster for 20 s: **0 s CPU time, 0.0 %, 4.7 MiB RSS** in TUI mode (4.1 MiB in `--dump`) |

Live check against the actual cluster on this machine (two nodes, Linux x86_64 +
macOS arm64) — discovery, login replay and live stats updates all behaved as §2
describes.

70+ tests, `cargo clippy --all-targets` clean.

---

## 8b. Phase 3 — node resource monitoring — **done**

`icecc-top-agent` samples `/proc` and `/sys` once a second and serves the last
snapshot over one `GET /metrics`; `icecc-top` polls every node at the address the
scheduler reported for it. Nothing is configured per node.

Implemented: CPU total and per-core utilisation, per-core frequency, memory
total/available/free/buffers/cached, swap, load average (with per-core
normalisation), CPU package temperature plus every other sensor, uptime, and
network throughput per interface. Disk I/O is still deferred — §3 listed it as
optional and nothing in the target UX needs it yet.

Decisions worth recording:

* **Units live in field names** (`total_kib`, `rx_bytes_per_sec`, `celsius`).
  The `FreeMem` trap in §9 is exactly what happens when they do not.
* **Unknown is never zero.** The first sample produces no snapshot at all,
  because rates need a delta and a snapshot of zeroes would render a busy node
  as idle; two samples inside the same millisecond are refused for the same
  reason. A node with no temperature sensor reports `None`, not 0 °C.
* **Sensor choice is explicit and attributed.** Hosts disagree between sensors —
  on the development machine `coretemp/Package id 0` read 87 °C, `thinkpad/CPU`
  89 °C, `acpitz` 88 °C and `x86_pkg_temp` 100 °C — so the agent picks by a
  documented preference order (`coretemp`/`k10temp`/`zenpower`/`cpu_thermal`
  package first, then the kernel package sensor, then ACPI) and reports *which*
  sensor it used. Readings of exactly 0 are dropped as "not fitted", the
  convention `thinkpad_acpi` uses, and sensors exposed through both hwmon and a
  thermal zone are reported once.
* **HTTP, not a private protocol.** It costs a little size and buys
  `curl node:9765/metrics` when a node misbehaves. The server is hand-rolled
  (one endpoint, `Connection: close`, read to EOF) so no HTTP stack is pulled
  into either binary.
* **Serialise once per sample, not per request**, so N monitors cost the same as
  one.
* **Three failure states, not one.** "No agent" (a rollout gap) is distinct from
  "agent error" (something answered but was unusable) and from "stale" (it
  answered before and has gone quiet). Only the last two are faults. A failed
  poll keeps the last known values so a row shows history rather than going
  blank, and the header carries coverage as `agents 1/2`.
* **Host identity is checked.** The agent reports its own hostname and every
  local address; a mismatch against the scheduler's name marks the row
  `[wrong host?]` rather than quietly graphing another machine's CPU.
* **Metrics are dropped on reconnect** along with everything else derived: host
  ids are assigned by the scheduler, and a new session can hand the same id to a
  different machine.

### What was verified, and how

| Claim | Evidence |
|---|---|
| the parsers read real files correctly | fixtures captured verbatim from this machine; totals cross-checked against `free -m` (64004 MiB), `nproc` (12), `/proc/loadavg` (exact match) and `coretemp` (82 °C) |
| the agent works end to end | `curl` returns a snapshot; `/healthz` returns `ok`; an unknown path returns 404 pointing at `/metrics` |
| a hung node cannot stall the poll loop | a listener that accepts and never answers is bounded by the per-node timeout, while a healthy agent on another port keeps being served |
| a node with no agent degrades gracefully | live two-node cluster: the macOS node reported `Connection refused` and rendered as `—` with `agents 1/2`, while the Linux node showed 12 % CPU, 47 % memory, 78 °C |
| 120 nodes are polled inside one interval | collector test with 120 targets against a local agent |
| the agent is deployable | `x86_64-unknown-linux-musl` build is a 2.0 MB static-pie binary with no dynamic dependencies; the systemd unit passes `systemd-analyze verify` |
| neither disturbs the machine | 30 s attached to the live cluster: agent **0.7 % CPU, 3.7 MiB RSS**; monitor **0.0 % CPU, 4.3 MiB RSS** |

## 9. Open questions for Phase 2+

- **Agent transport.** HTTP/JSON (trivially debuggable with `curl`, easy `node_exporter` parity) vs
  a compact binary frame (smaller, faster). Leaning HTTP/JSON at 1 Hz — at 100 nodes it is a few
  hundred KiB/s and debuggability is worth more.
- **Agent port.** Must not collide with iceccd (10245) or the scheduler text port (8766).
- **Node identity.** Matching agent to scheduler record by IP fails with NAT or multi-homed hosts;
  the agent should report the hostname *and* all local addresses so the monitor can match on any.
- **Reconnect and counters.** Whether to persist cumulative IN/OUT/LOCAL across reconnects by
  keeping the last known values and marking them stale, or to reset visibly. Leaning visible reset.
- **`FreeMem` units are not portable.** On Linux the field is MiB of available memory, confirmed
  against `free -m` (36732 vs 36566). A macOS node in the live cluster reported `FreeMem:12060904`,
  which as MiB would be 11.5 TiB and as KiB would be a plausible 11.5 GiB. Phase 2 does not display
  memory, so nothing is wrong today — but **Phase 3 must verify the unit per platform before
  rendering it**, and probably prefer the agent's own `/proc/meminfo` reading over the scheduler's
  value wherever an agent exists. (Not traced to a specific upstream cause; no shell on that node.)
- **Upstream contribution.** §2.4 (36 s accept latency on an idle scheduler) and icecream-sundae's
  hardcoded port are both worth reporting upstream regardless of what we build.
```
