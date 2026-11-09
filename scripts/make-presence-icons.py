#!/usr/bin/env python3
"""Draw the extension's off/on toolbar icons as PNGs, with no dependencies.

    python3 scripts/make-presence-icons.py

A flat hexagon with a white dot: slate when the extension is idle, blue when
it is reporting a Meet. Rendered at 4x and box-downsampled, so the small sizes
stay readable.
"""

from __future__ import annotations

import math
import struct
import zlib
from pathlib import Path

OUT = Path(__file__).resolve().parent.parent / "extensions" / "gmeet-presence" / "icons"
OFF_SIZES = (16, 32)
ON_SIZES = (16, 32, 128)
SCALE = 4
COLORS = {
    "off": (107, 114, 128, 255),
    "on": (37, 99, 235, 255),
}
DOT = (255, 255, 255, 255)


def hexagon(cx: float, cy: float, radius: float) -> list[tuple[float, float]]:
    return [
        (
            cx + radius * math.cos(math.radians(90 + 60 * corner)),
            cy - radius * math.sin(math.radians(90 + 60 * corner)),
        )
        for corner in range(6)
    ]


def inside(points: list[tuple[float, float]], x: float, y: float) -> bool:
    inside = False
    for index, (x1, y1) in enumerate(points):
        x2, y2 = points[(index + 1) % len(points)]
        if (y1 > y) != (y2 > y):
            crossing = x1 + (y - y1) * (x2 - x1) / (y2 - y1)
            if x < crossing:
                inside = not inside
    return inside


def render(size: int, color: tuple[int, int, int, int]) -> bytes:
    width = size * SCALE
    radius = width * 0.46
    center = width / 2
    shape = hexagon(center, center, radius)
    dot_radius = width * 0.13
    pixels = bytearray(width * width * 4)
    for y in range(width):
        for x in range(width):
            offset = (y * width + x) * 4
            in_dot = (x - center) ** 2 + (y - center) ** 2 <= dot_radius**2
            if in_dot:
                pixels[offset : offset + 4] = bytes(DOT)
            elif inside(shape, x + 0.5, y + 0.5):
                pixels[offset : offset + 4] = bytes(color)
    # Box-downsample by SCALE.
    out = bytearray(size * size * 4)
    for y in range(size):
        for x in range(size):
            total = [0, 0, 0, 0]
            for dy in range(SCALE):
                for dx in range(SCALE):
                    offset = ((y * SCALE + dy) * width + x * SCALE + dx) * 4
                    for channel in range(4):
                        total[channel] += pixels[offset + channel]
            offset = (y * size + x) * 4
            for channel in range(4):
                out[offset + channel] = total[channel] // (SCALE * SCALE)
    return bytes(out)


def png(size: int, rgba: bytes) -> bytes:
    raw = b"".join(
        b"\x00" + rgba[y * size * 4 : (y + 1) * size * 4] for y in range(size)
    )

    def chunk(kind: bytes, data: bytes) -> bytes:
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    for state, color in COLORS.items():
        sizes = OFF_SIZES if state == "off" else ON_SIZES
        for size in sizes:
            path = OUT / f"hex-{state}-{size}.png"
            path.write_bytes(png(size, render(size, color)))
            print(path.relative_to(OUT.parent.parent))


if __name__ == "__main__":
    main()
