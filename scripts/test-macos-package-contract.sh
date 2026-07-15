#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
. "$root/scripts/receipt-security.sh"

fail() {
  echo "macOS package contract: $*" >&2
  exit 1
}

for required in \
  packaging/macos/Info.plist \
  scripts/build-macos-app.sh \
  scripts/assemble-preview-manifest.py; do
  [ -f "$root/$required" ] || fail "$required is missing"
done

[ -x "$root/scripts/build-macos-app.sh" ] \
  || fail "scripts/build-macos-app.sh is not executable"
[ -x "$root/scripts/assemble-preview-manifest.py" ] \
  || fail "scripts/assemble-preview-manifest.py is not executable"

"$root/scripts/build-macos-app.sh" --help >/dev/null
"$root/scripts/assemble-preview-manifest.py" --help >/dev/null

if "$root/scripts/build-macos-app.sh" \
  --channel stable --binary /missing --output /missing >/dev/null 2>&1; then
  fail "stable packaging was accepted by the preview-only builder"
fi

python3 - "$root/packaging/macos/Info.plist" <<'PY'
import plistlib
import sys

with open(sys.argv[1], "rb") as handle:
    plist = plistlib.load(handle)
assert plist["CFBundleIdentifier"] == "ai.2lab.dbotter.preview"
assert plist["CFBundleExecutable"] == "dbotter"
assert plist["CFBundleIconFile"] == "dbotter.icns"
assert plist["CFBundleShortVersionString"] == "0.0.0"
assert plist["CFBundleVersion"] == "1.1"
PY

icon_sha=$(receipt_sha256_file "$root/assets/dbotter-icon.png")
[ "$icon_sha" = "5548922d61e5d3bc0dda0abe795e8dd77afda63a763c5482815e262d718559bd" ] \
  || fail "approved icon hash changed"

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/dbotter-package-contract.XXXXXX")
cleanup() {
  rm -f "$tmp_dir"/*.json
  rmdir "$tmp_dir" 2>/dev/null || true
}
trap cleanup EXIT HUP INT TERM

valid="$root/tests/fixtures/release/preview-manifest.valid.json"
jq -n --argjson index 0 \
  '{schema:"dbotter.preview-artifact.v1", manifest: ($root | del(.artifacts)), artifact: $root.artifacts[$index]}' \
  --argjson root "$(cat "$valid")" >"$tmp_dir/aarch64.json"
jq -n --argjson index 1 \
  '{schema:"dbotter.preview-artifact.v1", manifest: ($root | del(.artifacts)), artifact: $root.artifacts[$index]}' \
  --argjson root "$(cat "$valid")" >"$tmp_dir/x86_64.json"

"$root/scripts/assemble-preview-manifest.py" \
  --artifact "$tmp_dir/aarch64.json" \
  --artifact "$tmp_dir/x86_64.json" \
  --output "$tmp_dir/assembled.json" >/dev/null
jq -S . "$valid" >"$tmp_dir/expected.json"
jq -S . "$tmp_dir/assembled.json" >"$tmp_dir/actual.json"
cmp -s "$tmp_dir/expected.json" "$tmp_dir/actual.json" \
  || fail "assembled manifest differs from the validated fixture"

if "$root/scripts/assemble-preview-manifest.py" \
  --artifact "$tmp_dir/aarch64.json" \
  --artifact "$tmp_dir/x86_64.json" \
  --output "$tmp_dir/assembled.json" >/dev/null 2>&1; then
  fail "manifest assembler replaced an existing output"
fi
if "$root/scripts/assemble-preview-manifest.py" \
  --artifact "$tmp_dir/aarch64.json" \
  --output "$tmp_dir/missing-target.json" >/dev/null 2>&1; then
  fail "manifest assembler accepted one target"
fi
if "$root/scripts/assemble-preview-manifest.py" \
  --artifact "$tmp_dir/aarch64.json" \
  --artifact "$tmp_dir/aarch64.json" \
  --output "$tmp_dir/duplicate-target.json" >/dev/null 2>&1; then
  fail "manifest assembler accepted duplicate targets"
fi

echo "macOS package contract: ok"
