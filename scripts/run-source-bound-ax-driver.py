#!/usr/bin/env python3
"""Capture and enforce the exact native J2 AX driver identity.

The guard excludes a concurrently hostile process running as the same euid:
macOS exposes no fexecve-style API for executing the already-verified Mach-O
file descriptor. Euid-owned, non-writable TMPDIR parents keep other users out;
pre/post identity checks make pathname replacement fail closed at every phase.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import stat
import subprocess
import sys
from typing import Any

SCHEMA = "dbotter.source-bound-ax-driver.v1"
IDENTITY_KEYS = {
    "canonical_path",
    "device",
    "inode",
    "mode",
    "uid",
    "size",
    "sha256",
    "cdhashes",
}
MAX_IDENTITY_BYTES = 65_536


class GuardError(Exception):
    """A sanitized source-bound identity failure."""


def under(path: str, root: str) -> bool:
    try:
        return os.path.commonpath([path, root]) == root and path != root
    except ValueError:
        return False


def secure_directory(path: str) -> None:
    metadata = os.lstat(path)
    if (
        not stat.S_ISDIR(metadata.st_mode)
        or stat.S_ISLNK(metadata.st_mode)
        or metadata.st_uid != os.geteuid()
        or stat.S_IMODE(metadata.st_mode) & 0o022
    ):
        raise GuardError("path boundary is unsafe")


def tmp_roots() -> tuple[str, str]:
    raw = os.environ.get("TMPDIR", "")
    if not raw or not os.path.isabs(raw):
        raise GuardError("TMPDIR boundary is unsafe")
    lexical = os.path.normpath(raw)
    canonical = os.path.realpath(lexical)
    secure_directory(lexical)
    secure_directory(canonical)
    return lexical, canonical


def walk_secure_parent(path: str) -> tuple[str, str]:
    if not os.path.isabs(path):
        raise GuardError("path boundary is unsafe")
    lexical_tmp, canonical_tmp = tmp_roots()
    lexical = os.path.normpath(path)
    canonical = os.path.realpath(lexical)
    if not under(canonical, canonical_tmp):
        raise GuardError("path boundary is unsafe")

    if under(lexical, lexical_tmp):
        lexical_root = lexical_tmp
    elif under(lexical, canonical_tmp):
        lexical_root = canonical_tmp
    else:
        raise GuardError("path boundary is unsafe")

    for root, parent in [
        (lexical_root, os.path.dirname(lexical)),
        (canonical_tmp, os.path.dirname(canonical)),
    ]:
        secure_directory(root)
        relative = os.path.relpath(parent, root)
        current = root
        if relative != ".":
            for component in relative.split(os.sep):
                if component in {"", ".", ".."}:
                    raise GuardError("path boundary is unsafe")
                current = os.path.join(current, component)
                secure_directory(current)
    return lexical, canonical


def secure_executable(path: str) -> tuple[str, os.stat_result]:
    lexical, canonical = walk_secure_parent(path)
    metadata = os.lstat(lexical)
    mode = stat.S_IMODE(metadata.st_mode)
    if (
        not stat.S_ISREG(metadata.st_mode)
        or stat.S_ISLNK(metadata.st_mode)
        or metadata.st_uid != os.geteuid()
        or mode & 0o022
        or not mode & stat.S_IXUSR
    ):
        raise GuardError("executable boundary is unsafe")
    canonical_metadata = os.lstat(canonical)
    if (
        metadata.st_dev != canonical_metadata.st_dev
        or metadata.st_ino != canonical_metadata.st_ino
    ):
        raise GuardError("executable boundary is unsafe")
    return canonical, metadata


def metadata_fields(
    canonical: str, metadata: os.stat_result
) -> dict[str, str | int | list[str]]:
    return {
        "canonical_path": canonical,
        "device": metadata.st_dev,
        "inode": metadata.st_ino,
        "mode": stat.S_IMODE(metadata.st_mode),
        "uid": metadata.st_uid,
        "size": metadata.st_size,
        "sha256": "",
        "cdhashes": [],
    }


def stable_metadata(
    current: os.stat_result, expected: dict[str, str | int | list[str]]
) -> bool:
    return all(
        [
            current.st_dev == expected["device"],
            current.st_ino == expected["inode"],
            stat.S_IMODE(current.st_mode) == expected["mode"],
            current.st_uid == expected["uid"],
            current.st_size == expected["size"],
            stat.S_ISREG(current.st_mode),
        ]
    )


def sha256_from_stable_fd(
    canonical: str, expected: dict[str, str | int | list[str]]
) -> str:
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    descriptor = os.open(canonical, flags)
    try:
        if not stable_metadata(os.fstat(descriptor), expected):
            raise GuardError("identity changed during validation")
        digest = hashlib.sha256()
        while True:
            chunk = os.read(descriptor, 1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
        if not stable_metadata(os.fstat(descriptor), expected):
            raise GuardError("identity changed during validation")
        return digest.hexdigest()
    finally:
        os.close(descriptor)


def cdhashes(canonical: str) -> list[str]:
    verified = subprocess.run(
        [
            "/usr/bin/codesign",
            "--verify",
            "--strict",
            "--all-architectures",
            canonical,
        ],
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    if verified.returncode != 0:
        raise GuardError("code signature is invalid")
    details = subprocess.run(
        ["/usr/bin/codesign", "--display", "--verbose=4", canonical],
        check=False,
        capture_output=True,
        text=True,
    )
    if details.returncode != 0:
        raise GuardError("code identity is unavailable")
    values = sorted(
        {
            line.split("=", 1)[1].strip().lower()
            for line in (details.stdout + details.stderr).splitlines()
            if line.startswith("CDHash=")
        }
    )
    if not values or any(re.fullmatch(r"[0-9a-f]{40,64}", value) is None for value in values):
        raise GuardError("code identity is invalid")
    return values


def snapshot(path: str) -> dict[str, str | int | list[str]]:
    canonical, metadata = secure_executable(path)
    identity = metadata_fields(canonical, metadata)
    identity["sha256"] = sha256_from_stable_fd(canonical, identity)
    identity["cdhashes"] = cdhashes(canonical)
    if not stable_metadata(os.lstat(canonical), identity):
        raise GuardError("identity changed during validation")
    return identity


def exact_bytes(
    candidate: dict[str, str | int | list[str]],
    driver: dict[str, str | int | list[str]],
) -> None:
    if (
        candidate["size"] != driver["size"]
        or candidate["sha256"] != driver["sha256"]
        or candidate["cdhashes"] != driver["cdhashes"]
    ):
        raise GuardError("candidate bytes differ")

    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    candidate_fd = os.open(str(candidate["canonical_path"]), flags)
    driver_fd = os.open(str(driver["canonical_path"]), flags)
    try:
        if not stable_metadata(os.fstat(candidate_fd), candidate):
            raise GuardError("identity changed during comparison")
        if not stable_metadata(os.fstat(driver_fd), driver):
            raise GuardError("identity changed during comparison")
        while True:
            candidate_chunk = os.read(candidate_fd, 1024 * 1024)
            driver_chunk = os.read(driver_fd, 1024 * 1024)
            if candidate_chunk != driver_chunk:
                raise GuardError("candidate bytes differ")
            if not candidate_chunk:
                break
        if not stable_metadata(os.fstat(candidate_fd), candidate):
            raise GuardError("identity changed during comparison")
        if not stable_metadata(os.fstat(driver_fd), driver):
            raise GuardError("identity changed during comparison")
    finally:
        os.close(candidate_fd)
        os.close(driver_fd)


def capture_state(candidate_path: str, driver_path: str) -> dict[str, Any]:
    candidate_before = snapshot(candidate_path)
    driver_before = snapshot(driver_path)
    exact_bytes(candidate_before, driver_before)
    candidate = snapshot(candidate_path)
    driver = snapshot(driver_path)
    if candidate != candidate_before or driver != driver_before:
        raise GuardError("identity changed during capture")
    exact_bytes(candidate, driver)
    return {"schema": SCHEMA, "candidate": candidate, "driver": driver}


def secure_identity_parent(identity_path: str) -> tuple[str, str]:
    lexical, canonical = walk_secure_parent(identity_path)
    if os.path.lexists(lexical):
        raise GuardError("identity destination already exists")
    return lexical, canonical


def write_identity(identity_path: str, payload: dict[str, Any]) -> None:
    lexical, _ = secure_identity_parent(identity_path)
    encoded = (json.dumps(payload, sort_keys=True, separators=(",", ":")) + "\n").encode()
    if len(encoded) > MAX_IDENTITY_BYTES:
        raise GuardError("identity payload is oversized")
    temporary = f"{lexical}.tmp-{os.getpid()}"
    descriptor = -1
    try:
        descriptor = os.open(
            temporary,
            os.O_WRONLY
            | os.O_CREAT
            | os.O_EXCL
            | getattr(os, "O_NOFOLLOW", 0),
            0o600,
        )
        os.write(descriptor, encoded)
        os.fsync(descriptor)
        os.close(descriptor)
        descriptor = -1
        os.replace(temporary, lexical)
    finally:
        if descriptor >= 0:
            os.close(descriptor)
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass


def validate_identity_shape(payload: Any) -> dict[str, Any]:
    if not isinstance(payload, dict) or set(payload) != {"schema", "candidate", "driver"}:
        raise GuardError("identity payload is invalid")
    if payload["schema"] != SCHEMA:
        raise GuardError("identity payload is invalid")
    for role in ["candidate", "driver"]:
        identity = payload[role]
        if not isinstance(identity, dict) or set(identity) != IDENTITY_KEYS:
            raise GuardError("identity payload is invalid")
        if (
            not isinstance(identity["canonical_path"], str)
            or not os.path.isabs(identity["canonical_path"])
            or any(
                not isinstance(identity[field], int) or identity[field] < 0
                for field in ["device", "inode", "mode", "uid", "size"]
            )
            or not isinstance(identity["sha256"], str)
            or re.fullmatch(r"[0-9a-f]{64}", identity["sha256"]) is None
            or not isinstance(identity["cdhashes"], list)
            or not identity["cdhashes"]
            or any(
                not isinstance(value, str)
                or re.fullmatch(r"[0-9a-f]{40,64}", value) is None
                for value in identity["cdhashes"]
            )
        ):
            raise GuardError("identity payload is invalid")
    return payload


def load_identity(identity_path: str) -> dict[str, Any]:
    lexical, canonical = walk_secure_parent(identity_path)
    metadata = os.lstat(lexical)
    if (
        not stat.S_ISREG(metadata.st_mode)
        or stat.S_ISLNK(metadata.st_mode)
        or metadata.st_uid != os.geteuid()
        or stat.S_IMODE(metadata.st_mode) != 0o600
        or metadata.st_size <= 0
        or metadata.st_size > MAX_IDENTITY_BYTES
    ):
        raise GuardError("identity payload is unsafe")
    canonical_metadata = os.lstat(canonical)
    if (
        metadata.st_dev != canonical_metadata.st_dev
        or metadata.st_ino != canonical_metadata.st_ino
    ):
        raise GuardError("identity payload is unsafe")
    descriptor = os.open(canonical, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    try:
        before = os.fstat(descriptor)
        if before.st_dev != metadata.st_dev or before.st_ino != metadata.st_ino:
            raise GuardError("identity payload changed")
        raw = bytearray()
        while True:
            chunk = os.read(descriptor, 8192)
            if not chunk:
                break
            raw.extend(chunk)
            if len(raw) > MAX_IDENTITY_BYTES:
                raise GuardError("identity payload is oversized")
        after = os.fstat(descriptor)
        if (
            before.st_dev != after.st_dev
            or before.st_ino != after.st_ino
            or before.st_size != after.st_size
        ):
            raise GuardError("identity payload changed")
    finally:
        os.close(descriptor)
    try:
        return validate_identity_shape(json.loads(raw))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise GuardError("identity payload is invalid") from error


def verify_bound_state(
    candidate_path: str, driver_path: str, expected: dict[str, Any]
) -> None:
    candidate = snapshot(candidate_path)
    driver = snapshot(driver_path)
    if candidate != expected["candidate"] or driver != expected["driver"]:
        raise GuardError("identity changed")
    exact_bytes(candidate, driver)
    if (
        snapshot(candidate_path) != expected["candidate"]
        or snapshot(driver_path) != expected["driver"]
    ):
        raise GuardError("identity changed")


def run_bound(
    candidate_path: str,
    driver_path: str,
    identity_path: str,
    driver_args: list[str],
) -> int:
    expected = load_identity(identity_path)
    try:
        verify_bound_state(candidate_path, driver_path, expected)
    except GuardError as error:
        raise GuardError("identity changed before execution") from error

    arguments = driver_args[1:] if driver_args[:1] == ["--"] else driver_args
    if not arguments:
        raise GuardError("driver arguments are missing")
    completed = subprocess.run(
        [expected["driver"]["canonical_path"], *arguments],
        check=False,
    )

    try:
        verify_bound_state(candidate_path, driver_path, expected)
    except GuardError as error:
        raise GuardError("identity changed after execution") from error
    return completed.returncode if completed.returncode >= 0 else 128 - completed.returncode


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(
        description="Run an exact source-bound native J2 AX driver."
    )
    commands = result.add_subparsers(dest="command", required=True)
    for name in ["capture", "run"]:
        command = commands.add_parser(name)
        command.add_argument("--candidate", required=True)
        command.add_argument("--driver", required=True)
        command.add_argument("--identity", required=True)
        if name == "run":
            command.add_argument("driver_args", nargs=argparse.REMAINDER)
    return result


def main() -> int:
    arguments = parser().parse_args()
    try:
        if arguments.command == "capture":
            payload = capture_state(arguments.candidate, arguments.driver)
            write_identity(arguments.identity, payload)
            return 0
        return run_bound(
            arguments.candidate,
            arguments.driver,
            arguments.identity,
            arguments.driver_args,
        )
    except GuardError as error:
        print(f"source-bound AX driver {error}", file=sys.stderr)
        return 1
    except (OSError, subprocess.SubprocessError, ValueError, TypeError):
        print("source-bound AX driver validation failed", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
