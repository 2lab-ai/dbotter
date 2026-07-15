#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
manifest="$root/tests/fixtures/release/preview-manifest.valid.json"
valid="$root/tests/fixtures/release/installed-receipt.valid.json"
cases="$root/tests/fixtures/release/installed-receipt.invalid-cases.json"

fail() {
  echo "installed receipt test: $*" >&2
  exit 1
}

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/dbotter-installed-receipt.XXXXXX")
cleanup() {
  rm -f "$tmp_dir"/*.json
  rmdir "$tmp_dir" 2>/dev/null || true
}
trap cleanup EXIT HUP INT TERM

"$root/scripts/check-installed-receipt-contract.sh" \
  --manifest "$manifest" "$valid" >/dev/null

case_count=$(jq 'length' "$cases")
index=0
while [ "$index" -lt "$case_count" ]; do
  name=$(jq -r ".[$index].name" "$cases")
  operation=$(jq -r ".[$index].operation" "$cases")
  path=$(jq -c ".[$index].path" "$cases")
  candidate="$tmp_dir/$index.json"
  case "$operation" in
    set)
      value=$(jq -c ".[$index].value" "$cases")
      jq --argjson path "$path" --argjson value "$value" \
        'setpath($path; $value)' "$valid" >"$candidate"
      ;;
    remove)
      jq --argjson path "$path" 'delpaths([$path])' "$valid" >"$candidate"
      ;;
    *)
      fail "unknown fixture operation: $operation"
      ;;
  esac
  if "$root/scripts/check-installed-receipt-contract.sh" \
    --manifest "$manifest" "$candidate" >/dev/null 2>&1; then
    fail "invalid installed receipt was accepted: $name"
  fi
  index=$((index + 1))
done

jq '.probe = "dbotter-redis-local-only"' "$valid" >"$tmp_dir/redis-secret.json"
if "$root/scripts/check-installed-receipt-contract.sh" \
  --manifest "$manifest" "$tmp_dir/redis-secret.json" >/dev/null 2>&1; then
  fail "Redis fixture secret was accepted"
fi

echo "installed receipt test: ok"
