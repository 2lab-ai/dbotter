# dbotter — vertical traces

The trace is the source of truth. Update this document before changing the
cross-layer behavior.

## T0 — add or edit a connection profile

### 1. API entry

- GUI: connection pane `Add MySQL` / `Add Redis` / `Add MongoDB` or a profile's
  `Edit` action, followed by `Save`.
- Local single-user process; no network API/authz boundary.

### 2. Input

- Stable id, display name, driver, non-empty host, port `1..=65535`, optional
  database/username, TLS mode, and optional `secret_env` name.
- Redis database, when present, is a non-negative integer.
- `secret_env` is an environment-variable name only. The form and persistence
  command contain no password/token value. Session password entry is deferred.
- MongoDB profiles are valid prepared configuration even though their driver is
  `planned`.

### 3. Layer flow

profile form -> pure validation/defaulting ->
`UiCommand::UpsertProfile { operation_id, profile }` -> bounded Tokio channel ->
runtime background task -> service-side validation ->
`config::upsert_profile_path(config_path, profile)` atomic read-merge-write ->
`ApplicationService::replace_config(updated_config)` -> changed profile session
cache invalidation -> `UiEvent::ProfileSaved { operation_id, profile_id }` -> UI
sets `selected_profile = profile_id` -> `UiCommand::RefreshProfiles` -> service
snapshot -> `ProfilesLoaded` -> pure model fold retains the saved selection.

The service config snapshot is replaced before `ProfileSaved` is emitted.
Therefore a subsequent Test/Execute command in the same runtime observes the
saved profile. Cached sessions are keyed by both profile id and the complete
non-secret profile used to create them: unchanged profiles retain sessions;
changed/removed profiles reconnect on their next operation.

### 4. Side effects

- Config version 1 is atomically upserted by profile id.
- A changed profile's cached session is removed. In-flight `Arc` clones may
  finish, but are not reused by later commands.
- No database connection is attempted by Save, including MongoDB Save.

### 5. Error paths

- Invalid form -> inline errors; no command.
- Invalid direct/stale command -> `ProfileSaveFailed`; no config mutation.
- Full channel/double Save -> visible Busy/already-pending state; at most one
  upsert command.
- Config write/reload failure -> correlated `ProfileSaveFailed`; old service
  snapshot and cache remain authoritative.

### 6. Output

- Success closes the form, refreshes the list, and selects the saved profile.
- MongoDB remains visibly planned; Test and Execute are disabled after Save.

### 7. Observability

- UI events carry operation/profile ids; persisted profiles contain only the
  `secret_env` name. Structured operation spans are not implemented.
- Never serialize a plaintext password, token, resolved `SecretString`, or
  credential-bearing URI.

Contract tests: required/default/driver validation; command contains
`secret_env` but no secret literal; busy/double-save; atomic save immediately
usable by check/execute; changed profile invalidates its cached session; invalid
direct command does not mutate config. MongoDB form tests prove Save emits an
upsert command while Test/Execute controls remain disabled; the service's
planned-driver test separately proves a check never invokes the connector.

## T1 — test a MySQL profile

### 1. API entry

- GUI: connection form `Test` action.
- CLI: `dbotter check --profile mysql-local --format json`.
- Local single-user process; no network API/authz boundary.

### 2. Input

- Existing profile id; driver must be `mysql` and availability `ready`.
- Host non-empty, port non-zero, username/database valid for MySQL options.
- If `secret_env` is set, the environment variable must exist and be non-empty.
- Connect timeout is within configured bounds.

### 3. Layer flow

`UiCommand::TestConnection { operation_id, profile_id }` or CLI ids ->
`ApplicationService::check(operation_id, profile_id)` ->
config `ConnectionProfile` -> secrets `secret_env` -> session-only
`SecretString` -> `ApplicationService::session_for` -> cached session registry ->
`SessionConnector::connect` ->
`DriverKind::MySql` -> `MySqlConnectOptions { host, port, username, database,
password, ssl_mode }` -> `MySqlSession::connect` -> `SELECT 1` ping ->
`UiEvent::ConnectionReady { operation_id, profile_id, elapsed_ms }` -> pure `UiModel` fold
or JSON stdout.

No password is transformed into a URI, log field, error display, or receipt.

### 4. Side effects

- Service inserts `profile_id -> Arc<dyn SessionHandle>` after connect and
  removes it if the following ping fails.
- GUI updates connection state. Database state is unchanged.

### 5. Error paths

- Unknown profile -> `ServiceError::UnknownProfile`; no session mutation.
- Missing secret env -> `SecretError::MissingEnv`; no network attempt.
- Auth/connect/timeout -> typed driver error; no registry insertion.
- Planned driver -> unavailable; no network attempt.

### 6. Output

- GUI shows connected state and elapsed time.
- JSON: operation id, profile id, driver, redacted endpoint, elapsed ms,
  `status: "ok"`.

### 7. Observability

- The GUI status and JSON receipt expose ids, redacted endpoint, and elapsed
  time. Structured spans are not implemented.
- Backend error display is redacted; raw sources remain internal.

Contract tests: mock check/execute session reuse and id correlation; missing
secret and planned MongoDB rejection before connector invocation; backend error
display redaction. Live JSON behavior is covered by T6, not the hermetic unit
suite.

## T2 — execute MySQL SQL and return rows

### 1. API entry

- GUI: editor Execute action.
- CLI: `dbotter exec --profile mysql-local --text 'SELECT ...' --format json`.

### 2. Input

- Connected or connectable ready MySQL profile.
- Language is SQL; trimmed statement is non-empty and MySQL tokenization finds
  exactly one executable statement (with at most one trailing terminator).
- Row limit 1..=10_000; timeout within configured bounds.

### 3. Layer flow

editor/CLI ids + text -> `ExecuteRequest { operation_id, profile_id, language,
text }` -> validated SQL payload -> cached session lookup ->
`ApplicationService::execute(request)` -> session registry by `profile_id` ->
`SessionHandle::execute` -> `MySqlSession::execute` -> tokenize/validate one
statement -> SQLx prepare -> prepared column metadata + `fetch_many` -> MySQL
wire -> `MySqlRow` + column type info / mutation result -> typed `Cell` values -> `QueryResult` ->
`UiEvent::QueryFinished` -> latest result table or JSON stdout.

If prepare fails specifically with MySQL error 1295, the driver retries the
already validated statement with SQLx raw execution and derives result columns
from the first row. No other prepare error triggers raw execution. The driver
fetches at most `row_limit + 1`: the extra row ->
`QueryResult.truncated = true` and is not returned.

### 4. Side effects

- The submitted SQL may mutate the target database.
- dbotter stores no query result persistently in MVP.
- UI replaces its single in-memory latest result.

### 5. Error paths

- Language mismatch -> input error before session lookup. Empty or multiple SQL
  statements -> no SQL statement is sent, although session creation may already
  have connected because text validation is driver-local.
- Not connected -> service connects through T1, then executes; connect failure
  prevents SQL.
- Statement timeout -> `DriverError::Timeout`; result state is failure.
- SQL error/decode error -> typed error; no fabricated rows.
- CLI/UI receive only redacted `ServiceError`/`DriverError` display text; raw
  sqlx source messages are retained internally and never rendered.

### 6. Output

- Row query: ordered columns, typed rows, elapsed, truncated.
- Mutation: affected rows, optional last insert id, elapsed.
- CLI JSON is stable enough for receipt assertions.

### 7. Observability

- GUI/JSON output includes result timing, row data, affected rows, and redacted
  endpoint. No SQL hash or structured query span is emitted today.
- Raw SQL is not written by application logging; the explicit live receipt may
  record only the fixture input evidence defined by T6.

Contract tests: unit tests cover tokenizer boundaries and 1295-only fallback
selection. Opt-in live tests cover quoted/comment semicolons, multiple-statement
rejection, empty SELECT/SHOW/EXPLAIN metadata, CTE query/mutation, affected rows,
column order, and the 1295 raw fallback. Live cases require
`DBOTTER_TEST_MYSQL=1`; row-limit truncation does not yet have a live contract.

## T3 — load MySQL catalog (deferred)

Implementation status: deferred after the first MySQL/Redis receipt. The core
driver contract has no catalog operation yet. The desktop UI displays
"Catalog browsing is deferred in this MVP" and submits no catalog command.
The future-only sketch below remains the contract for later implementation.
None of its actions, commands, types, queries, or events exist in the MVP.

### 1. API entry

- GUI expands a connected MySQL profile or clicks Refresh.

### 2. Input

- Ready MySQL profile and optional database filter.

### 3. Layer flow

Future only: `UiAction::RefreshCatalog(profile_id)` -> `RuntimeCommand::LoadCatalog` ->
`MySqlSession::load_catalog` -> `information_schema.schemata/tables/columns` ->
`CatalogSnapshot { namespaces -> relations -> columns }` -> runtime event ->
pure catalog tree fold.

`profile.database` -> optional schema predicate ->
`information_schema.TABLE_SCHEMA`; identifiers are bound values, not SQL
string concatenation.

### 4. Side effects

- Runtime/UI replaces only that profile's catalog snapshot. DB unchanged.

### 5. Error paths

- Permission denied/timeout -> keep previous snapshot and mark it stale with
  the error; do not clear it silently.

### 6. Output

- Ordered namespaces, tables/views, columns, type names, nullable flag.

### 7. Observability

- Operation/profile, elapsed, namespace/relation/column counts.

## T4 — execute a Redis command

### 1. API entry

- GUI Redis command editor Execute.
- CLI: `dbotter exec --profile redis-local --text 'SET receipt ok'`.

### 2. Input

- Ready Redis profile, language `RedisCommand`, non-empty command line.
- `shell_words` parsing succeeds and yields at least a command token.
- MVP denies blocking/subscription commands (`SUBSCRIBE`, `PSUBSCRIBE`,
  `SSUBSCRIBE`, `MONITOR`) with `Unsupported` to protect the multiplexed
  session.

### 3. Layer flow

editor/CLI ids + text -> correlated `ExecuteRequest` -> cached session lookup ->
`ExecuteRequest.text` -> `shell_words::split` -> first token
`redis::Cmd` name + remaining tokens `.arg` -> multiplexed async connection ->
RESP `redis::Value` -> recursive JSON-like normalization -> shared
`QueryResult { columns:[value], rows }` -> `UiEvent::QueryFinished` -> result view/JSON.

### 4. Side effects

- Command-defined Redis mutation, e.g. `SET receipt ok`.
- Service session registry and in-memory result state only.

### 5. Error paths

- Parse error/empty -> no Redis command is sent; session creation may already
  have connected because parsing is driver-local.
- Denied blocking command -> unsupported and no Redis command is sent; an
  uncached session may already have connected.
- Redis auth/network/server error -> typed driver error, no fabricated value.

### 6. Output

- Scalar -> one row/one `value` column.
- Array -> one row per element when flat; nested/map/set -> JSON cell.
- Nil -> `Cell::Null`; integer/bulk string preserved by type where possible.

### 7. Observability

- GUI/JSON output includes ids, redacted endpoint, elapsed, and response shape.
  Structured Redis command spans are not implemented.
- The application does not log Redis arguments; T6 defines the fixture-only
  receipt evidence.

Contract tests: driver unit tests cover quoted parsing, the blocking-command
denylist, and flat/nested RESP normalization. T6 covers live PING, SET, GET, and
TTL through the CLI plus official-client readback.

## T5 — MongoDB planned driver is honest

### 1. API entry

- Driver picker lists MongoDB.

### 2. Input

- User selects MongoDB or a Mongo profile is loaded from config.

### 3. Layer flow

`DriverKind::MongoDb` -> registry `DriverDescriptor` ->
`availability = Planned`, empty ready capabilities, and
`CONNECT|PING|DOCUMENT|CATALOG` planned capabilities -> UI disables Test and
Execute and shows the reason. A direct `drivers::connect` or service check ->
`DriverError::Unavailable { driver: MongoDb, reason }`.

### 4. Side effects

- None.

### 5. Error paths

- Direct or stale UI invocation returns unavailable. No client/network attempt.

### 6. Output

- Visible planned status; no false connected state.

### 7. Observability

- A stale/direct UI action receives a correlated failure event. No separate
  MongoDB info event or network activity exists.

Contract tests: registry entry/capabilities/default port; UI action disabled;
service check returns unavailable before connector invocation.

## T6 — local Docker receipt

### 1. API entry

- Human/CI runs `docker compose up -d --wait`, then
  `scripts/verify-local.sh`.

### 2. Input

- `DBOTTER_CONFIG=config/local.example.toml`.
- `DBOTTER_MYSQL_PASSWORD` set to the Compose fixture password.
- Healthy `mysql` and `redis` Compose services.

### 3. Layer flow

script -> `dbotter check mysql-local` (T1) ->
`CREATE TABLE IF NOT EXISTS receipt...` (T2) -> upsert marker (T2) -> select
marker (T2) -> `dbotter check redis-local` (T1 generalized) -> `SET` (T4) ->
`GET` (T4) -> parsed stable JSON -> source-provenance assertion -> candidate
receipt JSON -> whole-candidate credential scan -> derived assertions -> one
combined receipt JSON.

`expected marker` -> dbotter CLI input -> driver command/SQL -> database value
-> `QueryResult.Cell` -> CLI JSON -> receipt assertion actual value.

Each application or official-client input follows:

`SQL/command/check payload` -> SHA-256 before execution ->
`{kind, fixture_statement, sha256}` receipt descriptor. The payload itself is
passed only to the process invocation; neither it nor a second copy in `argv`
is serialized. Fixture statement names are `mysql.check`,
`mysql.create_receipt_table`, `mysql.upsert_receipt_marker`,
`mysql.select_receipt_marker`, `redis.check`, `redis.set_receipt_marker`,
`redis.get_receipt_marker`, and `redis.ttl_receipt_marker`.

The separately documented fixture templates are:

- `mysql.create_receipt_table`: create `dbotter_receipt(run_id, marker,
  updated_at)` if absent;
- `mysql.upsert_receipt_marker`: insert `(:run_id, :marker)`, updating marker
  and timestamp on the `run_id` key;
- `mysql.select_receipt_marker`: select `run_id, marker` by `:run_id`;
- `redis.set_receipt_marker`: `SET :key :marker EX 300`;
- `redis.get_receipt_marker`: `GET :key`;
- `redis.ttl_receipt_marker`: `TTL :key`.

`git rev-parse/status/ls-files` -> repository-root/commit/branch/tracked/clean
facts -> `source.clean_committed`. The repository root must equal dbotter's
physical root, `HEAD` must be an attached commit, required receipt inputs must
be tracked, and porcelain status including untracked files must be empty.

### 4. Side effects

- MySQL fixture table/row and Redis fixture key.
- `artifacts/receipt.json` written last via temporary file + rename. A temporary
  candidate is serialized first, scanned, then transformed into the final
  receipt so the leak assertion never scans or asserts itself.

### 5. Error paths

- Any command non-zero, parse failure, wrong value, unhealthy service, source
  provenance failure, or secret leak scan -> `assertions.overall = false` and
  script non-zero. A failed receipt remains inspectable but cannot be presented
  as acceptance evidence.

### 6. Output

- Receipt proves MySQL connect + DDL/DML/query and Redis connect + write/read
  through dbotter. It includes normalized outputs, named SHA-256 input
  fingerprints, clean committed Git provenance, and pass assertions. It does
  not include raw SQL/Redis input text or credential-bearing argv.

### 7. Observability

- `docker compose ps`, image ids/digests, dbotter version/commit, UTC timestamp,
  durations, redacted endpoints. Vendor CLI output may be diagnostic evidence,
  but cannot substitute for dbotter-path assertions.
- Known fixture secrets, the provided MySQL password, and non-empty values from
  configured `secret_env` names are scanned in memory against the complete
  serialized candidate. URI syntax with userinfo plus password is also a leak.
  `credential_leak` is true iff any detector matches; `overall` requires its
  negation and `source.clean_committed` in addition to backend verdicts.

Contract tests: a clean candidate satisfies the jq receipt contract; setting
`source.dirty = true` while claiming overall pass is rejected; injecting a
fixture secret, a resolved secret, or `mysql://user:password@host/db` makes the
leak detector report a failure.
