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
expect_reject source-config-bool source '.source.build.config_contract.read_versions = [true, 2, 3]'
expect_reject source-config-suffix-missing source 'del(.source.build.config_contract.migration_backup_suffixes["2"])'
expect_reject source-config-suffix-extra source '.source.build.config_contract.migration_backup_suffixes["3"] = ".v3.bak"'
expect_reject source-config-suffix-type source '.source.build.config_contract.migration_backup_suffixes["1"] = 1'
expect_reject package-source package '.package.source_sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"'
expect_reject live-v1-schema live '.live.schema = "dbotter.live-contract-receipt.v1"'
expect_reject live-source live '.live.source.commit = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"'
expect_reject live-run live '.live.source.run_attempt = 3'
expect_reject live-missing-case live 'del(.live.suites.mysql_catalog.cases[0])'
expect_reject live-zero-case live '.live.suites.mysql_safety.cases[0] |= (.executed = 0 | .passed = 0)'
expect_reject live-unknown-case live '.live.suites.redis.cases[0].id = "redis.synthetic.pass"'
expect_reject live-auth-measurement live '.live.suites.mysql_safety.measurements.auth_failures = 3'
expect_reject live-raw-fallback live '.live.suites.mysql_safety.measurements.raw_fallback_attempts = 1'
expect_reject live-plaintext-fallback live '.live.suites.redis.measurements.plaintext_fallback_attempts = 1'
expect_reject cli-leak cli '.cli.leak = "dbotter-redis-local-only"'
expect_reject cli-source cli '.cli.source_sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"'
expect_reject cli-shim cli '.cli.shim.path = "/tmp/bin/dbotter"'
expect_reject cli-resolved-app cli '.cli.app.resolved_path = .cli.app.path'
expect_reject gui-driver gui '.gui.driver.source_sha256 = "invalid"'
expect_reject gui-driver-source gui '.gui.driver.source_repo_path = "docs/usable-mvp/spec.md" | .gui.driver.source_sha256 = "4c78aa0b957814d0dbaf46e8938a93701e2f85f0a6bb88772ef06b1b1da90cf3"'
expect_reject gui-source gui '.gui.source_sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"'
expect_reject gui-required-ax gui '.gui.ax_identifiers -= ["profile.connection_id"]'
expect_reject p7-digest p7 '.p7.external_export_verifier[0].actual_sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"'
expect_reject p7-user-leak p7 '.p7.assertions.user_content_leak = true'
expect_reject formula-version formula '.formula.version = "2026.07.14.1149"'
expect_reject formula-cellar-prefix formula '.formula.prefix = "/usr/local/Cellar/dbotter-preview/2026.07.15.123456.123456789.2"'

if "$assembler" --manifest "$manifest" >/dev/null 2>&1; then
  fail "assembler accepted missing evidence inputs"
fi

echo "installed receipt assembler test: ok"
