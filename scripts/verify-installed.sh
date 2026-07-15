#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
source "$ROOT/scripts/receipt-security.sh"
umask 077

usage() {
  cat <<'EOF'
Usage: scripts/verify-installed.sh --manifest PATH --config PATH [--output PATH]

Verifies Homebrew's actual dbotter command, its matching installed Preview app,
exact binary/config identities, and installed check/exec/browse/inspect paths.
EOF
}

fail() {
  echo "installed CLI verification: $*" >&2
  exit 1
}

manifest=""
config=""
output="$ROOT/artifacts/installed-cli-evidence.json"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --help|-h)
      usage
      exit 0
      ;;
    --manifest)
      [[ $# -ge 2 ]] || fail "--manifest requires a path"
      manifest="$2"
      shift 2
      ;;
    --config)
      [[ $# -ge 2 ]] || fail "--config requires a path"
      config="$2"
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

[[ -n "$manifest" && -f "$manifest" && ! -L "$manifest" ]] \
  || fail "--manifest must be a regular file, not a symlink"
[[ -n "$config" && -r "$config" && -f "$config" && ! -L "$config" ]] \
  || fail "--config must be a readable regular file, not a symlink"
[[ "$(uname -s)" == "Darwin" ]] || fail "installed verification requires macOS"
started_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
for dependency in brew git jq python3 plutil codesign stat; do
  command -v "$dependency" >/dev/null 2>&1 || fail "$dependency is required"
done

"$ROOT/scripts/validate-preview-manifest.py" "$manifest" >/dev/null
source_sha="$(jq -r '.source_sha' "$manifest")"
[[ "$(git rev-parse HEAD)" == "$source_sha" ]] \
  || fail "installed verifier checkout does not equal the manifest source SHA"
git diff --quiet && git diff --cached --quiet \
  || fail "installed verifier has tracked source changes"
manifest_sha256="$(receipt_sha256_file "$manifest")"
prefix="$(brew --prefix dbotter-preview)"
[[ "$prefix" == /* && "$prefix" != */ ]] || fail "Homebrew formula prefix is not canonical"
app_path="$prefix/Dbotter Preview.app"
executable="$prefix/Dbotter Preview.app/Contents/MacOS/dbotter"
[[ -d "$app_path" && ! -L "$app_path" ]] || fail "installed app is missing or is a symlink"
[[ -f "$executable" && ! -L "$executable" && -x "$executable" ]] \
  || fail "canonical installed executable is missing"

expected_shim="$(brew --prefix)/bin/dbotter"
shim="$(command -v dbotter || true)"
[[ "$shim" == "$expected_shim" ]] \
  || fail "PATH does not resolve the exact Homebrew bin/dbotter command"
realpath_of() {
  python3 - "$1" <<'PY'
import os
import sys
print(os.path.realpath(sys.argv[1]))
PY
}
executable_realpath="$(realpath_of "$executable")"
app_realpath="$(realpath_of "$app_path")"
shim_realpath="$(realpath_of "$shim")"
[[ "$shim" != "$executable_realpath" ]] || fail "CLI command path must remain a Homebrew bin identity"
[[ "$shim_realpath" == "$executable_realpath" ]] \
  || fail "Homebrew command does not resolve to the app's canonical executable"

codesign --verify --deep --strict "$app_path"
bundle_id="$(plutil -extract CFBundleIdentifier raw "$app_path/Contents/Info.plist")"
bundle_short="$(plutil -extract CFBundleShortVersionString raw "$app_path/Contents/Info.plist")"
bundle_build="$(plutil -extract CFBundleVersion raw "$app_path/Contents/Info.plist")"
[[ "$bundle_id" == "ai.2lab.dbotter.preview" ]] || fail "installed bundle id mismatch"

case "$(uname -m)" in
  arm64) arch=aarch64; target=aarch64-apple-darwin ;;
  x86_64) arch=x86_64; target=x86_64-apple-darwin ;;
  *) fail "unsupported installed architecture" ;;
esac
artifact="$(jq -c --arg target "$target" '.artifacts | map(select(.target == $target)) | if length == 1 then .[0] else error("artifact") end' "$manifest")" \
  || fail "manifest has no exact current-architecture artifact"
expected_executable_sha256="$(jq -r '.embedded_executable_sha256' <<<"$artifact")"
executable_sha256="$(receipt_sha256_file "$executable")"
shim_sha256="$(receipt_sha256_file "$shim")"
[[ "$executable_sha256" == "$expected_executable_sha256" ]] \
  || fail "installed executable hash disagrees with the signed manifest entry"
[[ "$shim_sha256" == "$executable_sha256" ]] || fail "CLI command bytes differ from app executable"
[[ "$bundle_short" == "$(jq -r '.package_version' "$manifest")" ]] \
  || fail "bundle short version disagrees with package version"
[[ "$bundle_build" == "$(jq -r '(.run_id|tostring) + "." + (.run_attempt|tostring)' "$manifest")" ]] \
  || fail "bundle build version disagrees with run tuple"

identity_json="$("$shim" version --format json)"
embedded_identity_json="$("$executable" version --format json)"
config_json="$("$shim" config-contract --format json)"
embedded_config_json="$("$executable" config-contract --format json)"
[[ "$(jq -S -c . <<<"$identity_json")" == "$(jq -S -c . <<<"$embedded_identity_json")" ]] \
  || fail "shim and embedded identities differ"
[[ "$(jq -S -c . <<<"$config_json")" == "$(jq -S -c . <<<"$embedded_config_json")" ]] \
  || fail "shim and embedded config contracts differ"
jq -e \
  --arg package_version "$(jq -r '.package_version' "$manifest")" \
  --arg build_id "$(jq -r '.tag | sub("^preview-"; "")' "$manifest")" \
  --arg source_sha "$(jq -r '.source_sha' "$manifest")" \
  --arg target "$target" \
  --arg arch "$arch" '
    (keys | sort) == ["arch", "build_id", "channel", "package_version", "source_sha", "target"]
    and .package_version == $package_version
    and .channel == "preview"
    and .build_id == $build_id
    and .source_sha == $source_sha
    and .target == $target
    and .arch == $arch
  ' <<<"$identity_json" >/dev/null || fail "installed six-field identity mismatch"
jq -e \
  --argjson expected "$(jq -c '.config_contract' "$manifest")" \
  '(keys | sort) == ["migration_backup_suffix", "read_versions", "write_version"] and . == $expected' \
  <<<"$config_json" >/dev/null || fail "installed three-field config contract mismatch"

formula_line="$(brew list --versions dbotter-preview)"
formula_version="$(awk '{print $2}' <<<"$formula_line")"
[[ "$formula_line" == "dbotter-preview $formula_version" ]] \
  || fail "Homebrew reports an ambiguous preview installation"
[[ "$formula_version" == "$(jq -r '.version' "$manifest")" ]] \
  || fail "installed formula version disagrees with manifest"

"$shim" --config "$config" check --profile mysql-installed --format json >/dev/null
"$shim" --config "$config" exec --profile mysql-installed --text 'SELECT 1 AS installed_path' --format json >/dev/null
"$shim" --config "$config" browse mysql schemas --profile mysql-installed --page-size 50 --format json >/dev/null
"$shim" --config "$config" browse mysql relations --profile mysql-installed --schema dbotter --page-size 50 --format json >/dev/null
"$shim" --config "$config" browse mysql columns --profile mysql-installed --schema dbotter --relation receipt --page-size 50 --format json >/dev/null
"$shim" --config "$config" browse redis keys --profile redis-installed --filter-mode literal-prefix --filter receipt: --cursor 0 --count 100 --format json >/dev/null
"$shim" --config "$config" inspect redis key --profile redis-installed --key-base64 cmVjZWlwdDptYXJrZXI= --format json >/dev/null

output_parent="$(dirname "$output")"
mkdir -p "$output_parent"
[[ ! -e "$output" && ! -L "$output" ]] || fail "refusing to replace installed evidence"
temporary="$(mktemp "$output_parent/.installed-cli.XXXXXX.json")"
trap 'rm -f "$temporary"' EXIT HUP INT TERM
device="$(stat -L -f '%d' "$executable")"
inode="$(stat -L -f '%i' "$executable")"
bytes="$(stat -L -f '%z' "$executable")"
jq -n \
  --arg started_at "$started_at" \
  --arg manifest_sha256 "$manifest_sha256" \
  --arg source_sha "$source_sha" \
  --arg formula_version "$formula_version" \
  --arg app_path "$app_path" \
  --arg app_realpath "$app_realpath" \
  --arg executable_realpath "$executable_realpath" \
  --arg shim_path "$shim" \
  --arg shim_realpath "$shim_realpath" \
  --argjson device "$device" \
  --argjson inode "$inode" \
  --argjson bytes "$bytes" \
  --arg executable_sha256 "$executable_sha256" \
  --argjson identity "$identity_json" \
  --argjson config_contract "$config_json" '
  {
    schema: "dbotter.installed-cli-evidence.v1",
    started_at: $started_at,
    source_sha: $source_sha,
    manifest_sha256: $manifest_sha256,
    formula: {name: "dbotter-preview", version: $formula_version},
    app: {
      path: $app_path,
      resolved_path: $app_realpath,
      bundle_id: "ai.2lab.dbotter.preview",
      executable: {
        realpath: $executable_realpath,
        device: $device,
        inode: $inode,
        bytes: $bytes,
        sha256: $executable_sha256,
        codesign_valid: true
      }
    },
    shim: {path: $shim_path, realpath: $shim_realpath, device: $device, inode: $inode, sha256: $executable_sha256},
    identity: $identity,
    config_contract: $config_contract,
    assertions: {
      formula: true,
      app_bundle: true,
      executable_hash: true,
      shim_same_executable: true,
      identity: true,
      config_contract: true,
      check: true,
      exec: true,
      mysql_browse: true,
      redis_browse: true,
      redis_inspect: true,
      overall: true
    }
  }' >"$temporary"
chmod 0600 "$temporary"
if receipt_candidate_has_static_leak "$temporary"; then
  fail "generated installed CLI evidence contains a credential or credential-bearing URI"
fi
mv "$temporary" "$output"
trap - EXIT HUP INT TERM
echo "installed CLI verification: ok: $output"
