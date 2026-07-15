#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$ROOT"

fail() {
  echo "live contracts: $*" >&2
  exit 1
}

config=""
expected_sha=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --config)
      [[ $# -ge 2 ]] || fail "--config requires a path"
      [[ -z "$config" ]] || fail "--config may be provided only once"
      config="$2"
      shift 2
      ;;
    --expected-sha)
      [[ $# -ge 2 ]] || fail "--expected-sha requires a value"
      [[ -z "$expected_sha" ]] || fail "--expected-sha may be provided only once"
      expected_sha="$2"
      shift 2
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done
[[ -n "$config" ]] || fail "--config is required"
[[ -r "$config" && -f "$config" && ! -L "$config" ]] \
  || fail "--config must be a readable regular file, not a symlink"
config="$(cd "$(dirname "$config")" && pwd -P)/$(basename "$config")"
started_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

for dependency in docker cargo git python3 openssl; do
  command -v "$dependency" >/dev/null 2>&1 || fail "$dependency is required"
done
head_sha="$(git rev-parse HEAD)"
[[ -n "$expected_sha" ]] || expected_sha="$head_sha"
[[ "$expected_sha" =~ ^[0-9a-f]{40}$ ]] || fail "--expected-sha must be one full Git SHA"
[[ "$head_sha" == "$expected_sha" ]] || fail "live checkout does not equal the expected source SHA"
[[ -z "$(git status --porcelain=v1 --untracked-files=all)" ]] \
  || fail "live checkout is not clean before generated fixtures"
docker info >/dev/null 2>&1 || fail "Docker is unavailable"
[[ "${DBOTTER_MYSQL_PASSWORD:-}" == "dbotter-local-only" ]] \
  || fail "DBOTTER_MYSQL_PASSWORD does not match the mandatory fixture"
[[ "${DBOTTER_REDIS_PASSWORD:-}" == "dbotter-redis-local-only" ]] \
  || fail "DBOTTER_REDIS_PASSWORD does not match the mandatory fixture"

mysql_test="p4_live_catalog_fixture_proves_pages_caps_permissions_and_cli"
mysql_safety_test="live_mysql_safety_receipt"
redis_test="redis_live_receipt"
grep -Fq "async fn $mysql_test()" tests/live_mysql.rs \
  || fail "mandatory ignored MySQL test is missing: $mysql_test"
grep -Fq "async fn $redis_test()" tests/live_redis.rs \
  || fail "mandatory ignored Redis test is missing: $redis_test"
grep -Fq "async fn $mysql_safety_test()" tests/live_mysql_safety.rs \
  || fail "mandatory ignored MySQL safety test is missing: $mysql_safety_test"
for profile in mysql-local redis-auth-local redis-tls-auth-local; do
  grep -Fq "id = \"$profile\"" "$config" || fail "mandatory profile is missing: $profile"
done

project="${DBOTTER_COMPOSE_PROJECT:-dbotter-e2e}"
./scripts/generate-redis-tls-fixture.sh
docker compose \
  -p "$project" \
  -f docker-compose.yml \
  -f tests/fixtures/mysql-catalog/compose.yml \
  up -d --wait mysql redis-auth redis-tls-auth

running_services="$(docker compose -p "$project" -f docker-compose.yml \
  -f tests/fixtures/mysql-catalog/compose.yml ps --status running --services | sort)"
for service in mysql redis-auth redis-tls-auth; do
  grep -Fxq "$service" <<<"$running_services" || fail "mandatory service is not running: $service"
done

export DBOTTER_CONFIG="$config"
export DBOTTER_P4_MYSQL_PORT=33306
export DBOTTER_TEST_MYSQL=1
export DBOTTER_TEST_MYSQL_PORT=33306
export DBOTTER_LIVE_REDIS_HOST=127.0.0.1
export DBOTTER_LIVE_REDIS_PORT=36380
export DBOTTER_LIVE_REDIS_TLS_HOST=localhost
export DBOTTER_LIVE_REDIS_TLS_PORT=36381
export DBOTTER_LIVE_REDIS_CA_FILE="$ROOT/artifacts/redis-tls/ca.pem"
export DBOTTER_LIVE_REDIS_WRONG_CA_FILE="$ROOT/artifacts/redis-tls/wrong-ca.pem"
run_id="${GITHUB_RUN_ID:-1}"
run_attempt="${GITHUB_RUN_ATTEMPT:-1}"
[[ "$run_id" =~ ^[1-9][0-9]*$ ]] || fail "GITHUB_RUN_ID must be a positive integer"
[[ "$run_attempt" =~ ^[1-9][0-9]*$ ]] || fail "GITHUB_RUN_ATTEMPT must be a positive integer"
mkdir -p artifacts/live
mysql_catalog_evidence="$ROOT/artifacts/live/mysql-catalog-suite.json"
mysql_safety_evidence="$ROOT/artifacts/live/mysql-safety-suite.json"
redis_evidence="$ROOT/artifacts/live/redis-suite.json"
receipt="$ROOT/artifacts/live-contract-receipt.json"
for output in \
  "$mysql_catalog_evidence" \
  "$mysql_safety_evidence" \
  "$redis_evidence" \
  "$receipt"; do
  [[ ! -e "$output" && ! -L "$output" ]] \
    || fail "immutable live evidence output already exists: $output"
done
export DBOTTER_EXPECTED_SOURCE_SHA="$head_sha"
export GITHUB_RUN_ID="$run_id"
export GITHUB_RUN_ATTEMPT="$run_attempt"
export DBOTTER_LIVE_MYSQL_CATALOG_EVIDENCE="$mysql_catalog_evidence"
export DBOTTER_LIVE_MYSQL_SAFETY_EVIDENCE="$mysql_safety_evidence"
export DBOTTER_LIVE_REDIS_EVIDENCE="$redis_evidence"

cargo test --locked --all-features --test live_mysql "$mysql_test" -- \
  --ignored --exact --nocapture
cargo test --locked --all-features --test live_mysql_safety "$mysql_safety_test" -- \
  --ignored --exact --nocapture
cargo test --locked --all-features --test mysql_contract -- --nocapture
cargo test --locked --all-features --test live_redis "$redis_test" -- \
  --ignored --exact --nocapture

finished_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
./scripts/assemble-live-contract-receipt.py \
  --source-sha "$head_sha" \
  --run-id "$run_id" \
  --run-attempt "$run_attempt" \
  --project "$project" \
  --started-at "$started_at" \
  --finished-at "$finished_at" \
  --mysql-catalog "$mysql_catalog_evidence" \
  --mysql-safety "$mysql_safety_evidence" \
  --redis "$redis_evidence" \
  --output "$receipt"

. "$ROOT/scripts/receipt-security.sh"
for evidence in \
  "$mysql_catalog_evidence" \
  "$mysql_safety_evidence" \
  "$redis_evidence" \
  "$receipt"; do
  [[ -s "$evidence" && -f "$evidence" && ! -L "$evidence" ]] \
    || fail "live evidence output is not a regular nonempty file: $evidence"
  if receipt_candidate_has_static_leak "$evidence" \
    || receipt_candidate_contains_secret "$evidence" "$DBOTTER_MYSQL_PASSWORD" \
    || receipt_candidate_contains_secret "$evidence" "$DBOTTER_REDIS_PASSWORD" \
    || receipt_candidate_contains_secret "$evidence" 'dbotter-definitely-wrong' \
    || receipt_candidate_contains_secret "$evidence" 'dbotter-redis-definitely-wrong'; then
    fail "live evidence contains credential-bearing text: $evidence"
  fi
done

echo "live contracts: ok"
