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

valid_manifest="tests/fixtures/release/preview-manifest.valid.json"
./scripts/check-release-contract.sh --manifest "$valid_manifest" >/dev/null

for invalid_manifest in tests/fixtures/release/preview-manifest.invalid-*.json; do
  if ./scripts/check-release-contract.sh --manifest "$invalid_manifest" >/dev/null 2>&1; then
    fail "invalid preview manifest was accepted: $invalid_manifest"
  fi
done

if ./scripts/check-release-contract.sh \
  --manifest "$valid_manifest" \
  --greater-than 2026.07.15.123456.123456789.2 >/dev/null 2>&1; then
  fail "non-increasing preview version was accepted"
fi

./scripts/check-release-contract.sh \
  --manifest "$valid_manifest" \
  --greater-than 2026.07.15.123455.123456789.1 >/dev/null

if ./scripts/check-release-contract.sh --unknown >/dev/null 2>&1; then
  fail "unknown release-contract argument was accepted"
fi

echo "release contract test: ok"
