# icecream-watcher — Architecture (Phase 1: Research)

Status: **Phase 1 research complete; Phases 2–6 implemented** (see §8), with the
UI reworked around Icecream data in §8e–§8g.
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
`icecream-watcher` will accept `host:port`.

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
monitor-side accumulators over this stream. `icecream-watcher` must reimplement the same accounting;
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
cluster whenever one daemon is slow to answer. It stays useful for `icecream-watcher --debug-dump` and for
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
`icecream-watcher` should therefore be **GPL-2.0-or-later**.

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
| **1 — recommended** | `icecream-watcher-agent` on each node | full 1 Hz `/proc` + `/sys` metrics | one static binary + socket unit |
| **2 — deferred** | existing Prometheus `node_exporter` | most Tier-1 metrics | none, where already deployed |

Tier 2 is **out of scope for now** and revisited only if a target cluster already runs
`node_exporter`. Nodes without an agent still appear, with resource cells rendered as `—` and a `no-agent` badge.
This matters: it means `icecream-watcher` is never worse than `icemon`, and gets better as agents roll out.

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
                        │              icecream-watcher (TUI)             │
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
icecream-watcher/
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
│   │   └── src/history.rs        sample ring buffers and trend detection
│   ├── icecream-watcher-agent/              icecream-watcher-agent
│   │   ├── src/parse.rs          pure /proc and /sys parsers
│   │   ├── src/sampler.rs        sampling, deltas, sensor selection
│   │   └── src/server.rs         single-endpoint HTTP/1.1
│   └── icecream-watcher/                the TUI binary
│       ├── src/main.rs           CLI, runtime wiring, terminal setup
│       ├── src/app.rs            state, key handling (sort modes: planned)
│       ├── src/collect.rs        parallel agent polling
│       └── src/ui/               rendering
│           ├── mod.rs            cluster band, node table, help overlay
│           ├── detail.rs         the per-node detail view
│           └── widgets.rs        block bars, sparklines, colour ramps
└── contrib/
    ├── capture/                  protocol captures + format docs
    │   ├── README.md
    │   └── lab-session.icwcap   golden-test fixture from a live scheduler
    └── systemd/                  hardened agent unit + deployment notes
```

The planned `docs/protocol.md` was **dropped**: §2 of this document is already
the authoritative, wire-verified protocol reference, and maintaining a second
copy would only let the two drift apart. The capture file format — the one thing
§2 does not cover — is documented in `contrib/capture/README.md`.

`icecc-proto` and `icecream-watcher-agent` stay independently usable — the protocol crate is the piece most
likely to be valuable to other people, and keeping it UI-free keeps it honest.

---

## 7. UI design direction

The stated goal is that nine questions be answerable in 1–2 seconds (which nodes are busy / idle /
CPU-bound / memory-bound, slot occupancy, whether the queue is growing, whether one node is slow,
whether anything is unhealthy, whether the cluster is being used efficiently). That argues against
a uniform table where every metric gets equal weight.

Proposed overview, three bands:

```
 icecream-watcher   build-master:8765   proto 43   up 04:12:07                          1.0s  [?] help
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
| UDP broadcast discovery works | plain `icecream-watcher` found the real cluster; answers arrived from both loopback and the LAN interface and the netname filter rejected a wrong netname |
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

`icecream-watcher-agent` samples `/proc` and `/sys` once a second and serves the last
snapshot over one `GET /metrics`; `icecream-watcher` polls every node at the address the
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

## 8c. Phase 4 — btop-like UI — **done**

Phase 3 added the data; this phase spends it on **visual hierarchy** rather than
on more columns. The prompt's nine questions were the acceptance criteria, and a
grid of equally-weighted numbers cannot answer them however many numbers it has
— which is fair criticism the earlier phases had earned: structurally, Phase 3's
table was the same shape as `icecream-sundae`'s.

What changed:

* A **cluster band** answers the whole-cluster questions before the table is
  read at all — slot occupancy as a bar, queue depth as a sparkline, throughput
  — so "how full is the cluster", "is the queue growing" and "is it being used
  efficiently" need no row-by-row reading.
* **Three bars per node** (CPU, memory, slots) with eighth-block partials, so a
  3 % difference between rows is visible and lengths are comparable without
  reading a figure.
* **`!` marks the limiting metric** — memory pressure first, then CPU
  saturation, then all slots taken on an otherwise idle machine — which is what
  separates "CPU constrained" from "memory constrained" at a glance. On SPEED it
  marks a node below half the cluster median, answering "is one node
  significantly slower".
* **Colour ramps only as a metric becomes a problem**, and temperature
  thresholds are deliberately high (80/90 °C): build machines run hot, and
  colouring 70 °C as alarming would cry wolf.
* **Idle is quiet, unhealthy is loud.** Idle rows dim; offline rows strike
  through and sink below live ones; badges name the one thing wrong with a row.
* **Two minutes of history per node**, sampled on a wall-clock timer rather than
  per event, so a quiet cluster's graphs still scroll and the horizontal axis
  means time.
* **Sorting and navigation** on the documented keys, with a help overlay.
* Everything else — IP, platform, protocol, features, per-core detail, network —
  was **removed** from the main screen and belongs in the Phase 5 detail view.

Decisions worth recording:

* **A gap is not a zero, in graphs too.** A missing sample is stored as `NaN`
  and drawn as a space, while a measured zero draws the lowest glyph. A node
  whose agent was down for ten seconds must not look like a node that was idle
  for ten seconds.
* **Young graphs grow in from the right**, padded with gaps on the left, rather
  than stretching a few samples across the full width and implying history that
  does not exist.
* **Queue and rate graphs are scaled to their own peak, and the peak is
  printed.** A queue has no natural maximum, so peak-scaling is the only honest
  choice — but a queue that has sat at seven all window then draws at full
  height, which without the stated scale reads as "at some limit".
* **Trends need a threshold.** Without one a queue oscillating between three and
  four jobs would alternate between "rising" and "draining" every second, so a
  trend is only named past a minimum delta and a minimum number of samples.
* **The bottleneck marker gets its own cell in the column budget.** It was
  initially truncated away — visible only in a test against the rendered buffer
  — which is exactly the row where it matters most.
* **The badge wins over the name.** A long hostname is elided with `…` so its
  badge survives; the badge is *why* the row deserves attention.
* **Narrow terminals drop whole columns** rather than squeezing every bar: a bar
  under about four cells conveys nothing, so breakpoints remove SPEED, then
  TEMP, then the slot bar, and a short terminal drops the cluster band to keep
  the table.

### What was verified, and how

| Claim | Evidence |
|---|---|
| the design answers the nine questions | rendered-buffer tests assert each one: band figures, per-metric bottleneck markers on different columns, a slow outlier flagged against the median, `1 down`/`1 no agent` health counts, and a growing queue labelled `rising` |
| bars are honest | widget tests cover exact width at every percentage, clamping, and that 1 % is distinguishable from 0 % and from 12 % |
| gaps and zeroes differ | asserted in both the history buffer and the sparkline |
| the band is comparable | a test fills every series and asserts all three graphs occupy identical columns; another asserts a single sample draws at the right edge |
| selection and sorting behave | busiest-first per key, unmeasured last, offline last, selection follows its node through a re-sort and is dropped when its node disappears |
| it survives any geometry | rendered at 10×3 through 300×100, with the help overlay open, and with 120 nodes scrolling to the selection |
| it works on the real cluster | run against the live two-node cluster under synthetic load: 95 % CPU with the `!` marker, a 46 % memory bar, a 2-minute CPU sparkline, 94 °C, and the agentless node drawn with `····` placeholders |

The README screenshot is generated by an ignored test that renders through the
real renderer, so the picture cannot drift from the code.

## 8d. Phase 5 — node detail view — **done**

`Enter` opens the selected node; `Esc` closes it. The overview answers "which
node should I look at", and this answers "what is going on with it": per-core
CPU with frequency, memory broken down with swap, the Icecream job ledger,
network throughput per interface, uptime, every thermal sensor, and which agent
answered.

Decisions worth recording:

* **Problems are stated before the numbers they would explain.** An offline
  node's panel opens with `OFFLINE — the values below are its last known ones`,
  and a hostname mismatch names both sides rather than leaving a plausible-looking
  set of another machine's metrics on screen. A node with nothing wrong says
  `healthy`, so silence is never ambiguous.
* **A flat, scrolled line list rather than a fixed layout.** A 2-core node and a
  128-core node both have to work; per-core bars wrap into as many columns as
  fit, and nothing is silently cut off the bottom.
* **Only the renderer knows the viewport**, so the scroll offset is clamped
  after painting rather than guessed. Without that, holding the scroll key past
  the end means pressing back the same number of times to return — the offset
  keeps counting up even though the view has stopped moving.
* **`Esc` unwinds one layer at a time**: overlay, then detail, then the session.
  Quitting straight out of a detail view would be a surprise.
* **The footer advertises only the keys that work in the current view.** Sorting
  is inert while the detail view is open — reordering a list nobody can see is
  not a feature — so the footer stops offering it.
* **`Enter` on a fresh screen selects a row rather than doing nothing.**

### What was verified, and how

| Claim | Evidence |
|---|---|
| the view carries what the overview drops | asserted against the full line list rather than one screenful, so a section scrolled below the fold still counts |
| scrolling reaches the end | a test scrolls down and asserts the last section becomes visible |
| unhealthy nodes lead with the problem | offline and wrong-host panels assert both the label and the explanation |
| a node with no agent is explained, not blank | asserts the "install icecream-watcher-agent" line *and* that scheduler-sourced facts are still shown |
| keys behave per view | `Esc` unwinds one layer at a time; arrows scroll without moving the selection; sort keys are inert |
| any geometry and any core count | rendered from 10×3 to 200×60, and with a 64-core node whose per-core bars must not overflow the line |
| it works on the real cluster | opened on the live Linux node: 12 per-core bars in six columns, 1699 MHz average, load 19.80 with 1.65 per core, 32.8 GiB of 62.5 GiB used, real features list |

A label longer than its column ran straight into its value (`queued from here0`)
— found by looking at the rendered output, not the code, and now guarded by a test.

## 8e. Phase 6 — robustness — **done**

Earlier phases made the tool correct while things work. This one is about what
it does when they do not, and it started by looking for the places where the
existing code was quietly wrong rather than by adding features.

The governing idea: **a monitor is a long-running process fed by a remote one**,
so every collection it keeps needs a bound that does not depend on the remote
behaving, and every "everything is fine" on screen has to be something it
actually checked.

### The frozen screen

The worst failure this tool can have is not a crash — it is displaying a
plausible cluster that stopped being true. Scheduler stats are change-driven
(§2.3), so an idle cluster and a severed link produce the same thing: nothing.
The application protocol cannot tell them apart, and nothing else was watching.

`SO_KEEPALIVE` with a 20 s idle time, 5 s probes and 3 retries closes that gap
at the only layer that can see it. The header additionally names silence
(`quiet 5m`) once it passes a minute, so a quiet cluster reads as quiet rather
than as unknown.

### Reconnection

The old behaviour was a fixed 2 s retry, forever — which against a dead
scheduler with broadcast discovery means a packet storm all night. It now backs
off 1, 2, 4 … to 30 s.

Backoff has a cost: a scheduler that comes back can go unnoticed for a whole
cap. Two things pay it off. A session that *reached login* resets the counter,
so a restart is picked up at once and only a genuinely absent scheduler is
backed off; and `r` cuts the wait short, which is what makes an aggressive cap
safe to ship. The header shows the countdown and the attempt number, because a
screen that says only "disconnected" gives no sign that anything is still being
tried.

### Bounded state

`MON_GET_CS` and `MON_JOB_BEGIN` put a job in the model and `MON_JOB_DONE`
takes it out — but nothing guarantees the third message. Upstream's
`handle_job_done` returns without notifying monitors when it cannot find the
job, and its cancellation lookup (`scheduler.cpp`, the `unknown_job_client_id`
branch) only matches jobs that have not been assigned yet, so a cancellation
racing with scheduling is a job whose end no monitor ever hears. The cost is not
mainly memory: a stuck pending job inflates the queue depth **for the rest of
the session**, and "is the queue growing?" is one of the nine questions this UI
exists to answer.

Jobs therefore expire, generously (30 minutes) and visibly — the count is kept
in `totals.expired_jobs` and logged with the job's name. Generous because
expiring early *understates* the queue, which is exactly as wrong as
overstating it, and a saturated cluster can legitimately keep a job queued for a
long time. Expiry also releases the node's slot, or one wrong number would just
be swapped for another. It runs before the history sample is taken, so the tick
that corrects a figure does not also record the stale one.

Offline nodes keep their row by default — "which node just died" is the point
of showing it — but now carry `offline_since`, so a row says `down 12m` rather
than just `down`. `--forget-offline` drops them for anyone running for days.

### The busy-spin

A closed `mpsc::Receiver` returns `None` immediately and forever. The agent
collector's `select!` branch ignored `None`, so if that task ever stopped, the
branch was permanently ready and the render loop would spin at 100 % CPU — on
the build machine the whole tool exists to stay out of the way of. Measured at
**82,688 wakeups in 100 ms** before the fix. The branch now retires itself.

Reachable only if the collector task dies, which nothing in the current code
does deliberately; it is fixed because the consequence is severe and the cost of
the guard is three lines, and because a regression test that could not fail
would have been worthless.

### Also covered

* **Clock changes** — audited rather than changed. Every staleness, uptime and
  retention decision reads the monotonic clock; the only wall-clock value in the
  system is the agent's `sampled_unix_ms`, which is reported and never used for
  staleness. NTP stepping the clock cannot age a node out or freeze a graph.
* **A different scheduler answering discovery** — the header says
  `⇄ moved from <old>`, judged against the *first* scheduler of the session so
  bouncing A → B → A is not reported as a move. Without it the whole screen
  silently changes which cluster it describes.
* **Degenerate geometry** — 0×0, 1×1, 0×40 and 40×0 are now in the render tests,
  because a terminal can report zero during a resize and a panic there leaves
  the user in a raw-mode alternate screen.

### What was verified, and how

Failure injection ran against an isolated lab (unique netname, non-default
ports); the partition test ran inside a private network namespace created with
`unshare -rn`, so the packet filtering could not touch anything on the machine.

| Claim | Evidence |
|---|---|
| a severed link is noticed | packets dropped mid-session in a private netns: `Connection timed out (os error 110)` **35 s** after the cut, matching 20 s + 3 × 5 s |
| …and would not be without keepalive | same test with the call removed: still "connected" after **101 s**; the system default `tcp_keepalive_time` is 7200, and `SO_KEEPALIVE` is off unless set, so the true answer is "not until a write fails" |
| backoff grows and caps | measured gaps of 1, 2, 4, 8, 16, 30, 30 s — **7 attempts in 70 s** where the old fixed delay would have made 35 |
| a restarted scheduler is picked up at once | lab scheduler killed and replaced: disconnect seen in **0 s** (a clean FIN needs no keepalive), reconnect **1 s** later, because a session that reached login resets the backoff |
| a departing node is marked, not dropped | lab daemon killed: scheduler sent `State:Offline`, the row kept its place and started counting downtime |
| the spin is real and the fix works | the regression test records **82,688** wakeups in 100 ms against the old code and 0 against the new |
| jobs cannot accumulate | 20,000 full job lifecycles leave an empty map; a job with no end expires, is counted, and releases its slot |
| expiry cannot be silent or early | asserted on `totals.expired_jobs`, on the history sample taken after expiry, and that a job inside its timeout is untouched |
| a move is reported, a bounce is not | asserted against the first scheduler of the session, not the previous one |
| keepalive is really set | socket options read back from the kernel, not inferred from the call returning `Ok` |
| it still costs nothing | 30 s attached to a lab cluster: **0.0 % CPU, 4.27 MiB RSS**, unchanged from Phase 3 |

257 tests, `cargo clippy --all-targets` clean.

### Not verified

The live cluster on this machine had no scheduler running at the time of this
work (`iceccd` was up, nothing on 8765), so everything above was verified
against lab schedulers rather than the two-node cluster used in Phases 2–5.
Nothing here is cluster-specific, but the Phase 5 note that the tool works on
the real cluster has not been re-confirmed since these changes.

## 8f. Dot graphs in the cluster band — **done**

`btop` was the reference for *how a screen reads*, not for what to measure: the
band still answers Icecream questions — compile slots, scheduler queue depth,
completion rate — and per-node CPU and memory stay where Phase 4 put them, as
supporting evidence for "which node is the bottleneck" rather than as the
subject.

What changed is resolution. A block sparkline gets eight levels out of one
character cell; a braille cell carries two dot columns by four dot rows, so an
`n`-row graph has `4n` levels and twice the horizontal detail. That only pays
off with vertical room, so the band is now progressive: one line per series
under 24 rows (block sparklines, as before), two over 24, three over 40. One row
of braille would be *four* levels — worse than what it replaced — so height, not
style, decides which is drawn, and the node table keeps its block sparklines.

Decisions worth recording:

* **The time axis is fixed at two minutes, at every width.** A wide terminal
  draws the same window larger rather than showing more of it, so two graphs
  side by side stay comparable and a graph does not silently change what its
  horizontal axis means when the window is resized. `History::stretched` keeps
  the Phase 4 promise that a young series grows in from the right: a buffer a
  third full occupies a third of the axis, right-aligned, instead of stretching
  a handful of samples across the box.
* **No value is invented between samples.** Widening repeats a sample across
  columns and narrowing averages a bucket; nothing interpolates.
* **Colour by height, but only where height means something.** Slot occupancy
  has a real maximum, so its gradient reads like the bars beside it. The queue
  and rate graphs are scaled to their own peak — where the top row means "the
  most we have seen", not "full" — so they take a flat colour instead.
* **The left column is enforced, not assumed.** A note one character over
  budget shunted its graph sideways and pushed the newest samples off the
  right-hand edge — the same class of bug as the Phase 4 three-column stagger,
  and invisible except in a rendered buffer. The column is now padded *or*
  elided to an exact width, and the alignment test checks every row of a series
  rather than only the row carrying its label, which is what let the first
  version through.

### The node table is Icecream-only

The same principle applied to the rows. CPU, memory and temperature columns are
a machine's business, not a compile cluster's, so they went to the detail view
that Phase 5 built for exactly this; the table now carries compile slots, jobs
in and out, the scheduler's load figure and compile speed, and ends each row with
two minutes of that node's slot occupancy in dots.

One judgement worth recording: the **conclusion** stays even though the gauges
go. "Which nodes are CPU-constrained and which memory-constrained" is one of the
nine questions this UI was built to answer, so a constrained node is badged
`cpu!` or `mem!` beside its name. Dropping the raw percentages is a change of
subject; dropping the answer would be a regression.

A row is one character tall, so its graph gets four vertical levels rather than
the eight a block sparkline gave — but twice the horizontal resolution, and the
exact figure is in the column beside it. For a trend strip that is the better
trade; for the band, where a taller graph is possible, it is not, which is why
the two use different glyphs at different heights.

Spare width goes to hostnames before graphs, up to 38 columns: an elided name
costs the reader more than a shorter strip. The graph stops growing at 60 cells,
where two dot columns per character already draw the whole 120-sample buffer one
sample to a dot; past that it would only be upscaling.

### Dots and colour in the rows

The slot bar is drawn in dots too. Two dot columns to a character means it
resolves half a cell, so one slot of twelve is visible where a block bar of the
same width would round it to nothing; the unfilled part keeps a baseline row
rather than going blank, so what the filled part is a fraction *of* stays on
screen.

**Slots are counted, not estimated.** `CUR` and `MAX` give the figures, and the
meter beside them spends **one character cell per slot**. That is forced by how
terminals work rather than chosen for looks: colour is a property of a character,
so two slots sharing a cell cannot be told apart however many dots it holds. A
busy slot is a filled left dot-column with the baseline running on to its right —
a bar with a gap built in — so a run of busy slots stays countable instead of
merging into one block.

Each busy slot is coloured by the node that **submitted** the job, the way
`icecream-sundae` attributes work, so a glance answers "whose work is this
machine doing" and not merely "how full is it" — which the figures already
answer better. A job that was already running when the monitor attached has no
known submitter (the scheduler replays node stats on login, not jobs) and takes
the compiling node's own colour. A node with more slots than the column has cells
falls back to a proportional bar: one smeared cell per eight slots would claim a
precision that is not there.

**Each node is drawn in its own colour**, keyed by a hash of its hostname so the
colour follows the machine through a re-sort, through other nodes joining and
leaving, and between sessions — recognising the same node across all of that is
the entire point, which rules out colouring by row position. FNV-1a rather than
`DefaultHasher`, whose output is not promised to be stable across Rust versions;
a colour that changed with the toolchain would defeat the purpose.

The palette was **searched rather than chosen by eye**. The first hand-picked
list paired colour 39 with 45 — one step apart in the 6×6×6 cube, and the same
colour to anyone glancing at a row. The search maximises the minimum pairwise
distance under three constraints: no warm hues, because red, orange, yellow and
tan carry *state* in this UI and a healthy node that hashed into that range would
read as a node in trouble; nothing so dark it vanishes on a dark terminal; and no
greys, which already mean "no measurement". Twelve colours means nodes will share
one on a large cluster — this is a hint for the eye, not an identifier.

State beats identity: an offline row is grey whatever colour it would have had,
because "this one is gone" matters more than which machine it was.

### What was verified, and how

| Claim | Evidence |
|---|---|
| slots are individually countable | the meter is asserted as an exact glyph run — `⣇⣀⣀⣀⣀⣀⣀⣀` for one of eight — scoped to the meter, because counting baseline dots across the whole row also counts the history graph's |
| a busy slot names its submitter | two jobs from different clients asserted to render in those clients' colours, read back from the buffer |
| a huge node degrades rather than lies | 128 slots in a 16-cell column falls back to a proportional bar |
| the dot bar is honest | exact width at every percentage, clamping, an empty bar still showing its extent, and half a character resolved where a block bar could not |
| nodes really are drawn differently | twelve nodes rendered and their name colours read back **from the buffer**, not from the text — a scheme that stopped being applied would look identical in a text-only assertion |
| a colour is an identity | asserted to survive a re-sort, and to lose to grey when the node goes offline |
| the palette is legible | every entry decoded from the colour cube and asserted to be non-warm, non-grey, bright enough, and at least three cube steps from every other |
| the table carries cluster figures, not machine ones | the header is asserted to hold SLOTS/IN/OUT/LOAD/SPEED and *not* CPU/MEM/TEMP |
| the constrained-node answer survived the change | `cpu!` and `mem!` badges asserted on the two constrained rows |
| a missing agent costs one column, not a row | with no agent, slots, speed and the counters still render from scheduler data; only LOAD is unknown |
| the glyph maths is right | dot-level tests: full is solid, a gap is blank, a measured zero draws the baseline, the area fills upward, and the two dot columns of a cell carry different samples |
| height buys resolution | one row cannot separate 50 % from 57 %; four rows can |
| the axis does not change with the width | a half-full buffer occupies half the axis at widths 20, 40, 100 and 250 |
| a young graph still grows in from the right | one sample out of two minutes draws in the right-hand quarter, asserted in the rendered buffer |
| graphs stay aligned | every row of every series asserted to occupy identical columns, at three terminal heights |
| the fallback is by height, not by taste | block sparklines and no braille at 24 rows; braille at 30, with the node table still showing its rows |
| it renders on a real cluster | attached to the live two-node cluster during an actual `cargo` build: slots 24/24, queue 76 and rising, 5 completions a second |

## 8g. The detail view, rebuilt around jobs — **done**

Phase 5 built this panel around the machine: per-core CPU, swap, uptime, network,
every thermal sensor. That was the wrong subject for a compile-cluster monitor,
and it went the same way the table's CPU and memory columns did.

What it answers now is the question the overview's slot meter raises. The meter
says how many slots are busy and, by colour, whose work is in them; the panel
says **which file each slot is compiling and for how long**, and then everything
the scheduler reports about the node — name, address, platform, protocol,
features, max jobs, whether it accepts remote work, speed, the scheduler's load
figure, load averages and free memory — the way `icecream-sundae` presents an
expanded node.

Decisions worth recording:

* **Elapsed time is honest by construction.** Every job in the model was seen to
  begin: the scheduler replays node stats on login but not jobs, so a job already
  compiling when the monitor attached is invisible until its `MON_JOB_DONE`
  arrives. That makes "since we saw it start" the same as "since it started" for
  everything listed — and the panel says how many *un*seen jobs have finished, so
  the gap is stated rather than hidden.
* **`Load` is spelled out as what it is.** The protocol's `Load` is the
  scheduler's placement weight, not CPU utilisation, and the field name invites
  exactly the wrong reading — so the value is rendered `772 of 1000 — the
  scheduler's placement weight`.
* **`FreeMem` is labelled, not converted.** §9's open question is now visible on
  screen rather than only in this document: a figure too large to be the MiB the
  protocol documents is shown raw and marked as probably KiB, with the converted
  value beside it. Silently guessing the unit is how the trap was set.
* **A job with no name says so.** `MON_JOB_BEGIN` carries no filename; it comes
  from the `MON_GET_CS` before it, which is missed if the monitor attaches in
  between. That renders as `(name not seen)` rather than an empty cell.
* **The agent is a footnote now.** Nothing on this panel comes from it. It is
  kept because the overview's `cpu!` and `mem!` badges are computed from it, and
  a badge with no way to see the figure behind it is worse than no badge.

Removed with the sections they served: the per-core bar layout, the temperature
ramp, and the byte-rate and kibibyte formatters.

### What was verified, and how

| Claim | Evidence |
|---|---|
| each slot names its file and its submitter | asserted on the rendered line, including the numbering and the free-slot count |
| an unnamed job is explained, not blank | a `MON_JOB_BEGIN` with no preceding `MON_GET_CS` renders `(name not seen)` |
| an idle node says so | "no remote jobs running (8 slots free)" rather than an empty section |
| a long path cannot overflow the line | 64 jobs with deeply nested paths, every rendered line asserted within the terminal |
| the load figure cannot be misread as CPU | asserted to carry "of 1000" and "placement weight" |
| the memory-unit trap is on screen | the macOS figure renders raw and marked KiB; the Linux figure renders as MiB |

The note explaining the memory unit was written long enough to be **truncated at
the panel edge** — found by asserting on the rendered buffer, which is the only
place that kind of mistake shows up.

## 8h. Sorting, reduced to what is on screen — **done**

The original brief fixed the sort keys as `c`, `m`, `l`, `i` — CPU, memory,
load, Icecream jobs. Two of those now sort by numbers that appear nowhere on the
table, which makes a keypress reorder the list for a reason the reader cannot
see. The keys are therefore `name`, `jobs`, `load` and `speed`, bound to
`n`, `i`, `l`, `p` (`s` was already the cycle, so speed could not have it), and
`c` and `m` are unbound rather than repurposed — a key that used to do one thing
and silently does another is worse than one that does nothing.

`LOAD` now sorts on the scheduler's own `Load` figure rather than on the load
average, so the order matches the column beside it.

This is a deliberate deviation from §14's key list, made after the table stopped
carrying machine metrics; `?` and the footer list only the keys that work.

| Claim | Evidence |
|---|---|
| the cycle visits exactly the four | asserted by walking `next()` until it wraps and comparing the labels |
| the freed keys are inert | `c` and `m` classify as `Ignored`, and `Ctrl-C` still quits |
| the order matches the column | load sorting asserted against the scheduler's figure, not the load average |

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
  hardcoded port are both worth reporting upstream regardless of what we build. Phase 6 adds a
  third: `handle_job_done` returns without `notify_monitors` when it cannot find the job, and the
  cancellation lookup only matches unassigned jobs, so a cancellation racing with scheduling leaves
  every monitor holding a job it is never told the end of. Worth reporting; we work around it with
  expiry (§8e).
```
