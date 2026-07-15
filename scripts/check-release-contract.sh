#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

fail() {
  echo "release contract: $*" >&2
  exit 1
}

manifest=""
greater_than=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --manifest)
      [ "$#" -ge 2 ] || fail "--manifest requires a path"
      [ -z "$manifest" ] || fail "--manifest may be provided only once"
      manifest="$2"
      shift 2
      ;;
    --greater-than)
      [ "$#" -ge 2 ] || fail "--greater-than requires a version"
      [ -z "$greater_than" ] || fail "--greater-than may be provided only once"
      greater_than="$2"
      shift 2
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done
[ -z "$greater_than" ] || [ -n "$manifest" ] \
  || fail "--greater-than requires --manifest"

require_literal() {
  local file="$1"
  local literal="$2"
  grep -Fq -- "$literal" "$file" || fail "$file is missing: $literal"
}

require_regex() {
  local file="$1"
  local regex="$2"
  grep -Eq -- "$regex" "$file" || fail "$file does not match: $regex"
}

for workflow in \
  .github/workflows/verify.yml \
  .github/workflows/ci.yml \
  .github/workflows/preview.yml \
  .github/workflows/release.yml; do
  test -s "$workflow" || fail "$workflow is missing or empty"
done
for executable in \
  scripts/package-version.sh \
  scripts/check-preview-version.py \
  scripts/validate-preview-manifest.py \
  scripts/check-installed-receipt-contract.sh \
  scripts/build-macos-app.sh \
  scripts/assemble-preview-manifest.py \
  scripts/assemble-installed-receipt.py \
  scripts/verify-hermetic.sh \
  scripts/verify-live-contracts.sh \
  scripts/verify-installed.sh \
  scripts/verify-installed-gui.sh \
  scripts/verify-local.sh; do
  test -x "$executable" || fail "$executable is missing or not executable"
done
for schema in \
  packaging/release/preview-manifest.schema.json \
  packaging/release/installed-receipt.schema.json \
  packaging/macos/Info.plist; do
  test -s "$schema" || fail "$schema is missing or empty"
done

package_version="$(./scripts/package-version.sh)"
manifest_version="$(sed -n 's/^version = "\([^"]*\)"$/\1/p' Cargo.toml | head -1)"
test -n "$manifest_version" || fail "Cargo.toml package version is missing"
test "$package_version" = "$manifest_version" \
  || fail "package version helper returned $package_version, expected $manifest_version"

require_literal Cargo.toml 'repository = "https://github.com/2lab-ai/dbotter"'
require_literal src/build_info.rs 'option_env!("DBOTTER_BUILD_CHANNEL")'
require_literal src/build_info.rs 'option_env!("DBOTTER_BUILD_ID")'
require_literal src/build_info.rs 'option_env!("DBOTTER_SOURCE_SHA")'
require_literal src/build_info.rs 'None => "dev"'
require_literal src/cli.rs 'version = crate::build_info::version_with_build()'

require_literal .github/workflows/verify.yml 'workflow_call:'
require_literal .github/workflows/verify.yml 'candidate_sha:'
require_literal .github/workflows/verify.yml './scripts/verify-hermetic.sh --expected-sha'
require_literal .github/workflows/verify.yml './scripts/verify-live-contracts.sh --config config/local.example.toml'
require_literal .github/workflows/verify.yml '--expected-sha "${{ inputs.candidate_sha }}"'
require_literal .github/workflows/verify.yml 'needs: hermetic'
require_literal scripts/verify-hermetic.sh 'cargo fmt --check'
require_literal scripts/verify-hermetic.sh 'cargo clippy --all-targets --all-features --locked -- -D warnings'
require_literal scripts/verify-hermetic.sh 'cargo test --all-features --locked'
require_literal scripts/verify-hermetic.sh 'src/export.rs'
require_literal scripts/verify-hermetic.sh 'tests/export_file_contract.rs'
require_literal scripts/verify-live-contracts.sh 'p4_live_catalog_fixture_proves_pages_caps_permissions_and_cli'
require_literal scripts/verify-live-contracts.sh 'redis_live_receipt'
require_literal scripts/verify-live-contracts.sh 'live checkout does not equal the expected source SHA'
require_literal scripts/verify-live-contracts.sh 'source_sha: $source_sha'
require_literal scripts/verify-installed-gui.sh 'profile.connection_id'
require_literal scripts/verify-installed-gui.sh 'result.export.json'
require_literal scripts/verify-installed-gui.sh 'DBOTTER_AX_DRIVER'
require_literal scripts/assemble-installed-receipt.py 'dbotter.p7-installed-evidence.v1'
require_literal scripts/assemble-installed-receipt.py 'dbotter.formula-install-evidence.v1'

for workflow in \
  .github/workflows/ci.yml \
  .github/workflows/preview.yml \
  .github/workflows/release.yml; do
  require_literal "$workflow" 'uses: ./.github/workflows/verify.yml'
  require_literal "$workflow" 'candidate_sha:'
done

preview=.github/workflows/preview.yml
require_literal "$preview" 'needs: verify'
require_literal "$preview" 'needs: [verify, plan]'
require_literal "$preview" 'needs: [verify, plan, build]'
require_literal "$preview" 'needs: [verify, plan, publish]'
require_literal "$preview" 'stamp="$(date -u +%Y-%m-%d-%H%M%S)"'
require_literal "$preview" 'GITHUB_RUN_ID'
require_literal "$preview" 'GITHUB_RUN_ATTEMPT'
require_literal "$preview" 'tag=preview-$build_id'
require_literal "$preview" 'check-preview-version.py --candidate'
require_literal "$preview" 'DBOTTER_BUILD_CHANNEL: preview'
require_literal "$preview" 'DBOTTER_SOURCE_SHA:'
require_literal "$preview" 'cargo build --release --all-features --locked --target'
require_literal "$preview" 'scripts/build-macos-app.sh'
require_literal "$preview" 'scripts/assemble-preview-manifest.py'
require_literal "$preview" 'release/preview-manifest.json'
require_literal "$preview" '--expected-source-sha'
require_literal "$preview" '--expected-tag'
require_literal "$preview" 'prerelease: true'
require_literal "$preview" 'make_latest: false'
require_literal "$preview" 'Reject tag or release replacement'
require_literal "$preview" 'test -n "$TAP_DISPATCH_TOKEN"'
require_literal "$preview" '-f "tag=${{ needs.plan.outputs.tag }}"'
require_literal "$preview" '-f "source_sha=${{ needs.plan.outputs.commit }}"'
require_literal "$preview" '-f "version=${{ needs.plan.outputs.version }}"'
require_literal "$preview" '-f "manifest_url=${{ needs.plan.outputs.manifest_url }}"'
require_literal "$preview" '-f "manifest_sha256=${{ needs.publish.outputs.manifest_sha256 }}"'

for target in \
  aarch64-apple-darwin \
  x86_64-apple-darwin \
  aarch64-unknown-linux-gnu \
  x86_64-unknown-linux-gnu; do
  require_literal "$preview" "$target"
done
for asset in \
  dbotter-preview-aarch64.tar.gz \
  dbotter-preview-x86_64.tar.gz \
  dbotter-preview-linux-aarch64 \
  dbotter-preview-linux-x86_64 \
  preview-manifest.json \
  SHA256SUMS; do
  require_literal "$preview" "$asset"
done

for forbidden in \
  'previews[15:]' \
  '--cleanup-tag' \
  'gh release delete' \
  'Prune old preview' \
  'delete-release' \
  'delete-ref'; do
  if grep -Fq -- "$forbidden" "$preview"; then
    fail "$preview contains forbidden immutable-preview pruning: $forbidden"
  fi
done

require_regex .github/workflows/release.yml '^[[:space:]]+- "v\*"$'
require_literal .github/workflows/release.yml 'cargo_version="$(./scripts/package-version.sh)"'
require_literal .github/workflows/release.yml '[ "$cargo_version" != "$tag_version" ]'
require_literal .github/workflows/release.yml 'needs: [verify, preflight]'
require_literal .github/workflows/release.yml 'needs: [verify, preflight, build]'
require_literal .github/workflows/release.yml 'DBOTTER_BUILD_CHANNEL: stable'
require_literal .github/workflows/release.yml 'DBOTTER_SOURCE_SHA:'
require_literal .github/workflows/release.yml 'prerelease: false'
require_literal .github/workflows/release.yml 'cargo build --release --all-features --locked'

for asset in \
  dbotter-macos-aarch64 \
  dbotter-macos-x86_64 \
  dbotter-linux-aarch64 \
  dbotter-linux-x86_64; do
  require_literal .github/workflows/release.yml "$asset"
done
require_literal .github/workflows/release.yml 'sha256sum dbotter-* > SHA256SUMS'
require_literal .github/workflows/release.yml 'release/SHA256SUMS'

if [ -n "$manifest" ]; then
  validator=(./scripts/validate-preview-manifest.py "$manifest")
  if [ -n "$greater_than" ]; then
    validator+=(--greater-than "$greater_than")
  fi
  "${validator[@]}"
fi

echo "release contract: ok"
