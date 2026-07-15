#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
SOURCE_RELATIVE="scripts/native-ax-driver.swift"
SOURCE="$ROOT/$SOURCE_RELATIVE"

usage() {
  cat <<'EOF'
Usage: scripts/build-native-ax-driver.sh --output PATH

Compiles the fixed, tracked native-ax-driver.swift source twice with the same
macOS SDK and deterministic linker flags, compares both outputs byte-for-byte,
and publishes the executable with an atomic no-replace hard link.
EOF
}

fail() {
  echo "native AX driver build: $*" >&2
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
[[ -f "$SOURCE" && ! -L "$SOURCE" ]] || fail "canonical driver source must be a regular file"
git -C "$ROOT" ls-files --error-unmatch -- "$SOURCE_RELATIVE" >/dev/null 2>&1 \
  || fail "canonical driver source is not tracked"

output_parent="$(dirname "$output")"
[[ -d "$output_parent" && ! -L "$output_parent" ]] \
  || fail "--output parent must be a directory and not a symlink"
[[ ! -e "$output" && ! -L "$output" ]] || fail "--output already exists"

swiftc="$(/usr/bin/xcrun --sdk macosx --find swiftc)"
sdk="$(/usr/bin/xcrun --sdk macosx --show-sdk-path)"
[[ -x "$swiftc" && -d "$sdk" ]] || fail "xcrun could not resolve the macOS swiftc and SDK"

temporary="$(mktemp -d "$output_parent/.native-ax-build.XXXXXX")"
first_dir="$temporary/first"
second_dir="$temporary/second"
module_cache="$temporary/module-cache"
first="$first_dir/native-ax-driver"
second="$second_dir/native-ax-driver"
cleanup() {
  case "$temporary" in
    "$output_parent"/.native-ax-build.*) rm -rf -- "$temporary" ;;
    *) echo "native AX driver build: refusing to clean unexpected temporary path" >&2 ;;
  esac
}
trap 'status=$?; trap - EXIT HUP INT TERM; cleanup; exit "$status"' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p "$first_dir" "$second_dir" "$module_cache"

# ld's historical deterministic switch is -no_uuid. macOS 26 dyld refuses to
# execute a Mach-O without LC_UUID, so current hosts retain the deterministic
# content-derived UUID and prove repeatability with identical basenames + cmp.
linker_uuid_flag=""
macos_major="$(/usr/bin/sw_vers -productVersion | /usr/bin/cut -d. -f1)"
if [[ "$macos_major" =~ ^[0-9]+$ ]] && (( macos_major < 26 )); then
  linker_uuid_flag="-no_uuid"
fi

compile() {
  destination="$1"
  swiftc_args=(
    -sdk "$sdk"
    -module-cache-path "$module_cache"
    -parse-as-library
    -module-name DbotterNativeAXDriver
    -O
    -whole-module-optimization
    -framework AppKit
    -framework ApplicationServices
  )
  if [[ -n "$linker_uuid_flag" ]]; then
    swiftc_args+=(-Xlinker "$linker_uuid_flag")
  fi
  "$swiftc" "${swiftc_args[@]}" "$SOURCE" -o "$destination"
  chmod 0755 "$destination"
}

compile "$first"
compile "$second"
cmp -s "$first" "$second" || fail "repeated swiftc outputs are not byte-identical"
[[ ! -e "$output" && ! -L "$output" ]] || fail "--output appeared during compilation"
ln "$first" "$output" || fail "could not publish --output without replacement"
[[ -f "$output" && ! -L "$output" && -x "$output" ]] \
  || fail "published driver is not an executable regular file"

echo "native AX driver build: ok: $output"
