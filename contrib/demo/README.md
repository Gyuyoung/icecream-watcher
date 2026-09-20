# The README demo

`docs/demo.gif` is the release binary running, recorded in a pty and
photographed frame by frame. It is not a mock-up and not a hand-drawn
animation: every pixel is something the renderer wrote.

Rebuild it with:

    contrib/demo/record.sh

## What it is made of

| File | Does |
|---|---|
| `make-capture.py` | Writes `contrib/capture/demo-cluster.icwcap`, a synthetic scheduler stream |
| `record.sh` | Runs the binary against that capture, presses keys at it, assembles the GIF |
| `frames.py` | Turns the recording into PNG frames, one per repaint |

The cluster is synthetic because a real one cannot be asked to do the same
thing twice — seven nodes, one of them offline and one local-only, filling up
and draining over fifteen seconds. Regenerate the capture with:

    python3 contrib/demo/make-capture.py contrib/capture/demo-cluster.icwcap

## If a frame looks wrong

ratatui writes only the cells that changed, so a frame is only ever as good as
the reconstruction of every frame before it. `frames.py` cuts strictly on
repaint boundaries for that reason, and `record.sh` checks the finished GIF
back against the frames it was made from. If a frame still shows a line with
another line's tail on it, throw the recording away and run it again — it is a
seam in that recording, not in the binary.

Inspect a GIF with `ffmpeg -i docs/demo.gif /tmp/f%04d.png` rather than with
Pillow: Pillow composes ffmpeg's frames wrongly and invents exactly this fault.

## Requirements

`python3` with Pillow, `ffmpeg`, and `script(1)` from util-linux. Frames are
drawn in JetBrains Mono Nerd Font; any monospace face with braille (U+2800) and
box drawing works, via `DEMO_FONT`:

    DEMO_FONT=/path/to/YourMono-%s.ttf contrib/demo/record.sh

The pattern takes `Regular` and `Bold`. Cells are 9px wide, which is one
advance of JetBrains Mono at 15px; a face with a different advance needs `CW`
in `frames.py` changed to match, or the glyphs drift out of their cells.
