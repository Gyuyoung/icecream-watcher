"""Write an icecream-watcher capture of a busy cluster, for the README demo."""
import struct, random, sys

MON_GET_CS, MON_JOB_BEGIN, MON_JOB_DONE, MON_STATS = 83, 84, 85, 87
PROTOCOL = 43

def s(text):                      # length includes the trailing NUL
    b = text.encode() + b"\0"
    return struct.pack(">I", len(b)) + b

frames = []                       # (millis, type, payload)
def emit(ms, ty, payload): frames.append((ms, ty, payload))

NODES = [   # id, name, max jobs, speed, local-only
    (1, "build01", 16, 3200, False),
    (2, "build02", 16, 2900, False),
    (3, "build03", 16, 3100, False),
    (4, "build04", 16,  940, False),
    (5, "build05",  8, 3050, False),
    (8, "build07", 16, 3000, False),
    (6, "laptop",  12,    0, True),
]
SOURCES = [
    "mojom/sensor/web_sensor_provider.mojom-blink.cc",
    "mojom/smart_card/smart_card.mojom-blink.cc",
    "renderer/modules/webaudio/audio_worklet_processor.cc",
    "renderer/core/layout/layout_block_flow.cc",
    "mojom/serial/serial.mojom-blink.cc",
    "renderer/platform/graphics/paint/paint_controller.cc",
    "renderer/core/css/resolver/style_resolver.cc",
    "mojom/speculation_rules/speculation_rules.mojom-blink.cc",
]

def stats(host_id, body): return struct.pack(">I", host_id) + s(body)

# --- the login replay: identity only, the way a scheduler sends it ----------
for nid, name, mx, speed, local in NODES:
    emit(0, MON_STATS, stats(nid,
        f"Name:{name}\nIP:10.0.0.{nid}\nMaxJobs:{mx}\nNoRemote:{str(local).lower()}\n"
        f"Speed:{speed}\nPlatform:x86_64\nVersion:43\nFeatures:env_xz env_zstd\n"))
# a node that has dropped out
emit(0, MON_STATS, stats(7, "Name:build06\nIP:10.0.0.7\nMaxJobs:16\nNoRemote:false\n"))
emit(120, MON_STATS, stats(7, "State:Offline\n"))

rng = random.Random(7)
job = 10_000
running = {}                      # job id -> (host, finish ms)
# how busy the cluster is over the recording, as a fraction of capacity
def load_at(ms):
    t = ms / 1000.0
    if t < 4:   return 0.20 + 0.70 * t / 4
    if t < 7:   return 0.90
    if t < 10:  return 0.90 - 0.55 * (t - 7) / 3
    return 0.35 + 0.50 * (t - 10) / 4

END = 15_000
for ms in range(200, END, 200):
    for jid in [j for j, (_, done) in running.items() if done <= ms]:
        host, _ = running.pop(jid)
        emit(ms, MON_JOB_DONE, struct.pack(
            ">12I", jid, 0, 0, rng.randint(900, 4200), 0, 0, 0, 0, 0, 120_000, 0, 1))
    for nid, name, mx, speed, local in NODES:
        if local: continue
        want = round(mx * load_at(ms))
        have = sum(1 for h, _ in running.values() if h == nid)
        for _ in range(max(0, want - have)):
            job += 1
            src = SOURCES[job % len(SOURCES)]
            emit(ms, MON_GET_CS, s(src) + struct.pack(">III", 1, job, 1 + job % 4))
            emit(ms + 20, MON_JOB_BEGIN, struct.pack(">III", job, 0, nid))
            running[job] = (nid, ms + rng.randint(1200, 5000))
    if ms % 2000 == 0:            # periodic stats, so load averages move
        for nid, name, mx, speed, local in NODES:
            busy = sum(1 for h, _ in running.values() if h == nid)
            cpu = min(1000, int(1000 * busy / mx)) if mx else 0
            emit(ms, MON_STATS, stats(nid,
                f"Name:{name}\nIP:10.0.0.{nid}\nMaxJobs:{mx}\nNoRemote:{str(local).lower()}\n"
                f"Speed:{speed}\nPlatform:x86_64\nVersion:43\nFeatures:env_xz env_zstd\n"
                f"Load:{cpu}\nLoadAvg1:{cpu*14}\nLoadAvg5:{cpu*12}\nLoadAvg10:{cpu*10}\n"
                f"FreeMem:{36732 - busy*380}\n"))

frames.sort(key=lambda f: f[0])
out = bytearray(b"ICWCAP01" + struct.pack(">I", PROTOCOL))
for ms, ty, payload in frames:
    out += struct.pack(">Q", ms) + struct.pack(">II", 4 + len(payload), ty) + payload
open(sys.argv[1], "wb").write(out)
print(f"{len(frames)} frames, {len(out)} bytes, {END/1000:.0f}s")
