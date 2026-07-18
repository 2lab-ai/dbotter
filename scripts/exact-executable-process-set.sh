#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/exact-executable-process-set.sh --assert-empty EXECUTABLE

Fails when any process has EXECUTABLE open as its executing text vnode. The
check uses lsof's file identity selection and does not depend on argv text.
EOF
}

fail() {
  echo "exact executable process guard: $*" >&2
  exit 2
}

[[ $# -eq 2 && "$1" == "--assert-empty" ]] || {
  usage >&2
  exit 2
}

executable="$2"
[[ "$executable" == /* ]] || fail "executable path must be absolute"
[[ -f "$executable" && ! -L "$executable" && -x "$executable" ]] \
  || fail "executable path must be an executable regular file"
command -v lsof >/dev/null 2>&1 || fail "lsof is required"

umask 077
error_file="$(mktemp "${TMPDIR:-/tmp}/dbotter-exact-process.XXXXXX")"
cleanup() {
  rm -f -- "$error_file"
}
trap cleanup EXIT

set +e
lsof_output="$(lsof -a -d txt -Fp "$executable" 2>"$error_file")"
lsof_status=$?
set -e

[[ ! -s "$error_file" ]] || fail "lsof could not inspect the executable"
case "$lsof_status" in
  0|1) ;;
  *) fail "lsof returned an unexpected status" ;;
esac

unexpected="$(
  printf '%s\n' "$lsof_output" \
    | sed -e '/^$/d' -e '/^p[1-9][0-9]*$/d' -e '/^ftxt$/d'
)"
[[ -z "$unexpected" ]] || fail "lsof returned an unexpected field"

process_ids="$(
  printf '%s\n' "$lsof_output" \
    | sed -n 's/^p\([1-9][0-9]*\)$/\1/p' \
    | LC_ALL=C sort -n -u
)"
if [[ "$lsof_status" -eq 0 && -z "$process_ids" ]]; then
  fail "lsof reported a match without a process identity"
fi
if [[ -n "$process_ids" ]]; then
  echo "exact executable process set is not empty" >&2
  exit 1
fi
