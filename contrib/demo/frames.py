"""Turn a `script(1)` recording of icecream-watcher into PNG frames.

    frames.py cast.ansi outdir [fps]

`timing.log`, written by `script -T`, must sit beside `cast.ansi`: it is what
says when each byte arrived, and so which bytes belong to which frame.

The terminal emulation here is only as complete as ratatui's output needs — an
absolute cursor move, a line erase, and SGR colour. It is not a terminal.
"""
import re, sys, os
from PIL import Image, ImageDraw, ImageFont

COLS, ROWS = 126, 30
# 9px is exactly one advance of JetBrains Mono at 15px, so cells land on whole
# pixels and the glyphs stay crisp. A font whose advance is not 9 will drift.
CW, CH = 9, 19
FONT = os.environ.get(
    "DEMO_FONT",
    os.path.expanduser(
        "~/.local/share/fonts/JetBrainsMonoNerdFont/JetBrainsMonoNerdFontMono-%s.ttf"
    ),
)
FPS = float(sys.argv[3]) if len(sys.argv) > 3 else 5.0
OUT = sys.argv[2]

if not os.path.exists(FONT % "Regular"):
    sys.exit(
        "no font at %s\n"
        "Set DEMO_FONT to a monospace family with braille (U+2800) and box\n"
        "drawing, as a printf pattern taking Regular/Bold." % (FONT % "Regular")
    )

raw = open(sys.argv[1], "rb").read()
cut = raw.rfind(b"\x1b[?1049l")          # drop script's footer after the alt screen
if cut > 0:
    raw = raw[:cut]

times, off = [], 0
for line in open(os.path.join(os.path.dirname(sys.argv[1]), "timing.log")):
    d, n = line.split()
    off += int(n)
    times.append((float(d), min(off, len(raw))))
t = 0.0
stamps = []
for d, o in times:
    t += d
    stamps.append((t, o))

NAMED = {30:(0,0,0),31:(205,49,49),32:(13,188,121),33:(229,229,16),34:(36,114,200),
         35:(188,63,188),36:(17,168,205),37:(229,229,229),90:(102,102,102),91:(241,76,76),
         92:(35,209,139),93:(245,245,67),94:(59,142,234),95:(214,112,214),96:(41,184,219),
         97:(255,255,255)}
CSI = re.compile(r"\[([0-9;?]*)([A-Za-z])")

def screen(data):
    text = data.decode("utf-8", "replace")
    grid = [[(" ", (229,229,229), None, False) for _ in range(COLS)] for _ in range(ROWS)]
    cy = cx = 0; fg = (229,229,229); bg = None; bold = False
    i = 0
    while i < len(text):
        ch = text[i]
        if ch == "\x1b":
            m = CSI.match(text, i+1)
            if not m:
                i += 2; continue
            params, cmd = m.group(1), m.group(2); i = m.end()
            nums = [int(x) for x in params.replace("?","").split(";") if x != ""]
            if cmd == "H":
                cy = nums[0]-1 if nums else 0
                cx = nums[1]-1 if len(nums) > 1 else 0
            elif cmd == "J" and (nums[0] if nums else 0) == 2:
                grid = [[(" ", fg, None, False) for _ in range(COLS)] for _ in range(ROWS)]
            elif cmd == "K":
                for x in range(cx, COLS): grid[cy][x] = (" ", fg, bg, bold)
            elif cmd == "m":
                if not nums: nums = [0]
                k = 0
                while k < len(nums):
                    n = nums[k]
                    if n == 0: fg = (229,229,229); bg = None; bold = False
                    elif n == 1: bold = True
                    elif n == 22: bold = False
                    elif n == 39: fg = (229,229,229)
                    elif n == 49: bg = None
                    elif n in NAMED: fg = NAMED[n]
                    elif 40 <= n <= 47: bg = NAMED[n-10]
                    elif 100 <= n <= 107: bg = NAMED[n-10]
                    elif n == 38 and k+1 < len(nums) and nums[k+1] == 2: fg = tuple(nums[k+2:k+5]); k += 4
                    elif n == 48 and k+1 < len(nums) and nums[k+1] == 2: bg = tuple(nums[k+2:k+5]); k += 4
                    elif n in (38,48) and k+1 < len(nums) and nums[k+1] == 5: k += 2
                    k += 1
            continue
        if ch == "\r": cx = 0; i += 1; continue
        if ch == "\n": cy = min(cy+1, ROWS-1); i += 1; continue
        if ch < " ": i += 1; continue
        if 0 <= cy < ROWS and 0 <= cx < COLS: grid[cy][cx] = (ch, fg, bg, bold)
        cx += 1
        if cx >= COLS: cx = 0; cy = min(cy+1, ROWS-1)
        i += 1
    return grid

reg = ImageFont.truetype(FONT % "Regular", 15)
bold = ImageFont.truetype(FONT % "Bold", 15)
os.makedirs(OUT, exist_ok=True)
end = stamps[-1][0]
n = 0
tick = 1.0 / FPS
start = 0.8                                  # let the first paint land
t = start
while t <= end:
    off = next((o for ts, o in reversed(stamps) if ts <= t), stamps[-1][1])
    # Snap back to the end of the last finished repaint. ratatui writes only
    # the cells that changed, so a frame cut anywhere else is half of one paint
    # and half of the one before it, and the seam shows.
    edge = raw.rfind(b"\x1b[?25l", 0, off)
    if edge <= 0:
        t += tick                    # nothing has been drawn yet
        continue
    g = screen(raw[:edge])
    PAD = 10
    img = Image.new("RGB", (COLS*CW + 2*PAD, ROWS*CH + 2*PAD), (0,0,0))
    d = ImageDraw.Draw(img)
    for y, row in enumerate(g):
        for x, (c, cfg, cbg, b) in enumerate(row):
            px, py = PAD + x*CW, PAD + y*CH
            if cbg: d.rectangle([px, py, px+CW-1, py+CH-1], fill=cbg)
            if c != " ": d.text((px, py+1), c, font=(bold if b else reg), fill=cfg)
    img.save(os.path.join(OUT, "f%04d.png" % n))
    n += 1; t += tick
print("%d frames at %.0f fps, %.1fs" % (n, FPS, n/FPS))
