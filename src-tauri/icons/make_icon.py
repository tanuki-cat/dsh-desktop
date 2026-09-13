#!/usr/bin/env python3
"""Render the DSH Desktop app icon: a stylized black orca on a light rounded tile.

Self-drawn geometry (no third-party artwork). 4x supersampling, auto-centred.
Usage: make_icon.py <output.png> [size]"""
import sys
from PIL import Image, ImageChops, ImageDraw

SS = 4


def bezier(points, steps=72):
    out = [points[0]]
    i = 1
    while i + 2 <= len(points) - 1:
        p0 = out[-1]
        c1, c2, p1 = points[i], points[i + 1], points[i + 2]
        for s in range(1, steps + 1):
            t = s / steps
            mt = 1 - t
            x = (mt ** 3) * p0[0] + 3 * (mt ** 2) * t * c1[0] + 3 * mt * (t ** 2) * c2[0] + (t ** 3) * p1[0]
            y = (mt ** 3) * p0[1] + 3 * (mt ** 2) * t * c1[1] + 3 * mt * (t ** 2) * c2[1] + (t ** 3) * p1[1]
            out.append((x, y))
        i += 3
    return out


# --- whale geometry, drawn in a 1024 grid (facing left) --------------------------
SHAPES = {
    'tail': [
        (660, 470),
        (726, 430), (772, 372), (790, 306),
        (828, 372), (828, 444), (790, 494),
        (846, 500), (890, 540), (912, 604),
        (842, 600), (778, 566), (726, 512),
    ],
    'dorsal': [
        (470, 366),
        (492, 286), (556, 250), (618, 292),
        (574, 320), (516, 344), (492, 386),
    ],
    'body': [
        (286, 552),
        (286, 436), (388, 350), (516, 350),
        (648, 350), (730, 424), (726, 524),
        (722, 620), (622, 676), (498, 676),
        (374, 676), (286, 656), (286, 552),
    ],
    'flipper': [
        (452, 596),
        (516, 642), (582, 656), (626, 630),
        (570, 606), (500, 584), (452, 596),
    ],
}
EYE = (410, 466, 34, 20, -16)


def bbox(pts):
    xs = [p[0] for p in pts]
    ys = [p[1] for p in pts]
    return min(xs), min(ys), max(xs), max(ys)


def flattened():
    return {name: bezier(shape) for name, shape in SHAPES.items()}


def offset_for(flat, scale):
    xs0 = min(bbox(p)[0] for p in flat.values())
    ys0 = min(bbox(p)[1] for p in flat.values())
    xs1 = max(bbox(p)[2] for p in flat.values())
    ys1 = max(bbox(p)[3] for p in flat.values())
    cx, cy = (xs0 + xs1) / 2, (ys0 + ys1) / 2
    return (512 - cx) * scale, (516 - cy) * scale


def shifted(points, dx, dy):
    return [(x + dx, y + dy) for x, y in points]


def render(size):
    scale = size * SS / 1024.0
    canvas = size * SS
    flat = flattened()
    flat = {name: [(512 + (x - 512) * 0.94, 516 + (y - 516) * 0.94) for x, y in pts]
            for name, pts in flat.items()}
    dx, dy = offset_for(flat, scale)
    def poly(name, colour):
        pts = [(x * scale + dx, y * scale + dy) for x, y in flat[name]]
        draw.polygon(pts, fill=colour)
    img = Image.new('RGBA', (canvas, canvas), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)
    draw.rounded_rectangle([40 * scale, 40 * scale, 984 * scale, 984 * scale], radius=212 * scale, fill=(241, 242, 244, 255))
    ink = (17, 18, 20, 255)
    white = (255, 255, 255, 255)
    poly('tail', ink)
    poly('dorsal', ink)
    poly('body', ink)
    # Belly = body ∩ a big ellipse below it, so the patch boundary is a smooth arc.
    body_mask = Image.new('L', (canvas, canvas), 0)
    ImageDraw.Draw(body_mask).polygon(
        [(x * scale + dx, y * scale + dy) for x, y in flat['body']], fill=255
    )
    belly_mask = Image.new('L', (canvas, canvas), 0)
    ImageDraw.Draw(belly_mask).ellipse(
        [(520 - 430) * scale + dx, (900 - 420) * scale + dy,
         (520 + 430) * scale + dx, (900 + 420) * scale + dy], fill=255
    )
    img.paste(white, mask=ImageChops.multiply(body_mask, belly_mask))
    poly('flipper', ink)
    cx, cy, rx, ry, angle = EYE
    w, h = int(rx * 2.4 * scale), int(ry * 2.4 * scale)
    eye = Image.new('RGBA', (w, h), (0, 0, 0, 0))
    ImageDraw.Draw(eye).ellipse([0, 0, w - 1, h - 1], fill=white)
    eye = eye.rotate(-angle, expand=True, resample=Image.BICUBIC)
    img.alpha_composite(eye, (int(cx * scale + dx - eye.width / 2), int(cy * scale + dy - eye.height / 2)))
    return img.resize((size, size), Image.LANCZOS)


if __name__ == '__main__':
    out = sys.argv[1] if len(sys.argv) > 1 else 'icon.png'
    size = int(sys.argv[2]) if len(sys.argv) > 2 else 1024
    render(size).save(out)
    print(f'wrote {out} at {size}x{size}')
