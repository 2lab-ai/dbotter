#!/usr/bin/env python3
"""Build a deterministic modern ICNS container from one exact iconset."""

from __future__ import annotations

import argparse
import os
import pathlib
import struct
import sys


PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"
ENTRIES = (
    ("icp4", "icon_16x16.png", 16),
    ("ic11", "icon_16x16@2x.png", 32),
    ("icp5", "icon_32x32.png", 32),
    ("ic12", "icon_32x32@2x.png", 64),
    ("ic07", "icon_128x128.png", 128),
    ("ic13", "icon_128x128@2x.png", 256),
    ("ic08", "icon_256x256.png", 256),
    ("ic14", "icon_256x256@2x.png", 512),
    ("ic09", "icon_512x512.png", 512),
    ("ic10", "icon_512x512@2x.png", 1024),
)


class BuildError(ValueError):
    pass


def read_png(path: pathlib.Path, expected_size: int) -> bytes:
    if path.is_symlink() or not path.is_file():
        raise BuildError(f"iconset member must be a regular file, not a link: {path}")
    try:
        data = path.read_bytes()
    except OSError as error:
        raise BuildError(f"iconset member is unreadable: {path}: {error}") from error
    if len(data) < 33 or data[:8] != PNG_SIGNATURE or data[12:16] != b"IHDR":
        raise BuildError(f"iconset member is not a canonical PNG: {path}")
    width, height, bit_depth, color_type = struct.unpack(">IIBB", data[16:26])
    if (width, height) != (expected_size, expected_size):
        raise BuildError(
            f"iconset member has dimensions {width}x{height}, expected "
            f"{expected_size}x{expected_size}: {path}"
        )
    if bit_depth != 8 or color_type not in {2, 6}:
        raise BuildError(f"iconset member must be 8-bit RGB or RGBA: {path}")
    return data


def build(iconset: pathlib.Path) -> bytes:
    if iconset.is_symlink() or not iconset.is_dir():
        raise BuildError("--iconset must be a real directory, not a link")
    expected_names = {name for _, name, _ in ENTRIES}
    actual_names = {path.name for path in iconset.iterdir()}
    if actual_names != expected_names:
        raise BuildError(
            f"iconset has wrong members; missing={sorted(expected_names - actual_names)}, "
            f"extra={sorted(actual_names - expected_names)}"
        )
    chunks: list[bytes] = []
    for kind, name, size in ENTRIES:
        payload = read_png(iconset / name, size)
        chunks.append(kind.encode("ascii") + struct.pack(">I", len(payload) + 8) + payload)
    body = b"".join(chunks)
    return b"icns" + struct.pack(">I", len(body) + 8) + body


def write_no_replace(data: bytes, output: pathlib.Path) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    descriptor = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
    try:
        with os.fdopen(descriptor, "wb") as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
    except BaseException:
        output.unlink(missing_ok=True)
        raise


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--iconset", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    args = parser.parse_args()
    try:
        write_no_replace(build(args.iconset), args.output)
    except (BuildError, FileExistsError, OSError) as error:
        print(f"ICNS build: {error}", file=sys.stderr)
        return 1
    print(f"ICNS build: ok: {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
