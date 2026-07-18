#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$ROOT"

fail() {
  echo "hermetic verification: $*" >&2
  exit 1
}

expected_sha=""
while [[ $# -gt 0 ]]; do
  case "$1" in
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
[[ "$expected_sha" =~ ^[0-9a-f]{40}$ ]] || fail "--expected-sha must be one full Git SHA"

for dependency in git jq cargo rustc python3 ruby; do
  command -v "$dependency" >/dev/null 2>&1 || fail "$dependency is required"
done

head_sha="$(git rev-parse HEAD)"
[[ "$head_sha" == "$expected_sha" ]] || fail "HEAD does not equal the expected candidate SHA"
[[ -z "$(git status --porcelain=v1 --untracked-files=all)" ]] \
  || fail "candidate checkout is not clean before generated artifacts"

required_inputs=(
  Cargo.toml
  Cargo.lock
  build.rs
  assets/dbotter-icon.png
  src/build_info.rs
  src/export.rs
  src/export_file.rs
  src/ui/result_view.rs
  tests/export_golden.rs
  tests/export_file_contract.rs
  tests/ui_raw_input.rs
  tests/ui_accesskit.rs
  tests/ui_contrast.rs
  tests/daily_use_j2_installed_contract.rs
  tests/live_evidence_recorder.rs
  tests/common/live_evidence.rs
  tests/live_mysql_safety.rs
  scripts/check-release-contract.sh
  scripts/test-release-contract.sh
  scripts/test-four-target-preview-contract.sh
  scripts/test-tap-dispatch-contract.sh
  scripts/test-tap-handshake-contract.sh
  scripts/dispatch-and-verify-tap.sh
  scripts/build-linux-artifact.sh
  scripts/validate-tap-dispatch.py
  scripts/check-installed-receipt-contract.sh
  scripts/validate-installed-receipt-config-contract.py
  scripts/test-installed-receipt-contract.sh
  scripts/test-macos-package-contract.sh
  scripts/test-macos-package-live.sh
  scripts/validate-macos-package.py
  scripts/build-icns.py
  scripts/test-workflow-contract.sh
  scripts/check-workflow-graph.rb
  scripts/test-live-evidence-contract.sh
  scripts/live_contract.py
  scripts/assemble-live-contract-receipt.py
  scripts/test-installed-verifier-contract.sh
  scripts/test-installed-receipt-assembler-contract.sh
  scripts/assemble-installed-receipt.py
  scripts/verify-installed.sh
  scripts/native-ax-driver.swift
  scripts/build-native-ax-driver.sh
  scripts/verify-installed-gui.sh
  scripts/native-j2-ax-driver.swift
  scripts/build-native-j2-ax-driver.sh
  scripts/verify-installed-j2.sh
  scripts/scan-private-workspace.py
  tests/fixtures/installed-j2/compose.yml
  packaging/macos/Info.plist
  packaging/macos/stable-ax-identifiers.json
  packaging/release/preview-manifest.schema.json
  packaging/release/installed-receipt.schema.json
)
for required in "${required_inputs[@]}"; do
  [[ -f "$required" ]] || fail "required P6/P7/P8 input is missing: $required"
  git ls-files --error-unmatch -- "$required" >/dev/null 2>&1 \
    || fail "required P6/P7/P8 input is not tracked: $required"
done

./scripts/check-release-contract.sh
./scripts/test-release-contract.sh
./scripts/test-four-target-preview-contract.sh
./scripts/test-tap-dispatch-contract.sh
./scripts/test-tap-handshake-contract.sh
./scripts/test-installed-receipt-contract.sh
./scripts/test-macos-package-contract.sh
./scripts/test-workflow-contract.sh
./scripts/test-live-evidence-contract.sh
./scripts/test-installed-verifier-contract.sh
./scripts/test-installed-receipt-assembler-contract.sh
./scripts/test-receipt-contract.sh
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-features --locked

verify_stamp="${DBOTTER_VERIFY_STAMP:-$(date -u +%Y-%m-%d-%H%M%S)}"
run_id="${GITHUB_RUN_ID:-1}"
run_attempt="${GITHUB_RUN_ATTEMPT:-1}"
[[ "$run_id" =~ ^[1-9][0-9]*$ ]] || fail "GITHUB_RUN_ID must be a positive integer"
[[ "$run_attempt" =~ ^[1-9][0-9]*$ ]] || fail "GITHUB_RUN_ATTEMPT must be a positive integer"
build_id="$verify_stamp-$run_id-$run_attempt-${expected_sha:0:12}"
preview_version="${verify_stamp:0:4}.${verify_stamp:5:2}.${verify_stamp:8:2}.${verify_stamp:11:6}.$run_id.$run_attempt"
./scripts/check-preview-version.py \
  --candidate "$preview_version" \
  --greater-than 1970.01.01.000000.1.1 >/dev/null

DBOTTER_BUILD_CHANNEL=preview \
DBOTTER_BUILD_ID="$build_id" \
DBOTTER_SOURCE_SHA="$expected_sha" \
  cargo build --release --all-features --locked

identity_json="$(target/release/dbotter version --format json)"
config_json="$(target/release/dbotter config-contract --format json)"
jq -e \
  --arg source_sha "$expected_sha" \
  --arg build_id "$build_id" '
    type == "object"
    and (keys | sort) == ["arch", "build_id", "channel", "package_version", "source_sha", "target"]
    and .channel == "preview"
    and .build_id == $build_id
    and .source_sha == $source_sha
  ' <<<"$identity_json" >/dev/null || fail "source-built identity is not exact"
jq -e '
  type == "object"
  and (keys | sort) == ["migration_backup_suffixes", "read_versions", "write_version"]
  and .read_versions == [1, 2, 3]
  and .write_version == 3
  and (.migration_backup_suffixes | type == "object")
  and (.migration_backup_suffixes | (keys | sort) == ["1", "2"])
  and .migration_backup_suffixes["1"] == ".v1.bak"
  and .migration_backup_suffixes["2"] == ".v2.bak"
' <<<"$config_json" >/dev/null || fail "source-built config contract is not exact"

mkdir -p artifacts
started_at="${verify_stamp:0:4}-${verify_stamp:5:2}-${verify_stamp:8:2}T${verify_stamp:11:2}:${verify_stamp:13:2}:${verify_stamp:15:2}Z"
finished_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
jq -n \
  --arg commit "$head_sha" \
  --arg expected_sha "$expected_sha" \
  --argjson run_id "$run_id" \
  --argjson run_attempt "$run_attempt" \
  --arg started_at "$started_at" \
  --arg finished_at "$finished_at" \
  --arg rustc_version "$(rustc --version)" \
  --arg cargo_version "$(cargo --version)" \
  --argjson identity "$identity_json" \
  --argjson config_contract "$config_json" '
  {
    schema: "dbotter.source-verification.v1",
    source: {
      kind: "ci_expected_sha",
      commit: $commit,
      expected_sha: $expected_sha,
      clean: true,
      run_id: $run_id,
      run_attempt: $run_attempt
    },
    build: {
      profile: "release",
      features: ["desktop", "mongodb"],
      rustc_version: $rustc_version,
      cargo_version: $cargo_version,
      identity: $identity,
      config_contract: $config_contract
    },
    started_at: $started_at,
    finished_at: $finished_at,
    assertions: {
      source: true,
      release_contract: true,
      receipt_contracts: true,
      format: true,
      clippy: true,
      tests: true,
      identity: true,
      config_contract: true,
      overall: true
    }
  }' >artifacts/source-verification.json

echo "hermetic verification: ok: $head_sha"
