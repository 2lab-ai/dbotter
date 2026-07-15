#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
. "$repo_dir/scripts/receipt-security.sh"

for dependency in jq awk sed grep; do
  if ! command -v "$dependency" >/dev/null 2>&1; then
    echo "error: $dependency is required" >&2
    exit 1
  fi
done

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/dbotter-receipt-contract.XXXXXX")
cleanup() {
  rm -f "$tmp_dir"/*.json
  rmdir "$tmp_dir" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM

fingerprint=$(receipt_sha256_text 'contract-fixture-input')
step=$(jq -n --arg sha "$fingerprint" '{
  name: "fixture",
  actor: "dbotter",
  profile_id: "fixture",
  input: {kind: "check", fixture_statement: "fixture.check", sha256: $sha},
  exit_code: 0,
  elapsed_ms: 0,
  result: {status: "ok"},
  streams: {
    stdout: {present: false, sha256: $sha},
    stderr: {present: false, sha256: $sha}
  }
}')

jq -n \
  --argjson step "$step" \
  '{
    schema_version: 2,
    source: {
      repository_root: "/repo/dbotter",
      repository_root_matches: true,
      git_head: "0123456789abcdef0123456789abcdef01234567",
      branch: "main",
      detached: false,
      dirty: false,
      required_files_tracked: true,
      clean_committed: true
    },
    mysql: {
      app_steps: [$step, $step, $step, $step],
      official_readback: $step
    },
    redis: {
      app_steps: [$step, $step, $step, $step],
      official_readback: {get: $step, ttl: $step}
    },
    assertions: {
      mysql: true,
      redis: true,
      source_provenance: true,
      credential_leak: false,
      overall: true
    }
  }' >"$tmp_dir/clean.json"

jq -e -f "$repo_dir/scripts/receipt-contract.jq" "$tmp_dir/clean.json" >/dev/null

jq '.mysql.app_steps[0].stdout = "raw output"' "$tmp_dir/clean.json" \
  >"$tmp_dir/raw-stream.json"
if jq -e -f "$repo_dir/scripts/receipt-contract.jq" "$tmp_dir/raw-stream.json" >/dev/null; then
  echo "error: contract accepted a raw stdout field" >&2
  exit 1
fi

jq '.source.dirty = true' "$tmp_dir/clean.json" >"$tmp_dir/dirty-false-claim.json"
if jq -e -f "$repo_dir/scripts/receipt-contract.jq" "$tmp_dir/dirty-false-claim.json" >/dev/null; then
  echo "error: contract accepted dirty provenance as an overall pass" >&2
  exit 1
fi

jq '.probe = "dbotter-local-only"' "$tmp_dir/clean.json" >"$tmp_dir/fixture-secret.json"
if ! receipt_candidate_has_static_leak "$tmp_dir/fixture-secret.json"; then
  echo "error: fixture secret injection was not detected" >&2
  exit 1
fi

jq '.probe = "mysql://fixture-user:fixture-password@127.0.0.1/db"' \
  "$tmp_dir/clean.json" >"$tmp_dir/credential-uri.json"
if ! receipt_candidate_has_static_leak "$tmp_dir/credential-uri.json"; then
  echo "error: credential-bearing URI injection was not detected" >&2
  exit 1
fi

jq '.probe = "dbotter-redis-local-only"' "$tmp_dir/clean.json" \
  >"$tmp_dir/redis-fixture-secret.json"
if ! receipt_candidate_has_static_leak "$tmp_dir/redis-fixture-secret.json"; then
  echo "error: Redis fixture secret injection was not detected" >&2
  exit 1
fi

resolved_secret='runtime-only-contract-secret'
jq '.probe = "runtime-only-contract-secret"' "$tmp_dir/clean.json" \
  >"$tmp_dir/resolved-secret.json"
if ! receipt_candidate_contains_secret "$tmp_dir/resolved-secret.json" "$resolved_secret"; then
  echo "error: resolved secret injection was not detected" >&2
  exit 1
fi

if receipt_candidate_has_static_leak "$tmp_dir/clean.json" \
  || receipt_candidate_contains_secret "$tmp_dir/clean.json" "$resolved_secret"; then
  echo "error: clean contract fixture produced a false-positive leak" >&2
  exit 1
fi

echo "receipt contract tests passed"
