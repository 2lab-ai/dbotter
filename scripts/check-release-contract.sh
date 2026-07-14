#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

fail() {
  echo "release contract: $*" >&2
  exit 1
}

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

for workflow in .github/workflows/ci.yml .github/workflows/preview.yml .github/workflows/release.yml; do
  test -s "$workflow" || fail "$workflow is missing or empty"
done
test -x scripts/package-version.sh || fail "scripts/package-version.sh is missing or not executable"

package_version="$(./scripts/package-version.sh)"
manifest_version="$(sed -n 's/^version = "\([^"]*\)"$/\1/p' Cargo.toml | head -1)"
test -n "$manifest_version" || fail "Cargo.toml package version is missing"
test "$package_version" = "$manifest_version" \
  || fail "package version helper returned $package_version, expected $manifest_version"

require_literal Cargo.toml 'repository = "https://github.com/2lab-ai/dbotter"'
require_literal src/build_info.rs 'option_env!("DBOTTER_BUILD_CHANNEL")'
require_literal src/build_info.rs 'option_env!("DBOTTER_BUILD_ID")'
require_literal src/build_info.rs 'None => "dev"'
require_literal src/cli.rs 'version = crate::build_info::version_with_build()'

require_literal .github/workflows/ci.yml 'cargo clippy --all-targets --all-features --locked -- -D warnings'
require_literal .github/workflows/ci.yml 'cargo test --all-features --locked'

require_literal .github/workflows/preview.yml 'stamp="$(date -u +%Y-%m-%d-%H%M)"'
require_literal .github/workflows/preview.yml 'tag=preview-$build_id'
require_literal .github/workflows/preview.yml 'DBOTTER_BUILD_CHANNEL: preview'
require_literal .github/workflows/preview.yml 'prerelease: true'
require_literal .github/workflows/preview.yml 'make_latest: false'
require_literal .github/workflows/preview.yml 'previews[15:]'
require_literal .github/workflows/preview.yml '--cleanup-tag'
require_literal .github/workflows/preview.yml 'cargo build --release --all-features --locked'

require_regex .github/workflows/release.yml '^[[:space:]]+- "v\*"$'
require_literal .github/workflows/preview.yml 'cargo_version="$(./scripts/package-version.sh)"'
require_literal .github/workflows/release.yml 'cargo_version="$(./scripts/package-version.sh)"'
require_literal .github/workflows/release.yml '[ "$cargo_version" != "$tag_version" ]'
require_literal .github/workflows/release.yml 'DBOTTER_BUILD_CHANNEL: stable'
require_literal .github/workflows/release.yml 'prerelease: false'
require_literal .github/workflows/release.yml 'cargo build --release --all-features --locked'

assets=(
  dbotter-macos-aarch64
  dbotter-macos-x86_64
  dbotter-linux-aarch64
  dbotter-linux-x86_64
)
for workflow in .github/workflows/preview.yml .github/workflows/release.yml; do
  for asset in "${assets[@]}"; do
    require_literal "$workflow" "$asset"
  done
  require_literal "$workflow" 'sha256sum dbotter-* > SHA256SUMS'
  require_literal "$workflow" 'release/SHA256SUMS'
done

echo "release contract: ok"
