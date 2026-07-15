#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$ROOT"

fail() {
  echo "workflow contract: $*" >&2
  exit 1
}

checker="$ROOT/scripts/check-workflow-graph.rb"
"$checker" --workflow-dir "$ROOT/.github/workflows" >/dev/null

tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/dbotter-workflow-mutations.XXXXXX")"
cleanup() {
  rm -rf "$tmp_dir"
}
trap cleanup EXIT HUP INT TERM

mutation_case() {
  local name="$1"
  local old="$2"
  local new="$3"
  local case_dir="$tmp_dir/$name"
  mkdir -p "$case_dir"
  cp .github/workflows/*.yml "$case_dir/"
  python3 - "$case_dir/preview.yml" "$old" "$new" <<'PY'
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
old = sys.argv[2]
new = sys.argv[3]
text = path.read_text(encoding="utf-8")
if text.count(old) < 1:
    raise SystemExit(f"mutation anchor is missing: {old!r}")
path.write_text(text.replace(old, new), encoding="utf-8")
PY
  if "$checker" --workflow-dir "$case_dir" >/dev/null 2>&1; then
    fail "negative workflow mutation was accepted: $name"
  fi
}

mutation_case \
  queue_only \
  './scripts/dispatch-and-verify-tap.sh' \
  'gh workflow run bump.yml'
mutation_case \
  missing_verify_need \
  'needs: [verify, plan, publish]' \
  'needs: [plan, publish]'
mutation_case \
  no_release_remeasure \
  '--release-dir release' \
  '--release-dir descriptors-only'
mutation_case \
  missing_linux_descriptor \
  'artifacts/package-linux-x86_64/preview-artifact-linux-x86_64.json' \
  'artifacts/package-linux-x86_64/missing-descriptor.json'
mutation_case \
  missing_tap_checkout \
  'ref: ${{ needs.plan.outputs.commit }}' \
  'ref: ${{ github.sha }}'

duplicate_dir="$tmp_dir/duplicate-key"
mkdir -p "$duplicate_dir"
cp .github/workflows/*.yml "$duplicate_dir/"
python3 - "$duplicate_dir/preview.yml" <<'PY'
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
path.write_text("name: Duplicate\n" + path.read_text(encoding="utf-8"), encoding="utf-8")
PY
if "$checker" --workflow-dir "$duplicate_dir" >/dev/null 2>&1; then
  fail "duplicate YAML key mutation was accepted"
fi

echo "workflow contract: ok"
