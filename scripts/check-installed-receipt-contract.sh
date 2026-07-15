#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
. "$root/scripts/receipt-security.sh"

fail() {
  echo "installed receipt contract: $*" >&2
  exit 1
}

manifest=""
receipt=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --manifest)
      [ "$#" -ge 2 ] || fail "--manifest requires a path"
      [ -z "$manifest" ] || fail "--manifest may be provided only once"
      manifest=$2
      shift 2
      ;;
    --*)
      fail "unknown argument: $1"
      ;;
    *)
      [ -z "$receipt" ] || fail "exactly one receipt path is allowed"
      receipt=$1
      shift
      ;;
  esac
done

[ -n "$manifest" ] || fail "--manifest is required"
[ -n "$receipt" ] || fail "receipt path is required"
[ -f "$receipt" ] || fail "receipt does not exist: $receipt"
command -v jq >/dev/null 2>&1 || fail "jq is required"

"$root/scripts/validate-preview-manifest.py" "$manifest" >/dev/null
manifest_sha256=$(receipt_sha256_file "$manifest")

if receipt_candidate_has_static_leak "$receipt"; then
  fail "receipt contains a credential or credential-bearing URI"
fi

jq -e \
  --arg manifest_sha256 "$manifest_sha256" \
  --slurpfile manifest "$manifest" \
  -f "$root/scripts/installed-receipt-contract.jq" \
  "$receipt" >/dev/null \
  || fail "receipt does not satisfy dbotter.installed-receipt.v1"

echo "installed receipt contract: ok"
