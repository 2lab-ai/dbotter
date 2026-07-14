# dbotter — architecture

## Decision summary

dbotter is one Rust package with a library and one binary. The headless CLI and
desktop runtime both call `ApplicationService`; profile lookup, secret
resolution, capability checks, connection creation, session reuse, ping, and
execute are implemented there once. Catalog loading is not part of the current
service or driver seam.

## Runtime topology

```text
headless CLI ---------------------------> ApplicationService
                                                |
eframe UI -> bounded UiCommand channel -> ui::runtime task
   ^                                            |
   |                                      ApplicationService
   +------ bounded UiEvent channel --------------+
                                                |
                                   profile-keyed session cache
                                      /                    \
                               MySqlSession            RedisSession
                                SQLx pool         connection manager
```

The CLI awaits the service directly. The desktop UI uses nonblocking
`try_send`/`try_recv` at the render boundary; `src/ui/runtime.rs` consumes
commands on Tokio and awaits the same service. `UiModel` owns snapshots,
pending operation ids, statuses, and the latest `QueryResult`, but no database
client. MongoDB has registry metadata only and cannot create a session.

`ApplicationService` owns `Arc<RwLock<Config>>` and
`Arc<RwLock<HashMap<ProfileId, CachedSession>>>`. It clones data while a lock is
held, drops the guard, and only then awaits driver I/O. Each cache entry includes
the complete non-secret profile used to connect, so a profile edit invalidates
only changed or removed sessions.

## File map

Every path below exists in the current tree:

```text
.gitignore
01-spec.md                       product contract
02-architecture.md               this architecture
03-traces.md                     current and explicitly deferred vertical traces
04-patch-plan.md                 implementation/status plan
AGENTS.md                        repository agent contract
CLAUDE.md                        documentation entrypoint
Cargo.lock
Cargo.toml
README.md                        build, desktop, and local acceptance runbook
docker-compose.yml               MySQL + Redis; opt-in MongoDB fixture only
justfile                         default and all-feature quality gates
config/local.example.toml        non-secret local profiles
scripts/verify-local.sh          live receipt producer and assertions
scripts/receipt-security.sh      receipt fingerprints and leak detectors
scripts/receipt-contract.jq      receipt-v2 structural/pass invariant contract
scripts/test-receipt-contract.sh static receipt contract/security tests
src/main.rs                      Tokio entrypoint and process error display
src/lib.rs                       module exports
src/cli.rs                       `gui`, `check`, `exec`, and `drivers`
src/error.rs                     top-level `AppError`
src/model.rs                     ids, profiles, descriptors, requests, results
src/config.rs                    TOML load and atomic profile upsert
src/secrets.rs                   environment secret resolution
src/service.rs                   shared orchestration and session cache
src/drivers/mod.rs               session enum, registry, and `DriverError`
src/drivers/mysql.rs             connect/ping/SQL execute and cell decoding
src/drivers/redis.rs             connect/ping/command execute and RESP mapping
src/drivers/mongodb.rs           planned descriptor and unavailable error
src/ui/mod.rs                    eframe startup and bounded bridge construction
src/ui/adapter.rs                bounded command/event ports
src/ui/runtime.rs                background Tokio-to-service bridge
src/ui/model.rs                  pure snapshots and correlation-safe event fold
src/ui/app.rs                    profile/editor/result rendering
src/ui/profile_form.rs           profile validation and form state
tests/contracts.rs               driver registry, wire names, error redaction
tests/mysql_contract.rs          opt-in live MySQL contracts
tests/service_contract.rs        service/cache/config contracts with fakes
```

`artifacts/receipt.json` is generated and ignored; it is not a source-tree file.

## Core types

The current public shapes in `src/model.rs` are:

```rust
enum DriverKind { MySql, Redis, MongoDb }
enum DriverAvailability { Ready, Planned }
bitflags DriverCapabilities { CONNECT, PING, SQL, COMMAND, DOCUMENT, CATALOG }
enum QueryLanguage { Sql, RedisCommand, MongoDocument }

struct ConnectionProfile {
    id: String,
    name: String,
    driver: DriverKind,
    host: String,
    port: u16,
    database: Option<String>,
    username: Option<String>,
    tls: TlsMode,
    secret_env: Option<String>,
}

struct ExecuteRequest {
    operation_id: OperationId,
    profile_id: ProfileId,
    language: QueryLanguage,
    text: String,
    row_limit: u32,
    timeout: Duration,
}

struct QueryResult {
    columns: Vec<Column>,
    rows: Vec<Vec<Cell>>,
    affected_rows: u64,
    last_insert_id: Option<u64>,
    elapsed_ms: u128,
    truncated: bool,
    notices: Vec<String>,
}
```

`ProfileId` and `OperationId` are newtypes. `ConnectionProfile.id` remains a
serialized `String`; there is no editor-tab id type. Runtime credentials use
`SecretString` and are never serializable.

Descriptors separate `capabilities` from `planned_capabilities`: MySQL is ready
for connect/ping/SQL and plans catalog; Redis is ready for
connect/ping/command; MongoDB has no ready capabilities and plans
connect/ping/document/catalog. `reason` carries the planned explanation.

## Current driver and service seam

The injectable boundary used by service tests is defined in `src/service.rs`:

```rust
#[async_trait]
trait SessionConnector: Send + Sync {
    async fn connect(
        &self,
        profile: &ConnectionProfile,
        secret: Option<&SecretString>,
        timeout: Duration,
    ) -> Result<Arc<dyn SessionHandle>, DriverError>;
}

#[async_trait]
trait SessionHandle: Send + Sync {
    async fn ping(&self, timeout: Duration) -> Result<(), DriverError>;
    async fn execute(&self, request: &ExecuteRequest)
        -> Result<QueryResult, DriverError>;
}
```

`DriverConnector` adapts that boundary to `drivers::connect`, which returns the
current `Session::{MySql, Redis}` enum. There is no `Driver` trait,
`DriverSession` trait, `CatalogSnapshot`, or `load_catalog` method today.

Catalog remains a planned MySQL capability. A future typed catalog request and
snapshot may extend the seam, but the current UI only displays that catalog
browsing is deferred and emits no catalog command. MongoDB's future execution
seam must remain document-native rather than pretending BSON is SQL.

## Dependency choices

The manifest currently enables:

- `tokio` with `rt-multi-thread`, `macros`, and `time`;
- optional `eframe` and `egui_extras` behind `desktop`;
- optional `mongodb` behind `mongodb` as a compile-only future adapter;
- `sqlx` with Tokio, rustls ring/webpki, MySQL, chrono, JSON, and decimal;
- `redis` with async Tokio and connection-manager support;
- `async-trait`, `bitflags`, `chrono`, `clap`, `futures-util`,
  `rust_decimal`, `secrecy`, `serde`, `serde_json`, `shell-words`, `sqlparser`,
  `thiserror`, `toml`, `tracing`, and `tracing-subscriber`;
- `tempfile` as the only direct dev dependency.

`sqlparser` is used only for MySQL-aware statement-boundary tokenization. MySQL
and SQLx still parse and execute SQL and provide result metadata.

## Concurrency and correlation

- UI command and event channels are bounded. A full command channel returns
  `SubmitError::Busy` without blocking the render thread.
- Save, Test, and Execute carry `OperationId`; terminal events include the same
  id and profile id. The pure fold ignores stale completion events.
- The UI runtime processes its command stream sequentially. The service cache
  is independently safe for concurrent CLI/test callers and reconciles a
  duplicate connection race before insertion.
- Connect, ping, and execute are timeout-bounded. There is no user cancellation
  command, server-side cancellation, or close-profile operation in the MVP.
- A successful profile save updates the on-disk config, service snapshot, and
  cache before `ProfileSaved` is emitted.

## Error boundary

`ConfigError`, `SecretError`, `DriverError`, `ServiceError`, and `AppError` are
the current typed layers. UI busy/disconnected conditions use `SubmitError` and
terminal service failures use `UiEvent`; there is no `RuntimeError` type.

`DriverError::MySql` and `DriverError::Redis` display stable redacted messages
while retaining backend errors as sources. CLI/UI render the outer display
message, not backend source text. Profile endpoints contain driver, host, and
port only.

## Config and secrets

Config schema version 1 stores only non-secret profile fields. An upsert reloads
the current file, merges by profile id, writes a same-directory 0600 temporary
file on Unix, calls `sync_all`, and renames it into place. Directory fsync is not
implemented. Missing config files load as an empty version-1 config.

The local fixture uses `config/local.example.toml`; MySQL refers to
`DBOTTER_MYSQL_PASSWORD` by name and Redis has no fixture password.

## Licensing boundary

dbotter is Apache-2.0. DBeaver is behavior/product research only. Do not copy
DBeaver PRO Redis or MongoDB implementation code.
