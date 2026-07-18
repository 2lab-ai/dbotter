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
[[ "${DBOTTER_MYSQL_PASSWORD:-}" == "dbotter-local-only" ]] \
  || fail "DBOTTER_MYSQL_PASSWORD does not match the dedicated fixture"
[[ "${DBOTTER_MYSQL_ROOT_PASSWORD:-}" == "root-local-only" ]] \
  || fail "DBOTTER_MYSQL_ROOT_PASSWORD does not match the dedicated fixture"

for dependency in \
  brew codesign docker git jq lsof plutil python3 shasum stat xcrun; do
  command -v "$dependency" >/dev/null 2>&1 || fail "$dependency is required"
done

builder="$ROOT/scripts/build-native-j2-ax-driver.sh"
driver_source="$ROOT/scripts/native-j2-ax-driver.swift"
ax_guard="$ROOT/scripts/run-source-bound-ax-driver.py"
scanner="$ROOT/scripts/scan-private-workspace.py"
process_guard="$ROOT/scripts/exact-executable-process-set.sh"
fixture_compose="$ROOT/tests/fixtures/installed-j2/compose.yml"
[[ -x "$builder" && -f "$builder" && ! -L "$builder" ]] \
  || fail "scripts/build-native-j2-ax-driver.sh is unavailable"
[[ -f "$driver_source" && ! -L "$driver_source" ]] \
  || fail "scripts/native-j2-ax-driver.swift is unavailable"
[[ -x "$ax_guard" && -f "$ax_guard" && ! -L "$ax_guard" ]] \
  || fail "scripts/run-source-bound-ax-driver.py is unavailable"
[[ -x "$scanner" && -f "$scanner" && ! -L "$scanner" ]] \
  || fail "scripts/scan-private-workspace.py is unavailable"
[[ -x "$process_guard" && -f "$process_guard" && ! -L "$process_guard" ]] \
  || fail "scripts/exact-executable-process-set.sh is unavailable"
[[ -f "$fixture_compose" && ! -L "$fixture_compose" ]] \
  || fail "installed J2 fixture compose is unavailable"
git -C "$ROOT" ls-files --error-unmatch \
  scripts/build-native-j2-ax-driver.sh \
  scripts/exact-executable-process-set.sh \
  scripts/native-j2-ax-driver.swift \
  scripts/run-source-bound-ax-driver.py \
  scripts/scan-private-workspace.py \
  tests/fixtures/installed-j2/compose.yml >/dev/null 2>&1 \
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
expected_executable_device_inode="$(stat -f '%d:%i' "$executable")"

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
[[ "$(grep -Fc 'host = "127.0.0.1"' "$config")" -eq 2 ]] \
  || fail "fixture profiles must use the dedicated loopback host"
[[ "$(grep -Fc 'port = 33316' "$config")" -eq 2 ]] \
  || fail "fixture profiles must use the dedicated MySQL port"
[[ "$(grep -Fc 'secret_env = "DBOTTER_MYSQL_PASSWORD"' "$config")" -eq 2 ]] \
  || fail "fixture profiles must use the exact Environment credential name"

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
[[ "$(docker inspect -f '{{.State.Health.Status}}' "$mysql_container")" == "healthy" ]] \
  || fail "MySQL fixture container is not healthy"
[[ "$(docker inspect -f '{{ index .Config.Labels "com.docker.compose.project" }}' \
  "$mysql_container")" == "dbotter-installed-j2" ]] \
  || fail "MySQL fixture has the wrong Compose project"
[[ "$(docker inspect -f '{{ index .Config.Labels "com.docker.compose.service" }}' \
  "$mysql_container")" == "mysql" ]] \
  || fail "MySQL fixture has the wrong Compose service"
[[ "$(docker inspect -f '{{ index .Config.Labels "ai.2lab.dbotter.fixture" }}' \
  "$mysql_container")" == "installed-j2-v1" ]] \
  || fail "MySQL fixture identity label is invalid"
[[ "$(docker inspect -f '{{.Config.Image}}' "$mysql_container")" == "mysql:8.4" ]] \
  || fail "MySQL fixture image tag is invalid"
docker inspect "$mysql_container" | jq -e '
  .[0].HostConfig.PortBindings["3306/tcp"]
    == [{"HostIp":"127.0.0.1","HostPort":"33316"}]
  and any(.[0].Mounts[]; .Type == "tmpfs" and .Destination == "/var/lib/mysql")
' >/dev/null || fail "MySQL fixture port or tmpfs isolation is invalid"

temporary="$(mktemp -d "${TMPDIR:-/tmp}/dbotter-installed-j2.XXXXXX")"
driver_candidate="$temporary/native-j2-ax-driver"
driver="$driver_candidate"
stable_driver="${DBOTTER_J2_AX_DRIVER_PATH:-}"
if [[ -n "$stable_driver" ]]; then
  [[ "$stable_driver" == /* ]] \
    || fail "DBOTTER_J2_AX_DRIVER_PATH must be absolute"
  driver="$stable_driver"
fi
driver_identity="$temporary/native-j2-ax-driver.identity.json"
seed_evidence="$temporary/seed.json"
restart_evidence="$temporary/restart.json"
history_evidence="$temporary/history-open.json"
explicit_evidence="$temporary/explicit-run.json"
second_evidence="$temporary/second-instance.json"
corrupt_evidence="$temporary/corrupt-reopen.json"
seed_pid=""
restart_pid=""
corrupt_pid=""
output_temporary=""
MAX_PROFILE_SHARD_BYTES=33554432
result_sentinel=""
opt_out_marker="j2_opt_out_private_marker"
clear_marker="j2_clear_private_marker"
private_store_payload_scan_pass_count=0
private_store_payload_scan_clean=false

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
  kill -0 "$pid" >/dev/null 2>&1 \
    && fail "installed app PID remained alive after TERM/KILL"
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
  [[ ! -d "$temporary" || -L "$temporary" ]] || rm -rf -- "$temporary"
  [[ -z "$output_temporary" ]] || rm -f -- "$output_temporary"
  exit "$status"
}
trap cleanup EXIT HUP INT TERM

"$process_guard" --assert-empty "$executable" \
  || fail "stale exact installed-app process exists"

"$builder" --output "$driver_candidate"
[[ -f "$driver_candidate" && ! -L "$driver_candidate" && -x "$driver_candidate" ]] \
  || fail "J2 AX driver build produced no executable"
"$ax_guard" capture \
  --candidate "$driver_candidate" \
  --driver "$driver" \
  --identity "$driver_identity" \
  || fail "J2 AX driver source-bound capture failed"

run_ax_driver() {
  "$ax_guard" run \
    --candidate "$driver_candidate" \
    --driver "$driver" \
    --identity "$driver_identity" \
    -- "$@"
}

workspace_contract="$temporary/workspace-contract.json"
"$executable" workspace-contract --format json >"$workspace_contract"
jq -e '
  .schema == "dbotter.workspace-contract.v1"
  and (.limits | keys | length) == 8
  and .limits.editor_tabs_per_profile == 20
  and .limits.editor_tabs_total == 100
  and .limits.editor_source_bytes == 262144
  and .limits.history_entries_per_profile == 2000
  and .limits.history_entries_total == 10000
  and .limits.history_source_bytes == 65536
  and .limits.profile_shard_bytes == 33554432
  and .limits.workspace_store_bytes == 134217728
  and (.probes | keys | length) == 16
  and .probes.editor_tabs_per_profile_exact == true
  and .probes.editor_tabs_per_profile_plus_one_rejected == true
  and .probes.editor_tabs_total_exact == true
  and .probes.editor_tabs_total_plus_one_rejected == true
  and .probes.editor_source_bytes_exact == true
  and .probes.editor_source_bytes_plus_one_rejected == true
  and .probes.history_entries_per_profile_exact == true
  and .probes.history_entries_per_profile_plus_one_rejected == true
  and .probes.history_entries_total_exact == true
  and .probes.history_entries_total_plus_one_rejected == true
  and .probes.history_source_bytes_exact == true
  and .probes.history_source_bytes_plus_one_rejected == true
  and .probes.profile_shard_bytes_exact == true
  and .probes.profile_shard_bytes_plus_one_rejected == true
  and .probes.workspace_store_bytes_exact == true
  and .probes.workspace_store_bytes_plus_one_rejected == true
' "$workspace_contract" >/dev/null || fail "installed workspace contract is invalid"
workspace_bounds_exact=true

[[ "$(mysql_root_exec \
  "SELECT CONCAT(@@global.general_log, ':', @@global.log_output)")" == "1:TABLE" ]] \
  || fail "dedicated MySQL fixture must start with TABLE general logging enabled"
mysql_server_version="$(mysql_root_exec "SELECT VERSION()")"
[[ "$mysql_server_version" =~ ^8\.4\.[0-9]+ ]] \
  || fail "dedicated MySQL fixture did not report an 8.4 server"
mysql_image_id="$(docker inspect -f '{{.Image}}' "$mysql_container")"
[[ "$mysql_image_id" =~ ^sha256:[0-9a-f]{64}$ ]] \
  || fail "MySQL fixture image ID is invalid"
mysql_image_digest="$(
  docker image inspect "$mysql_image_id" | jq -r '
    [.[0].RepoDigests[]? | select(startswith("mysql@sha256:"))]
    | if length >= 1 then .[0] else error("mysql digest") end
  '
)"
[[ "$mysql_image_digest" =~ ^mysql@sha256:[0-9a-f]{64}$ ]] \
  || fail "MySQL fixture repository digest is invalid"

result_sentinel="DBOTTER_J2_RESULT_$(python3 -c \
  'import secrets; print(secrets.token_hex(16))')"
[[ "$result_sentinel" =~ ^DBOTTER_J2_RESULT_[0-9a-f]{32}$ ]] \
  || fail "private result sentinel generation failed"
export DBOTTER_J2_RESULT_SENTINEL="$result_sentinel"
export DBOTTER_J2_OPT_OUT_MARKER="$opt_out_marker"
export DBOTTER_J2_CLEAR_MARKER="$clear_marker"
mysql_root_exec "
  DROP TABLE IF EXISTS dbotter.dbotter_j2_private_result;
  CREATE TABLE dbotter.dbotter_j2_private_result (
    id BIGINT PRIMARY KEY,
    value VARCHAR(128) NOT NULL
  );
  INSERT INTO dbotter.dbotter_j2_private_result (id, value) VALUES
    (1, '$result_sentinel'),
    (2, 'alpha'),
    (3, 'zulu')
" >/dev/null

mysql_execute_count() {
  local argument="$1"
  case "$argument" in
    "SELECT id, value FROM dbotter_j2_private_result ORDER BY id" \
      |"SELECT 39 AS j2_unselected" \
      |"SELECT 40 AS j2_selected" \
      |"SELECT 41 AS j2_first" \
      |"SELECT 42 AS j2_second" \
      |"SELECT 6 AS j2_opt_out_private_marker" \
      |"SELECT 7 AS j2_clear_private_marker" \
      |"SELECT 84 AS j2_healthy") ;;
    *) fail "refusing an unknown MySQL observation argument" ;;
  esac
  mysql_root_exec \
    "SELECT COUNT(*) FROM mysql.general_log WHERE command_type = 'Execute' AND argument = '$argument'"
}

privacy_query="SELECT id, value FROM dbotter_j2_private_result ORDER BY id"
unselected_query="SELECT 39 AS j2_unselected"
selected_query="SELECT 40 AS j2_selected"
first_query="SELECT 41 AS j2_first"
second_query="SELECT 42 AS j2_second"
opt_out_query="SELECT 6 AS $opt_out_marker"
clear_query="SELECT 7 AS $clear_marker"
healthy_query="SELECT 84 AS j2_healthy"
privacy_before="$(mysql_execute_count "$privacy_query")"
unselected_before="$(mysql_execute_count "$unselected_query")"
selected_before="$(mysql_execute_count "$selected_query")"
first_before="$(mysql_execute_count "$first_query")"
second_before="$(mysql_execute_count "$second_query")"
opt_out_before="$(mysql_execute_count "$opt_out_query")"
clear_before="$(mysql_execute_count "$clear_query")"
healthy_before="$(mysql_execute_count "$healthy_query")"
for baseline in \
  "$privacy_before" "$unselected_before" "$selected_before" \
  "$first_before" "$second_before" "$opt_out_before" "$clear_before" \
  "$healthy_before"; do
  [[ "$baseline" =~ ^[0-9]+$ ]] || fail "invalid MySQL observation baseline"
done

run_ax_driver \
  --phase seed \
  --app-path "$app_path" \
  --config "$config" \
  --output "$seed_evidence"

jq -e '
  .schema == "dbotter.installed-j2-ax-observations.v1"
  and .phase == "seed"
  and (.pid | type == "number" and . > 0 and floor == .)
  and .checkpoints.current_selection_all_exercised == true
  and .checkpoints.syntax_autocomplete_exercised == true
  and .checkpoints.result_inspection_completed == true
  and .checkpoints.history_filters_and_metrics_visible == true
  and .checkpoints.history_source_exact_retained == true
  and .checkpoints.history_source_plus_one_omitted == true
  and .checkpoints.tab_bound_enforced == true
  and .checkpoints.persistence_opt_out_and_clear == true
  and .checkpoints.persistence_off_edit_save_disabled_execute == true
  and .checkpoints.failed_query_error_retained == true
  and .checkpoints.failed_query_error_retained_after_later_results == true
  and .checkpoints.tabs_created_renamed_reordered == true
  and .checkpoints.saved_visible_before_kill == true
  and (.split_value | type == "number")
' "$seed_evidence" >/dev/null || fail "seed AX evidence is invalid"
seed_pid="$(jq -r '.pid' "$seed_evidence")"
kill -0 "$seed_pid" >/dev/null 2>&1 || fail "seed Preview PID exited"

privacy_after_seed="$(mysql_execute_count "$privacy_query")"
unselected_after_seed="$(mysql_execute_count "$unselected_query")"
selected_after_seed="$(mysql_execute_count "$selected_query")"
first_after_seed="$(mysql_execute_count "$first_query")"
second_after_seed="$(mysql_execute_count "$second_query")"
opt_out_after_seed="$(mysql_execute_count "$opt_out_query")"
clear_after_seed="$(mysql_execute_count "$clear_query")"
(( privacy_after_seed == privacy_before + 1 )) \
  || fail "current execution did not dispatch the exact private-result query once"
(( unselected_after_seed == unselected_before )) \
  || fail "selection execution dispatched the unselected statement"
(( selected_after_seed == selected_before + 1 )) \
  || fail "selection execution did not dispatch the selected statement exactly once"
(( first_after_seed == first_before + 1 )) \
  || fail "Run all did not dispatch the first statement exactly once"
(( second_after_seed == second_before + 1 )) \
  || fail "Run all did not dispatch the second statement exactly once"
(( opt_out_after_seed == opt_out_before + 1 )) \
  || fail "persistence Off did not execute its exact marker query once"
(( clear_after_seed == clear_before + 1 )) \
  || fail "durable clear setup did not execute its exact marker query once"

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
(( shard_bytes <= MAX_PROFILE_SHARD_BYTES )) \
  || fail "committed profile shard exceeds the 32 MiB bound"
shard_bound_enforced=true

"$scanner" \
  --root "$workspace_root" \
  --forbidden-env DBOTTER_MYSQL_PASSWORD \
  --forbidden-env DBOTTER_MYSQL_ROOT_PASSWORD \
  --forbidden-env DBOTTER_J2_RESULT_SENTINEL \
  --forbidden-env DBOTTER_J2_OPT_OUT_MARKER \
  --forbidden-env DBOTTER_J2_CLEAR_MARKER >/dev/null
private_store_payload_scan_pass_count=$((private_store_payload_scan_pass_count + 1))

kill -KILL "$seed_pid"
for _ in {1..100}; do
  kill -0 "$seed_pid" >/dev/null 2>&1 || break
  sleep 0.1
done
kill -0 "$seed_pid" >/dev/null 2>&1 && fail "force-killed seed PID remained alive"
seed_pid=""

run_ax_driver \
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

zero_dispatch_before="$(mysql_execute_count "$second_query")"
[[ "$zero_dispatch_before" =~ ^[0-9]+$ ]] || fail "invalid pre-open MySQL counter"

run_ax_driver \
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
zero_dispatch_after_open="$(mysql_execute_count "$second_query")"
[[ "$zero_dispatch_after_open" == "$zero_dispatch_before" ]] \
  || fail "opening history dispatched a database query"

run_ax_driver \
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
zero_dispatch_after_run="$(mysql_execute_count "$second_query")"
[[ "$zero_dispatch_after_run" =~ ^[0-9]+$ ]] \
  || fail "invalid post-run MySQL counter"
(( zero_dispatch_after_run == zero_dispatch_before + 1 )) \
  || fail "explicit history rerun did not dispatch exactly one expected query"
fresh_result_after_explicit_run=true

run_ax_driver \
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
  "$process_guard" --assert-empty "$executable" >/dev/null 2>&1 && break
  sleep 0.1
done
[[ "$(stat -f '%d:%i' "$executable")" == "$expected_executable_device_inode" ]] \
  || fail "installed executable identity changed before the corruption fixture"
"$process_guard" --assert-empty "$executable" \
  || fail "writer process set did not close before corruption fixture"

primary_shard_name="$(jq -r '.shard' "$primary_manifest")"
primary_shard="$primary_profile_directory/$primary_shard_name"
[[ -f "$primary_shard" && ! -L "$primary_shard" ]] \
  || fail "latest primary shard is unavailable before corruption"
printf '\n' >>"$primary_shard"

run_ax_driver \
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

healthy_after="$(mysql_execute_count "$healthy_query")"
[[ "$healthy_after" =~ ^[0-9]+$ ]] \
  || fail "healthy profile produced an invalid server observation"
(( healthy_after == healthy_before + 1 )) \
  || fail "healthy profile did not dispatch its exact query once"
healthy_query_count=1
healthy_profile_remains_usable=true

stop_pid "$corrupt_pid"
corrupt_pid=""
[[ "$(stat -f '%d:%i' "$executable")" == "$expected_executable_device_inode" ]] \
  || fail "installed executable identity changed before the final scan"
"$process_guard" --assert-empty "$executable" \
  || fail "installed executable process set was not empty before the final scan"

"$scanner" \
  --root "$workspace_root" \
  --forbidden-env DBOTTER_MYSQL_PASSWORD \
  --forbidden-env DBOTTER_MYSQL_ROOT_PASSWORD \
  --forbidden-env DBOTTER_J2_RESULT_SENTINEL \
  --forbidden-env DBOTTER_J2_OPT_OUT_MARKER \
  --forbidden-env DBOTTER_J2_CLEAR_MARKER >/dev/null
private_store_payload_scan_pass_count=$((private_store_payload_scan_pass_count + 1))
(( private_store_payload_scan_pass_count == 2 )) \
  || fail "private workspace payload scan pass count is not exact"
private_store_payload_scan_clean=true

tabs_created_renamed_reordered="$(
  jq -r '.checkpoints.tabs_created_renamed_reordered' "$seed_evidence"
)"
current_selection_all_exercised="$(
  jq -r '.checkpoints.current_selection_all_exercised' "$seed_evidence"
)"
syntax_autocomplete_exercised="$(
  jq -r '.checkpoints.syntax_autocomplete_exercised' "$seed_evidence"
)"
result_inspection_completed="$(
  jq -r '.checkpoints.result_inspection_completed' "$seed_evidence"
)"
history_filters_and_metrics_visible="$(
  jq -r '.checkpoints.history_filters_and_metrics_visible' "$seed_evidence"
)"
history_source_exact_retained="$(
  jq -r '.checkpoints.history_source_exact_retained' "$seed_evidence"
)"
history_source_plus_one_omitted="$(
  jq -r '.checkpoints.history_source_plus_one_omitted' "$seed_evidence"
)"
tab_bound_enforced="$(
  jq -r '.checkpoints.tab_bound_enforced' "$seed_evidence"
)"
persistence_opt_out_and_clear="$(
  jq -r '.checkpoints.persistence_opt_out_and_clear' "$seed_evidence"
)"
persistence_off_edit_save_disabled_execute="$(
  jq -r '.checkpoints.persistence_off_edit_save_disabled_execute' "$seed_evidence"
)"
failed_query_error_retained="$(
  jq -r '.checkpoints.failed_query_error_retained' "$seed_evidence"
)"
failed_query_error_retained_after_later_results="$(
  jq -r '.checkpoints.failed_query_error_retained_after_later_results' "$seed_evidence"
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

[[ "$(mysql_root_exec \
  "SELECT CONCAT(@@global.general_log, ':', @@global.log_output)")" == "1:TABLE" ]] \
  || fail "installed verification changed the dedicated fixture logging state"
current_query_delta=$((privacy_after_seed - privacy_before))
unselected_query_delta=$((unselected_after_seed - unselected_before))
selected_query_delta=$((selected_after_seed - selected_before))
run_all_first_delta=$((first_after_seed - first_before))
run_all_second_delta=$((second_after_seed - second_before))
persistence_opt_out_query_delta=$((opt_out_after_seed - opt_out_before))
persistence_clear_query_delta=$((clear_after_seed - clear_before))

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
  --arg mysql_server_version "$mysql_server_version" \
  --arg mysql_image_digest "$mysql_image_digest" \
  --argjson workspace_manifest_generation "$workspace_manifest_generation" \
  --argjson manifest_bytes "$manifest_bytes" \
  --argjson shard_bytes "$shard_bytes" \
  --argjson max_profile_shard_bytes "$MAX_PROFILE_SHARD_BYTES" \
  --argjson quarantine_files "$quarantine_files" \
  --argjson current_query_delta "$current_query_delta" \
  --argjson unselected_query_delta "$unselected_query_delta" \
  --argjson selected_query_delta "$selected_query_delta" \
  --argjson run_all_first_delta "$run_all_first_delta" \
  --argjson run_all_second_delta "$run_all_second_delta" \
  --argjson persistence_opt_out_query_delta "$persistence_opt_out_query_delta" \
  --argjson persistence_clear_query_delta "$persistence_clear_query_delta" \
  --argjson zero_dispatch_before "$zero_dispatch_before" \
  --argjson zero_dispatch_after_open "$zero_dispatch_after_open" \
  --argjson zero_dispatch_after_run "$zero_dispatch_after_run" \
  --argjson healthy_query_count "$healthy_query_count" \
  --argjson tabs_created_renamed_reordered "$tabs_created_renamed_reordered" \
  --argjson current_selection_all_exercised "$current_selection_all_exercised" \
  --argjson syntax_autocomplete_exercised "$syntax_autocomplete_exercised" \
  --argjson result_inspection_completed "$result_inspection_completed" \
  --argjson history_filters_and_metrics_visible "$history_filters_and_metrics_visible" \
  --argjson history_source_exact_retained "$history_source_exact_retained" \
  --argjson history_source_plus_one_omitted "$history_source_plus_one_omitted" \
  --argjson tab_bound_enforced "$tab_bound_enforced" \
  --argjson shard_bound_enforced "$shard_bound_enforced" \
  --argjson workspace_bounds_exact "$workspace_bounds_exact" \
  --argjson persistence_opt_out_and_clear "$persistence_opt_out_and_clear" \
  --argjson persistence_off_edit_save_disabled_execute \
    "$persistence_off_edit_save_disabled_execute" \
  --argjson failed_query_error_retained "$failed_query_error_retained" \
  --argjson failed_query_error_retained_after_later_results \
    "$failed_query_error_retained_after_later_results" \
  --argjson private_store_payload_scan_clean "$private_store_payload_scan_clean" \
  --argjson private_store_payload_scan_pass_count \
    "$private_store_payload_scan_pass_count" \
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
      executable_sha256: $executable_sha256,
      mysql_server_version: $mysql_server_version,
      mysql_image_digest: $mysql_image_digest
    },
    checkpoints: {
      current_selection_all_exercised: $current_selection_all_exercised,
      syntax_autocomplete_exercised: $syntax_autocomplete_exercised,
      result_inspection_completed: $result_inspection_completed,
      history_filters_and_metrics_visible: $history_filters_and_metrics_visible,
      history_source_exact_retained: $history_source_exact_retained,
      history_source_plus_one_omitted: $history_source_plus_one_omitted,
      tab_bound_enforced: $tab_bound_enforced,
      shard_bound_enforced: $shard_bound_enforced,
      workspace_bounds_exact: $workspace_bounds_exact,
      persistence_opt_out_and_clear: $persistence_opt_out_and_clear,
      persistence_off_edit_save_disabled_execute:
        $persistence_off_edit_save_disabled_execute,
      failed_query_error_retained: $failed_query_error_retained,
      failed_query_error_retained_after_later_results:
        $failed_query_error_retained_after_later_results,
      private_store_payload_scan_clean: $private_store_payload_scan_clean,
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
      max_profile_shard_bytes: $max_profile_shard_bytes,
      root_mode: 448,
      file_mode: 384,
      quarantine_files: $quarantine_files,
      payload_scan_clean: $private_store_payload_scan_clean,
      payload_scan_pass_count: $private_store_payload_scan_pass_count
    },
    mysql_observation: {
      current_query_delta: $current_query_delta,
      unselected_query_delta: $unselected_query_delta,
      selected_query_delta: $selected_query_delta,
      run_all_first_delta: $run_all_first_delta,
      run_all_second_delta: $run_all_second_delta,
      persistence_opt_out_query_delta: $persistence_opt_out_query_delta,
      persistence_clear_query_delta: $persistence_clear_query_delta,
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
  and (.installed_identity.mysql_server_version | test("^8\\.4\\.[0-9]+"))
  and (.installed_identity.mysql_image_digest
      | test("^mysql@sha256:[0-9a-f]{64}$"))
  and all(.checkpoints[]; . == true)
  and .private_store.workspace_manifest_generation > 0
  and .private_store.root_mode == 448
  and .private_store.file_mode == 384
  and .private_store.quarantine_files > 0
  and .private_store.shard_bytes <= .private_store.max_profile_shard_bytes
  and .private_store.payload_scan_clean == true
  and .private_store.payload_scan_pass_count == 2
  and .mysql_observation.current_query_delta == 1
  and .mysql_observation.unselected_query_delta == 0
  and .mysql_observation.selected_query_delta == 1
  and .mysql_observation.run_all_first_delta == 1
  and .mysql_observation.run_all_second_delta == 1
  and .mysql_observation.persistence_opt_out_query_delta == 1
  and .mysql_observation.persistence_clear_query_delta == 1
  and .mysql_observation.zero_dispatch_before
      == .mysql_observation.zero_dispatch_after_open
  and .mysql_observation.after_explicit_run
      == (.mysql_observation.zero_dispatch_before + 1)
  and .mysql_observation.healthy_profile_query_count == 1
' "$output_temporary" >/dev/null || fail "assembled J2 evidence is invalid"

if receipt_candidate_has_static_leak "$output_temporary" \
  || receipt_candidate_contains_secret "$output_temporary" "$DBOTTER_MYSQL_PASSWORD" \
  || receipt_candidate_contains_secret "$output_temporary" "$DBOTTER_MYSQL_ROOT_PASSWORD" \
  || receipt_candidate_contains_secret "$output_temporary" "$result_sentinel" \
  || receipt_candidate_contains_secret "$output_temporary" "$opt_out_marker" \
  || receipt_candidate_contains_secret "$output_temporary" "$clear_marker"; then
  fail "assembled J2 evidence contains a credential or value-bearing payload"
fi

ln "$output_temporary" "$output" \
  || fail "could not publish --output without replacement"
rm -f -- "$output_temporary"
output_temporary=""
[[ -f "$output" && ! -L "$output" && "$(stat -f '%Lp' "$output")" == "600" ]] \
  || fail "published J2 evidence is not a private regular file"

echo "installed J2 verification: ok: $output"
