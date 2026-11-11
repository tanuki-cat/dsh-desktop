#!/usr/bin/env python3
"""Derive the alternative app icons from the default one.

The shell ships a handful of icon choices (see `src-tauri/src/icon.rs`) baked into the binary with
`include_bytes!`, so the variants live in the repository rather than being recoloured at runtime:
a theme that needs image maths on every launch would also need that maths to be identical on three
platforms.

Only the standard library is used. `make_icon.py` needs Pillow because it draws geometry; this
script only maps an existing bitmap through a colour ramp, which is a per-pixel loop over the
decoded pixels, so requiring a package for it would make `make icon-presets` fail on a machine
where `make icons` never runs.

The ramp is a three-stop duotone (shadow / mid / highlight) driven by relative luminance, not a
hue rotation: the source is photographic artwork, and rotating hues turns skin tones and the whale
hat into unrelated colours. Keeping the luminance structure and replacing only the colour keeps
the figure readable at 32 px, which is the size that actually matters in the Dock.

Usage: make-icon-presets.py <source.png> <output-dir> [size]
"""
import os
import struct
import sys
import zlib

# (shadow, mid, highlight) in sRGB. Each preset keeps the three-tone structure of the original —
# a dark tile, a mid-tone figure, bright highlights — so the artwork stays recognisable.
PRESETS = {
    # Deep sea: the shipped blue, pulled towards indigo and cooled down.
    "deep": ((12, 24, 58), (38, 94, 176), (222, 240, 255)),
    # Midnight: near-black tile with a slate figure, for a dark Dock.
    "night": ((8, 9, 12), (60, 64, 74), (226, 230, 236)),
    # Dawn: warm sand and amber, the counterweight to the two cold presets.
    "dawn": ((40, 22, 12), (176, 96, 44), (255, 244, 226)),
}

# Side length of the generated files. The Dock never shows more than 512 physical pixels for one
# tile, and every extra pixel is bytes inside the binary (the presets are `include_bytes!`).
DEFAULT_SIZE = 512


def read_png(path):
    """Decode an 8-bit RGBA PNG into (width, height, bytearray)."""
    data = open(path, "rb").read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise SystemExit(f"{path}: not a PNG")
    offset = 8
    header = None
    idat = bytearray()
    while offset < len(data):
        (length,) = struct.unpack(">I", data[offset : offset + 4])
        kind = data[offset + 4 : offset + 8]
        body = data[offset + 8 : offset + 8 + length]
        offset += 12 + length
        if kind == b"IHDR":
            header = struct.unpack(">IIBBBBB", body)
        elif kind == b"IDAT":
            idat += body
        elif kind == b"IEND":
            break
    if header is None:
        raise SystemExit(f"{path}: no IHDR")
    width, height, depth, colour, _, _, interlace = header
    if (depth, colour, interlace) != (8, 6, 0):
        # Re-encoding a palette or 16-bit source would be a different script (and Pillow); the
        # checked-in icon.png is 8-bit RGBA, and the gate below keeps it that way.
        raise SystemExit(f"{path}: expected 8-bit RGBA, got depth={depth} colour={colour}")
    raw = zlib.decompress(bytes(idat))
    stride = width * 4
    pixels = bytearray(width * height * 4)
    previous = bytearray(stride)
    for row in range(height):
        start = row * (stride + 1)
        filter_kind = raw[start]
        line = bytearray(raw[start + 1 : start + 1 + stride])
        if filter_kind == 1:
            for i in range(4, stride):
                line[i] = (line[i] + line[i - 4]) & 0xFF
        elif filter_kind == 2:
            for i in range(stride):
                line[i] = (line[i] + previous[i]) & 0xFF
        elif filter_kind == 3:
            for i in range(stride):
                left = line[i - 4] if i >= 4 else 0
                line[i] = (line[i] + ((left + previous[i]) >> 1)) & 0xFF
        elif filter_kind == 4:
            for i in range(stride):
                left = line[i - 4] if i >= 4 else 0
                up = previous[i]
                upleft = previous[i - 4] if i >= 4 else 0
                estimate = left + up - upleft
                pa, pb, pc = abs(estimate - left), abs(estimate - up), abs(estimate - upleft)
                if pa <= pb and pa <= pc:
                    predictor = left
                elif pb <= pc:
                    predictor = up
                else:
                    predictor = upleft
                line[i] = (line[i] + predictor) & 0xFF
        else:
            if filter_kind != 0:
                raise SystemExit(f"{path}: unknown row filter {filter_kind}")
        pixels[row * stride : (row + 1) * stride] = line
        previous = line
    return width, height, pixels


def write_png(path, width, height, pixels):
    stride = width * 4
    raw = bytearray()
    for row in range(height):
        raw.append(0)  # filter: none
        raw += pixels[row * stride : (row + 1) * stride]

    def chunk(kind, body):
        return (
            struct.pack(">I", len(body))
            + kind
            + body
            + struct.pack(">I", zlib.crc32(kind + body) & 0xFFFFFFFF)
        )

    out = b"\x89PNG\r\n\x1a\n"
    out += chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
    out += chunk(b"IDAT", zlib.compress(bytes(raw), 9))
    out += chunk(b"IEND", b"")
    with open(path, "wb") as handle:
        handle.write(out)


def luminance(r, g, b):
    """Relative luminance, with the sRGB transfer function folded in.

    A plain weighted average would wash the midtones out: the source artwork is mostly bright, so
    most pixels would land in the last third of the ramp and the three presets would look alike.
    """

    def channel(value):
        value /= 255.0
        return value / 12.92 if value <= 0.04045 else ((value + 0.055) / 1.055) ** 2.4

    return 0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)


def ramp(tones, level):
    """Position `level` (0..1) on a three-stop ramp, returning an sRGB triple."""
    if level <= 0.5:
        a, b = tones[0], tones[1]
        t = level * 2.0
    else:
        a, b = tones[1], tones[2]
        t = (level - 0.5) * 2.0
    return tuple(round(a[i] + (b[i] - a[i]) * t) for i in range(3))


def recolour(pixels, tones):
    out = bytearray(len(pixels))
    for i in range(0, len(pixels), 4):
        alpha = pixels[i + 3]
        if alpha == 0:
            continue
        level = luminance(pixels[i], pixels[i + 1], pixels[i + 2])
        # A slight contrast stretch: the source's background and figure are close in luminance in
        # places, and a flat map leaves the silhouette mushy at small sizes.
        level = min(1.0, max(0.0, (level - 0.08) / 0.84))
        r, g, b = ramp(tones, level)
        out[i] = r
        out[i + 1] = g
        out[i + 2] = b
        out[i + 3] = alpha
    return out


def shrink(width, height, pixels, size):
    """Box-filter downscale, in premultiplied alpha so transparent edges do not darken."""
    if width == size and height == size:
        return size, size, pixels
    if width % size or height % size:
        raise SystemExit(f"cannot box-filter {width}x{height} to {size}x{size}")
    factor = width // size
    out = bytearray(size * size * 4)
    for y in range(size):
        for x in range(size):
            r = g = b = a = 0
            for dy in range(factor):
                base = ((y * factor + dy) * width + x * factor) * 4
                for dx in range(factor):
                    i = base + dx * 4
                    alpha = pixels[i + 3]
                    r += pixels[i] * alpha
                    g += pixels[i + 1] * alpha
                    b += pixels[i + 2] * alpha
                    a += alpha
            count = factor * factor
            o = (y * size + x) * 4
            if a:
                out[o] = r // a
                out[o + 1] = g // a
                out[o + 2] = b // a
            out[o + 3] = a // count
    return size, size, out


def main():
    if len(sys.argv) < 3:
        raise SystemExit(__doc__.strip().splitlines()[-1])
    source, out_dir = sys.argv[1], sys.argv[2]
    size = int(sys.argv[3]) if len(sys.argv) > 3 else DEFAULT_SIZE
    width, height, pixels = read_png(source)
    if width != height:
        raise SystemExit(f"{source}: the app icon has to be square, got {width}x{height}")
    os.makedirs(out_dir, exist_ok=True)
    for name, tones in PRESETS.items():
        target = os.path.join(out_dir, f"{name}.png")
        out_w, out_h, out = shrink(width, height, recolour(pixels, tones), size)
        write_png(target, out_w, out_h, out)
        print(f"wrote {target} at {out_w}x{out_h}")


if __name__ == "__main__":
    main()
