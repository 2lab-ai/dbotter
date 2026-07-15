#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
ASSEMBLER="$ROOT/scripts/assemble-live-contract-receipt.py"

fail() {
  echo "live evidence contract test: $*" >&2
  exit 1
}

for dependency in jq python3; do
  command -v "$dependency" >/dev/null 2>&1 || fail "$dependency is required"
done
[[ -x "$ASSEMBLER" ]] || fail "live evidence assembler is missing or not executable"

tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/dbotter-live-evidence.XXXXXX")"
trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM

source_sha="0123456789abcdef0123456789abcdef01234567"
run_id=123456789
run_attempt=2
started_at="2026-07-15T12:34:56Z"
finished_at="2026-07-15T12:45:56Z"

make_suite() {
  local suite="$1"
  local test_name="$2"
  local cases_json="$3"
  local measurements_json="$4"
  local output="$5"
  jq -n \
    --arg suite "$suite" \
    --arg test "$test_name" \
    --arg source_sha "$source_sha" \
    --argjson run_id "$run_id" \
    --argjson run_attempt "$run_attempt" \
    --arg started_at "$started_at" \
    --arg finished_at "$finished_at" \
    --argjson cases "$cases_json" \
    --argjson measurements "$measurements_json" '
      {
        schema: "dbotter.live-suite-evidence.v1",
        suite: $suite,
        test: $test,
        source: {
          kind: "ci_expected_sha",
          commit: $source_sha,
          run_id: $run_id,
          run_attempt: $run_attempt
        },
        started_at: $started_at,
        finished_at: $finished_at,
        cases: $cases,
        measurements: $measurements
      }
    ' >"$output"
}

cases_from_ids() {
  jq -cn '$ARGS.positional | map({id: ., executed: 1, passed: 1})' --args "$@"
}

mysql_catalog_cases="$(cases_from_ids \
  mysql.catalog.browse_pages \
  mysql.catalog.capability_gating \
  mysql.catalog.cli_round_trip \
  mysql.catalog.mutation \
  mysql.catalog.permission_denied)"
mysql_safety_cases="$(cases_from_ids \
  mysql.auth.environment.available.correct \
  mysql.auth.environment.available.wrong \
  mysql.auth.environment.empty \
  mysql.auth.environment.missing \
  mysql.auth.recovery \
  mysql.auth.session.correct \
  mysql.auth.session.wrong \
  mysql.execute.mutation \
  mysql.execute.read \
  mysql.marker.prepared.current_target_rejected \
  mysql.marker.prepared.explicit_selection_rejected \
  mysql.marker.rows_absent \
  mysql.marker.ui_explicit_selection_rejected \
  mysql.prepared_unsupported.no_raw_fallback \
  mysql.prepared_unsupported.session_retained \
  mysql.prepared_unsupported.static_recovery)"
redis_cases="$(cases_from_ids \
  redis.auth.plaintext.environment.available.correct \
  redis.auth.plaintext.environment.available.wrong \
  redis.auth.plaintext.environment.empty \
  redis.auth.plaintext.environment.missing \
  redis.auth.plaintext.recovery \
  redis.auth.plaintext.session.correct \
  redis.auth.plaintext.session.wrong \
  redis.auth.tls.environment.available.correct \
  redis.auth.tls.environment.available.wrong \
  redis.auth.tls.environment.empty \
  redis.auth.tls.environment.missing \
  redis.auth.tls.recovery \
  redis.auth.tls.session.correct \
  redis.auth.tls.session.wrong \
  redis.browse.plaintext \
  redis.browse.tls \
  redis.cli_round_trip \
  redis.inspect.plaintext \
  redis.inspect.tls \
  redis.tls.ca_preserved \
  redis.tls.host_recovery \
  redis.tls.wrong_ca.action \
  redis.tls.wrong_ca.code \
  redis.tls.wrong_ca.focus_ca \
  redis.tls.wrong_host.action \
  redis.tls.wrong_host.code \
  redis.tls.wrong_host.focus_host)"

make_suite \
  mysql_catalog \
  p4_live_catalog_fixture_proves_pages_caps_permissions_and_cli \
  "$mysql_catalog_cases" \
  '{"catalog_pages":3,"catalog_rows":7,"denied_operations":2,"mutations_verified":1}' \
  "$tmp_dir/mysql-catalog.json"
make_suite \
  mysql_safety \
  live_mysql_safety_receipt \
  "$mysql_safety_cases" \
  '{"auth_failures":4,"marker_rows_after":0,"prepared_attempts":2,"prepared_unsupported_attempts":1,"raw_fallback_attempts":0}' \
  "$tmp_dir/mysql-safety.json"
make_suite \
  redis \
  redis_live_receipt \
  "$redis_cases" \
  '{"auth_failures":8,"plaintext_fallback_attempts":0,"required_tls_attempts":3,"scan_pages":4,"tls_recovery_attempts":1}' \
  "$tmp_dir/redis.json"

assemble() {
  "$ASSEMBLER" \
    --source-sha "$source_sha" \
    --run-id "$run_id" \
    --run-attempt "$run_attempt" \
    --project dbotter-e2e \
    --started-at "$started_at" \
    --finished-at "$finished_at" \
    --mysql-catalog "$1" \
    --mysql-safety "$2" \
    --redis "$3" \
    --output "$4"
}

assemble \
  "$tmp_dir/mysql-catalog.json" \
  "$tmp_dir/mysql-safety.json" \
  "$tmp_dir/redis.json" \
  "$tmp_dir/receipt.json"

jq -e \
  --arg source_sha "$source_sha" \
  --argjson run_id "$run_id" \
  --argjson run_attempt "$run_attempt" '
    .schema == "dbotter.live-contract-receipt.v2"
    and .source == {
      kind: "ci_expected_sha",
      commit: $source_sha,
      run_id: $run_id,
      run_attempt: $run_attempt
    }
    and (.suites | keys) == ["mysql_catalog", "mysql_safety", "redis"]
    and .suites.mysql_safety.measurements.raw_fallback_attempts == 0
    and .suites.redis.measurements.plaintext_fallback_attempts == 0
    and ([.suites[].cases[].executed] | all(. > 0))
    and ([.suites[].cases[].passed] | all(. > 0))
  ' "$tmp_dir/receipt.json" >/dev/null \
  || fail "valid measured suite evidence did not assemble exactly"

expect_reject() {
  local label="$1"
  local mysql_catalog="$2"
  local mysql_safety="$3"
  local redis="$4"
  if assemble "$mysql_catalog" "$mysql_safety" "$redis" "$tmp_dir/rejected.json" \
    >"$tmp_dir/rejected.stdout" 2>"$tmp_dir/rejected.stderr"; then
    fail "$label was accepted"
  fi
}

jq 'del(.cases[0])' "$tmp_dir/mysql-catalog.json" >"$tmp_dir/missing-case.json"
expect_reject "missing mandatory case" "$tmp_dir/missing-case.json" "$tmp_dir/mysql-safety.json" "$tmp_dir/redis.json"

jq '.cases[0].executed = 0 | .cases[0].passed = 0' \
  "$tmp_dir/mysql-safety.json" >"$tmp_dir/zero-count.json"
expect_reject "zero execution count" "$tmp_dir/mysql-catalog.json" "$tmp_dir/zero-count.json" "$tmp_dir/redis.json"

jq '.cases += [.cases[0]]' "$tmp_dir/redis.json" >"$tmp_dir/duplicate-case.json"
expect_reject "duplicate case" "$tmp_dir/mysql-catalog.json" "$tmp_dir/mysql-safety.json" "$tmp_dir/duplicate-case.json"

jq '.cases[0].id = "redis.unknown.synthetic"' \
  "$tmp_dir/redis.json" >"$tmp_dir/unknown-case.json"
expect_reject "unknown case" "$tmp_dir/mysql-catalog.json" "$tmp_dir/mysql-safety.json" "$tmp_dir/unknown-case.json"

jq '.assertions = {overall: true}' \
  "$tmp_dir/mysql-safety.json" >"$tmp_dir/synthesized-constant.json"
expect_reject "synthesized boolean constant" "$tmp_dir/mysql-catalog.json" "$tmp_dir/synthesized-constant.json" "$tmp_dir/redis.json"

jq '.source.commit = "ffffffffffffffffffffffffffffffffffffffff"' \
  "$tmp_dir/mysql-catalog.json" >"$tmp_dir/source-mismatch.json"
expect_reject "source mismatch" "$tmp_dir/source-mismatch.json" "$tmp_dir/mysql-safety.json" "$tmp_dir/redis.json"

jq '.measurements.plaintext_fallback_attempts = 1' \
  "$tmp_dir/redis.json" >"$tmp_dir/fallback.json"
expect_reject "plaintext fallback" "$tmp_dir/mysql-catalog.json" "$tmp_dir/mysql-safety.json" "$tmp_dir/fallback.json"

jq '.leak = "mysql://fixture-user:fixture-password@127.0.0.1/db"' \
  "$tmp_dir/mysql-catalog.json" >"$tmp_dir/credential-uri.json"
expect_reject "credential-bearing text" "$tmp_dir/credential-uri.json" "$tmp_dir/mysql-safety.json" "$tmp_dir/redis.json"

cp "$tmp_dir/mysql-catalog.json" "$tmp_dir/duplicate-key.json"
python3 - "$tmp_dir/duplicate-key.json" <<'PY'
from pathlib import Path
import sys

path = Path(sys.argv[1])
text = path.read_text(encoding="utf-8")
path.write_text(text.replace('"suite": "mysql_catalog",', '"suite": "mysql_catalog", "suite": "mysql_catalog",', 1), encoding="utf-8")
PY
expect_reject "duplicate JSON key" "$tmp_dir/duplicate-key.json" "$tmp_dir/mysql-safety.json" "$tmp_dir/redis.json"

echo "live evidence contract tests passed"
