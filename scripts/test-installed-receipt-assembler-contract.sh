#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
assembler="$root/scripts/assemble-installed-receipt.py"
manifest="$root/tests/fixtures/release/preview-manifest.valid.json"
evidence="$root/tests/fixtures/release/installed-evidence.valid.json"
expected="$root/tests/fixtures/release/installed-receipt.valid.json"

fail() {
  echo "installed receipt assembler test: $*" >&2
  exit 1
}

[ -x "$assembler" ] || fail "production assembler is missing or not executable"
[ -f "$evidence" ] || fail "valid evidence fixture is missing"

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/dbotter-installed-assembler.XXXXXX")
cleanup() {
  rm -f "$tmp_dir"/*.json
  rmdir "$tmp_dir" 2>/dev/null || true
}
trap cleanup EXIT HUP INT TERM

for input in source package live cli gui p7 formula; do
  jq ".$input" "$evidence" >"$tmp_dir/$input.json"
done

assemble() {
  output=$1
  "$assembler" \
    --manifest "$manifest" \
    --source-evidence "$tmp_dir/source.json" \
    --package-evidence "$tmp_dir/package.json" \
    --live-evidence "$tmp_dir/live.json" \
    --cli-evidence "$tmp_dir/cli.json" \
    --gui-evidence "$tmp_dir/gui.json" \
    --p7-evidence "$tmp_dir/p7.json" \
    --formula-evidence "$tmp_dir/formula.json" \
    --finished-at 2026-07-15T13:10:00Z \
    --output "$output"
}

assemble "$tmp_dir/assembled.json" >/dev/null
jq -S . "$expected" >"$tmp_dir/expected.json"
jq -S . "$tmp_dir/assembled.json" >"$tmp_dir/actual.json"
cmp -s "$tmp_dir/expected.json" "$tmp_dir/actual.json" \
  || fail "assembled receipt differs from the validated receipt"

if assemble "$tmp_dir/assembled.json" >/dev/null 2>&1; then
  fail "assembler replaced an existing receipt"
fi

expect_reject() {
  name=$1
  input=$2
  filter=$3
  jq "$filter" "$evidence" | jq ".$input" >"$tmp_dir/$input.json"
  if assemble "$tmp_dir/invalid-$name.json" >/dev/null 2>&1; then
    fail "assembler accepted invalid $name evidence"
  fi
  jq ".$input" "$evidence" >"$tmp_dir/$input.json"
}

expect_reject source-overall source '.source.assertions.overall = false'
expect_reject package-source package '.package.source_sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"'
expect_reject live-tls live '.live.assertions.redis_auth_tls = false'
expect_reject cli-leak cli '.cli.leak = "dbotter-redis-local-only"'
expect_reject gui-driver gui '.gui.driver.source_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"'
expect_reject p7-digest p7 '.p7.external_export_verifier[0].actual_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"'
expect_reject formula-version formula '.formula.version = "2026.07.14.1149"'

if "$assembler" --manifest "$manifest" >/dev/null 2>&1; then
  fail "assembler accepted missing evidence inputs"
fi

echo "installed receipt assembler test: ok"
