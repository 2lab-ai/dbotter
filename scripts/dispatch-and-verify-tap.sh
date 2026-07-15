#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
TAP_REPOSITORY="2lab-ai/homebrew-tap"
SOURCE_REPOSITORY="2lab-ai/dbotter"

usage() {
  cat <<'EOF'
Usage: scripts/dispatch-and-verify-tap.sh \
  --tag TAG --source-sha SHA --version VERSION \
  --manifest-url URL --manifest-sha256 SHA256 --output PATH

Dispatches the exact five immutable inputs, waits for the matching tap run to
complete successfully, independently proves the formula commit is reachable
from master, fetches exact manifest/formula bytes, and validates the proof.
EOF
}

fail() {
  echo "tap handshake: $*" >&2
  exit 1
}

tag=""
source_sha=""
version=""
manifest_url=""
manifest_sha256=""
output=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --help|-h)
      usage
      exit 0
      ;;
    --tag)
      [[ $# -ge 2 ]] || fail "--tag requires a value"
      tag="$2"
      shift 2
      ;;
    --source-sha)
      [[ $# -ge 2 ]] || fail "--source-sha requires a value"
      source_sha="$2"
      shift 2
      ;;
    --version)
      [[ $# -ge 2 ]] || fail "--version requires a value"
      version="$2"
      shift 2
      ;;
    --manifest-url)
      [[ $# -ge 2 ]] || fail "--manifest-url requires a value"
      manifest_url="$2"
      shift 2
      ;;
    --manifest-sha256)
      [[ $# -ge 2 ]] || fail "--manifest-sha256 requires a value"
      manifest_sha256="$2"
      shift 2
      ;;
    --output)
      [[ $# -ge 2 ]] || fail "--output requires a path"
      output="$2"
      shift 2
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

[[ "$tag" =~ ^preview-[0-9]{4}-[0-9]{2}-[0-9]{2}-[0-9]{6}-[1-9][0-9]*-[1-9][0-9]*-[0-9a-f]{12}$ ]] \
  || fail "--tag has an invalid value"
[[ "$source_sha" =~ ^[0-9a-f]{40}$ ]] || fail "--source-sha must be one full Git SHA"
[[ "$version" =~ ^[0-9]{4}\.[0-9]{2}\.[0-9]{2}\.[0-9]{6}\.[1-9][0-9]*\.[1-9][0-9]*$ ]] \
  || fail "--version has an invalid value"
[[ "$manifest_sha256" =~ ^[0-9a-f]{64}$ ]] || fail "--manifest-sha256 is invalid"
expected_manifest_url="https://github.com/$SOURCE_REPOSITORY/releases/download/$tag/preview-manifest.json"
[[ "$manifest_url" == "$expected_manifest_url" ]] \
  || fail "--manifest-url is not the exact immutable release URL"
[[ -n "$output" ]] || fail "--output is required"
[[ ! -e "$output" && ! -L "$output" ]] || fail "refusing to replace output: $output"
[[ -n "${GH_TOKEN:-}" ]] || fail "GH_TOKEN is required"

for dependency in gh jq python3 base64; do
  command -v "$dependency" >/dev/null 2>&1 || fail "$dependency is required"
done

poll_attempts="${DBOTTER_TAP_POLL_ATTEMPTS:-240}"
poll_interval="${DBOTTER_TAP_POLL_INTERVAL_SECONDS:-5}"
[[ "$poll_attempts" =~ ^[1-9][0-9]*$ ]] || fail "poll attempts must be positive"
[[ "$poll_interval" =~ ^[0-9]+$ ]] || fail "poll interval must be a non-negative integer"

temporary="$(mktemp -d "${TMPDIR:-/tmp}/dbotter-tap-handshake.XXXXXX")"
cleanup() {
  rm -rf "$temporary"
}
trap cleanup EXIT HUP INT TERM

dispatch_started="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
gh workflow run bump.yml \
  --repo "$TAP_REPOSITORY" \
  -f "tag=$tag" \
  -f "source_sha=$source_sha" \
  -f "version=$version" \
  -f "manifest_url=$manifest_url" \
  -f "manifest_sha256=$manifest_sha256"

expected_title="dbotter-preview $tag $source_sha"
run_id=""
for ((attempt = 1; attempt <= poll_attempts; attempt++)); do
  runs_json="$(gh api "repos/$TAP_REPOSITORY/actions/workflows/bump.yml/runs?event=workflow_dispatch&per_page=100")" \
    || fail "could not list tap workflow runs"
  matches="$(jq -c \
    --arg title "$expected_title" \
    --arg started "$dispatch_started" '
      [.workflow_runs[]
        | select(.display_title == $title)
        | select(.event == "workflow_dispatch")
        | select(.created_at >= $started)]
    ' <<<"$runs_json")" || fail "tap workflow run list is malformed"
  match_count="$(jq 'length' <<<"$matches")"
  if [[ "$match_count" -gt 1 ]]; then
    fail "more than one matching tap workflow run appeared"
  fi
  if [[ "$match_count" -eq 1 ]]; then
    run_id="$(jq -r '.[0].id' <<<"$matches")"
    [[ "$run_id" =~ ^[1-9][0-9]*$ ]] || fail "matching tap run id is invalid"
    break
  fi
  if [[ "$attempt" -lt "$poll_attempts" ]]; then
    sleep "$poll_interval"
  fi
done
[[ -n "$run_id" ]] || fail "matching tap workflow run did not appear"

run_json=""
for ((attempt = 1; attempt <= poll_attempts; attempt++)); do
  run_json="$(gh api "repos/$TAP_REPOSITORY/actions/runs/$run_id")" \
    || fail "could not read matching tap workflow run"
  jq -e \
    --argjson run_id "$run_id" \
    --arg title "$expected_title" '
      .id == $run_id
      and .display_title == $title
      and .event == "workflow_dispatch"
      and (.run_attempt | type == "number" and . >= 1 and floor == .)
      and (.status | type == "string")
      and (.conclusion == null or (.conclusion | type == "string"))
    ' <<<"$run_json" >/dev/null || fail "matching tap run identity is malformed"
  status="$(jq -r .status <<<"$run_json")"
  if [[ "$status" == "completed" ]]; then
    break
  fi
  if [[ "$attempt" -lt "$poll_attempts" ]]; then
    sleep "$poll_interval"
  fi
done
[[ "$(jq -r .status <<<"$run_json")" == "completed" ]] \
  || fail "matching tap workflow did not complete"
[[ "$(jq -r .conclusion <<<"$run_json")" == "success" ]] \
  || fail "matching tap workflow did not conclude successfully"
run_attempt="$(jq -r .run_attempt <<<"$run_json")"

proof_dir="$temporary/proof"
mkdir -p "$proof_dir"
gh run download "$run_id" \
  --repo "$TAP_REPOSITORY" \
  --name "dbotter-tap-$tag" \
  --dir "$proof_dir"
proof="$proof_dir/dbotter-tap-dispatch.json"
[[ -f "$proof" && ! -L "$proof" ]] || fail "tap proof artifact is missing or is a link"
proof_file_count="$(find "$proof_dir" -type f -maxdepth 1 | wc -l | tr -d ' ')"
[[ "$proof_file_count" == "1" ]] || fail "tap proof artifact contains unexpected files"

formula_commit="$(jq -er '.tap.formula_commit' "$proof")" \
  || fail "tap proof formula commit is missing"
formula_blob="$(jq -er '.tap.formula_blob' "$proof")" \
  || fail "tap proof formula blob is missing"
[[ "$formula_commit" =~ ^[0-9a-f]{40}$ ]] || fail "tap proof formula commit is invalid"
[[ "$formula_blob" =~ ^[0-9a-f]{40}$ ]] || fail "tap proof formula blob is invalid"

master_json="$(gh api "repos/$TAP_REPOSITORY/git/ref/heads/master")" \
  || fail "could not read tap master ref"
master_commit="$(jq -er 'select(.object.type == "commit") | .object.sha' <<<"$master_json")" \
  || fail "tap master ref is malformed"
[[ "$master_commit" =~ ^[0-9a-f]{40}$ ]] || fail "tap master commit is invalid"
compare_json="$(gh api "repos/$TAP_REPOSITORY/compare/$formula_commit...$master_commit")" \
  || fail "could not compare formula commit to tap master"
jq -e \
  --arg formula_commit "$formula_commit" '
    (.status == "ahead" or .status == "identical")
    and .base_commit.sha == $formula_commit
    and .merge_base_commit.sha == $formula_commit
  ' <<<"$compare_json" >/dev/null \
  || fail "formula commit is not reachable from tap master"

formula_api="$temporary/formula-api.json"
gh api "repos/$TAP_REPOSITORY/contents/Formula/dbotter-preview.rb?ref=$formula_commit" \
  >"$formula_api" || fail "could not fetch formula at the proven commit"
jq -e \
  --arg blob "$formula_blob" '
    .type == "file"
    and .path == "Formula/dbotter-preview.rb"
    and .encoding == "base64"
    and .sha == $blob
    and (.content | type == "string")
  ' "$formula_api" >/dev/null || fail "formula API response disagrees with the proven blob"
formula="$temporary/dbotter-preview.rb"
jq -r .content "$formula_api" | tr -d '\n' | base64 --decode >"$formula" \
  || fail "formula blob content is not valid base64"

release_dir="$temporary/release"
mkdir -p "$release_dir"
gh release download "$tag" \
  --repo "$SOURCE_REPOSITORY" \
  --pattern preview-manifest.json \
  --dir "$release_dir"
manifest="$release_dir/preview-manifest.json"
[[ -f "$manifest" && ! -L "$manifest" ]] || fail "released preview manifest is missing or is a link"

"$ROOT/scripts/validate-tap-dispatch.py" \
  --proof "$proof" \
  --manifest "$manifest" \
  --formula "$formula" \
  --expected-tag "$tag" \
  --expected-source-sha "$source_sha" \
  --expected-version "$version" \
  --expected-manifest-url "$manifest_url" \
  --expected-manifest-sha256 "$manifest_sha256" \
  --expected-formula-commit "$formula_commit" \
  --expected-formula-blob "$formula_blob" \
  --expected-workflow-run-id "$run_id" \
  --expected-workflow-run-attempt "$run_attempt" >/dev/null

python3 - "$proof" "$output" <<'PY'
import os
import pathlib
import sys

source = pathlib.Path(sys.argv[1])
output = pathlib.Path(sys.argv[2])
output.parent.mkdir(parents=True, exist_ok=True)
descriptor = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
try:
    with source.open("rb") as source_handle, os.fdopen(descriptor, "wb") as output_handle:
        while chunk := source_handle.read(1024 * 1024):
            output_handle.write(chunk)
        output_handle.flush()
        os.fsync(output_handle.fileno())
except BaseException:
    output.unlink(missing_ok=True)
    raise
PY

echo "tap handshake: ok: run=$run_id attempt=$run_attempt commit=$formula_commit"
