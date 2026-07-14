#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_dir"
. "$repo_dir/scripts/receipt-security.sh"

: "${DBOTTER_MYSQL_PASSWORD:?set DBOTTER_MYSQL_PASSWORD for the local fixture}"
export DBOTTER_CONFIG=${DBOTTER_CONFIG:-"$repo_dir/config/local.example.toml"}
project_name=dbotter-e2e
compose_file=$repo_dir/docker-compose.yml
redis_expiry_seconds=300
started_at_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)
started_epoch=$(date -u +%s)
run_id="dbotter-$(date -u +%Y%m%dT%H%M%SZ)-$$"

for dependency in jq docker git rustc cargo uname hostname awk sed grep; do
  if ! command -v "$dependency" >/dev/null 2>&1; then
    echo "error: $dependency is required to build the acceptance receipt" >&2
    exit 1
  fi
done

case "$(jq --version)" in
  jq-1.7*) ;;
  *)
    echo "error: jq 1.7.x is required" >&2
    exit 1
    ;;
esac

if [ ! -r "$DBOTTER_CONFIG" ]; then
  echo "error: DBOTTER_CONFIG is not readable: $DBOTTER_CONFIG" >&2
  exit 1
fi

compose() {
  docker compose -p "$project_name" -f "$compose_file" "$@"
}

sanitize_profiles() {
  awk '
    function unquote(value) {
      sub(/^[[:space:]]*"/, "", value)
      sub(/"[[:space:]]*$/, "", value)
      return value
    }
    function emit() {
      if (in_profile) {
        printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n", \
          id, name, driver, host, port, database, username, tls, secret_env
      }
    }
    /^\[\[profiles\]\][[:space:]]*$/ {
      emit()
      in_profile = 1
      id = name = driver = host = port = database = username = tls = secret_env = ""
      next
    }
    in_profile && /^[[:space:]]*[a-z_]+[[:space:]]*=/ {
      key = $0
      sub(/[[:space:]]*=.*/, "", key)
      gsub(/^[[:space:]]+|[[:space:]]+$/, "", key)
      value = $0
      sub(/^[^=]*=[[:space:]]*/, "", value)
      value = unquote(value)
      if (key == "id") id = value
      else if (key == "name") name = value
      else if (key == "driver") driver = value
      else if (key == "host") host = value
      else if (key == "port") port = value
      else if (key == "database") database = value
      else if (key == "username") username = value
      else if (key == "tls") tls = value
      else if (key == "secret_env") secret_env = value
    }
    END { emit() }
  ' "$DBOTTER_CONFIG" | jq -R -s '
    split("\n")
    | map(select(length > 0) | split("\t"))
    | map({
        id: .[0],
        name: .[1],
        driver: .[2],
        endpoint: (.[2] + "://" + .[3] + ":" + .[4]),
        database: (if .[5] == "" then null else .[5] end),
        username: (if .[6] == "" then null else .[6] end),
        tls: .[7],
        secret_env: (if .[8] == "" then null else .[8] end)
      })'
}

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/dbotter-receipt.XXXXXX")
receipt_temp=
cleanup() {
  for name in \
    mysql_check mysql_create mysql_upsert mysql_select \
    redis_check redis_set redis_get redis_ttl \
    mysql_official redis_official_get redis_official_ttl; do
    rm -f "$tmp_dir/$name.stdout" "$tmp_dir/$name.stderr" "$tmp_dir/$name.json"
  done
  rm -f "$tmp_dir/receipt-candidate.json"
  if [ -n "$receipt_temp" ]; then
    rm -f "$receipt_temp"
  fi
  rmdir "$tmp_dir" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM

# Capture every verification command without letting one failure erase the
# remaining diagnostics. Raw SQL/command text and argv are execution-only; the
# receipt stores one named input descriptor with one SHA-256 fingerprint.
capture() {
  name=$1
  actor=$2
  profile=$3
  input_kind=$4
  fixture_statement=$5
  input_value=$6
  shift 6
  stdout_file=$tmp_dir/$name.stdout
  stderr_file=$tmp_dir/$name.stderr
  input_sha256=$(receipt_sha256_text "$input_value")
  command_started=$(date -u +%s)
  set +e
  "$@" >"$stdout_file" 2>"$stderr_file"
  exit_code=$?
  set -e
  command_finished=$(date -u +%s)
  elapsed_ms=$(( (command_finished - command_started) * 1000 ))
  if jq -e . "$stdout_file" >/dev/null 2>&1; then
    result_json=$(jq -c . "$stdout_file")
  else
    result_json=null
  fi
  stdout_sha256=sha256:$(receipt_sha256_file "$stdout_file")
  stderr_sha256=sha256:$(receipt_sha256_file "$stderr_file")
  if [ -s "$stdout_file" ]; then stdout_present=true; else stdout_present=false; fi
  if [ -s "$stderr_file" ]; then stderr_present=true; else stderr_present=false; fi
  jq -n \
    --arg name "$name" \
    --arg actor "$actor" \
    --arg profile "$profile" \
    --arg input_kind "$input_kind" \
    --arg fixture_statement "$fixture_statement" \
    --arg input_sha256 "$input_sha256" \
    --argjson exit_code "$exit_code" \
    --argjson elapsed_ms "$elapsed_ms" \
    --argjson result "$result_json" \
    --arg stdout_sha256 "$stdout_sha256" \
    --arg stderr_sha256 "$stderr_sha256" \
    --argjson stdout_present "$stdout_present" \
    --argjson stderr_present "$stderr_present" \
    '{
      name: $name,
      actor: $actor,
      profile_id: (if $profile == "" then null else $profile end),
      input: {
        kind: $input_kind,
        fixture_statement: $fixture_statement,
        sha256: $input_sha256
      },
      exit_code: $exit_code,
      elapsed_ms: $elapsed_ms,
      result: $result,
      streams: {
        stdout: {present: $stdout_present, sha256: $stdout_sha256},
        stderr: {present: $stderr_present, sha256: $stderr_sha256}
      }
    }' >"$tmp_dir/$name.json"
}

service_metadata() {
  service=$1
  container_id=$(compose ps -q "$service" 2>/dev/null || true)
  if [ -z "$container_id" ]; then
    jq -n --arg service "$service" '{service: $service, present: false}'
    return
  fi
  base=$(docker inspect "$container_id" | jq -c '.[0] | {
    container_id: .Id,
    configured_image: .Config.Image,
    image_id: .Image,
    status: .State.Status,
    health: (.State.Health.Status // null)
  }')
  image_id=$(printf '%s' "$base" | jq -r .image_id)
  repo_digests=$(docker image inspect "$image_id" --format '{{json .RepoDigests}}' 2>/dev/null || printf '[]')
  jq -n \
    --arg service "$service" \
    --argjson base "$base" \
    --argjson repo_digests "$repo_digests" \
    '$base + {service: $service, present: true, repo_digests: $repo_digests}'
}

binary=$repo_dir/target/debug/dbotter
cargo build --locked --manifest-path "$repo_dir/Cargo.toml"

mysql_marker="mysql:$run_id"
mysql_create_sql='CREATE TABLE IF NOT EXISTS dbotter_receipt (run_id VARCHAR(96) PRIMARY KEY, marker VARCHAR(128) NOT NULL, updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP)'
mysql_upsert_sql="INSERT INTO dbotter_receipt (run_id, marker) VALUES ('$run_id', '$mysql_marker') ON DUPLICATE KEY UPDATE marker = VALUES(marker), updated_at = CURRENT_TIMESTAMP"
mysql_select_sql="SELECT run_id, marker FROM dbotter_receipt WHERE run_id = '$run_id'"

capture mysql_check dbotter mysql-local profile-check mysql.check \
  'check:mysql-local' \
  "$binary" check --profile mysql-local --format json
capture mysql_create dbotter mysql-local sql mysql.create_receipt_table \
  "$mysql_create_sql" \
  "$binary" exec --profile mysql-local --text "$mysql_create_sql" --format json
capture mysql_upsert dbotter mysql-local sql mysql.upsert_receipt_marker \
  "$mysql_upsert_sql" \
  "$binary" exec --profile mysql-local --text "$mysql_upsert_sql" --format json
capture mysql_select dbotter mysql-local sql mysql.select_receipt_marker \
  "$mysql_select_sql" \
  "$binary" exec --profile mysql-local --text "$mysql_select_sql" --format json

redis_key="dbotter:receipt:$run_id"
redis_marker="redis:$run_id"
redis_set_command="SET $redis_key $redis_marker EX $redis_expiry_seconds"
redis_get_command="GET $redis_key"
redis_ttl_command="TTL $redis_key"

capture redis_check dbotter redis-local profile-check redis.check \
  'check:redis-local' \
  "$binary" check --profile redis-local --format json
capture redis_set dbotter redis-local redis-command redis.set_receipt_marker \
  "$redis_set_command" \
  "$binary" exec --profile redis-local --text "$redis_set_command" --format json
capture redis_get dbotter redis-local redis-command redis.get_receipt_marker \
  "$redis_get_command" \
  "$binary" exec --profile redis-local --text "$redis_get_command" --format json
capture redis_ttl dbotter redis-local redis-command redis.ttl_receipt_marker \
  "$redis_ttl_command" \
  "$binary" exec --profile redis-local --text "$redis_ttl_command" --format json

# These are independent reads through the official clients bundled in the
# images. MYSQL_PWD is injected into the container environment, never argv in
# the receipt, and the same run_id generated above is used on both paths.
capture mysql_official official-client mysql-local sql \
  mysql.select_receipt_marker "$mysql_select_sql" \
  compose exec -T -e "MYSQL_PWD=$DBOTTER_MYSQL_PASSWORD" mysql \
    mysql --protocol=TCP --host=127.0.0.1 --user=dbotter --database=dbotter \
    --batch --skip-column-names --execute "$mysql_select_sql"

capture redis_official_get official-client redis-local redis-command \
  redis.get_receipt_marker "$redis_get_command" \
  compose exec -T redis redis-cli --raw GET "$redis_key"
capture redis_official_ttl official-client redis-local redis-command \
  redis.ttl_receipt_marker "$redis_ttl_command" \
  compose exec -T redis redis-cli --raw TTL "$redis_key"

mysql_steps=$(jq -s . \
  "$tmp_dir/mysql_check.json" "$tmp_dir/mysql_create.json" \
  "$tmp_dir/mysql_upsert.json" "$tmp_dir/mysql_select.json")
redis_steps=$(jq -s . \
  "$tmp_dir/redis_check.json" "$tmp_dir/redis_set.json" \
  "$tmp_dir/redis_get.json" "$tmp_dir/redis_ttl.json")
mysql_official=$(jq --rawfile output "$tmp_dir/mysql_official.stdout" '
  . + {normalized_output: (
    $output | sub("[\\r\\n]+$"; "") | split("\t")
    | if length == 2 then {run_id: .[0], marker: .[1]} else null end
  )}' "$tmp_dir/mysql_official.json")
redis_official_get=$(jq --rawfile output "$tmp_dir/redis_official_get.stdout" '
  . + {normalized_output: ($output | sub("[\\r\\n]+$"; ""))}' \
  "$tmp_dir/redis_official_get.json")
redis_official_ttl=$(jq --rawfile output "$tmp_dir/redis_official_ttl.stdout" '
  . + {normalized_output: (($output | sub("[\\r\\n]+$"; "") | tonumber?) // null)}' \
  "$tmp_dir/redis_official_ttl.json")

mysql_verdict=$(jq -n \
  --arg run_id "$run_id" \
  --arg marker "$mysql_marker" \
  --argjson steps "$mysql_steps" \
  --argjson official "$mysql_official" '
  ($steps | all(.exit_code == 0))
  and ($steps | all(.result.status == "ok"))
  and ($steps[3].result.status == "ok")
  and ($steps[3].result.result.rows[0][0] == {type: "text", value: $run_id})
  and ($steps[3].result.result.rows[0][1] == {type: "text", value: $marker})
  and ($official.exit_code == 0)
  and ($official.normalized_output == {run_id: $run_id, marker: $marker})')

redis_verdict=$(jq -n \
  --arg marker "$redis_marker" \
  --argjson expiry "$redis_expiry_seconds" \
  --argjson steps "$redis_steps" \
  --argjson official_get "$redis_official_get" \
  --argjson official_ttl "$redis_official_ttl" '
  ($steps | all(.exit_code == 0))
  and ($steps | all(.result.status == "ok"))
  and ($steps[1].result.result.rows[0][0] == {type: "text", value: "OK"})
  and ($steps[2].result.result.rows[0][0] == {type: "text", value: $marker})
  and ($steps[3].result.result.rows[0][0].type == "int")
  and ($steps[3].result.result.rows[0][0].value > 0)
  and ($steps[3].result.result.rows[0][0].value <= $expiry)
  and ($official_get.exit_code == 0)
  and ($official_get.normalized_output == $marker)
  and ($official_ttl.exit_code == 0)
  and (($official_ttl.normalized_output // -999) > 0)
  and (($official_ttl.normalized_output // -999) <= $expiry)')

docker_context=$(docker context show)
docker_version=$(docker version --format '{{json .}}' | jq -c .)
compose_version=$(docker compose version --short)
compose_ps_raw=$(compose ps --format json)
compose_ps=$(printf '%s\n' "$compose_ps_raw" | jq -s '
  if length == 1 and (.[0] | type == "array") then .[0] else . end')
mysql_service=$(service_metadata mysql)
redis_service=$(service_metadata redis)
sanitized_profiles=$(sanitize_profiles)
compose_sha256=$(receipt_sha256_file "$compose_file")
binary_sha256=$(receipt_sha256_file "$binary")

repository_root=$(CDPATH= cd -- "$repo_dir" && pwd -P)
discovered_git_root=$(git -C "$repo_dir" rev-parse --show-toplevel 2>/dev/null || true)
repository_root_matches=false
if [ -n "$discovered_git_root" ]; then
  discovered_git_root=$(CDPATH= cd -- "$discovered_git_root" && pwd -P)
  if [ "$discovered_git_root" = "$repository_root" ]; then
    repository_root_matches=true
  fi
else
  discovered_git_root=unavailable
fi

git_head=$(git -C "$repo_dir" rev-parse --verify HEAD 2>/dev/null || printf unavailable)
if git -C "$repo_dir" cat-file -e "$git_head^{commit}" 2>/dev/null; then
  head_is_commit=true
else
  head_is_commit=false
fi
git_branch=$(git -C "$repo_dir" symbolic-ref --quiet --short HEAD 2>/dev/null || true)
if [ -n "$git_branch" ]; then
  git_detached=false
else
  git_detached=true
fi
if [ -n "$(git -C "$repo_dir" status --porcelain=v1 --untracked-files=all 2>/dev/null || true)" ]; then
  git_dirty=true
else
  git_dirty=false
fi

required_files_tracked=true
for required_file in \
  Cargo.toml Cargo.lock config/local.example.toml docker-compose.yml \
  src/main.rs src/lib.rs scripts/verify-local.sh \
  scripts/receipt-security.sh scripts/receipt-contract.jq \
  scripts/test-receipt-contract.sh; do
  if ! git -C "$repo_dir" ls-files --error-unmatch -- "$required_file" >/dev/null 2>&1; then
    required_files_tracked=false
  fi
done

# A clean porcelain status does not expose ignored source files. Reject any
# ignored file in paths that can affect the Rust build or receipt behavior;
# generated target/artifact paths are intentionally outside this query.
ignored_source_inputs=$(git -C "$repo_dir" status --porcelain=v1 \
  --ignored=matching --untracked-files=all -- \
  Cargo.toml Cargo.lock build.rs rust-toolchain rust-toolchain.toml \
  .cargo src scripts config docker-compose.yml 2>/dev/null \
  | awk '$1 == "!!" { print }' || true)
if [ -n "$ignored_source_inputs" ]; then
  required_files_tracked=false
fi

if [ "$repository_root_matches" = true ] \
  && [ "$head_is_commit" = true ] \
  && [ "$git_detached" = false ] \
  && [ "$git_dirty" = false ] \
  && [ "$required_files_tracked" = true ]; then
  source_clean_committed=true
else
  source_clean_committed=false
fi

source_failures=$(jq -cn \
  --argjson root_matches "$repository_root_matches" \
  --argjson head_is_commit "$head_is_commit" \
  --argjson detached "$git_detached" \
  --argjson dirty "$git_dirty" \
  --argjson tracked "$required_files_tracked" '
  [
    if $root_matches then empty else "repository-root-mismatch" end,
    if $head_is_commit then empty else "head-is-not-a-commit" end,
    if ($detached | not) then empty else "detached-head" end,
    if ($dirty | not) then empty else "dirty-worktree" end,
    if $tracked then empty else "required-files-untracked" end
  ]')

finished_at_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)
finished_epoch=$(date -u +%s)
total_elapsed_ms=$(( (finished_epoch - started_epoch) * 1000 ))

mkdir -p artifacts
receipt_candidate=$tmp_dir/receipt-candidate.json
jq -n \
  --arg started_at_utc "$started_at_utc" \
  --arg finished_at_utc "$finished_at_utc" \
  --arg run_id "$run_id" \
  --arg host "$(hostname)" \
  --arg os "$(uname -s)" \
  --arg arch "$(uname -m)" \
  --arg repository_root "$repository_root" \
  --arg discovered_git_root "$discovered_git_root" \
  --arg git_head "$git_head" \
  --arg git_branch "$git_branch" \
  --argjson repository_root_matches "$repository_root_matches" \
  --argjson head_is_commit "$head_is_commit" \
  --argjson git_detached "$git_detached" \
  --argjson git_dirty "$git_dirty" \
  --argjson required_files_tracked "$required_files_tracked" \
  --argjson source_clean_committed "$source_clean_committed" \
  --argjson source_failures "$source_failures" \
  --arg rustc_version "$(rustc --version)" \
  --arg cargo_version "$(cargo --version)" \
  --arg binary_path "$binary" \
  --arg binary_version "$($binary --version)" \
  --arg binary_sha256 "$binary_sha256" \
  --arg docker_context "$docker_context" \
  --argjson docker_version "$docker_version" \
  --arg compose_version "$compose_version" \
  --arg project_name "$project_name" \
  --arg compose_file "$compose_file" \
  --arg compose_sha256 "$compose_sha256" \
  --argjson compose_ps "$compose_ps" \
  --argjson mysql_service "$mysql_service" \
  --argjson redis_service "$redis_service" \
  --argjson profiles "$sanitized_profiles" \
  --argjson mysql_steps "$mysql_steps" \
  --argjson redis_steps "$redis_steps" \
  --argjson mysql_official "$mysql_official" \
  --argjson redis_official_get "$redis_official_get" \
  --argjson redis_official_ttl "$redis_official_ttl" \
  --argjson mysql_verdict "$mysql_verdict" \
  --argjson redis_verdict "$redis_verdict" \
  --argjson total_elapsed_ms "$total_elapsed_ms" \
  '{
    schema_version: 2,
    started_at_utc: $started_at_utc,
    finished_at_utc: $finished_at_utc,
    elapsed_ms: $total_elapsed_ms,
    run_id: $run_id,
    host: {hostname: $host, os: $os, arch: $arch},
    source: {
      repository_root: $repository_root,
      discovered_git_root: $discovered_git_root,
      repository_root_matches: $repository_root_matches,
      git_head: $git_head,
      head_is_commit: $head_is_commit,
      branch: (if $git_branch == "" then null else $git_branch end),
      detached: $git_detached,
      dirty: $git_dirty,
      required_files_tracked: $required_files_tracked,
      clean_committed: $source_clean_committed,
      failures: $source_failures
    },
    toolchain: {
      rustc: $rustc_version,
      cargo: $cargo_version,
      binary: {
        path: $binary_path,
        version: $binary_version,
        sha256: $binary_sha256
      }
    },
    docker: {
      context: $docker_context,
      version: $docker_version,
      client: ($docker_version.Client // null),
      server: ($docker_version.Server // null),
      compose_version: $compose_version,
      project: $project_name,
      compose_file: $compose_file,
      compose_sha256: $compose_sha256,
      ps: $compose_ps,
      services: {mysql: $mysql_service, redis: $redis_service}
    },
    profiles: $profiles,
    mysql: {
      app_steps: $mysql_steps,
      official_readback: $mysql_official,
      verdict: (if $mysql_verdict then "pass" else "fail" end)
    },
    redis: {
      app_steps: $redis_steps,
      official_readback: {
        get: $redis_official_get,
        ttl: $redis_official_ttl
      },
      verdict: (if $redis_verdict then "pass" else "fail" end)
    },
    mongodb: {
      status: "prepared-not-run",
      compose_profile: "mongodb",
      configured_image: "mongo:8.0",
      profile_id: "mongodb-local",
      reason: "MVP driver is planned; live MongoDB acceptance is out of scope"
    }
  }' >"$receipt_candidate"

# Scan the complete serialized candidate before adding a leak assertion. This
# prevents the assertion from becoming a self-referential scan input.
credential_leak=false
if receipt_candidate_has_static_leak "$receipt_candidate" \
  || receipt_candidate_contains_secret "$receipt_candidate" "$DBOTTER_MYSQL_PASSWORD"; then
  credential_leak=true
fi

# Resolve configured secret_env values only after validating their names. Each
# value stays in shell memory and is compared directly with the candidate; it
# is never written to a pattern file or receipt field.
configured_secret_envs=$(printf '%s' "$sanitized_profiles" | jq -r '.[] | .secret_env // empty')
for secret_env_name in $configured_secret_envs; do
  case "$secret_env_name" in
    ''|[!A-Za-z_]*|*[!A-Za-z0-9_]*) continue ;;
  esac
  eval 'resolved_secret=${'"$secret_env_name"'-}'
  if receipt_candidate_contains_secret "$receipt_candidate" "$resolved_secret"; then
    credential_leak=true
  fi
  resolved_secret=
done

receipt_temp=artifacts/.receipt.json.$$.tmp
jq \
  --argjson mysql_verdict "$mysql_verdict" \
  --argjson redis_verdict "$redis_verdict" \
  --argjson source_clean_committed "$source_clean_committed" \
  --argjson credential_leak "$credential_leak" '
  . + {
    assertions: {
      mysql: $mysql_verdict,
      redis: $redis_verdict,
      source_provenance: $source_clean_committed,
      credential_leak: $credential_leak,
      overall: (
        $mysql_verdict
        and $redis_verdict
        and $source_clean_committed
        and ($credential_leak | not)
      )
    }
  }' "$receipt_candidate" >"$receipt_temp"

if ! jq -e -f "$repo_dir/scripts/receipt-contract.jq" "$receipt_temp" >/dev/null; then
  echo "error: generated receipt violates scripts/receipt-contract.jq" >&2
  exit 1
fi

mv "$receipt_temp" artifacts/receipt.json
receipt_temp=
jq . artifacts/receipt.json

overall_verdict=$(jq -r '.assertions.overall' artifacts/receipt.json)
if [ "$overall_verdict" != true ]; then
  echo "error: live acceptance failed; inspect artifacts/receipt.json" >&2
  exit 1
fi
