#!/usr/bin/env python3
"""Fail closed when a private workspace contains forbidden runtime values."""

from __future__ import annotations

import argparse
import base64
import json
import os
import stat
import sys
from pathlib import Path

MAX_FILE_BYTES = 32 * 1024 * 1024
MAX_TREE_BYTES = 128 * 1024 * 1024
MAX_ENTRIES = 25_000
MAX_DEPTH = 64
MAX_NEEDLE_BYTES = 64 * 1024
READ_BYTES = 64 * 1024


class ScanError(ValueError):
    pass


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", required=True)
    parser.add_argument(
        "--forbidden-env",
        action="append",
        default=[],
        dest="forbidden_env",
    )
    return parser.parse_args()


def forbidden_values(names: list[str]) -> list[bytes]:
    values: list[bytes] = []
    for name in names:
        if not name or any(
            character not in "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_"
            for character in name
        ):
            raise ScanError("forbidden environment name is invalid")
        value = os.environ.get(name)
        if value is None or value == "":
            raise ScanError("forbidden environment value is unavailable")
        encoded = value.encode("utf-8")
        if len(encoded) > MAX_NEEDLE_BYTES:
            raise ScanError("forbidden environment value exceeds the scan bound")
        variants = [
            encoded,
            json.dumps(value, ensure_ascii=True)[1:-1].encode("utf-8"),
            json.dumps(value, ensure_ascii=False)[1:-1].encode("utf-8"),
            base64.b64encode(encoded),
            encoded.hex().encode("ascii"),
        ]
        for variant in variants:
            if variant and variant not in values:
                values.append(variant)
    if not values:
        raise ScanError("at least one forbidden environment value is required")
    return values


def contains_forbidden(candidate: bytes, needles: list[bytes]) -> bool:
    return any(needle in candidate for needle in needles)


def scan_name(name: str, needles: list[bytes]) -> None:
    if contains_forbidden(os.fsencode(name), needles):
        raise ScanError("workspace entry name contains a forbidden runtime value")


def scan_file(directory_fd: int, name: str, needles: list[bytes]) -> int:
    flags = os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW
    descriptor = os.open(name, flags, dir_fd=directory_fd)
    try:
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode):
            raise ScanError("workspace contains a non-regular entry")
        if stat.S_IMODE(before.st_mode) != 0o600:
            raise ScanError("workspace file mode is not 0600")
        if before.st_uid != os.geteuid() or before.st_nlink != 1:
            raise ScanError("workspace file ownership or link count is unsafe")
        if before.st_size > MAX_FILE_BYTES:
            raise ScanError("workspace file exceeds the per-file scan bound")

        carry = b""
        carry_bytes = max(len(needle) for needle in needles) - 1
        bytes_read = 0
        while chunk := os.read(descriptor, READ_BYTES):
            bytes_read += len(chunk)
            if bytes_read > MAX_FILE_BYTES:
                raise ScanError("workspace file grew beyond the per-file scan bound")
            candidate = carry + chunk
            if contains_forbidden(candidate, needles):
                raise ScanError("workspace contains a forbidden runtime value")
            carry = candidate[-carry_bytes:] if carry_bytes > 0 else b""

        after = os.fstat(descriptor)
        if (
            before.st_dev != after.st_dev
            or before.st_ino != after.st_ino
            or before.st_size != after.st_size
            or before.st_mtime_ns != after.st_mtime_ns
            or before.st_ctime_ns != after.st_ctime_ns
            or bytes_read != after.st_size
        ):
            raise ScanError("workspace file changed during the scan")
        return bytes_read
    finally:
        os.close(descriptor)


def scan_directory(
    directory_fd: int,
    needles: list[bytes],
    depth: int,
    totals: dict[str, int],
) -> None:
    if depth > MAX_DEPTH:
        raise ScanError("workspace exceeds the directory depth bound")
    metadata = os.fstat(directory_fd)
    if not stat.S_ISDIR(metadata.st_mode):
        raise ScanError("workspace walk reached a non-directory")
    if stat.S_IMODE(metadata.st_mode) != 0o700:
        raise ScanError("workspace directory mode is not 0700")
    if metadata.st_uid != os.geteuid():
        raise ScanError("workspace directory ownership is unsafe")

    with os.scandir(directory_fd) as entries:
        for entry in entries:
            totals["entries"] += 1
            if totals["entries"] > MAX_ENTRIES:
                raise ScanError("workspace exceeds the entry-count bound")
            scan_name(entry.name, needles)
            child_metadata = entry.stat(follow_symlinks=False)
            if stat.S_ISDIR(child_metadata.st_mode):
                flags = os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW | os.O_DIRECTORY
                child_fd = os.open(entry.name, flags, dir_fd=directory_fd)
                try:
                    scan_directory(child_fd, needles, depth + 1, totals)
                finally:
                    os.close(child_fd)
            elif stat.S_ISREG(child_metadata.st_mode):
                totals["bytes"] += scan_file(directory_fd, entry.name, needles)
                totals["files"] += 1
                if totals["bytes"] > MAX_TREE_BYTES:
                    raise ScanError("workspace exceeds the total scan bound")
            else:
                raise ScanError("workspace contains an unsafe entry")
    after = os.fstat(directory_fd)
    if (
        metadata.st_dev != after.st_dev
        or metadata.st_ino != after.st_ino
        or metadata.st_mtime_ns != after.st_mtime_ns
        or metadata.st_ctime_ns != after.st_ctime_ns
    ):
        raise ScanError("workspace directory changed during the scan")


def scan_tree(root: Path, needles: list[bytes]) -> tuple[int, int]:
    flags = os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW | os.O_DIRECTORY
    root_fd = os.open(root, flags)
    try:
        totals = {"bytes": 0, "entries": 0, "files": 0}
        scan_directory(root_fd, needles, 0, totals)
        return totals["files"], totals["bytes"]
    finally:
        os.close(root_fd)


def main() -> int:
    try:
        args = parse_args()
        root = Path(args.root)
        if not root.is_absolute():
            raise ScanError("--root must be absolute")
        needles = forbidden_values(args.forbidden_env)
        files, total_bytes = scan_tree(root, needles)
        print(f"private workspace scan: ok: files={files} bytes={total_bytes}")
        return 0
    except (OSError, UnicodeError, ScanError) as error:
        print(f"private workspace scan: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
