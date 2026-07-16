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
  candidate="$tmp_dir/$index.json"
  python3 - "$cases" "$index" "$valid" "$candidate" <<'PY'
import json
import pathlib
import sys

cases_path, index, valid_path, candidate_path = sys.argv[1:]
case = json.loads(pathlib.Path(cases_path).read_text(encoding="utf-8"))[int(index)]
document = json.loads(pathlib.Path(valid_path).read_text(encoding="utf-8"))
cursor = document
for component in case["path"][:-1]:
    cursor = cursor[component]
leaf = case["path"][-1]
if case["operation"] == "set":
    cursor[leaf] = case["value"]
elif case["operation"] == "remove":
    del cursor[leaf]
else:
    raise SystemExit(f"unknown fixture operation: {case['operation']}")
pathlib.Path(candidate_path).write_text(
    json.dumps(document, indent=2) + "\n", encoding="utf-8"
)
PY
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
