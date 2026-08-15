#!/usr/bin/env python3
"""Render the AudioHub app icon + menu-bar template icon with the stdlib only.

No network, no third-party imaging libs: build-app.sh must work on a clean
machine. Output is deterministic, so re-running never churns the tree.

  icon.png   1024x1024 RGBA app master (sips/iconutil turn it into icon.icns)
  tray.png   44x44 macOS *template* image -- AppKit ignores RGB and keeps only
             alpha, so the glyph must carry the whole shape. For eyeballing.
             This is the `idle` state; the other three are .rgba only.

  tray-<state>.rgba    44x44 raw RGBA, one per STATES entry, include_bytes!'d by
             src-tauri/src/icon.rs. Raw so the shell needs no PNG decoder
             (tauri's image-png feature would drag the whole `image` crate in
             for one 44x44 glyph).

  dock-bg-<theme>.rgba   256x256 raw RGBA, the rounded-square plate only.
  dock-wave-<state>.a8   256x256 raw single-byte alpha, the mark only.

The dock tile ships as plate + mask rather than eight pre-composited images
because the two vary independently -- 2 plates + 4 masks is 786 KB embedded
where 8 composites would be 2 MB. src-tauri/src/icon.rs does the source-over.

# Why the states are amplitudes

Every state glyph is the same eight-point mark with its vertical excursion
scaled. Loud/quiet is the one visual axis an audio tool can spend without
inventing a vocabulary, and it survives being a pure alpha mask -- which the
macOS menu bar requires (see `TEMPLATE` note in src-tauri/src/icon.rs).
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

# (name, amplitude, alpha). Order is the wire order: it must match the
# `IconState` discriminants in src-tauri/src/icon.rs, which are const-asserted
# against the byte lengths but *not* against the ordering -- keep them aligned
# by hand.
STATES = [
    ("offline", 0.00, 0.55),
    ("connecting", 0.35, 0.80),
    ("idle", 0.70, 0.95),
    ("active", 1.00, 1.00),
]

# Dock plate gradients: (top, bottom). Dark matches the UI shell's --bg-1 ->
# --bg; light matches the light theme's equivalent pair.
THEMES = {
    "dark": ((0x1F, 0x24, 0x2E), (0x0F, 0x11, 0x15)),
    "light": ((0xF7, 0xF9, 0xFB), (0xE4, 0xE8, 0xEE)),
}

DOCK_PX = 256


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


def wave_points(size, pad, amp=1.0):
    """Map the 24-unit mark into a `size` canvas, vertically centred on its bbox.

    `amp` scales the vertical excursion about that centre: 1.0 is the brand
    mark as drawn, 0.0 collapses it to a flat line. Horizontal extent and
    stroke width never change, so every state occupies the same box.
    """
    inner = size - 2 * pad
    scale = inner / 24.0
    ys = [p[1] for p in WAVE]
    cy = (min(ys) + max(ys)) / 2.0
    half = size / 2.0
    return [
        (half + (x - 12.0) * scale, half + (y - cy) * amp * scale) for x, y in WAVE
    ], scale


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


def render_tray(size=44, amp=1.0, alpha=1.0):
    """Glyph only: a background would render as a solid block in the menu bar.

    RGB is forced to zero and the whole shape lives in alpha, which is what a
    macOS template image is. Windows has no template concept, so icon.rs tints
    the same mask at runtime instead of shipping a second polarity.
    """
    px = bytearray(size * size * 4)
    pts, scale = wave_points(size, 4, amp)
    rad = max(1.15, WAVE_STROKE * scale / 2.0)
    for y in range(size):
        row = y * size * 4
        for x in range(size):
            c = stroke_coverage(pts, rad, x + 0.5, y + 0.5)
            if c > 0:
                i = row + x * 4
                px[i] = px[i + 1] = px[i + 2] = 0
                px[i + 3] = int(round(c * alpha * 255))
    return px


def render_dock_plate(size, top, bottom):
    """The rounded-square gradient plate, no mark. Same geometry as the master."""
    px = bytearray(size * size * 4)
    m = size * 100 // 1024
    x0, y0, x1, y1 = m, m, size - m, size - m
    r = size * 185 / 1024.0
    for y in range(max(0, y0 - 2), min(size, y1 + 2)):
        yy = y + 0.5
        t = max(0.0, min(1.0, (yy - y0) / float(y1 - y0)))
        bg = tuple(int(top[k] + (bottom[k] - top[k]) * t) for k in range(3))
        row = y * size * 4
        for x in range(max(0, x0 - 2), min(size, x1 + 2)):
            c = rrect_coverage(x + 0.5, yy, x0, y0, x1, y1, r)
            if c > 0:
                blend(px, size, row + x * 4, bg, c)
    return px


def render_dock_wave(size, amp):
    """Single-byte alpha mask of the mark, positioned as in the app master."""
    a = bytearray(size * size)
    pts, scale = wave_points(size, size * 220 // 1024, amp)
    rad = WAVE_STROKE * scale / 2.0
    for y in range(size):
        row = y * size
        for x in range(size):
            c = stroke_coverage(pts, rad, x + 0.5, y + 0.5)
            if c > 0:
                a[row + x] = int(round(c * 255))
    return a


def render_logo(size=512):
    """The mark alone, accent-coloured on transparency — the README logo.

    A third rendering rather than a reuse, because the other two are both wrong
    for a document that is read on two backgrounds:

      * `icon.png` carries its own dark plate. That is right for a dock tile and
        wrong here: on GitHub's light theme it reads as a black box pasted on
        the page.
      * `render_tray` forces RGB to zero, because a macOS template image is
        alpha-only and AppKit supplies the colour. Dropped into a README it is a
        black mark, invisible on the dark theme.

    Accent on transparency clears both: #31C8B0 holds against white and against
    #0d1117, so one file serves both themes and there is no `<picture>` with a
    `prefers-color-scheme` pair to keep in sync.

    Geometry comes from the same WAVE/wave_points as the app icon and the tray
    glyph, so the logo cannot drift away from the thing it stands for.
    """
    px = bytearray(size * size * 4)
    # Proportionally the tray's padding (4/44), which frames the mark without
    # the dock tile's much larger inset.
    pts, scale = wave_points(size, size * 4 // 44, 1.0)
    rad = WAVE_STROKE * scale / 2.0
    for y in range(size):
        row = y * size * 4
        for x in range(size):
            c = stroke_coverage(pts, rad, x + 0.5, y + 0.5)
            if c > 0:
                i = row + x * 4
                px[i], px[i + 1], px[i + 2] = ACCENT
                px[i + 3] = int(round(c * 255))
    return px


TRAY_PX = 44  # keep in sync with TRAY_PX in src-tauri/src/icon.rs (const-asserted)


def main():
    here = Path(__file__).resolve().parent
    write_png(here / "icon.png", 1024, 1024, render_app_icon(1024))
    # Outside this directory on purpose: `assets/` is where a reader looks for a
    # README image, while everything else here is an application asset consumed
    # by the bundler or include_bytes!'d by icon.rs. Generated from the same
    # geometry all the same, so the README can never show a stale mark.
    logo = here.parents[2] / "assets" / "logo.png"
    logo.parent.mkdir(parents=True, exist_ok=True)
    write_png(logo, 512, 512, render_logo(512))

    for name, amp, alpha in STATES:
        tray = render_tray(TRAY_PX, amp, alpha)
        (here / f"tray-{name}.rgba").write_bytes(bytes(tray))
        if name == "idle":
            write_png(here / "tray.png", TRAY_PX, TRAY_PX, tray)
        (here / f"dock-wave-{name}.a8").write_bytes(
            bytes(render_dock_wave(DOCK_PX, amp))
        )

    for theme, (top, bottom) in THEMES.items():
        plate = render_dock_plate(DOCK_PX, top, bottom)
        (here / f"dock-bg-{theme}.rgba").write_bytes(bytes(plate))

    states = " ".join(n for n, _, _ in STATES)
    print(
        f"icon.png 1024x1024; tray-*.rgba {TRAY_PX}x{TRAY_PX} [{states}]; "
        f"dock-bg-*/dock-wave-* {DOCK_PX}x{DOCK_PX} -> {here}"
    )


if __name__ == "__main__":
    sys.exit(main())
