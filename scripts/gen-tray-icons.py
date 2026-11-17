#!/usr/bin/env python3
"""Generate the punktfunk-tray status icons (committed, like the other branding assets).

Renders the brand mark — the two overlapping circles ("lens") from web's brand-mark.tsx, the
same geometry gen-branding.ps1 uses — with a status dot in the lower-right corner:

  running    colored mark + green dot
  stopped    grayscale mark + gray dot
  error      colored mark + red dot
  degraded   colored mark + amber dot        (starting / running-but-status-unreachable)
  streaming  colored mark + bright-violet dot

Outputs (all checked in; re-run only when the brand or the palette changes):
  packaging/windows/branding/punktfunk-tray-<state>.ico   16/20/24/32/48 px PNG-entry icos
                                                          (Vista+ format)
  packaging/linux/icons/hicolor/{22x22,48x48}/apps/punktfunk-tray[-<state>].png
                                                          (running is the unsuffixed base name)

Pure stdlib (zlib PNG writer, analytic 4x-supersampled rasterizer) so it runs on any dev box —
no PIL/ImageMagick/librsvg needed.
"""

import math
import struct
import zlib
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent

# Brand-mark geometry in its 1000-unit viewbox (brand-mark.tsx; mirrors gen-branding.ps1).
R = 194.41
C1 = (403.037, 597.262)  # light circle, behind
C2 = (597.8075, 402.8525)  # deep circle, in front
BB_MIN = (C1[0] - R, C2[1] - R)
BB_MAX = (C2[0] + R, C1[1] + R)
MARK_CENTER = ((BB_MIN[0] + BB_MAX[0]) / 2, (BB_MIN[1] + BB_MAX[1]) / 2)
MARK_SPAN = BB_MAX[0] - BB_MIN[0]  # the bbox is square

COL_LIGHT = (0xA7, 0x9F, 0xF8)
COL_DEEP = (0x6C, 0x5B, 0xF3)
COL_HI = (0xD2, 0xC9, 0xFB)
RING = (0x1C, 0x15, 0x30)  # dot outline, the brand tile background

STATES = {
    "running": {"dot": (0x2E, 0xCC, 0x71), "gray": False},
    "stopped": {"dot": (0x8A, 0x8A, 0x8A), "gray": True},
    "error": {"dot": (0xE7, 0x4C, 0x3C), "gray": False},
    "degraded": {"dot": (0xF0, 0xA0, 0x30), "gray": False},
    "streaming": {"dot": (0xB4, 0x4C, 0xF0), "gray": False},
}


def luma(c):
    y = round(0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2])
    return (y, y, y)


def render(size, dot_rgb, gray, ss=4):
    """RGBA rows, 4x supersampled: mark centered upper-left-ish, dot lower-right."""
    n = size * ss
    mark_c = (0.44 * n, 0.44 * n)
    scale = (0.82 * n) / MARK_SPAN
    dot_c = (0.76 * n, 0.76 * n)
    dot_r = 0.21 * n
    ring_r = dot_r + max(0.055 * n, 1.0 * ss)
    c_light = luma(COL_LIGHT) if gray else COL_LIGHT
    c_deep = luma(COL_DEEP) if gray else COL_DEEP
    c_hi = luma(COL_HI) if gray else COL_HI
    c1 = (mark_c[0] + (C1[0] - MARK_CENTER[0]) * scale, mark_c[1] + (C1[1] - MARK_CENTER[1]) * scale)
    c2 = (mark_c[0] + (C2[0] - MARK_CENTER[0]) * scale, mark_c[1] + (C2[1] - MARK_CENTER[1]) * scale)
    r = R * scale

    rows = []
    for y in range(size):
        row = bytearray()
        for x in range(size):
            # Premultiplied accumulation over the ss×ss sample grid (no fringe on the rim).
            ar = ag = ab = aa = 0.0
            for sy in range(ss):
                for sx in range(ss):
                    px = x * ss + sx + 0.5
                    py = y * ss + sy + 0.5
                    d1 = math.hypot(px - c1[0], py - c1[1])
                    d2 = math.hypot(px - c2[0], py - c2[1])
                    dd = math.hypot(px - dot_c[0], py - dot_c[1])
                    col = None
                    if dd < dot_r:
                        col = dot_rgb
                    elif dd < ring_r:
                        col = RING
                    elif d1 < r and d2 < r:
                        col = c_hi
                    elif d2 < r:
                        col = c_deep
                    elif d1 < r:
                        col = c_light
                    if col is not None:
                        ar += col[0]
                        ag += col[1]
                        ab += col[2]
                        aa += 255.0
            samples = ss * ss
            a = aa / samples
            if a < 1.0:
                row += b"\x00\x00\x00\x00"
            else:
                row += bytes(
                    (round(ar / aa * 255), round(ag / aa * 255), round(ab / aa * 255), round(a))
                )
        rows.append(bytes(row))
    return rows


def png_bytes(size, rows):
    def chunk(tag, data):
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    idat = zlib.compress(b"".join(b"\x00" + r for r in rows), 9)
    return (
        b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", idat) + chunk(b"IEND", b"")
    )


def ico_bytes(pngs):
    """PNG-entry .ico (Vista+)."""
    header = struct.pack("<HHH", 0, 1, len(pngs))
    entries = b""
    blobs = b""
    offset = len(header) + 16 * len(pngs)
    for size, png in pngs:
        entries += struct.pack(
            "<BBBBHHII", size if size < 256 else 0, size if size < 256 else 0, 0, 0, 1, 32, len(png), offset
        )
        blobs += png
        offset += len(png)
    return header + entries + blobs


def main():
    ico_dir = REPO / "packaging/windows/branding"
    for state, spec in STATES.items():
        pngs = [
            (s, png_bytes(s, render(s, spec["dot"], spec["gray"])))
            for s in (16, 20, 24, 32, 48)
        ]
        out = ico_dir / f"punktfunk-tray-{state}.ico"
        out.write_bytes(ico_bytes(pngs))
        print(f"wrote {out.relative_to(REPO)}")

        for s in (22, 48):
            name = "punktfunk-tray" if state == "running" else f"punktfunk-tray-{state}"
            png_dir = REPO / f"packaging/linux/icons/hicolor/{s}x{s}/apps"
            png_dir.mkdir(parents=True, exist_ok=True)
            out = png_dir / f"{name}.png"
            out.write_bytes(png_bytes(s, render(s, spec["dot"], spec["gray"])))
            print(f"wrote {out.relative_to(REPO)}")


if __name__ == "__main__":
    main()
