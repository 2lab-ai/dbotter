#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
project=${DBOTTER_COMPOSE_PROJECT:-dbotter-p5}

if ! command -v docker >/dev/null 2>&1; then
  echo "error: Docker is required for the Redis live receipt" >&2
  exit 1
fi
if ! docker info >/dev/null 2>&1; then
  echo "error: Docker is not available" >&2
  exit 1
fi

"$repo_dir/scripts/generate-redis-tls-fixture.sh"
docker compose -p "$project" -f "$repo_dir/docker-compose.yml" \
  up -d --wait --force-recreate redis-auth redis-tls-auth

export DBOTTER_REDIS_PASSWORD=dbotter-redis-local-only
export DBOTTER_LIVE_REDIS_HOST=127.0.0.1
export DBOTTER_LIVE_REDIS_PORT=36380
export DBOTTER_LIVE_REDIS_TLS_HOST=localhost
export DBOTTER_LIVE_REDIS_TLS_PORT=36381
export DBOTTER_LIVE_REDIS_CA_FILE="$repo_dir/artifacts/redis-tls/ca.pem"
export DBOTTER_LIVE_REDIS_WRONG_CA_FILE="$repo_dir/artifacts/redis-tls/wrong-ca.pem"

cd "$repo_dir"
cargo test --locked --offline --all-features --test live_redis redis_live_receipt -- \
  --ignored --exact --nocapture
