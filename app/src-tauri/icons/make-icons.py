#!/usr/bin/env python3
"""Render the AudioHub app icon + menu-bar template icon with the stdlib only.

No network, no third-party imaging libs: build-app.sh must work on a clean
machine. Output is deterministic, so re-running never churns the tree.

  icon.png   1024x1024 RGBA app master (sips/iconutil turn it into icon.icns)
  tray.png   44x44 macOS *template* image -- AppKit ignores RGB and keeps only
             alpha, so the glyph must carry the whole shape. For eyeballing.
  tray.rgba  the same pixels raw, include_bytes!'d by src-tauri/src/main.rs so
             the shell needs no PNG decoder (tauri's image-png feature would
             drag in the whole `image` crate for one 44x44 glyph).
"""

import math
import struct
import sys
import zlib
from pathlib import Path

ACCENT = (0x31, 0xC8, 0xB0)

# The brand mark, in the 24x24 viewBox shared with ui/index.html.
WAVE = [(3, 12), (5, 12), (7, 7), (10, 17), (13, 3), (16, 15), (18, 12), (21, 12)]
WAVE_STROKE = 1.8  # viewBox units


def write_png(path, w, h, px):
    """px: bytearray of w*h*4 RGBA, non-premultiplied."""
    raw = bytearray()
    stride = w * 4
    for y in range(h):
        raw.append(0)  # filter type 0; the images are small enough not to care
        raw += px[y * stride:(y + 1) * stride]

    def chunk(tag, data):
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    out = b"\x89PNG\r\n\x1a\n"
    out += chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
    out += chunk(b"IDAT", zlib.compress(bytes(raw), 9))
    out += chunk(b"IEND", b"")
    path.write_bytes(out)


def seg_dist(px, py, ax, ay, bx, by):
    vx, vy = bx - ax, by - ay
    wx, wy = px - ax, py - ay
    L = vx * vx + vy * vy
    t = 0.0 if L <= 0 else max(0.0, min(1.0, (wx * vx + wy * vy) / L))
    dx, dy = wx - t * vx, wy - t * vy
    return math.hypot(dx, dy)


def stroke_coverage(pts, radius, x, y):
    """Analytic AA: coverage falls off over one pixel around the stroke edge.

    Round joins/caps come for free from taking the min over segment distances.
    """
    d = min(seg_dist(x, y, *pts[i], *pts[i + 1]) for i in range(len(pts) - 1))
    return max(0.0, min(1.0, radius - d + 0.5))


def rrect_coverage(x, y, x0, y0, x1, y1, r):
    cx = max(x0 + r, min(x1 - r, x))
    cy = max(y0 + r, min(y1 - r, y))
    d = math.hypot(x - cx, y - cy) - r
    return max(0.0, min(1.0, 0.5 - d))


def blend(px, w, idx, rgb, a):
    """Source-over onto non-premultiplied RGBA."""
    dr, dg, db, da = px[idx], px[idx + 1], px[idx + 2], px[idx + 3]
    na = a + da / 255.0 * (1 - a)
    if na <= 0:
        return
    for k in range(3):
        s = rgb[k] / 255.0
        d = px[idx + k] / 255.0
        px[idx + k] = int(round(((s * a + d * (da / 255.0) * (1 - a)) / na) * 255))
    px[idx + 3] = int(round(na * 255))


def wave_points(size, pad):
    """Map the 24-unit mark into a `size` canvas, vertically centred on its bbox."""
    inner = size - 2 * pad
    scale = inner / 24.0
    ys = [p[1] for p in WAVE]
    cy = (min(ys) + max(ys)) / 2.0
    half = size / 2.0
    return [(half + (x - 12.0) * scale, half + (y - cy) * scale) for x, y in WAVE], scale


def render_app_icon(size=1024):
    px = bytearray(size * size * 4)
    # macOS icon grid: art sits in a rounded square inset from the full canvas.
    m = size * 100 // 1024
    x0, y0, x1, y1 = m, m, size - m, size - m
    r = size * 185 / 1024.0

    for y in range(y0 - 2, y1 + 2):
        yy = y + 0.5
        # Vertical gradient matching the UI shell (--bg-1 -> --bg).
        t = (yy - y0) / float(y1 - y0)
        t = max(0.0, min(1.0, t))
        bg = (int(0x1F + (0x0F - 0x1F) * t),
              int(0x24 + (0x11 - 0x24) * t),
              int(0x2E + (0x15 - 0x2E) * t))
        row = y * size * 4
        for x in range(x0 - 2, x1 + 2):
            c = rrect_coverage(x + 0.5, yy, x0, y0, x1, y1, r)
            if c > 0:
                blend(px, size, row + x * 4, bg, c)

    pts, scale = wave_points(size, size * 220 // 1024)
    rad = WAVE_STROKE * scale / 2.0
    bx0 = int(min(p[0] for p in pts) - rad - 2)
    bx1 = int(max(p[0] for p in pts) + rad + 2)
    by0 = int(min(p[1] for p in pts) - rad - 2)
    by1 = int(max(p[1] for p in pts) + rad + 2)
    for y in range(max(0, by0), min(size, by1)):
        row = y * size * 4
        for x in range(max(0, bx0), min(size, bx1)):
            c = stroke_coverage(pts, rad, x + 0.5, y + 0.5)
            if c > 0:
                blend(px, size, row + x * 4, ACCENT, c)
    return px


def render_tray(size=44):
    """Glyph only: a background would render as a solid block in the menu bar."""
    px = bytearray(size * size * 4)
    pts, scale = wave_points(size, 4)
    rad = max(1.15, WAVE_STROKE * scale / 2.0)
    for y in range(size):
        row = y * size * 4
        for x in range(size):
            c = stroke_coverage(pts, rad, x + 0.5, y + 0.5)
            if c > 0:
                i = row + x * 4
                px[i] = px[i + 1] = px[i + 2] = 0
                px[i + 3] = int(round(c * 255))
    return px


TRAY_PX = 44  # keep in sync with TRAY_PX in src-tauri/src/main.rs (const-asserted)


def main():
    here = Path(__file__).resolve().parent
    write_png(here / "icon.png", 1024, 1024, render_app_icon(1024))
    tray = render_tray(TRAY_PX)
    write_png(here / "tray.png", TRAY_PX, TRAY_PX, tray)
    (here / "tray.rgba").write_bytes(bytes(tray))
    print(f"icon.png 1024x1024, tray.png/tray.rgba {TRAY_PX}x{TRAY_PX} -> {here}")


if __name__ == "__main__":
    sys.exit(main())
