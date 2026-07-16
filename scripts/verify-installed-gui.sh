#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
source "$ROOT/scripts/receipt-security.sh"
umask 077

usage() {
  cat <<'EOF'
Usage: scripts/verify-installed-gui.sh --app-path PATH --config PATH --manifest PATH --output PATH

Builds the tracked native AX journey driver reproducibly, then runs it only
after source/package/P7 preflight. The driver emits raw OS observations; this
verifier independently proves PID identity and derives the final verdicts.
EOF
}

fail() {
  echo "installed GUI verification: $*" >&2
  exit 1
}

app_path=""
config=""
manifest=""
output=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --help|-h)
      usage
      exit 0
      ;;
    --app-path)
      [[ $# -ge 2 ]] || fail "--app-path requires a path"
      app_path="$2"
      shift 2
      ;;
    --config)
      [[ $# -ge 2 ]] || fail "--config requires a path"
      config="$2"
      shift 2
      ;;
    --manifest)
      [[ $# -ge 2 ]] || fail "--manifest requires a path"
      manifest="$2"
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

[[ "$(uname -s)" == "Darwin" ]] || fail "installed GUI verification requires macOS"
[[ -n "$config" && -r "$config" && -f "$config" && ! -L "$config" ]] \
  || fail "--config must be a readable regular file, not a symlink"
[[ -n "$manifest" && -f "$manifest" && ! -L "$manifest" ]] \
  || fail "--manifest must be a regular file, not a symlink"
[[ -n "$output" && ! -e "$output" && ! -L "$output" ]] \
  || fail "--output is required and must not already exist"

for p7_dependency in \
  src/export.rs \
  src/export_file.rs \
  src/ui/result_view.rs \
  tests/export_golden.rs \
  tests/export_file_contract.rs \
  tests/ui_raw_input.rs \
  tests/ui_accesskit.rs \
  tests/ui_contrast.rs; do
  [[ -f "$ROOT/$p7_dependency" ]] || fail "P7 dependency is missing: $p7_dependency"
done

for dependency in brew git jq python3 plutil codesign lsof pgrep cargo xcrun cmp; do
  command -v "$dependency" >/dev/null 2>&1 || fail "$dependency is required"
done
ax_driver_builder="$ROOT/scripts/build-native-ax-driver.sh"
ax_driver_source="$ROOT/scripts/native-ax-driver.swift"
ax_driver_source_repo_path="scripts/native-ax-driver.swift"
[[ -f "$ax_driver_builder" && -x "$ax_driver_builder" && ! -L "$ax_driver_builder" ]] \
  || fail "canonical native AX driver builder is unavailable"
[[ -f "$ax_driver_source" && ! -L "$ax_driver_source" ]] \
  || fail "canonical native AX driver source is unavailable"
ax_driver_source_realpath="$(python3 - "$ax_driver_source" <<'PY'
import os
import sys
print(os.path.realpath(sys.argv[1]))
PY
)"
[[ "$ax_driver_source_realpath" == "$ax_driver_source" ]] \
  || fail "canonical native AX driver source path is not exact"
git ls-files --error-unmatch -- "$ax_driver_source_repo_path" >/dev/null 2>&1 \
  || fail "canonical native AX driver source is not tracked"
git ls-files --error-unmatch -- scripts/build-native-ax-driver.sh >/dev/null 2>&1 \
  || fail "canonical native AX driver builder is not tracked"
ax_driver_source_sha256="$(receipt_sha256_file "$ax_driver_source_realpath")"

expected_app="$(brew --prefix dbotter-preview)/Dbotter Preview.app"
[[ "$app_path" == "$expected_app" ]] || fail "--app-path is not the exact Homebrew app path"
[[ -d "$app_path" && ! -L "$app_path" ]] || fail "exact Homebrew app is missing"
executable="$app_path/Contents/MacOS/dbotter"
[[ -f "$executable" && ! -L "$executable" && -x "$executable" ]] \
  || fail "exact app executable is missing"
codesign --verify --deep --strict "$app_path"
[[ "$(plutil -extract CFBundleIdentifier raw "$app_path/Contents/Info.plist")" == "ai.2lab.dbotter.preview" ]] \
  || fail "bundle id mismatch"
"$ROOT/scripts/validate-preview-manifest.py" "$manifest" >/dev/null
source_sha="$(jq -r '.source_sha' "$manifest")"
[[ "$(git rev-parse HEAD)" == "$source_sha" ]] \
  || fail "installed GUI verifier checkout does not equal the manifest source SHA"
git diff --quiet && git diff --cached --quiet \
  || fail "installed GUI verifier has tracked source changes"

cargo test --all-features --locked
source_contracts=true

case "$(uname -m)" in
  arm64) target=aarch64-apple-darwin ;;
  x86_64) target=x86_64-apple-darwin ;;
  *) fail "unsupported installed architecture" ;;
esac
expected_sha256="$(jq -r --arg target "$target" '.artifacts | map(select(.target == $target)) | if length == 1 then .[0].embedded_executable_sha256 else error("artifact") end' "$manifest")" \
  || fail "manifest lacks the exact installed target"
[[ "$(receipt_sha256_file "$executable")" == "$expected_sha256" ]] \
  || fail "exact app executable hash mismatch"

if pgrep -f "$executable" >/dev/null 2>&1; then
  fail "stale exact-app process exists; native driver must start from a clean process set"
fi

required_ax_ids=(
  connection.new
  connection.new.mysql
  connection.new.redis
  connection.mongodb.planned
  profile.connection_id
  profile.host
  profile.redis_tls.ca_file
  profile.redis_tls.ca_file.pick
  profile.credential.session.keep
  profile.credential.session.replace
  profile.credential.session.forget
  profile.delete.active_warning
  editor.target
  editor.input
  editor.row_limit
  editor.timeout
  editor.execute
  editor.history
  editor.cancel
  result.table
  result.copy.cell
  result.copy.row
  result.copy.all
  result.export.csv
  result.export.tsv
  result.export.json
)

temporary="$(mktemp -d "${TMPDIR:-/tmp}/dbotter-installed-gui.XXXXXX")"
output_temporary=""
cleanup() {
  [[ ! -d "$temporary" || -L "$temporary" ]] || rm -rf -- "$temporary"
  [[ -z "$output_temporary" ]] || rm -f "$output_temporary"
}
trap cleanup EXIT HUP INT TERM
mkdir -m 0700 "$temporary/exports"
ax_driver="$temporary/native-ax-driver"
"$ax_driver_builder" --output "$ax_driver"
[[ -f "$ax_driver" && -x "$ax_driver" && ! -L "$ax_driver" ]] \
  || fail "canonical native AX driver build produced no executable"
ax_driver_sha256="$(receipt_sha256_file "$ax_driver")"
printf '%s\n' "${required_ax_ids[@]}" \
  | jq -R -s 'split("\n") | map(select(length > 0))' \
  >"$temporary/required-ax-ids.json"

"$ax_driver" \
  --phase launch \
  --app-path "$app_path" \
  --config "$config" \
  --manifest "$manifest" \
  --output "$temporary/launch-evidence.json"

[[ -f "$temporary/launch-evidence.json" && ! -L "$temporary/launch-evidence.json" ]] \
  || fail "native AX driver produced no regular launch evidence file"
if receipt_candidate_has_static_leak "$temporary/launch-evidence.json"; then
  fail "native AX launch evidence contains a credential or credential-bearing URI"
fi
jq -e \
  --arg app_path "$app_path" '
    (keys | sort) == ["app_path", "bundle_id", "pid", "pid_executable", "schema", "stale_process_disposition"]
    and .schema == "dbotter.installed-gui-launch-evidence.v1"
    and .app_path == $app_path
    and .bundle_id == "ai.2lab.dbotter.preview"
    and (.pid | type == "number" and . > 0 and floor == .)
    and (.stale_process_disposition == "none" or .stale_process_disposition == "terminated")
    and (.pid_executable | (keys | sort) == ["device", "inode", "realpath", "sha256"])
  ' "$temporary/launch-evidence.json" >/dev/null \
  || fail "native AX launch evidence is incomplete or unsafe"

pid="$(jq -r '.pid' "$temporary/launch-evidence.json")"
kill -0 "$pid" >/dev/null 2>&1 || fail "launched app PID is no longer alive"
pid_text_path="$(lsof -a -p "$pid" -d txt -Fn | sed -n 's/^n//p' | head -1)"
[[ -n "$pid_text_path" ]] || fail "lsof could not resolve the launched PID executable"
pid_realpath="$(python3 - "$pid_text_path" <<'PY'
import os
import sys
print(os.path.realpath(sys.argv[1]))
PY
)"
expected_realpath="$(python3 - "$executable" <<'PY'
import os
import sys
print(os.path.realpath(sys.argv[1]))
PY
)"
[[ "$pid_realpath" == "$expected_realpath" ]] || fail "launched PID executable path mismatch"
pid_device="$(stat -L -f '%d' "$pid_realpath")"
pid_inode="$(stat -L -f '%i' "$pid_realpath")"
pid_sha256="$(receipt_sha256_file "$pid_realpath")"
jq -e \
  --arg realpath "$pid_realpath" \
  --argjson device "$pid_device" \
  --argjson inode "$pid_inode" \
  --arg sha256 "$pid_sha256" '
    .pid_executable == {realpath: $realpath, device: $device, inode: $inode, sha256: $sha256}
  ' "$temporary/launch-evidence.json" >/dev/null \
  || fail "driver launch PID identity disagrees with OS readback"
[[ "$pid_sha256" == "$expected_sha256" ]] || fail "launched PID executable hash mismatch"

"$ax_driver" \
  --phase journey \
  --app-path "$app_path" \
  --config "$config" \
  --manifest "$manifest" \
  --pid "$pid" \
  --launch-evidence "$temporary/launch-evidence.json" \
  --required-ids "$temporary/required-ax-ids.json" \
  --export-directory "$temporary/exports" \
  --output "$temporary/native-ax-observations.json"

[[ -f "$temporary/native-ax-observations.json" && ! -L "$temporary/native-ax-observations.json" ]] \
  || fail "native AX driver produced no regular observation file"
if receipt_candidate_has_static_leak "$temporary/native-ax-observations.json"; then
  fail "native AX observations contain a credential or credential-bearing URI"
fi
jq -e \
  --arg app_path "$app_path" \
  --argjson pid "$pid" \
  --arg realpath "$pid_realpath" \
  --argjson device "$pid_device" \
  --argjson inode "$pid_inode" \
  --arg sha256 "$pid_sha256" \
  --arg stale_process_disposition "$(jq -r '.stale_process_disposition' "$temporary/launch-evidence.json")" \
  --slurpfile required "$temporary/required-ax-ids.json" '
  . as $evidence
  | (keys | sort) == ["app_path", "ax_elements", "bundle_id", "interaction_observations", "pid", "pid_executable", "schema", "stale_process_disposition"]
  and .schema == "dbotter.native-ax-observations.v1"
  and .app_path == $app_path
  and .bundle_id == "ai.2lab.dbotter.preview"
  and .pid == $pid
  and .stale_process_disposition == $stale_process_disposition
  and .pid_executable == {realpath: $realpath, device: $device, inode: $inode, sha256: $sha256}
  and (.ax_elements | type == "array" and map(.identifier) == $required[0])
  and all(.ax_elements[];
    (keys | sort) == ["enabled", "focused", "identifier", "order", "role", "title", "value_present", "value_protected"]
    and (.enabled | type == "boolean")
    and (.focused | type == "boolean")
    and (.identifier | type == "string" and length > 0 and length <= 128)
    and (.order | type == "array" and all(.[]; type == "number" and . >= 0 and floor == .))
    and (.role | type == "string" and length > 0 and length <= 128)
    and (.title | type == "string" and length > 0 and length <= 256)
    and (.value_present | type == "boolean")
    and (.value_protected | type == "boolean")
  )
  and (.interaction_observations | type == "array" and length == 7)
  and (.interaction_observations | map(.identifier)) == [
    "editor.execute",
    "result.copy.cell", "result.copy.row", "result.copy.all",
    "result.export.csv", "result.export.tsv", "result.export.json"
  ]
  and all(.interaction_observations[];
    (keys | sort) == ["ax_error", "clipboard", "export", "identifier", "kind", "mechanism"]
    and (.ax_error | type == "number" and floor == . and . == 0)
  )
  and (.interaction_observations[0]
    | .kind == "keyboard"
    and .mechanism == "CGEvent.postToPid"
    and .clipboard == null
    and .export == null
  )
  and all(.interaction_observations[1:4][];
    .kind == "press"
    and .mechanism == "AXUIElementPerformAction"
    and .export == null
    and (.clipboard | (keys | sort) == ["after_count", "before_count", "byte_count", "types"])
    and (.clipboard.before_count | type == "number" and . >= 0 and floor == .)
    and (.clipboard.after_count | type == "number" and floor == .)
    and (.clipboard.after_count > .clipboard.before_count)
    and (.clipboard.byte_count | type == "number" and . > 0 and floor == .)
    and (.clipboard.types | type == "array" and length > 0 and length == (unique | length))
    and all(.clipboard.types[]; type == "string" and length > 0 and length <= 128)
  )
  and all(.interaction_observations[4:7][];
    .kind == "press"
    and .mechanism == "AXUIElementPerformAction"
    and .clipboard == null
    and (.export | (keys | sort) == ["basename", "byte_count", "exists", "mode", "regular"])
    and (.export.basename | type == "string")
    and (.export.exists == true)
    and (.export.regular == true)
    and (.export.mode == 384)
    and (.export.byte_count | type == "number" and . > 0 and floor == .)
  )
  and .interaction_observations[4].export.basename == "dbotter-native-ax-result.csv"
  and .interaction_observations[5].export.basename == "dbotter-native-ax-result.tsv"
  and .interaction_observations[6].export.basename == "dbotter-native-ax-result.json"
  ' "$temporary/native-ax-observations.json" >/dev/null \
  || fail "native AX observations are incomplete or unsafe"

kill -0 "$pid" >/dev/null 2>&1 || fail "launched app PID is no longer alive"

output_parent="$(dirname "$output")"
mkdir -p "$output_parent"
output_temporary="$(mktemp "$output_parent/.installed-gui.XXXXXX.json")"
jq \
  --arg source_sha "$source_sha" \
  --arg executable_sha256 "$ax_driver_sha256" \
  --arg source_repo_path "$ax_driver_source_repo_path" \
  --arg source_sha256 "$ax_driver_source_sha256" \
  --argjson pid "$pid" \
  --arg realpath "$pid_realpath" \
  --argjson device "$pid_device" \
  --argjson inode "$pid_inode" \
  --arg sha256 "$pid_sha256" \
  --argjson source_contracts "$source_contracts" \
  --slurpfile required "$temporary/required-ax-ids.json" '
    . as $observations
    | ($observations.ax_elements | map(.identifier)) as $ax_identifiers
    | ($ax_identifiers == $required[0]
        and all($observations.ax_elements[];
          (.enabled | type == "boolean")
          and (.focused | type == "boolean")
          and (.role | type == "string" and length > 0)
          and (.title | type == "string" and length > 0)
        )) as $accessibility
    | ($ax_identifiers == $required[0]) as $ax_identifier_readback
    | (all($observations.interaction_observations[1:4][];
          .ax_error == 0
          and .clipboard.after_count > .clipboard.before_count
          and .clipboard.byte_count > 0
          and (.clipboard.types | length > 0)
        )) as $clipboard_contracts
    | (all($observations.interaction_observations[4:7][];
          .ax_error == 0
          and .export.exists == true
          and .export.regular == true
          and .export.mode == 384
          and .export.byte_count > 0
        )) as $export_contracts
    | ($observations.pid == $pid
        and $observations.pid_executable == {
          realpath: $realpath,
          device: $device,
          inode: $inode,
          sha256: $sha256
        }) as $pid_identity
    | ($observations.stale_process_disposition == "none"
        or $observations.stale_process_disposition == "terminated") as $stale_process_handled
    | {
        accessibility: $accessibility,
        ax_identifier_readback: $ax_identifier_readback,
        clipboard_contracts: ($source_contracts and $clipboard_contracts),
        contrast: $source_contracts,
        create_recovery: $source_contracts,
        disclosure: ($source_contracts and $accessibility),
        export_contracts: ($source_contracts and $export_contracts),
        pid_identity: $pid_identity,
        recovery_totality: $source_contracts,
        session_intents: $source_contracts,
        stale_process_handled: $stale_process_handled,
        tls_split_recovery: $source_contracts
      } as $derived
    | {
        schema: "dbotter.installed-gui-evidence.v1",
        source_sha: $source_sha,
        driver: {
          executable_sha256: $executable_sha256,
          source_repo_path: $source_repo_path,
          source_sha256: $source_sha256
        },
        app_path: $observations.app_path,
        bundle_id: $observations.bundle_id,
        pid: $observations.pid,
        pid_executable: $observations.pid_executable,
        stale_process_disposition: $observations.stale_process_disposition,
        ax_identifiers: $ax_identifiers,
        assertions: ($derived + {overall: all($derived[]; . == true)})
      }
  ' "$temporary/native-ax-observations.json" >"$output_temporary"
chmod 0600 "$output_temporary"
if receipt_candidate_has_static_leak "$output_temporary"; then
  fail "generated installed GUI evidence contains a credential or credential-bearing URI"
fi
if ! jq -e '.assertions.overall == true' "$output_temporary" >/dev/null; then
  fail "derived installed GUI assertions are not all true"
fi
ln "$output_temporary" "$output" 2>/dev/null \
  || fail "refusing to replace installed GUI evidence"
rm -f "$output_temporary"
output_temporary=""
echo "installed GUI verification: ok: $output"
