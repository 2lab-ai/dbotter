#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$ROOT"

fail() {
  echo "four-target preview contract: $*" >&2
  exit 1
}

sha256_file() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/dbotter-four-target.XXXXXX")"
cleanup() {
  rm -rf "$tmp_dir"
}
trap cleanup EXIT HUP INT TERM

release_dir="$tmp_dir/release"
descriptor_dir="$tmp_dir/descriptors"
mkdir -p "$release_dir" "$descriptor_dir"

tag="preview-2026-07-15-123456-123456789-2-0123456789ab"
source_sha="0123456789abcdef0123456789abcdef01234567"
version="2026.07.15.123456.123456789.2"
package_version="0.1.0"

metadata="$tmp_dir/metadata.json"
jq -n \
  --arg tag "$tag" \
  --arg source_sha "$source_sha" \
  --arg version "$version" \
  --arg package_version "$package_version" \
  '{
    tag: $tag,
    source_sha: $source_sha,
    version: $version,
    package_version: $package_version,
    config_contract: {
      read_versions: [1, 2, 3],
      write_version: 3,
      migration_backup_suffixes: {"1": ".v1.bak", "2": ".v2.bak"}
    },
    run_id: 123456789,
    run_attempt: 2,
    created_at: "2026-07-15T12:34:56Z"
  }' >"$metadata"

descriptors=()
for target_arch in \
  "aarch64-apple-darwin:aarch64" \
  "x86_64-apple-darwin:x86_64"; do
  target="${target_arch%%:*}"
  arch="${target_arch##*:}"
  stage="$tmp_dir/stage-$arch/Dbotter Preview.app/Contents/MacOS"
  mkdir -p "$stage"
  printf 'signed macOS executable for %s\n' "$target" >"$stage/dbotter"
  chmod 0755 "$stage/dbotter"
  archive="$release_dir/dbotter-preview-$arch.tar.gz"
  COPYFILE_DISABLE=1 tar -C "$tmp_dir/stage-$arch" -czf "$archive" "Dbotter Preview.app"
  archive_sha="$(sha256_file "$archive")"
  embedded_sha="$(sha256_file "$stage/dbotter")"
  archive_bytes="$(wc -c <"$archive" | tr -d ' ')"
  descriptor="$descriptor_dir/$target.json"
  jq -n \
    --argjson manifest "$(cat "$metadata")" \
    --arg target "$target" \
    --arg arch "$arch" \
    --arg url "https://github.com/2lab-ai/dbotter/releases/download/$tag/dbotter-preview-$arch.tar.gz" \
    --argjson bytes "$archive_bytes" \
    --arg sha256 "$archive_sha" \
    --arg embedded_sha256 "$embedded_sha" \
    --arg package_version "$package_version" \
    '{
      schema: "dbotter.preview-artifact.v1",
      manifest: $manifest,
      artifact: {
        target: $target,
        arch: $arch,
        kind: "macos-app-tar-gz",
        url: $url,
        bytes: $bytes,
        sha256: $sha256,
        embedded_executable_sha256: $embedded_sha256,
        bundle_id: "ai.2lab.dbotter.preview",
        bundle_short_version: $package_version,
        bundle_build_version: "123456789.2"
      }
    }' >"$descriptor"
  descriptors+=(--artifact "$descriptor")
done

for target_arch in \
  "aarch64-unknown-linux-gnu:aarch64" \
  "x86_64-unknown-linux-gnu:x86_64"; do
  target="${target_arch%%:*}"
  arch="${target_arch##*:}"
  executable="$release_dir/dbotter-preview-linux-$arch"
  printf '#!/bin/sh\nprintf "native Linux executable for %s\\n"\n' "$target" >"$executable"
  chmod 0755 "$executable"
  executable_sha="$(sha256_file "$executable")"
  executable_bytes="$(wc -c <"$executable" | tr -d ' ')"
  descriptor="$descriptor_dir/$target.json"
  jq -n \
    --argjson manifest "$(cat "$metadata")" \
    --arg target "$target" \
    --arg arch "$arch" \
    --arg url "https://github.com/2lab-ai/dbotter/releases/download/$tag/dbotter-preview-linux-$arch" \
    --argjson bytes "$executable_bytes" \
    --arg sha256 "$executable_sha" \
    '{
      schema: "dbotter.preview-artifact.v1",
      manifest: $manifest,
      artifact: {
        target: $target,
        arch: $arch,
        kind: "linux-native-executable",
        url: $url,
        bytes: $bytes,
        sha256: $sha256,
        executable_mode: "0755"
      }
    }' >"$descriptor"
  descriptors+=(--artifact "$descriptor")
done

manifest="$tmp_dir/preview-manifest.json"
./scripts/assemble-preview-manifest.py \
  --release-dir "$release_dir" \
  "${descriptors[@]}" \
  --output "$manifest" >/dev/null

./scripts/validate-preview-manifest.py \
  "$manifest" \
  --expected-source-sha "$source_sha" \
  --expected-tag "$tag" >/dev/null

jq -e '
  (.artifacts | length) == 4
  and ([.artifacts[].target] | unique | length) == 4
  and ([.artifacts[] | select(.kind == "macos-app-tar-gz")] | length) == 2
  and ([.artifacts[] | select(.kind == "linux-native-executable")] | length) == 2
' "$manifest" >/dev/null || fail "assembled manifest is not the exact four-target union"

mac_with_linux_field="$tmp_dir/mac-with-linux-field.json"
jq '.artifacts[0].executable_mode = "0755"' "$manifest" >"$mac_with_linux_field"
if ./scripts/validate-preview-manifest.py "$mac_with_linux_field" >/dev/null 2>&1; then
  fail "macOS artifact accepted a Linux-only field"
fi

linux_with_mac_field="$tmp_dir/linux-with-mac-field.json"
jq '(.artifacts[] | select(.kind == "linux-native-executable") | .bundle_id) = "ai.2lab.dbotter.preview"' \
  "$manifest" >"$linux_with_mac_field"
if ./scripts/validate-preview-manifest.py "$linux_with_mac_field" >/dev/null 2>&1; then
  fail "Linux artifact accepted a macOS-only field"
fi

missing_target="$tmp_dir/missing-target.json"
jq 'del(.artifacts[-1])' "$manifest" >"$missing_target"
if ./scripts/validate-preview-manifest.py "$missing_target" >/dev/null 2>&1; then
  fail "manifest accepted fewer than four native targets"
fi

printf 'tamper\n' >>"$release_dir/dbotter-preview-linux-x86_64"
if ./scripts/assemble-preview-manifest.py \
  --release-dir "$release_dir" \
  "${descriptors[@]}" \
  --output "$tmp_dir/tampered.json" >/dev/null 2>&1; then
  fail "assembler trusted a stale descriptor instead of remeasuring final bytes"
fi

echo "four-target preview contract: ok"
