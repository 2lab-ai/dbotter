#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

fail() {
  echo "workflow contract: $*" >&2
  exit 1
}

for required in \
  .github/workflows/verify.yml \
  .github/workflows/ci.yml \
  .github/workflows/preview.yml \
  .github/workflows/release.yml \
  scripts/verify-hermetic.sh \
  scripts/verify-live-contracts.sh; do
  [ -s "$root/$required" ] || fail "$required is missing or empty"
done

grep -Fq 'workflow_call:' "$root/.github/workflows/verify.yml" \
  || fail "verify.yml is not reusable"
for workflow in ci.yml preview.yml release.yml; do
  grep -Fq 'uses: ./.github/workflows/verify.yml' "$root/.github/workflows/$workflow" \
    || fail "$workflow does not call the reusable verify gate"
done

preview="$root/.github/workflows/preview.yml"
for immutable_input in \
  'stamp="$(date -u +%Y-%m-%d-%H%M%S)"' \
  'GITHUB_RUN_ID' \
  'GITHUB_RUN_ATTEMPT' \
  'tag=preview-$build_id' \
  'DBOTTER_SOURCE_SHA' \
  'scripts/build-macos-app.sh' \
  'scripts/assemble-preview-manifest.py' \
  'release/preview-manifest.json' \
  'tag=${{ needs.plan.outputs.tag }}' \
  'source_sha=${{ needs.plan.outputs.commit }}' \
  'version=${{ needs.plan.outputs.version }}' \
  'manifest_url=${{ needs.plan.outputs.manifest_url }}' \
  'manifest_sha256=${{ needs.publish.outputs.manifest_sha256 }}'; do
  grep -Fq -- "$immutable_input" "$preview" \
    || fail "preview workflow is missing immutable input: $immutable_input"
done

for forbidden in \
  'previews[15:]' \
  '--cleanup-tag' \
  'gh release delete' \
  'Prune old preview' \
  'delete-release' \
  'delete-ref'; do
  if grep -Fq -- "$forbidden" "$preview"; then
    fail "preview workflow contains forbidden immutable-release pruning: $forbidden"
  fi
done

echo "workflow contract: ok"
