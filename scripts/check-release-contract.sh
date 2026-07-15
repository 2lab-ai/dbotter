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
  scripts/validate-tap-dispatch.py \
  scripts/dispatch-and-verify-tap.sh \
  scripts/build-linux-artifact.sh \
  scripts/check-installed-receipt-contract.sh \
  scripts/build-macos-app.sh \
  scripts/build-icns.py \
  scripts/validate-macos-package.py \
  scripts/assemble-preview-manifest.py \
  scripts/assemble-installed-receipt.py \
  scripts/assemble-live-contract-receipt.py \
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
  packaging/macos/Info.plist \
  packaging/macos/stable-ax-identifiers.json; do
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

./scripts/check-workflow-graph.rb >/dev/null
require_literal scripts/verify-hermetic.sh 'cargo fmt --check'
require_literal scripts/verify-hermetic.sh 'cargo clippy --all-targets --all-features --locked -- -D warnings'
require_literal scripts/verify-hermetic.sh 'cargo test --all-features --locked'
require_literal scripts/verify-hermetic.sh 'src/export.rs'
require_literal scripts/verify-hermetic.sh 'tests/export_file_contract.rs'
require_literal scripts/verify-live-contracts.sh 'p4_live_catalog_fixture_proves_pages_caps_permissions_and_cli'
require_literal scripts/verify-live-contracts.sh 'live_mysql_safety_receipt'
require_literal scripts/verify-live-contracts.sh 'redis_live_receipt'
require_literal scripts/verify-live-contracts.sh 'live checkout does not equal the expected source SHA'
require_literal scripts/verify-live-contracts.sh '--source-sha "$head_sha"'
require_literal scripts/verify-live-contracts.sh 'assemble-live-contract-receipt.py'
require_literal scripts/verify-installed-gui.sh 'profile.connection_id'
require_literal scripts/verify-installed-gui.sh 'result.export.json'
require_literal scripts/verify-installed-gui.sh 'DBOTTER_AX_DRIVER'
require_literal scripts/assemble-installed-receipt.py 'dbotter.p7-installed-evidence.v1'
require_literal scripts/assemble-installed-receipt.py 'dbotter.formula-install-evidence.v1'
require_literal scripts/assemble-installed-receipt.py 'dbotter.live-contract-receipt.v2'

if [ -n "$manifest" ]; then
  validator=(./scripts/validate-preview-manifest.py "$manifest")
  if [ -n "$greater_than" ]; then
    validator+=(--greater-than "$greater_than")
  fi
  "${validator[@]}"
fi

echo "release contract: ok"
