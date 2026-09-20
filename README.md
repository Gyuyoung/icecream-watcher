# icecream-watcher

A terminal monitor for [Icecream](https://github.com/icecc/icecream) (`icecc`)
distributed compile clusters — the cluster equivalent of `btop`, where each
build node reads like a process.

![icecream-watcher against a seven-node cluster](docs/demo.gif)

Nothing on the build nodes to install: it is one TCP connection to the
scheduler.

## Build

Needs a Rust toolchain (1.75+). No `libicecc` and no C++ build dependencies —
the scheduler protocol is implemented natively.

```sh
cargo build --release
./target/release/icecream-watcher
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
| `--colors-256` | round the load gradient to the 216-colour cube |
| `--job-timeout SECS` | forget a job whose completion never arrives (default 1800; 0 disables) |
| `--reconnect-max-delay SECS` | ceiling on the reconnect backoff (default 30) |

## Keys

| Key | Action |
|---|---|
| `q`, `Esc` | back out one step: the help overlay, the detail view, the highlighted row — then ask to quit |
| `Ctrl-C` | quit, from wherever you are, without asking |
| `↑` / `k`, `↓` / `j` | move the selection, or scroll the detail view |
| `PgUp` / `PgDn` | move or scroll ten rows |
| `s` | cycle the sort key |
| `n`, `i`, `p` | sort by name, jobs, perf |
| `r` | redraw; while disconnected, retry the connection now |
| `?` | help |
| `Enter` | open or close the node detail view |

## Layout

| Crate | Contents |
|---|---|
| `crates/icecc-proto` | the scheduler monitor protocol: framing, handshake, message decoders, discovery. No UI. |
| `crates/icecc-metrics` | the node metrics wire format, plus a minimal client |
| `crates/icecc-model` | cluster state and job accounting |
| `crates/icecream-watcher` | the TUI |

`icecc-proto` is deliberately independent of the rest and is the piece most
likely to be useful to other tools.

## More

- [docs/screen.md](docs/screen.md) — what every figure on the screen means, and
  what it refuses to guess
- [ARCHITECTURE.md](ARCHITECTURE.md) — the design and the roadmap
- [contrib/demo/](contrib/demo/) — how the GIF above is recorded

`cargo test` runs everything; the protocol tests replay captured scheduler
bytes, so they need no cluster.

## Licence

GPL-2.0-or-later. The protocol and the job-accounting model are derived from
Icecream, `icemon` and `icecream-sundae`, all GPL-2.0-or-later.
