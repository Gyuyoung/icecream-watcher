# Deploying `icecream-watcher-agent`

The agent supplies everything the Icecream scheduler cannot: CPU utilisation,
per-core load, memory totals, swap, temperature, frequency, uptime and network
throughput. Without it `icecream-watcher` still works — those columns simply read `—`
and the header shows the coverage as `agents 3/8`.

## Install

On each build node:

```sh
install -m 0755 icecream-watcher-agent /usr/local/bin/
install -m 0644 icecream-watcher-agent.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now icecream-watcher-agent
```

Check it:

```sh
curl -s http://localhost:9765/metrics | head -c 200
curl -s http://localhost:9765/healthz     # -> ok
icecream-watcher-agent --once                    # one snapshot, no listener
```

A static build is the easiest thing to copy around, and avoids depending on the
node's libc version:

```sh
cargo build --release --target x86_64-unknown-linux-musl -p icecream-watcher-agent
```

## Access

The agent binds `0.0.0.0:9765` by default, because `icecream-watcher` reaches it at the
address the scheduler reports for that node. It is read-only — one `GET`
returning a fixed snapshot, no write path, no shell, no arbitrary paths — but it
does disclose hostname, addresses, load, memory and thermal data. On an
untrusted network, restrict it:

```ini
# /etc/systemd/system/icecream-watcher-agent.service.d/bind.conf
[Service]
Environment=ICECREAM_WATCHER_AGENT_BIND=10.0.0.11
```

or firewall the port to the build network. There is no authentication; if you
need it, put the port behind your existing network controls rather than
expecting the agent to do it.

## Port

9765 by default, chosen to avoid Icecream's own ports (scheduler 8765, its text
interface 8766, `iceccd` 10245) and the 8767–8769 range the icecream test suite
uses. If you change it, pass the same value to the monitor:

```sh
icecream-watcher --agent-port 9800
```

## Cost

One `/proc` and `/sys` sweep per second, and the snapshot is serialised once per
sample rather than once per request — so a hundred monitors watching cost the
same as one. The unit pins the agent to `Nice=10`, idle I/O and a low CPU
weight, so it yields to compilers by construction.
