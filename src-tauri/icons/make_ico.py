#!/usr/bin/env python3
"""Render icon.ico (Windows) from icon.png.

tauri-build refuses to build for a Windows target without `icons/icon.ico` (it embeds the
icon into the .exe resource section). Pillow writes a multi-size .ico in one shot.

Usage: make_ico.py [output.ico] [source.png]
"""
import sys
from PIL import Image

SIZES = [(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)]


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else "icon.ico"
    src = sys.argv[2] if len(sys.argv) > 2 else "icon.png"
    image = Image.open(src)
    image.save(out, format="ICO", sizes=SIZES)
    joined = ", ".join(f"{w}x{h}" for w, h in SIZES)
    print(f"wrote {out} from {src} ({joined})")


if __name__ == "__main__":
    main()
