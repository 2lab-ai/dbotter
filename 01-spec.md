# dbotter — MVP product contract

Status: implemented MVP contract, 2026-07-14.

## Product boundary

dbotter is a local, single-user desktop database client written in Rust. It is a
behavior-level rewrite inspired by DBeaver, not a line-for-line translation of
DBeaver's Java/Eclipse/JDBC/OSGi implementation.

DBeaver upstream evidence reviewed on 2026-07-14:

- <https://github.com/dbeaver/dbeaver> describes a desktop SQL/data client and a
  Java + Eclipse RCP + OSGi + JDBC architecture.
- The upstream Community repository is Apache-2.0. Redis and MongoDB are listed
  as PRO/non-JDBC data sources, so dbotter must not copy unavailable PRO code.

The clean first milestone is deliberately smaller than DBeaver: connection
profiles, MySQL connect/query/results, Redis command execution, and a MongoDB
driver boundary that compiles and advertises planned capabilities without
pretending to work. Catalog browsing is deferred.

## Users and acceptance

A local developer can:

1. start dbotter as a native desktop app;
2. add a MySQL profile without persisting its password in plaintext;
3. test the profile, execute a SQL statement, and inspect tabular rows or
   affected-row counts; catalog browsing is visibly deferred in this MVP;
4. add a Redis profile, connect, and execute commands such as `PING`, `SET`,
   `GET`, `DEL`, and `SCAN`;
5. see MongoDB in the driver registry as `planned`, with its module and
   capability contract present but connect/query actions disabled;
6. execute the same MySQL and Redis paths headlessly so Docker verification can
   produce a deterministic JSON receipt.

## Functional requirements

### FR1 — connection profiles

- Profile fields: stable id, display name, driver kind, host, port, optional
  database, optional username, TLS mode, and `secret_env`.
- Persist profiles in `~/.config/dbotter/config.toml`, overridable by
  `DBOTTER_CONFIG`.
- Persist no password/token value. Resolve it at connect time from
  `secret_env`. Session-only password entry is deferred.
- Config mutations use atomic read-merge-write. A profile upsert is keyed by id.
- Logs and errors never include the secret or a credential-bearing URI.

### FR2 — driver contract

- Every driver exposes metadata (`kind`, display name, default port,
  availability, ready capabilities, planned capabilities) separately from
  live runtime operations. A planned flag is never rendered as ready.
- Current operations are `connect`, `ping`, and `execute`. `load_catalog` has no
  runtime command, service method, or driver method in this MVP.
- A request carries an operation id, profile id, query language, statement
  text, row limit, and timeout. Every service outcome echoes both ids.
- A response is backend-neutral: columns, typed cells, rows, affected rows,
  elapsed time, truncation flag, and backend notices.
- Planned drivers return typed `DriverError::Unavailable`; denied runtime
  operations return typed `DriverError::Unsupported`. Production paths contain
  no `todo!`, `panic!`, `unwrap()`, or `expect()`.
- CLI and GUI use the same `ApplicationService`: profile lookup, secret
  resolution, capability validation, connect/ping, cached session reuse, and
  execute are not reimplemented at either entrypoint.
- Public error display is redacted and stable. Raw sqlx/Redis errors remain in
  the source chain for diagnostics but their messages are not shown by CLI/UI.

### FR3 — MySQL MVP

- Connect using `sqlx::mysql::MySqlConnectOptions`, never by formatting a URI.
- Bound connect timeout and statement timeout.
- MySQL-dialect tokenization validates exactly one executable statement before
  any SQL statement is sent. The shared service may establish a session before
  driver-level text validation. One optional trailing terminator is accepted;
  semicolons inside comments or quoted values do not split statements.
- Prepared-statement metadata, rather than leading-keyword heuristics,
  distinguishes result sets from mutations. Result sets return up to
  `row_limit` rows and mark the result truncated when another row exists.
- Mutation/DDL statements return affected rows and last insert id where the
  driver supplies it.
- Only MySQL error 1295 (unsupported by the prepared-statement protocol) may
  retry the already validated statement through the raw protocol. Other
  prepare failures are returned without retry.
- Catalog loading is deferred until the driver contract grows a typed catalog
  operation. The UI says so explicitly and does not display synthetic schema.
- One editor action maps to one statement in MVP. Multi-statement scripts,
  query plans, transactions, data editing, SSH tunnels, import/export, and AI
  are explicitly deferred.

### FR4 — Redis runtime

- Connect with an async multiplexed connection.
- Parse one command line with shell-style quoting into command + arguments.
- Convert RESP values to the shared result model without losing nested values;
  nested arrays/maps may be rendered as JSON in a single `value` column.
- Passwords use the same secret resolution and masking rules as MySQL.
- Pub/sub, cluster topology, Redis modules, and key editing are deferred.

### FR5 — MongoDB scaffold

- `DriverKind::MongoDb`, metadata, default port 27017, capability flags, config
  validation, UI registry entry, and `drivers/mongodb.rs` exist.
- Availability is `Planned`; connect and execute controls are disabled.
- Calling the module directly returns typed unavailable errors.
- The future seam is document-native: a Mongo request/result must not be forced
  through SQL text semantics. `QueryLanguage::{Sql, RedisCommand,
  MongoDocument}` is already part of the request contract.

### FR6 — desktop UI

- `eframe`/`egui` native window with connection/status navigation, editor, and
  result/status panes. The connection pane marks catalog browsing deferred.
- UI state never owns network clients. It sends runtime commands through a
  bounded channel and polls runtime events without blocking the render thread.
- Connection actions and execute actions have explicit pending/success/failure
  states. Double-submit while pending is rejected.
- Results are rendered from the same `QueryResult` returned by headless mode.

### FR7 — headless verification and receipts

- `dbotter check --profile <id> --format json` exercises secret resolution,
  driver selection, connect, and ping.
- `dbotter exec --profile <id> --text <statement> --format json` exercises the
  exact application service and driver used by the GUI.
- `scripts/verify-local.sh` starts no services itself. It assumes the repository
  Compose stack is healthy, seeds MySQL through dbotter, exercises Redis through
  dbotter, writes `artifacts/receipt.json`, and exits non-zero on any mismatch.
- Receipt fields: UTC start/finish, source and toolchain metadata,
  Docker/Compose service metadata, sanitized profiles, normalized app and
  official-client outputs, elapsed time, and per-backend assertions. Every app
  or official-client input is recorded only as a stable fixture statement name
  plus its SHA-256 fingerprint. SQL text, Redis command text, and
  credential-bearing argv are not serialized.
- A passing receipt is source-bound to dbotter's own Git repository: `HEAD` is
  an attached commit, the worktree (including untracked files) is clean, and
  the required source/receipt files are tracked. A missing Git repository,
  nested or untracked checkout, detached `HEAD`, or dirty tree records a
  provenance failure, forces `assertions.overall = false`, and makes the
  verifier exit non-zero. The verification binary is rebuilt from this tree.
- `credential_leak` is derived, never asserted as a constant. The verifier
  serializes a receipt candidate without derived assertions, scans the complete
  candidate for fixture passwords, credential-bearing URI syntax, the provided
  MySQL fixture password, and non-empty values resolved from configured
  `secret_env` names, then injects the derived boolean and overall verdict. The
  resolved values are compared in memory and are never written as scan input.
- A static jq contract enforces input fingerprints, source provenance/pass
  coupling, and leak/pass coupling. Its shell test rejects injected fixture
  secrets, resolved secrets, credential-bearing URIs, and false clean-tree
  claims.

## Non-goals for the first receipt

- DBeaver feature parity or plugin compatibility.
- JDBC/ODBC compatibility.
- MongoDB live connectivity.
- Credential persistence/keychain integration.
- Catalog browsing, SSH/proxy tunnels, editable grids, schema changes, transactions, execution
  plans, import/export, ER diagrams, AI, extensions, or auto-update.
- Production distribution/signing.

## Quality gates

- `just check`: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test`.
- `just check-all`: the same with `--all-features` so MongoDB scaffolding does
  not silently rot.
- Contract tests start from the vertical traces in `03-traces.md`.
- Production paths contain no `unwrap`, `expect`, `panic`, or `todo`.
- Local live acceptance additionally requires `artifacts/receipt.json` proving
  MySQL and Redis through the dbotter binary, not only through vendor CLIs.
