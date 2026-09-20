#!/bin/sh
# Record icecream-watcher replaying the demo capture, and build docs/demo.gif.
#
# The binary really runs: this drives the release build in a pty, presses keys
# at it, and photographs what it wrote. Nothing in the GIF is drawn by hand.
#
# Needs: python3 with Pillow, ffmpeg, util-linux script(1), and a monospace
# font with braille and box drawing (DEMO_FONT, see frames.py).
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
work=${TMPDIR:-/tmp}/icw-demo.$$
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/frames"

cd "$root"
cargo build --release

# Keystrokes, timed against the 15 s the capture covers: pick a node, sit on
# the overview while the cluster fills up, open the detail view, come back.
{
    sleep 2.5; printf '\033[B'
    sleep 0.5; printf '\033[B'
    sleep 0.5; printf '\033[B'
    sleep 5.5; printf '\r'
    sleep 3.5; printf '\033'
    sleep 2.5
} | script -q -T "$work/timing.log" -O "$work/cast.ansi" \
      -c "stty rows 30 cols 126; ./target/release/icecream-watcher \
          --replay contrib/capture/demo-cluster.icwcap --replay-realtime" \
    >/dev/null

python3 contrib/demo/frames.py "$work/cast.ansi" "$work/frames" 5

# One palette for the whole clip, and every frame written in full.
#
# `stats_mode=diff` with `diff_mode=rectangle` is the usual way to shrink a
# screencast, and it is wrong for this one: it repaints only a bounding box per
# frame and trusts the rest to be unchanged, which dithering makes untrue. It
# left words from the previous frame standing in the middle of the next one.
ffmpeg -y -v error -framerate 5 -i "$work/frames/f%04d.png" \
    -vf "scale=962:-1:flags=lanczos,split[a][b];\
[a]palettegen=max_colors=192:stats_mode=full[p];\
[b][p]paletteuse=dither=bayer:bayer_scale=5" \
    -loop 0 docs/demo.gif

# Check the GIF against the frames it was made from. GIF encoders shrink a
# screencast by repainting only what changed, and when that goes wrong it
# leaves words from one frame standing in the next — which is invisible in a
# file listing and obvious to anyone watching.
mkdir -p "$work/check"
ffmpeg -y -v error -i docs/demo.gif "$work/check/%04d.png"
python3 - "$work/frames" "$work/check" <<'PY'
import glob, sys
from PIL import Image, ImageChops
src = sorted(glob.glob(sys.argv[1] + "/f*.png"))
dec = sorted(glob.glob(sys.argv[2] + "/*.png"))
if len(src) != len(dec):
    sys.exit("gif has %d frames, rendered %d" % (len(dec), len(src)))
worst = 0
for i, (a, b) in enumerate(zip(src, dec)):
    B = Image.open(b).convert("RGB")
    A = Image.open(a).convert("RGB").resize(B.size, Image.LANCZOS)
    n = sum(1 for p in ImageChops.difference(A, B).convert("L").getdata() if p > 110)
    if n > worst:
        worst, where = n, i
if worst > 1500:
    sys.exit("frame %d differs from its source in %d pixels" % (where, worst))
print("%d frames verified against their source" % len(src))
PY

ls -l docs/demo.gif
