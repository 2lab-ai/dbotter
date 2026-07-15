#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$ROOT"

fail() {
  echo "live contracts: $*" >&2
  exit 1
}

config=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --config)
      [[ $# -ge 2 ]] || fail "--config requires a path"
      [[ -z "$config" ]] || fail "--config may be provided only once"
      config="$2"
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

for dependency in docker cargo jq openssl; do
  command -v "$dependency" >/dev/null 2>&1 || fail "$dependency is required"
done
docker info >/dev/null 2>&1 || fail "Docker is unavailable"
[[ "${DBOTTER_MYSQL_PASSWORD:-}" == "dbotter-local-only" ]] \
  || fail "DBOTTER_MYSQL_PASSWORD does not match the mandatory fixture"
[[ "${DBOTTER_REDIS_PASSWORD:-}" == "dbotter-redis-local-only" ]] \
  || fail "DBOTTER_REDIS_PASSWORD does not match the mandatory fixture"

mysql_test="p4_live_catalog_fixture_proves_pages_caps_permissions_and_cli"
redis_test="redis_live_receipt"
grep -Fq "async fn $mysql_test()" tests/live_mysql.rs \
  || fail "mandatory ignored MySQL test is missing: $mysql_test"
grep -Fq "async fn $redis_test()" tests/live_redis.rs \
  || fail "mandatory ignored Redis test is missing: $redis_test"
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

cargo test --locked --all-features --test live_mysql "$mysql_test" -- \
  --ignored --exact --nocapture
cargo test --locked --all-features --test mysql_contract -- --nocapture
cargo test --locked --all-features --test live_redis "$redis_test" -- \
  --ignored --exact --nocapture

mkdir -p artifacts
jq -n \
  --arg started_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --arg project "$project" \
  --arg mysql_test "$mysql_test" \
  --arg redis_test "$redis_test" '
  {
    schema: "dbotter.live-contract-receipt.v1",
    started_at: $started_at,
    project: $project,
    tests: {
      mysql_catalog: $mysql_test,
      mysql_prepared_auth: "mysql_contract",
      redis_keyspace_tls_auth: $redis_test
    },
    assertions: {
      mysql_catalog: true,
      mysql_prepared_only: true,
      mysql_auth: true,
      redis_keyspace: true,
      redis_auth_plaintext: true,
      redis_auth_tls: true,
      redis_tls_ca_recovery: true,
      redis_tls_host_recovery: true,
      redis_plaintext_fallback: false,
      overall: true
    }
  }' >artifacts/live-contract-receipt.json

echo "live contracts: ok"
