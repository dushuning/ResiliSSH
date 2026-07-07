#!/usr/bin/env python3
"""Crop AI icon to 1024x1024 RGBA, removing letterbox bars."""
from __future__ import annotations

import sys
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_SRC = ROOT / "logo.png"
DEFAULT_OUT = ROOT / "src-tauri/icons/source-square.png"


def fix_icon(src: Path, out: Path) -> None:
    im = Image.open(src).convert("RGBA")
    w, h = im.size
    side = min(w, h)
    left = (w - side) // 2
    top = (h - side) // 2
    im = im.crop((left, top, left + side, top + side))
    im = im.resize((1024, 1024), Image.Resampling.LANCZOS)

    px = im.load()
    for y in range(im.height):
        for x in range(im.width):
            r, g, b, _ = px[x, y]
            if r < 20 and g < 20 and b < 20:
                px[x, y] = (0, 0, 0, 0)
            elif r > 240 and g > 240 and b > 240:
                px[x, y] = (0, 0, 0, 0)

    out.parent.mkdir(parents=True, exist_ok=True)
    im.save(out)


if __name__ == "__main__":
    src = Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_SRC
    out = Path(sys.argv[2]) if len(sys.argv) > 2 else DEFAULT_OUT
    fix_icon(src, out)
    print(f"fixed icon -> {out}")
