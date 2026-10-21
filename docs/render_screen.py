#!/usr/bin/env python3
"""Turn a cell dump from `cargo run --example dump_screen` into a PNG that looks
like a terminal, so the README screenshots can be regenerated from the real UI.

    cargo run -q --example dump_screen -- popup | python3 docs/render_screen.py docs/popup.png
"""
import json
import sys

from PIL import Image, ImageDraw, ImageFont

# palette sampled from a real capture (docs/screenshot.png)
BG = "#292c33"
FG = {"default": "#d9dde3", "yellow": "#e5e54d", "green": "#5fd08a", "red": "#e5534b", "cyan": "#5cc3e8"}
DIM = "#7a7c80"
FONT = "/System/Library/Fonts/Menlo.ttc"
SIZE = 24
LINE = 27
PAD_X, PAD_Y = 10, 12

out = sys.argv[1]
cells = [json.loads(l) for l in sys.stdin if l.strip()]
cols = max(c["x"] for c in cells) + 1
rows = max(c["y"] for c in cells) + 1
regular = ImageFont.truetype(FONT, SIZE, index=0)
bold = ImageFont.truetype(FONT, SIZE, index=1)
cw = regular.getlength("M")
W = int(PAD_X * 2 + cols * cw)
H = int(PAD_Y * 2 + rows * LINE)
im = Image.new("RGB", (W, H), BG)
d = ImageDraw.Draw(im)
for c in cells:
    x = PAD_X + c["x"] * cw
    y = PAD_Y + c["y"] * LINE
    fg = DIM if c["dim"] else FG.get(c["fg"], FG["default"])
    bg = BG
    if c["rev"]:
        fg, bg = BG, (fg if fg != DIM else FG["default"])
    if bg != BG:
        d.rectangle([x, y, x + cw, y + LINE], fill=bg)
    if c["s"].strip():
        d.text((x, y + 2), c["s"], font=bold if c["bold"] else regular, fill=fg)
im.save(out, optimize=True)
print(f"wrote {out} ({W}x{H}, {cols}x{rows} cells)")
