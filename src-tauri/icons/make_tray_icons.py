"""Draws Picard's menu bar icons as 44x44 black-on-transparent template PNGs.

macOS tints template images for light and dark menu bars, so only the alpha
channel matters. Run from this folder with the standard library only:

    python3 make_tray_icons.py

Writes tray-ok.png, tray-low.png and tray-cleaning.png next to this file.
"""

import struct
import sys
import zlib
from pathlib import Path

SIZE = 44


def inside_rounded_rect(x, y, x0, y0, x1, y1, r):
    cx = min(max(x, x0 + r), x1 - r)
    cy = min(max(y, y0 + r), y1 - r)
    return (x - cx) ** 2 + (y - cy) ** 2 <= r * r


def drive_outline(x, y):
    outer = inside_rounded_rect(x, y, 3, 11, 41, 33, 5)
    inner = inside_rounded_rect(x, y, 6.5, 14.5, 37.5, 29.5, 2)
    led = (x - 32) ** 2 + (y - 22) ** 2 <= 2.6**2
    slot = 10 <= x <= 24 and 20.5 <= y <= 23.5
    return (outer and not inner) or led or slot


def drive_full(x, y):
    """A solid drive with the slot and light cut out: the disk is full."""
    outer = inside_rounded_rect(x, y, 3, 11, 41, 33, 5)
    led = (x - 32) ** 2 + (y - 22) ** 2 <= 2.6**2
    slot = 10 <= x <= 24 and 20.5 <= y <= 23.5
    return outer and not led and not slot


def drive_cleaning(x, y):
    """The drive half filled: between ok (empty outline) and low (solid)."""
    if drive_outline(x, y):
        return True
    inner = inside_rounded_rect(x, y, 6.5, 14.5, 37.5, 29.5, 2)
    slot = 10 <= x <= 24 and 20.5 <= y <= 23.5
    return inner and x <= 22 and not slot


def render(shape):
    rows = []
    for y in range(SIZE):
        row = b"\x00"
        for x in range(SIZE):
            alpha = 255 if shape(x + 0.5, y + 0.5) else 0
            row += bytes((0, 0, 0, alpha))
        rows.append(row)
    raw = b"".join(rows)

    def chunk(kind, data):
        return (
            struct.pack(">I", len(data))
            + kind
            + data
            + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
        )

    header = struct.pack(">IIBBBBB", SIZE, SIZE, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def main():
    here = Path(sys.argv[0]).resolve().parent
    for name, shape in [
        ("tray-ok.png", drive_outline),
        ("tray-low.png", drive_full),
        ("tray-cleaning.png", drive_cleaning),
    ]:
        (here / name).write_bytes(render(shape))
        print(f"wrote {name}")


if __name__ == "__main__":
    main()
