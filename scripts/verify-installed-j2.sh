#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
# shellcheck source=scripts/receipt-security.sh
source "$ROOT/scripts/receipt-security.sh"
umask 077

usage() {
  cat <<'EOF'
Usage: scripts/verify-installed-j2.sh \
  --app-path PATH --config PATH --manifest PATH \
  --mysql-container NAME --output PATH

Runs the six-step installed J2 acceptance against an isolated classified-v3
fixture. It force-kills only the exact process it launches and corrupts only the
first fixture profile's private shard after proving the config is under TMPDIR.
The output contains booleans, counts, identities, modes and sizes, never profile
text, SQL text, result values, credentials or backend prose.
EOF
}

fail() {
  echo "installed J2 verification: $*" >&2
  exit 1
}

app_path=""
config=""
manifest=""
mysql_container=""
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
    --mysql-container)
      [[ $# -ge 2 ]] || fail "--mysql-container requires a name"
      mysql_container="$2"
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

[[ "$(uname -s)" == "Darwin" ]] || fail "macOS is required"
[[ -n "$app_path" && -d "$app_path" && ! -L "$app_path" ]] \
  || fail "--app-path must be an exact app directory"
[[ -n "$config" && -f "$config" && ! -L "$config" && -r "$config" ]] \
  || fail "--config must be a readable regular file"
[[ -n "$manifest" && -f "$manifest" && ! -L "$manifest" ]] \
  || fail "--manifest must be a regular file"
[[ "$mysql_container" =~ ^[a-zA-Z0-9][a-zA-Z0-9_.-]{0,127}$ ]] \
  || fail "--mysql-container is invalid"
[[ -n "$output" && ! -e "$output" && ! -L "$output" ]] \
  || fail "--output must not already exist"
[[ -n "${DBOTTER_MYSQL_PASSWORD:-}" ]] || fail "DBOTTER_MYSQL_PASSWORD is required"
[[ -n "${DBOTTER_MYSQL_ROOT_PASSWORD:-}" ]] \
  || fail "DBOTTER_MYSQL_ROOT_PASSWORD is required"

for dependency in \
  brew codesign docker git jq lsof pgrep plutil python3 shasum stat xcrun; do
  command -v "$dependency" >/dev/null 2>&1 || fail "$dependency is required"
done

builder="$ROOT/scripts/build-native-j2-ax-driver.sh"
driver_source="$ROOT/scripts/native-j2-ax-driver.swift"
[[ -x "$builder" && -f "$builder" && ! -L "$builder" ]] \
  || fail "scripts/build-native-j2-ax-driver.sh is unavailable"
[[ -f "$driver_source" && ! -L "$driver_source" ]] \
  || fail "scripts/native-j2-ax-driver.swift is unavailable"
git -C "$ROOT" ls-files --error-unmatch \
  scripts/build-native-j2-ax-driver.sh \
  scripts/native-j2-ax-driver.swift >/dev/null 2>&1 \
  || fail "canonical J2 AX dependencies must be tracked"

"$ROOT/scripts/validate-preview-manifest.py" "$manifest" >/dev/null
source_sha="$(jq -r '.source_sha' "$manifest")"
tag="$(jq -r '.tag' "$manifest")"
[[ "$(git -C "$ROOT" rev-parse HEAD)" == "$source_sha" ]] \
  || fail "checkout HEAD does not equal the Preview source SHA"
git -C "$ROOT" diff --quiet
git -C "$ROOT" diff --cached --quiet

expected_app="$(brew --prefix dbotter-preview)/Dbotter Preview.app"
[[ "$app_path" == "$expected_app" ]] || fail "--app-path is not the exact installed app"
executable="$app_path/Contents/MacOS/dbotter"
[[ -f "$executable" && ! -L "$executable" && -x "$executable" ]] \
  || fail "installed executable is unavailable"
codesign --verify --deep --strict "$app_path"
[[ "$(plutil -extract CFBundleIdentifier raw "$app_path/Contents/Info.plist")" \
    == "ai.2lab.dbotter.preview" ]] || fail "bundle identifier mismatch"

case "$(uname -m)" in
  arm64) target="aarch64-apple-darwin" ;;
  x86_64) target="x86_64-apple-darwin" ;;
  *) fail "unsupported installed architecture" ;;
esac
expected_executable_sha256="$(
  jq -r --arg target "$target" '
    .artifacts
    | map(select(.target == $target))
    | if length == 1 then .[0].embedded_executable_sha256 else error("artifact") end
  ' "$manifest"
)"
actual_executable_sha256="$(shasum -a 256 "$executable" | awk '{print $1}')"
[[ "$actual_executable_sha256" == "$expected_executable_sha256" ]] \
  || fail "installed executable hash does not match the Preview manifest"

config_realpath="$(python3 - "$config" <<'PY'
import os
import sys
print(os.path.realpath(sys.argv[1]))
PY
)"
tmp_realpath="$(python3 - "${TMPDIR:-/tmp}" <<'PY'
import os
import sys
print(os.path.realpath(sys.argv[1]))
PY
)"
case "$config_realpath" in
  "$tmp_realpath"/*) ;;
  *) fail "destructive J2 fixture config must be isolated under TMPDIR" ;;
esac
[[ "$(stat -f '%Lp' "$config")" == "600" ]] || fail "fixture config mode must be 0600"
grep -Eq '^version = 3$' "$config" || fail "fixture config must be classified v3"
grep -Fq 'name = "J2 Primary"' "$config" || fail "fixture lacks J2 Primary"
grep -Fq 'name = "J2 Healthy"' "$config" || fail "fixture lacks J2 Healthy"
grep -Fq 'credential_mode = "environment"' "$config" \
  || fail "fixture must use an Environment credential"

instance_ids=()
while IFS= read -r instance_id; do
  instance_ids+=("$instance_id")
done < <(sed -n 's/^instance_id = "\([0-9a-f][0-9a-f]*\)"$/\1/p' "$config")
[[ "${#instance_ids[@]}" -eq 2 ]] || fail "fixture must contain exactly two instance IDs"
for instance_id in "${instance_ids[@]}"; do
  [[ "$instance_id" =~ ^[0-9a-f]{32}$ ]] || fail "fixture instance ID is invalid"
done
[[ "${instance_ids[0]}" != "${instance_ids[1]}" ]] \
  || fail "fixture instance IDs must be unique"

config_parent="$(dirname "$config_realpath")"
config_basename="$(basename "$config_realpath")"
workspace_root="$config_parent/.${config_basename}.workspace"
[[ ! -e "$workspace_root" && ! -L "$workspace_root" ]] \
  || fail "isolated fixture workspace must start absent"

docker inspect "$mysql_container" >/dev/null 2>&1 \
  || fail "MySQL fixture container is unavailable"
[[ "$(docker inspect -f '{{.State.Running}}' "$mysql_container")" == "true" ]] \
  || fail "MySQL fixture container is not running"

temporary="$(mktemp -d "${TMPDIR:-/tmp}/dbotter-installed-j2.XXXXXX")"
driver="$temporary/native-j2-ax-driver"
seed_evidence="$temporary/seed.json"
restart_evidence="$temporary/restart.json"
history_evidence="$temporary/history-open.json"
explicit_evidence="$temporary/explicit-run.json"
second_evidence="$temporary/second-instance.json"
corrupt_evidence="$temporary/corrupt-reopen.json"
seed_pid=""
restart_pid=""
corrupt_pid=""
general_log_enabled=false
output_temporary=""

stop_pid() {
  local pid="$1"
  [[ -n "$pid" && "$pid" =~ ^[1-9][0-9]*$ ]] || return 0
  kill -0 "$pid" >/dev/null 2>&1 || return 0
  kill -TERM "$pid" >/dev/null 2>&1 || true
  for _ in {1..50}; do
    kill -0 "$pid" >/dev/null 2>&1 || return 0
    sleep 0.1
  done
  kill -KILL "$pid" >/dev/null 2>&1 || true
  for _ in {1..50}; do
    kill -0 "$pid" >/dev/null 2>&1 || return 0
    sleep 0.1
  done
}

mysql_root_exec() {
  docker exec \
    -e "MYSQL_PWD=$DBOTTER_MYSQL_ROOT_PASSWORD" \
    "$mysql_container" \
    mysql --batch --skip-column-names --user=root --database=mysql --execute "$1"
}

cleanup() {
  local status=$?
  trap - EXIT HUP INT TERM
  stop_pid "$corrupt_pid"
  stop_pid "$restart_pid"
  stop_pid "$seed_pid"
  if [[ "$general_log_enabled" == true ]]; then
    mysql_root_exec "SET GLOBAL general_log = OFF" >/dev/null 2>&1 || true
  fi
  [[ ! -d "$temporary" || -L "$temporary" ]] || rm -rf -- "$temporary"
  [[ -z "$output_temporary" ]] || rm -f -- "$output_temporary"
  exit "$status"
}
trap cleanup EXIT HUP INT TERM

if pgrep -f "$executable" >/dev/null 2>&1; then
  fail "stale exact installed-app process exists"
fi

"$builder" --output "$driver"
[[ -f "$driver" && ! -L "$driver" && -x "$driver" ]] \
  || fail "J2 AX driver build produced no executable"

mysql_root_exec \
  "SET GLOBAL general_log = OFF; TRUNCATE TABLE mysql.general_log; SET GLOBAL log_output = 'TABLE'; SET GLOBAL general_log = ON" \
  >/dev/null
general_log_enabled=true

"$driver" \
  --phase seed \
  --app-path "$app_path" \
  --config "$config" \
  --output "$seed_evidence"

jq -e '
  .schema == "dbotter.installed-j2-ax-observations.v1"
  and .phase == "seed"
  and (.pid | type == "number" and . > 0 and floor == .)
  and .checkpoints.tabs_created_renamed_reordered == true
  and .checkpoints.saved_visible_before_kill == true
  and (.split_value | type == "number")
' "$seed_evidence" >/dev/null || fail "seed AX evidence is invalid"
seed_pid="$(jq -r '.pid' "$seed_evidence")"
kill -0 "$seed_pid" >/dev/null 2>&1 || fail "seed Preview PID exited"

pid_text_path="$(lsof -a -p "$seed_pid" -d txt -Fn | sed -n 's/^n//p' | head -1)"
pid_realpath="$(python3 - "$pid_text_path" <<'PY'
import os
import sys
print(os.path.realpath(sys.argv[1]))
PY
)"
executable_realpath="$(python3 - "$executable" <<'PY'
import os
import sys
print(os.path.realpath(sys.argv[1]))
PY
)"
[[ "$pid_realpath" == "$executable_realpath" ]] \
  || fail "seed PID does not execute the exact installed binary"
[[ "$(shasum -a 256 "$pid_realpath" | awk '{print $1}')" \
    == "$expected_executable_sha256" ]] || fail "seed PID executable hash mismatch"

primary_profile_directory="$workspace_root/profiles/${instance_ids[0]}"
primary_manifest="$primary_profile_directory/manifest.json"
for _ in {1..150}; do
  [[ -f "$primary_manifest" && ! -L "$primary_manifest" ]] && break
  sleep 0.1
done
[[ -f "$primary_manifest" && ! -L "$primary_manifest" ]] \
  || fail "visible Saved produced no private manifest"
[[ "$(stat -f '%Lp' "$workspace_root")" == "700" ]] \
  || fail "workspace root mode is not 0700"
[[ "$(stat -f '%Lp' "$primary_profile_directory")" == "700" ]] \
  || fail "profile workspace mode is not 0700"
[[ "$(stat -f '%Lp' "$primary_manifest")" == "600" ]] \
  || fail "workspace manifest mode is not 0600"

workspace_manifest_generation="$(jq -r '.generation' "$primary_manifest")"
[[ "$workspace_manifest_generation" =~ ^[1-9][0-9]*$ ]] \
  || fail "workspace manifest generation is invalid"
primary_shard_name="$(jq -r '.shard' "$primary_manifest")"
[[ "$primary_shard_name" =~ ^shard-[0-9]{20}\.json$ ]] \
  || fail "workspace manifest shard name is invalid"
primary_shard="$primary_profile_directory/$primary_shard_name"
[[ -f "$primary_shard" && ! -L "$primary_shard" ]] \
  || fail "workspace manifest references no regular shard"
[[ "$(stat -f '%Lp' "$primary_shard")" == "600" ]] \
  || fail "workspace shard mode is not 0600"
manifest_bytes="$(stat -f '%z' "$primary_manifest")"
shard_bytes="$(stat -f '%z' "$primary_shard")"

kill -KILL "$seed_pid"
for _ in {1..100}; do
  kill -0 "$seed_pid" >/dev/null 2>&1 || break
  sleep 0.1
done
kill -0 "$seed_pid" >/dev/null 2>&1 && fail "force-killed seed PID remained alive"
seed_pid=""

"$driver" \
  --phase restart \
  --app-path "$app_path" \
  --config "$config" \
  --seed-evidence "$seed_evidence" \
  --output "$restart_evidence"

jq -e '
  .schema == "dbotter.installed-j2-ax-observations.v1"
  and .phase == "restart"
  and .checkpoints.tabs_restored == true
  and .checkpoints.results_omitted_after_restart == true
' "$restart_evidence" >/dev/null || fail "restart AX evidence is invalid"
restart_pid="$(jq -r '.pid' "$restart_evidence")"
kill -0 "$restart_pid" >/dev/null 2>&1 || fail "restart Preview PID exited"

mysql_count() {
  mysql_root_exec \
    "SELECT COUNT(*) FROM mysql.general_log WHERE command_type = 'Execute' AND argument = 'SELECT 42 AS j2_second'"
}

zero_dispatch_before="$(mysql_count)"
[[ "$zero_dispatch_before" =~ ^[0-9]+$ ]] || fail "invalid pre-open MySQL counter"

"$driver" \
  --phase history-open \
  --app-path "$app_path" \
  --config "$config" \
  --pid "$restart_pid" \
  --output "$history_evidence"

jq -e '
  .schema == "dbotter.installed-j2-ax-observations.v1"
  and .phase == "history-open"
  and .checkpoints.history_opened_without_run == true
' "$history_evidence" >/dev/null || fail "history-open AX evidence is invalid"
zero_dispatch_after_open="$(mysql_count)"
[[ "$zero_dispatch_after_open" == "$zero_dispatch_before" ]] \
  || fail "opening history dispatched a database query"

"$driver" \
  --phase explicit-run \
  --app-path "$app_path" \
  --config "$config" \
  --pid "$restart_pid" \
  --output "$explicit_evidence"

jq -e '
  .schema == "dbotter.installed-j2-ax-observations.v1"
  and .phase == "explicit-run"
  and .checkpoints.explicit_run_completed == true
' "$explicit_evidence" >/dev/null || fail "explicit-run AX evidence is invalid"
zero_dispatch_after_run="$(mysql_count)"
[[ "$zero_dispatch_after_run" =~ ^[0-9]+$ ]] \
  || fail "invalid post-run MySQL counter"
(( zero_dispatch_after_run == zero_dispatch_before + 1 )) \
  || fail "explicit history rerun did not dispatch exactly one expected query"
fresh_result_after_explicit_run=true

"$driver" \
  --phase second-instance \
  --app-path "$app_path" \
  --config "$config" \
  --output "$second_evidence"

jq -e '
  .schema == "dbotter.installed-j2-ax-observations.v1"
  and .phase == "second-instance"
  and .checkpoints.second_instance_read_only == true
' "$second_evidence" >/dev/null || fail "second-instance AX evidence is invalid"
second_instance_read_only=true
kill -0 "$restart_pid" >/dev/null 2>&1 \
  || fail "writer process exited during second-instance proof"

stop_pid "$restart_pid"
restart_pid=""
for _ in {1..100}; do
  pgrep -f "$executable" >/dev/null 2>&1 || break
  sleep 0.1
done
pgrep -f "$executable" >/dev/null 2>&1 \
  && fail "writer process set did not close before corruption fixture"

primary_shard_name="$(jq -r '.shard' "$primary_manifest")"
primary_shard="$primary_profile_directory/$primary_shard_name"
[[ -f "$primary_shard" && ! -L "$primary_shard" ]] \
  || fail "latest primary shard is unavailable before corruption"
printf '\n' >>"$primary_shard"

"$driver" \
  --phase corrupt-reopen \
  --app-path "$app_path" \
  --config "$config" \
  --output "$corrupt_evidence"

jq -e '
  .schema == "dbotter.installed-j2-ax-observations.v1"
  and .phase == "corrupt-reopen"
  and .checkpoints.corrupt_profile_quarantined == true
  and .checkpoints.healthy_profile_remains_usable == true
' "$corrupt_evidence" >/dev/null || fail "corrupt-reopen AX evidence is invalid"
corrupt_pid="$(jq -r '.pid' "$corrupt_evidence")"
kill -0 "$corrupt_pid" >/dev/null 2>&1 || fail "corrupt-reopen Preview PID exited"

shopt -s nullglob
quarantine_entries=("$workspace_root"/quarantine/q-*)
shopt -u nullglob
quarantine_files="${#quarantine_entries[@]}"
(( quarantine_files > 0 )) || fail "corrupt shard produced no bounded quarantine entry"
corrupt_profile_quarantined=true

healthy_query_count="$(
  mysql_root_exec \
    "SELECT COUNT(*) FROM mysql.general_log WHERE command_type = 'Execute' AND argument = 'SELECT 84 AS j2_healthy'"
)"
[[ "$healthy_query_count" =~ ^[1-9][0-9]*$ ]] \
  || fail "healthy profile produced no independent server observation"
healthy_profile_remains_usable=true

stop_pid "$corrupt_pid"
corrupt_pid=""
mysql_root_exec "SET GLOBAL general_log = OFF" >/dev/null
general_log_enabled=false

tabs_created_renamed_reordered="$(
  jq -r '.checkpoints.tabs_created_renamed_reordered' "$seed_evidence"
)"
saved_visible_before_kill="$(
  jq -r '.checkpoints.saved_visible_before_kill' "$seed_evidence"
)"
tabs_restored="$(jq -r '.checkpoints.tabs_restored' "$restart_evidence")"
results_omitted_after_restart="$(
  jq -r '.checkpoints.results_omitted_after_restart' "$restart_evidence"
)"
history_opened_without_run="$(
  jq -r '.checkpoints.history_opened_without_run' "$history_evidence"
)"

output_parent="$(dirname "$output")"
[[ -d "$output_parent" && ! -L "$output_parent" ]] \
  || fail "--output parent must be a directory and not a symlink"
output_temporary="$(mktemp "$output_parent/.installed-j2.XXXXXX.json")"
chmod 0600 "$output_temporary"
jq -n \
  --arg schema "dbotter.installed-j2-evidence.v1" \
  --arg source_sha "$source_sha" \
  --arg tag "$tag" \
  --arg target "$target" \
  --arg executable_sha256 "$actual_executable_sha256" \
  --argjson workspace_manifest_generation "$workspace_manifest_generation" \
  --argjson manifest_bytes "$manifest_bytes" \
  --argjson shard_bytes "$shard_bytes" \
  --argjson quarantine_files "$quarantine_files" \
  --argjson zero_dispatch_before "$zero_dispatch_before" \
  --argjson zero_dispatch_after_open "$zero_dispatch_after_open" \
  --argjson zero_dispatch_after_run "$zero_dispatch_after_run" \
  --argjson healthy_query_count "$healthy_query_count" \
  --argjson tabs_created_renamed_reordered "$tabs_created_renamed_reordered" \
  --argjson saved_visible_before_kill "$saved_visible_before_kill" \
  --argjson tabs_restored "$tabs_restored" \
  --argjson results_omitted_after_restart "$results_omitted_after_restart" \
  --argjson history_opened_without_run "$history_opened_without_run" \
  --argjson fresh_result_after_explicit_run "$fresh_result_after_explicit_run" \
  --argjson second_instance_read_only "$second_instance_read_only" \
  --argjson corrupt_profile_quarantined "$corrupt_profile_quarantined" \
  --argjson healthy_profile_remains_usable "$healthy_profile_remains_usable" '
  {
    schema: $schema,
    source_sha: $source_sha,
    tag: $tag,
    installed_identity: {
      target: $target,
      executable_sha256: $executable_sha256
    },
    checkpoints: {
      tabs_created_renamed_reordered: $tabs_created_renamed_reordered,
      saved_visible_before_kill: $saved_visible_before_kill,
      tabs_restored: $tabs_restored,
      results_omitted_after_restart: $results_omitted_after_restart,
      history_opened_without_run: $history_opened_without_run,
      fresh_result_after_explicit_run: $fresh_result_after_explicit_run,
      second_instance_read_only: $second_instance_read_only,
      corrupt_profile_quarantined: $corrupt_profile_quarantined,
      healthy_profile_remains_usable: $healthy_profile_remains_usable
    },
    private_store: {
      workspace_manifest_generation: $workspace_manifest_generation,
      manifest_bytes: $manifest_bytes,
      shard_bytes: $shard_bytes,
      root_mode: 448,
      file_mode: 384,
      quarantine_files: $quarantine_files
    },
    mysql_observation: {
      zero_dispatch_before: $zero_dispatch_before,
      zero_dispatch_after_open: $zero_dispatch_after_open,
      after_explicit_run: $zero_dispatch_after_run,
      healthy_profile_query_count: $healthy_query_count
    }
  }
' >"$output_temporary"

jq -e '
  (keys | sort) == [
    "checkpoints", "installed_identity", "mysql_observation", "private_store",
    "schema", "source_sha", "tag"
  ]
  and .schema == "dbotter.installed-j2-evidence.v1"
  and (.source_sha | test("^[0-9a-f]{40}$"))
  and (.tag | startswith("preview-"))
  and all(.checkpoints[]; . == true)
  and .private_store.workspace_manifest_generation > 0
  and .private_store.root_mode == 448
  and .private_store.file_mode == 384
  and .private_store.quarantine_files > 0
  and .mysql_observation.zero_dispatch_before
      == .mysql_observation.zero_dispatch_after_open
  and .mysql_observation.after_explicit_run
      == (.mysql_observation.zero_dispatch_before + 1)
  and .mysql_observation.healthy_profile_query_count > 0
' "$output_temporary" >/dev/null || fail "assembled J2 evidence is invalid"

if receipt_candidate_has_static_leak "$output_temporary" \
  || receipt_candidate_contains_secret "$output_temporary" "$DBOTTER_MYSQL_PASSWORD" \
  || receipt_candidate_contains_secret "$output_temporary" "$DBOTTER_MYSQL_ROOT_PASSWORD"; then
  fail "assembled J2 evidence contains a credential or value-bearing payload"
fi

ln "$output_temporary" "$output" \
  || fail "could not publish --output without replacement"
rm -f -- "$output_temporary"
output_temporary=""
[[ -f "$output" && ! -L "$output" && "$(stat -f '%Lp' "$output")" == "600" ]] \
  || fail "published J2 evidence is not a private regular file"

echo "installed J2 verification: ok: $output"
