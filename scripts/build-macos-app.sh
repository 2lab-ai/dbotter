#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"

usage() {
  cat <<'EOF'
Usage: scripts/build-macos-app.sh --channel preview --binary PATH --output DIR

Packages one already-built, preview-identified macOS binary into a signed
Dbotter Preview.app, a tar.gz release artifact, an artifact descriptor, and a
safe package receipt. Ad-hoc signing is the default; set
DBOTTER_CODESIGN_IDENTITY for a configured signing identity.
EOF
}

fail() {
  echo "macOS package: $*" >&2
  exit 1
}

sha256_file() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    fail "shasum or sha256sum is required"
  fi
}

channel=""
binary=""
output=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --help|-h)
      usage
      exit 0
      ;;
    --channel)
      [[ $# -ge 2 ]] || fail "--channel requires a value"
      channel="$2"
      shift 2
      ;;
    --binary)
      [[ $# -ge 2 ]] || fail "--binary requires a path"
      binary="$2"
      shift 2
      ;;
    --output)
      [[ $# -ge 2 ]] || fail "--output requires a directory"
      output="$2"
      shift 2
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

[[ "$channel" == "preview" ]] || fail "only the preview channel may be packaged"
[[ -n "$binary" ]] || fail "--binary is required"
[[ -n "$output" ]] || fail "--output is required"
[[ "$(uname -s)" == "Darwin" ]] || fail "macOS packaging requires a Darwin runner"

for command_name in jq plutil sips iconutil codesign lipo tar stat python3; do
  command -v "$command_name" >/dev/null 2>&1 || fail "$command_name is required"
done

[[ -f "$binary" && ! -L "$binary" && -x "$binary" ]] \
  || fail "--binary must be an executable regular file, not a symlink"
binary="$(cd "$(dirname "$binary")" && pwd -P)/$(basename "$binary")"

identity_json="$("$binary" version --format json)" \
  || fail "binary identity command failed"
config_json="$("$binary" config-contract --format json)" \
  || fail "binary config-contract command failed"
jq -e '
  type == "object"
  and (keys | sort) == ["arch", "build_id", "channel", "package_version", "source_sha", "target"]
  and (.package_version | type == "string" and test("^(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)$"))
  and .channel == "preview"
  and (.build_id | type == "string")
  and (.source_sha | type == "string" and test("^[0-9a-f]{40}$"))
  and (.target == "aarch64-apple-darwin" or .target == "x86_64-apple-darwin")
  and (.arch == "aarch64" or .arch == "x86_64")
' <<<"$identity_json" >/dev/null || fail "binary identity is not the exact preview six-field object"
jq -e '
  type == "object"
  and (keys | sort) == ["migration_backup_suffix", "read_versions", "write_version"]
  and .read_versions == [1, 2]
  and .write_version == 2
  and .migration_backup_suffix == ".v1.bak"
' <<<"$config_json" >/dev/null || fail "binary config contract is not the exact approved object"

package_version="$(jq -r '.package_version' <<<"$identity_json")"
build_id="$(jq -r '.build_id' <<<"$identity_json")"
source_sha="$(jq -r '.source_sha' <<<"$identity_json")"
target="$(jq -r '.target' <<<"$identity_json")"
arch="$(jq -r '.arch' <<<"$identity_json")"
if [[ ! "$build_id" =~ ^([0-9]{4})-([0-9]{2})-([0-9]{2})-([0-9]{6})-([1-9][0-9]*)-([1-9][0-9]*)-([0-9a-f]{12})$ ]]; then
  fail "preview build_id must include UTC seconds, run id/attempt, and sha12"
fi
year="${BASH_REMATCH[1]}"
month="${BASH_REMATCH[2]}"
day="${BASH_REMATCH[3]}"
clock="${BASH_REMATCH[4]}"
run_id="${BASH_REMATCH[5]}"
run_attempt="${BASH_REMATCH[6]}"
short_sha="${BASH_REMATCH[7]}"
[[ "$short_sha" == "${source_sha:0:12}" ]] || fail "build_id sha12 disagrees with source_sha"
[[ -z "${DBOTTER_EXPECTED_SOURCE_SHA:-}" || "$source_sha" == "$DBOTTER_EXPECTED_SOURCE_SHA" ]] \
  || fail "binary source_sha disagrees with DBOTTER_EXPECTED_SOURCE_SHA"

case "$target:$arch" in
  aarch64-apple-darwin:aarch64) mach_arch="arm64" ;;
  x86_64-apple-darwin:x86_64) mach_arch="x86_64" ;;
  *) fail "target and architecture are not an approved matching macOS pair" ;;
esac
[[ "$(lipo -archs "$binary")" == "$mach_arch" ]] \
  || fail "Mach-O architecture disagrees with embedded identity"

tag="preview-$build_id"
version="$year.$month.$day.$clock.$run_id.$run_attempt"
bundle_build_version="$run_id.$run_attempt"
created_at="$year-$month-${day}T${clock:0:2}:${clock:2:2}:${clock:4:2}Z"
python3 - "$created_at" <<'PY'
import datetime as dt
import sys

dt.datetime.strptime(sys.argv[1], "%Y-%m-%dT%H:%M:%SZ")
PY
[[ -z "${DBOTTER_EXPECTED_TAG:-}" || "$tag" == "$DBOTTER_EXPECTED_TAG" ]] \
  || fail "derived tag disagrees with DBOTTER_EXPECTED_TAG"

icon_source="$ROOT/assets/dbotter-icon.png"
[[ -f "$icon_source" && ! -L "$icon_source" ]] || fail "approved icon source is missing"
icon_source_sha256="$(sha256_file "$icon_source")"
[[ "$icon_source_sha256" == "5548922d61e5d3bc0dda0abe795e8dd77afda63a763c5482815e262d718559bd" ]] \
  || fail "approved icon source hash changed"

mkdir -p "$output"
output="$(cd "$output" && pwd -P)"
app_name="Dbotter Preview.app"
archive_name="dbotter-preview-$arch.tar.gz"
descriptor_name="preview-artifact-$arch.json"
receipt_name="package-receipt-$arch.json"
for candidate in "$output/$app_name" "$output/$archive_name" "$output/$descriptor_name" "$output/$receipt_name"; do
  [[ ! -e "$candidate" && ! -L "$candidate" ]] || fail "refusing to replace existing output: $candidate"
done

temporary="$(mktemp -d "$output/.dbotter-package.XXXXXX")"
cleanup() {
  rm -rf "$temporary"
}
trap cleanup EXIT HUP INT TERM

work_app="$temporary/$app_name"
mkdir -p "$work_app/Contents/MacOS" "$work_app/Contents/Resources"
cp "$binary" "$work_app/Contents/MacOS/dbotter"
chmod 0755 "$work_app/Contents/MacOS/dbotter"
cp "$ROOT/packaging/macos/Info.plist" "$work_app/Contents/Info.plist"
plutil -replace CFBundleShortVersionString -string "$package_version" "$work_app/Contents/Info.plist"
plutil -replace CFBundleVersion -string "$bundle_build_version" "$work_app/Contents/Info.plist"

iconset="$temporary/dbotter.iconset"
mkdir -p "$iconset"
for specification in \
  "16 icon_16x16.png" \
  "32 icon_16x16@2x.png" \
  "32 icon_32x32.png" \
  "64 icon_32x32@2x.png" \
  "128 icon_128x128.png" \
  "256 icon_128x128@2x.png" \
  "256 icon_256x256.png" \
  "512 icon_256x256@2x.png" \
  "512 icon_512x512.png" \
  "1024 icon_512x512@2x.png"; do
  read -r size filename <<<"$specification"
  sips -z "$size" "$size" "$icon_source" --out "$iconset/$filename" >/dev/null
done
iconutil -c icns "$iconset" -o "$work_app/Contents/Resources/dbotter.icns"

codesign_identity="${DBOTTER_CODESIGN_IDENTITY:--}"
codesign --force --deep --sign "$codesign_identity" --timestamp=none "$work_app"
codesign --verify --deep --strict "$work_app"

embedded="$work_app/Contents/MacOS/dbotter"
embedded_identity="$("$embedded" version --format json)"
embedded_config="$("$embedded" config-contract --format json)"
[[ "$(jq -S -c . <<<"$embedded_identity")" == "$(jq -S -c . <<<"$identity_json")" ]] \
  || fail "post-sign executable identity changed"
[[ "$(jq -S -c . <<<"$embedded_config")" == "$(jq -S -c . <<<"$config_json")" ]] \
  || fail "post-sign executable config contract changed"
[[ "$(plutil -extract CFBundleIdentifier raw "$work_app/Contents/Info.plist")" == "ai.2lab.dbotter.preview" ]] \
  || fail "bundle id mismatch"
[[ "$(plutil -extract CFBundleShortVersionString raw "$work_app/Contents/Info.plist")" == "$package_version" ]] \
  || fail "bundle short version mismatch"
[[ "$(plutil -extract CFBundleVersion raw "$work_app/Contents/Info.plist")" == "$bundle_build_version" ]] \
  || fail "bundle build version mismatch"
[[ "$package_version" != "$version" && "$bundle_build_version" != "$version" ]] \
  || fail "bundle version fields must not contain the Homebrew version"

unsigned_executable_sha256="$(sha256_file "$binary")"
embedded_executable_sha256="$(sha256_file "$embedded")"
COPYFILE_DISABLE=1 tar -C "$temporary" -czf "$temporary/$archive_name" "$app_name"
archive_sha256="$(sha256_file "$temporary/$archive_name")"
archive_bytes="$(stat -f '%z' "$temporary/$archive_name")"
[[ "$archive_sha256" != "$embedded_executable_sha256" ]] \
  || fail "archive and embedded executable transformations must have distinct identities"

artifact_url="https://github.com/2lab-ai/dbotter/releases/download/$tag/$archive_name"
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
  --arg url "$artifact_url" \
  --argjson bytes "$archive_bytes" \
  --arg sha256 "$archive_sha256" \
  --arg embedded_sha256 "$embedded_executable_sha256" \
  --arg package "$package_version" \
  --arg bundle_build "$bundle_build_version" \
  '{
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
      kind: "macos-app-tar-gz",
      url: $url,
      bytes: $bytes,
      sha256: $sha256,
      embedded_executable_sha256: $embedded_sha256,
      bundle_id: "ai.2lab.dbotter.preview",
      bundle_short_version: $package,
      bundle_build_version: $bundle_build
    }
  }' >"$temporary/$descriptor_name"

jq -n \
  --arg tag "$tag" \
  --arg source_sha "$source_sha" \
  --arg target "$target" \
  --arg arch "$arch" \
  --arg unsigned_sha256 "$unsigned_executable_sha256" \
  --arg embedded_sha256 "$embedded_executable_sha256" \
  --arg archive_sha256 "$archive_sha256" \
  --argjson archive_bytes "$archive_bytes" \
  --arg bundle_short "$package_version" \
  --arg bundle_build "$bundle_build_version" \
  --arg icon_sha256 "$icon_source_sha256" \
  --arg codesign_identity "$codesign_identity" \
  --argjson identity "$identity_json" \
  --argjson config_contract "$config_json" \
  '{
    schema: "dbotter.package-receipt.v1",
    tag: $tag,
    source_sha: $source_sha,
    target: $target,
    arch: $arch,
    unsigned_executable_sha256: $unsigned_sha256,
    post_sign_executable_sha256: $embedded_sha256,
    archive_sha256: $archive_sha256,
    archive_bytes: $archive_bytes,
    bundle_id: "ai.2lab.dbotter.preview",
    bundle_short_version: $bundle_short,
    bundle_build_version: $bundle_build,
    icon: {source: "assets/dbotter-icon.png", sha256: $icon_sha256},
    signing: {identity: $codesign_identity, verified: true},
    identity: $identity,
    config_contract: $config_contract
  }' >"$temporary/$receipt_name"

mv "$work_app" "$output/$app_name"
mv "$temporary/$archive_name" "$output/$archive_name"
mv "$temporary/$descriptor_name" "$output/$descriptor_name"
mv "$temporary/$receipt_name" "$output/$receipt_name"
echo "macOS package: ok: $output/$archive_name"
