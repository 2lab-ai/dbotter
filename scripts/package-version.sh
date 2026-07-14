#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

cargo metadata --format-version 1 --no-deps | python3 -c '
import json
import pathlib
import sys

metadata = json.load(sys.stdin)
root_manifest = pathlib.Path("Cargo.toml").resolve()
matches = [
    package["version"]
    for package in metadata["packages"]
    if pathlib.Path(package["manifest_path"]).resolve() == root_manifest
]
if len(matches) != 1:
    raise SystemExit(f"expected one root package, found {len(matches)}")
print(matches[0])
'
