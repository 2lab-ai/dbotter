#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
SOURCE_RELATIVE="scripts/native-j2-ax-driver.swift"
SOURCE="$ROOT/$SOURCE_RELATIVE"

usage() {
  cat <<'EOF'
Usage: scripts/build-native-j2-ax-driver.sh --output PATH

Compiles the fixed tracked J2 native AX driver twice with the same macOS SDK,
compares the outputs byte-for-byte, and publishes one no-replace executable.
EOF
}

fail() {
  echo "native J2 AX driver build: $*" >&2
  exit 1
}

output=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --help|-h)
      usage
      exit 0
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

[[ "$(uname -s)" == "Darwin" ]] || fail "macOS is required"
[[ -n "$output" ]] || fail "--output is required"
[[ -f "$SOURCE" && ! -L "$SOURCE" ]] || fail "canonical driver source is unavailable"
git -C "$ROOT" ls-files --error-unmatch -- "$SOURCE_RELATIVE" >/dev/null 2>&1 \
  || fail "canonical driver source is not tracked"

output_parent="$(dirname "$output")"
[[ -d "$output_parent" && ! -L "$output_parent" ]] \
  || fail "--output parent must be a directory and not a symlink"
[[ ! -e "$output" && ! -L "$output" ]] || fail "--output already exists"

sdk="$(xcrun --sdk macosx --show-sdk-path)"
[[ -d "$sdk" ]] || fail "xcrun could not resolve the macOS SDK"

temporary="$(mktemp -d "$output_parent/.native-j2-ax-build.XXXXXX")"
first_dir="$temporary/first"
second_dir="$temporary/second"
module_cache="$temporary/module-cache"
first="$first_dir/native-j2-ax-driver"
second="$second_dir/native-j2-ax-driver"
cleanup() {
  case "$temporary" in
    "$output_parent"/.native-j2-ax-build.*) rm -rf -- "$temporary" ;;
    *) echo "native J2 AX driver build: refusing unexpected cleanup path" >&2 ;;
  esac
}
on_exit() {
  local status=$?
  trap - EXIT HUP INT TERM
  cleanup
  exit "$status"
}
trap on_exit EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p "$first_dir" "$second_dir" "$module_cache"

macos_major="$(sw_vers -productVersion | cut -d. -f1)"

compile() {
  local destination="$1"
  local compile_arguments=(
    -sdk "$sdk"
    -module-cache-path "$module_cache"
    -parse-as-library
    -module-name DbotterNativeJ2AXDriver
    -O
    -whole-module-optimization
    -framework ApplicationServices
    -framework AppKit
  )
  if [[ "$macos_major" =~ ^[0-9]+$ ]] && (( macos_major < 26 )); then
    compile_arguments+=(-Xlinker -no_uuid)
  fi
  compile_arguments+=(
    "$SOURCE"
    -o "$destination"
  )
  xcrun --sdk macosx swiftc "${compile_arguments[@]}"
  chmod 0755 "$destination"
}

compile "$first"
compile "$second"
cmp -s "$first" "$second" || fail "repeated swiftc outputs are not byte-identical"
[[ ! -e "$output" && ! -L "$output" ]] || fail "--output appeared during compilation"
ln "$first" "$output" || fail "could not publish --output without replacement"
[[ -f "$output" && ! -L "$output" && -x "$output" ]] \
  || fail "published driver is not an executable regular file"

echo "native J2 AX driver build: ok: $output"
