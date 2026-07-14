# dbotter — implementation and conformance plan

Status is recorded per slice so planned work is not mistaken for a current
runtime capability. `Cargo.toml` and `Cargo.lock`, rather than a dependency
sketch in this document, are the dependency source of truth.

## Patch 0 — repository contract and build skeleton (implemented)

Files: `AGENTS.md`, `CLAUDE.md`, `.gitignore`, `01-spec.md`,
`02-architecture.md`, `03-traces.md`, `04-patch-plan.md`, `Cargo.toml`,
`Cargo.lock`, `justfile`, `src/main.rs`, `src/lib.rs`, `src/cli.rs`,
`src/error.rs`.

- Rust edition 2024 and MSRV 1.94 are declared in the manifest.
- The single binary exposes GUI, check, exec, and driver-list commands.
- Gates: `just check` and `just check-all`.

## Patch 1 — model, registry, config, and secrets (implemented)

Files: `src/model.rs`, `src/config.rs`, `src/secrets.rs`,
`src/drivers/mod.rs`, `src/drivers/mongodb.rs`, `tests/contracts.rs`, and config
unit tests in `src/config.rs`.

- Profiles persist only non-secret fields and an optional environment-variable
  name; profile upsert is atomic read-merge-write.
- Descriptors separate ready from planned capabilities. MongoDB is planned,
  has no ready capabilities, and returns `Unavailable` without network access.
- Contract tests cover registry order/capabilities, wire names, error redaction,
  secret references, version errors, and profile replacement.

## Patch 2 — shared service and MySQL slice (implemented)

Files: `src/service.rs`, `src/drivers/mysql.rs`, `src/cli.rs`,
`tests/service_contract.rs`, `tests/mysql_contract.rs`.

- The service owns profile lookup, secret resolution, capability/language/limit
  validation, session reuse, ping, and execute.
- MySQL validates one statement with the MySQL tokenizer, prepares for metadata,
  consumes `fetch_many`, and permits validated raw fallback only for error 1295.
- Fake service tests are hermetic. Live MySQL contracts require
  `DBOTTER_TEST_MYSQL=1`; the normal Cargo gate does not contact MySQL.

## Patch 3 — minimum desktop UI (implemented); catalog (deferred)

Files: `src/ui/mod.rs`, `src/ui/adapter.rs`, `src/ui/runtime.rs`,
`src/ui/model.rs`, `src/ui/app.rs`, `src/ui/profile_form.rs`.

- The bounded bridge, profile form, Test/Execute actions, pending/error states,
  stale-event protection, and typed result table are implemented and unit
  tested in their owning modules.
- The background bridge calls `ApplicationService`; UI state performs no driver
  I/O and owns no live client.
- T3 catalog refresh remains deferred. There is no catalog command, model,
  driver method, query, tree, or standalone catalog source file.

## Patch 4 — Redis runtime slice (implemented)

Files: `src/drivers/redis.rs`, registry/service/UI files already named above,
and Redis driver unit tests in `src/drivers/redis.rs`.

- Redis uses an async connection manager, shell-style command parsing, a
  subscription/monitor denylist, and typed RESP-to-`Cell` normalization.
- Unit tests cover quoting, every denied command, flat arrays, nil, and nested
  map/array shape. The Docker receipt covers live PING, SET, GET, and TTL.
- There is no standalone `tests/redis_contract.rs`; private parser/normalizer
  helpers are tested in their owning module.

## Patch 5 — Compose fixture and final receipt (implemented; live run external)

Files: `docker-compose.yml`, `config/local.example.toml`,
`scripts/verify-local.sh`, `scripts/receipt-security.sh`,
`scripts/receipt-contract.jq`, `scripts/test-receipt-contract.sh`, `README.md`.
`artifacts/receipt.json` is generated and ignored rather than tracked.

- MySQL binds `127.0.0.1:33306`; Redis binds `127.0.0.1:36379`. Both services
  have health checks and named volumes. MongoDB is opt-in and is not part of the
  first live acceptance.
- `scripts/verify-local.sh` assumes the caller has started healthy MySQL and
  Redis services. It must prove both dbotter paths and official-client readback
  before emitting an overall pass.
- Live execution is intentionally outside hermetic Cargo gates and requires a
  clean committed target checkout as defined by T6.

## Patch 6 — conformance audit (current)

- Keep the File Map limited to paths that exist.
- Compare T0..T6 names, types, events, error paths, tests, and receipt fields
  against source before claiming implementation.
- Run a File Map existence check, shell receipt-contract tests when present,
  `cargo fmt --check`, all-feature Clippy with warnings denied, and all-feature
  tests offline.
- Do not call catalog or MongoDB runtime-ready. Catalog is a future trace only;
  MongoDB is a planned descriptor/scaffold only.

## Deferred work

- T3: typed MySQL catalog request/result, service/driver operation, UI command,
  catalog state, and tests.
- MongoDB: document-native request/result and live driver implementation.
- Cancellation, close-profile lifecycle, transactions, editable grids,
  import/export, SSH/proxy, and distribution/signing.
