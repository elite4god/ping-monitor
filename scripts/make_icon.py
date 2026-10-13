#!/usr/bin/env python3
"""Regenerates the app icon files (pure Python, no dependencies):
  wix/ping-monitor.ico     256x256 icon for the exe, MSI and shortcuts
  assets/icon-128.rgba     raw 128x128 RGBA window icon, embedded in the app
Run from the project root:  python3 scripts/make_icon.py
"""
import math, struct, zlib

BG, FG = (30, 37, 48), (90, 200, 120)
PTS = [(6, 36), (18, 36), (24, 16), (34, 50), (42, 28), (48, 36), (58, 36)]  # 64x64 viewbox


def render(size):
    """Returns raw RGBA bytes (straight alpha), row by row."""
    k = size / 64.0
    pts = [(x * k, y * k) for x, y in PTS]
    half, radius = 2.0 * k, 12 * k

    def seg_dist(px, py, a, b):
        ax, ay = a
        bx, by = b
        dx, dy = bx - ax, by - ay
        t = max(0, min(1, ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)))
        return math.hypot(px - (ax + t * dx), py - (ay + t * dy))

    out = bytearray()
    for y in range(size):
        for x in range(size):
            px, py = x + 0.5, y + 0.5
            cx = min(max(px, radius), size - radius)
            cy = min(max(py, radius), size - radius)
            a_bg = max(0.0, min(1.0, radius + 0.5 - math.hypot(px - cx, py - cy)))
            d = min(seg_dist(px, py, pts[i], pts[i + 1]) for i in range(len(pts) - 1))
            a_fg = max(0.0, min(1.0, half + 0.5 - d))
            col = [BG[i] * (1 - a_fg) + FG[i] * a_fg for i in range(3)]
            out += bytes(int(c) for c in col) + bytes([int(255 * a_bg)])
    return bytes(out)


def png(size, rgba):
    def chunk(t, d):
        c = struct.pack(">I", len(d)) + t + d
        return c + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)
    rows = b"".join(b"\x00" + rgba[y * size * 4:(y + 1) * size * 4] for y in range(size))
    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(rows, 9)) + chunk(b"IEND", b""))


big = png(256, render(256))
ico = struct.pack("<HHH", 0, 1, 1) + struct.pack("<BBBBHHII", 0, 0, 0, 0, 1, 32, len(big), 22) + big
open("wix/ping-monitor.ico", "wb").write(ico)
open("assets/icon-128.rgba", "wb").write(render(128))
print("wrote wix/ping-monitor.ico and assets/icon-128.rgba")
