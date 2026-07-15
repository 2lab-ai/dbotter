#!/usr/bin/env python3
"""Remeasure four native release files and assemble an immutable manifest."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import stat
import subprocess
import sys
import tarfile
import tempfile
from typing import Any


DESCRIPTOR_KEYS = {"schema", "manifest", "artifact"}
MANIFEST_KEYS = {
    "tag",
    "source_sha",
    "version",
    "package_version",
    "config_contract",
    "run_id",
    "run_attempt",
    "created_at",
}
TARGET_FILES = {
    "aarch64-apple-darwin": "dbotter-preview-aarch64.tar.gz",
    "x86_64-apple-darwin": "dbotter-preview-x86_64.tar.gz",
    "aarch64-unknown-linux-gnu": "dbotter-preview-linux-aarch64",
    "x86_64-unknown-linux-gnu": "dbotter-preview-linux-x86_64",
}
MACOS_TARGETS = {"aarch64-apple-darwin", "x86_64-apple-darwin"}
EMBEDDED_EXECUTABLE = "Dbotter Preview.app/Contents/MacOS/dbotter"


class AssembleError(ValueError):
    pass


def object_without_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise AssembleError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def load_descriptor(path: pathlib.Path) -> dict[str, Any]:
    try:
        with path.open("r", encoding="utf-8") as handle:
            value = json.load(handle, object_pairs_hook=object_without_duplicate_keys)
    except FileNotFoundError as error:
        raise AssembleError(f"descriptor does not exist: {path}") from error
    except (OSError, json.JSONDecodeError) as error:
        raise AssembleError(f"descriptor is not valid readable JSON: {path}: {error}") from error
    if not isinstance(value, dict) or set(value) != DESCRIPTOR_KEYS:
        raise AssembleError(f"descriptor has wrong fields: {path}")
    if value["schema"] != "dbotter.preview-artifact.v1":
        raise AssembleError(f"descriptor has wrong schema: {path}")
    manifest = value["manifest"]
    if not isinstance(manifest, dict) or set(manifest) != MANIFEST_KEYS:
        raise AssembleError(f"descriptor manifest metadata has wrong fields: {path}")
    if not isinstance(value["artifact"], dict):
        raise AssembleError(f"descriptor artifact must be an object: {path}")
    return value


def sha256_path(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while chunk := handle.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def embedded_sha256(path: pathlib.Path) -> str:
    try:
        with tarfile.open(path, mode="r:gz") as archive:
            executable_members: list[tarfile.TarInfo] = []
            for member in archive.getmembers():
                member_path = pathlib.PurePosixPath(member.name)
                if member_path.is_absolute() or ".." in member_path.parts:
                    raise AssembleError(f"archive contains an unsafe path: {path}")
                if member.issym() or member.islnk():
                    raise AssembleError(f"archive contains a link: {path}")
                if member.name == EMBEDDED_EXECUTABLE:
                    executable_members.append(member)
            if len(executable_members) != 1 or not executable_members[0].isreg():
                raise AssembleError(
                    f"archive must contain exactly one regular {EMBEDDED_EXECUTABLE}: {path}"
                )
            handle = archive.extractfile(executable_members[0])
            if handle is None:
                raise AssembleError(f"archive executable is unreadable: {path}")
            digest = hashlib.sha256()
            while chunk := handle.read(1024 * 1024):
                digest.update(chunk)
            return digest.hexdigest()
    except (OSError, tarfile.TarError) as error:
        raise AssembleError(f"release archive is unreadable: {path}: {error}") from error


def remeasure_artifact(
    raw_artifact: dict[str, Any], release_dir: pathlib.Path
) -> dict[str, Any]:
    target = raw_artifact.get("target")
    if target not in TARGET_FILES:
        raise AssembleError(f"descriptor target is not approved: {target!r}")
    asset = release_dir / TARGET_FILES[target]
    try:
        asset_stat = asset.lstat()
    except OSError as error:
        raise AssembleError(f"release asset is missing or unreadable: {asset}") from error
    if stat.S_ISLNK(asset_stat.st_mode) or not stat.S_ISREG(asset_stat.st_mode):
        raise AssembleError(f"release asset must be a regular file, not a link: {asset}")

    actual_bytes = asset_stat.st_size
    actual_sha256 = sha256_path(asset)
    if raw_artifact.get("bytes") != actual_bytes:
        raise AssembleError(f"descriptor byte count disagrees with final release file: {asset}")
    if raw_artifact.get("sha256") != actual_sha256:
        raise AssembleError(f"descriptor digest disagrees with final release file: {asset}")

    if target in MACOS_TARGETS:
        actual_embedded_sha256 = embedded_sha256(asset)
        if raw_artifact.get("embedded_executable_sha256") != actual_embedded_sha256:
            raise AssembleError(
                f"descriptor embedded digest disagrees with final release archive: {asset}"
            )
    elif stat.S_IMODE(asset_stat.st_mode) != 0o755:
        raise AssembleError(f"Linux release executable mode is not 0755: {asset}")
    return raw_artifact


def write_no_replace(document: dict[str, Any], output: pathlib.Path, validator: pathlib.Path) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    if output.exists() or output.is_symlink():
        raise AssembleError(f"output already exists: {output}")
    descriptor, temporary_name = tempfile.mkstemp(
        dir=output.parent, prefix=".preview-manifest.", suffix=".json"
    )
    temporary = pathlib.Path(temporary_name)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
            json.dump(document, handle, indent=2, sort_keys=True)
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        subprocess.run([sys.executable, str(validator), str(temporary)], check=True)
        os.link(temporary, output)
        directory_fd = os.open(output.parent, os.O_RDONLY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    except FileExistsError as error:
        raise AssembleError(f"output already exists: {output}") from error
    except subprocess.CalledProcessError as error:
        raise AssembleError("assembled preview manifest failed validation") from error
    finally:
        temporary.unlink(missing_ok=True)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--artifact", action="append", type=pathlib.Path, required=True)
    parser.add_argument("--release-dir", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    args = parser.parse_args()
    root = pathlib.Path(__file__).resolve().parent.parent
    validator = root / "scripts" / "validate-preview-manifest.py"
    try:
        if len(args.artifact) != 4:
            raise AssembleError("exactly four --artifact descriptors are required")
        if args.release_dir.is_symlink() or not args.release_dir.is_dir():
            raise AssembleError("--release-dir must be a real directory, not a link")
        descriptors = [load_descriptor(path) for path in args.artifact]
        metadata = descriptors[0]["manifest"]
        if any(descriptor["manifest"] != metadata for descriptor in descriptors[1:]):
            raise AssembleError("artifact descriptors disagree on manifest metadata")
        artifacts = sorted(
            (
                remeasure_artifact(descriptor["artifact"], args.release_dir)
                for descriptor in descriptors
            ),
            key=lambda artifact: str(artifact.get("target", "")),
        )
        manifest = {**metadata, "artifacts": artifacts}
        write_no_replace(manifest, args.output, validator)
    except AssembleError as error:
        print(f"preview manifest assembly: {error}", file=sys.stderr)
        return 1
    print(f"preview manifest assembly: ok: {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
