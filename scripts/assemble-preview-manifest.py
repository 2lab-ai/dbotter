#!/usr/bin/env python3
"""Assemble two verified macOS descriptors into one immutable preview manifest."""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import subprocess
import sys
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
    parser.add_argument("--output", type=pathlib.Path, required=True)
    args = parser.parse_args()
    root = pathlib.Path(__file__).resolve().parent.parent
    validator = root / "scripts" / "validate-preview-manifest.py"
    try:
        if len(args.artifact) != 2:
            raise AssembleError("exactly two --artifact descriptors are required")
        descriptors = [load_descriptor(path) for path in args.artifact]
        metadata = descriptors[0]["manifest"]
        if descriptors[1]["manifest"] != metadata:
            raise AssembleError("artifact descriptors disagree on manifest metadata")
        artifacts = sorted(
            (descriptor["artifact"] for descriptor in descriptors),
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
