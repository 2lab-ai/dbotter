#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"

usage() {
  echo "Usage: scripts/test-macos-package-live.sh --binary PATH --expected-source-sha SHA --expected-tag TAG"
}

fail() {
  echo "macOS package live test: $*" >&2
  exit 1
}

binary=""
expected_source_sha=""
expected_tag=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --help|-h) usage; exit 0 ;;
    --binary) binary="${2:-}"; shift 2 ;;
    --expected-source-sha) expected_source_sha="${2:-}"; shift 2 ;;
    --expected-tag) expected_tag="${2:-}"; shift 2 ;;
    *) fail "unknown argument: $1" ;;
  esac
done

[[ "$(uname -s)" == "Darwin" ]] || fail "live package test requires macOS"
[[ -x "$binary" && ! -L "$binary" ]] || fail "--binary must be an executable regular file"
[[ "$expected_source_sha" =~ ^[0-9a-f]{40}$ ]] || fail "--expected-source-sha is invalid"
[[ "$expected_tag" =~ ^preview-.*-${expected_source_sha:0:12}$ ]] || fail "--expected-tag is invalid"

tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/dbotter-macos-package-live.XXXXXX")"
cleanup() {
  rm -rf "$tmp_dir"
}
trap cleanup EXIT HUP INT TERM

if "$ROOT/scripts/build-macos-app.sh" \
  --channel preview \
  --binary "$binary" \
  --output "$tmp_dir/missing-pins" >/dev/null 2>&1; then
  fail "macOS packager accepted missing source/tag acceptance pins"
fi

output="$tmp_dir/output"
"$ROOT/scripts/build-macos-app.sh" \
  --channel preview \
  --binary "$binary" \
  --output "$output" \
  --expected-source-sha "$expected_source_sha" \
  --expected-tag "$expected_tag" >/dev/null

identity="$("$binary" version --format json)"
arch="$(jq -r .arch <<<"$identity")"
app="$output/Dbotter Preview.app"
archive="$output/dbotter-preview-$arch.tar.gz"
descriptor="$output/preview-artifact-$arch.json"
receipt="$output/package-receipt-$arch.json"
for artifact in "$app" "$archive" "$descriptor" "$receipt"; do
  [[ -e "$artifact" && ! -L "$artifact" ]] || fail "expected package output is missing: $artifact"
done

"$ROOT/scripts/validate-macos-package.py" \
  --app "$app" \
  --archive "$archive" \
  --descriptor "$descriptor" \
  --receipt "$receipt" \
  --expected-source-sha "$expected_source_sha" \
  --expected-tag "$expected_tag" >/dev/null

iconset="$tmp_dir/validated.iconset"
iconutil -c iconset "$app/Contents/Resources/dbotter.icns" -o "$iconset"
[[ -f "$iconset/icon_512x512@2x.png" ]] || fail "generated ICNS cannot be decoded by iconutil"
codesign --verify --deep --strict "$app"

mutated_descriptor="$tmp_dir/mutated-descriptor.json"
jq '.artifact.sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"' \
  "$descriptor" >"$mutated_descriptor"
if "$ROOT/scripts/validate-macos-package.py" \
  --app "$app" \
  --archive "$archive" \
  --descriptor "$mutated_descriptor" \
  --receipt "$receipt" \
  --expected-source-sha "$expected_source_sha" \
  --expected-tag "$expected_tag" >/dev/null 2>&1; then
  fail "validator accepted a mutated archive digest"
fi

mutated_ax_receipt="$tmp_dir/mutated-ax-receipt.json"
jq '.ax_identifiers += ["unreviewed.control"]' "$receipt" >"$mutated_ax_receipt"
if "$ROOT/scripts/validate-macos-package.py" \
  --app "$app" \
  --archive "$archive" \
  --descriptor "$descriptor" \
  --receipt "$mutated_ax_receipt" \
  --expected-source-sha "$expected_source_sha" \
  --expected-tag "$expected_tag" >/dev/null 2>&1; then
  fail "validator accepted an unreviewed AX identifier"
fi

mutated_archive="$tmp_dir/dbotter-preview-$arch.tar.gz"
cp "$archive" "$mutated_archive"
printf 'tamper\n' >>"$mutated_archive"
if "$ROOT/scripts/validate-macos-package.py" \
  --app "$app" \
  --archive "$mutated_archive" \
  --descriptor "$descriptor" \
  --receipt "$receipt" \
  --expected-source-sha "$expected_source_sha" \
  --expected-tag "$expected_tag" >/dev/null 2>&1; then
  fail "validator accepted mutated archive bytes"
fi

if "$ROOT/scripts/build-macos-app.sh" \
  --channel preview \
  --binary "$binary" \
  --output "$output" \
  --expected-source-sha "$expected_source_sha" \
  --expected-tag "$expected_tag" >/dev/null 2>&1; then
  fail "macOS packager replaced existing outputs"
fi

echo "macOS package live test: ok"
