#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"

usage() {
  cat <<'EOF'
Usage: scripts/build-linux-artifact.sh --channel preview --binary PATH \
  --output DIR --expected-source-sha SHA --expected-tag TAG

Stages one native Linux executable and emits its exact preview artifact
descriptor plus a source/build-bound package receipt.
EOF
}

fail() {
  echo "Linux package: $*" >&2
  exit 1
}

channel=""
binary=""
output=""
expected_source_sha=""
expected_tag=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --help|-h)
      usage
      exit 0
      ;;
    --channel) channel="${2:-}"; shift 2 ;;
    --binary) binary="${2:-}"; shift 2 ;;
    --output) output="${2:-}"; shift 2 ;;
    --expected-source-sha) expected_source_sha="${2:-}"; shift 2 ;;
    --expected-tag) expected_tag="${2:-}"; shift 2 ;;
    *) fail "unknown argument: $1" ;;
  esac
done

[[ "$channel" == "preview" ]] || fail "only the preview channel may be packaged"
[[ "$expected_source_sha" =~ ^[0-9a-f]{40}$ ]] || fail "--expected-source-sha is required"
[[ "$expected_tag" =~ ^preview-[0-9]{4}-[0-9]{2}-[0-9]{2}-[0-9]{6}-[1-9][0-9]*-[1-9][0-9]*-[0-9a-f]{12}$ ]] \
  || fail "--expected-tag is required"
[[ "$(uname -s)" == "Linux" ]] || fail "native Linux packaging requires a Linux runner"
for dependency in jq file sha256sum stat rustc cargo python3; do
  command -v "$dependency" >/dev/null 2>&1 || fail "$dependency is required"
done
[[ -f "$binary" && ! -L "$binary" && -x "$binary" ]] \
  || fail "--binary must be an executable regular file, not a symlink"
[[ -n "$output" ]] || fail "--output is required"
binary="$(cd "$(dirname "$binary")" && pwd -P)/$(basename "$binary")"

identity_json="$("$binary" version --format json)" || fail "binary identity command failed"
config_json="$("$binary" config-contract --format json)" || fail "binary config-contract command failed"
jq -e '
  type == "object"
  and (keys | sort) == ["arch", "build_id", "channel", "package_version", "source_sha", "target"]
  and .channel == "preview"
  and (.source_sha | type == "string" and test("^[0-9a-f]{40}$"))
  and (.package_version | type == "string" and test("^(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)$"))
  and (.target == "aarch64-unknown-linux-gnu" or .target == "x86_64-unknown-linux-gnu")
  and (.arch == "aarch64" or .arch == "x86_64")
' <<<"$identity_json" >/dev/null || fail "binary identity is not the exact Linux preview object"
jq -e '
  type == "object"
  and (keys | sort) == ["migration_backup_suffix", "read_versions", "write_version"]
  and .read_versions == [1, 2]
  and .write_version == 2
  and .migration_backup_suffix == ".v1.bak"
' <<<"$config_json" >/dev/null || fail "binary config contract is not exact"

source_sha="$(jq -r .source_sha <<<"$identity_json")"
build_id="$(jq -r .build_id <<<"$identity_json")"
package_version="$(jq -r .package_version <<<"$identity_json")"
target="$(jq -r .target <<<"$identity_json")"
arch="$(jq -r .arch <<<"$identity_json")"
[[ "$source_sha" == "$expected_source_sha" ]] || fail "binary source SHA disagrees with expected source"
[[ "$target:$arch" == "aarch64-unknown-linux-gnu:aarch64" || "$target:$arch" == "x86_64-unknown-linux-gnu:x86_64" ]] \
  || fail "Linux target and architecture identity are swapped"
if [[ ! "$build_id" =~ ^([0-9]{4})-([0-9]{2})-([0-9]{2})-([0-9]{6})-([1-9][0-9]*)-([1-9][0-9]*)-([0-9a-f]{12})$ ]]; then
  fail "preview build_id has an invalid value"
fi
year="${BASH_REMATCH[1]}"
month="${BASH_REMATCH[2]}"
day="${BASH_REMATCH[3]}"
clock="${BASH_REMATCH[4]}"
run_id="${BASH_REMATCH[5]}"
run_attempt="${BASH_REMATCH[6]}"
short_sha="${BASH_REMATCH[7]}"
[[ "$short_sha" == "${source_sha:0:12}" ]] || fail "build_id short SHA disagrees with source"
tag="preview-$build_id"
[[ "$tag" == "$expected_tag" ]] || fail "derived tag disagrees with expected tag"
version="$year.$month.$day.$clock.$run_id.$run_attempt"
created_at="$year-$month-${day}T${clock:0:2}:${clock:2:2}:${clock:4:2}Z"
python3 - "$created_at" <<'PY'
import datetime as dt
import sys

dt.datetime.strptime(sys.argv[1], "%Y-%m-%dT%H:%M:%SZ")
PY

file_identity="$(file -b "$binary")"
case "$target" in
  aarch64-unknown-linux-gnu)
    [[ "$file_identity" == *"ELF 64-bit LSB"* && "$file_identity" == *"ARM aarch64"* ]] \
      || fail "binary bytes are not a native aarch64 ELF executable"
    ;;
  x86_64-unknown-linux-gnu)
    [[ "$file_identity" == *"ELF 64-bit LSB"* && "$file_identity" == *"x86-64"* ]] \
      || fail "binary bytes are not a native x86_64 ELF executable"
    ;;
esac

mkdir -p "$output"
output="$(cd "$output" && pwd -P)"
asset_name="dbotter-preview-linux-$arch"
descriptor_name="preview-artifact-linux-$arch.json"
receipt_name="package-receipt-linux-$arch.json"
for candidate in "$output/$asset_name" "$output/$descriptor_name" "$output/$receipt_name"; do
  [[ ! -e "$candidate" && ! -L "$candidate" ]] || fail "refusing to replace output: $candidate"
done
temporary="$(mktemp -d "$output/.dbotter-linux-package.XXXXXX")"
cleanup() {
  rm -rf "$temporary"
}
trap cleanup EXIT HUP INT TERM

cp "$binary" "$temporary/$asset_name"
chmod 0755 "$temporary/$asset_name"
asset_sha256="$(sha256sum "$temporary/$asset_name" | awk '{print $1}')"
asset_bytes="$(stat -c '%s' "$temporary/$asset_name")"
asset_url="https://github.com/2lab-ai/dbotter/releases/download/$tag/$asset_name"

jq -n \
  --arg tag "$tag" \
  --arg source_sha "$source_sha" \
  --arg version "$version" \
  --arg package_version "$package_version" \
  --argjson config_contract "$config_json" \
  --argjson run_id "$run_id" \
  --argjson run_attempt "$run_attempt" \
  --arg created_at "$created_at" \
  --arg target "$target" \
  --arg arch "$arch" \
  --arg url "$asset_url" \
  --argjson bytes "$asset_bytes" \
  --arg sha256 "$asset_sha256" '
    {
      schema: "dbotter.preview-artifact.v1",
      manifest: {
        tag: $tag,
        source_sha: $source_sha,
        version: $version,
        package_version: $package_version,
        config_contract: $config_contract,
        run_id: $run_id,
        run_attempt: $run_attempt,
        created_at: $created_at
      },
      artifact: {
        target: $target,
        arch: $arch,
        kind: "linux-native-executable",
        url: $url,
        bytes: $bytes,
        sha256: $sha256,
        executable_mode: "0755"
      }
    }
  ' >"$temporary/$descriptor_name"

jq -n \
  --arg tag "$tag" \
  --arg source_sha "$source_sha" \
  --arg target "$target" \
  --arg arch "$arch" \
  --arg sha256 "$asset_sha256" \
  --argjson bytes "$asset_bytes" \
  --arg rustc_version "$(rustc --version)" \
  --arg cargo_version "$(cargo --version)" \
  --argjson identity "$identity_json" \
  --argjson config_contract "$config_json" '
    {
      schema: "dbotter.linux-package-receipt.v1",
      tag: $tag,
      source_sha: $source_sha,
      target: $target,
      arch: $arch,
      identity: $identity,
      config_contract: $config_contract,
      artifact: {sha256: $sha256, bytes: $bytes, executable_mode: "0755"},
      build: {
        profile: "release",
        features: ["all"],
        locked: true,
        rustc_version: $rustc_version,
        cargo_version: $cargo_version
      }
    }
  ' >"$temporary/$receipt_name"

mv "$temporary/$asset_name" "$output/$asset_name"
mv "$temporary/$descriptor_name" "$output/$descriptor_name"
mv "$temporary/$receipt_name" "$output/$receipt_name"
echo "Linux package: ok: $output/$asset_name"
