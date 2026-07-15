#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

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

echo "macOS package contract: ok"
