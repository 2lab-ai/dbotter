#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

fail() {
  echo "release contract test: $*" >&2
  exit 1
}

missing_manifest="$(mktemp -u "${TMPDIR:-/tmp}/dbotter-missing-manifest.XXXXXX.json")"
if ./scripts/check-release-contract.sh --manifest "$missing_manifest" >/dev/null 2>&1; then
  fail "missing --manifest input was accepted"
fi

echo "release contract test: ok"
