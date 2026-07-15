# dbotter — usable MVP architecture

Status: **approved target architecture with the P1 and P2 foundations
independently reviewed GREEN. T0 remains RED overall; T1, T2, T3, T8, and T9
are Implementing; T4–T7 and T10 are Not started.**

Normative detail lives in `docs/usable-mvp/{spec,trace,plan}.md`. This document
is the repository architecture entrypoint and must remain consistent with those
frozen artifacts and the T0–T10 ledger in `03-traces.md`.

## Decision summary

dbotter remains one Rust package with a library and one binary. The native UI
and headless CLI share `ApplicationService`; neither reimplements profile
lookup, credential resolution, capability validation, connection lifecycle,
typed resource browsing, execution, or public error conversion.

The UI owns pure/display state only. Live sessions, task registry, config
writer, secrets, and filesystem export workers stay behind typed service and
runtime boundaries. No lock crosses `.await`.

### P1/P2 checkpoint boundary

P1 implements the config/profile mutation and reconciliation foundation,
credential storage/resolution types, atomic observed-state and session-cache
race foundations, and closed public-error/recovery mappings. That bounded slice
is independently reviewed GREEN.

P2 implements and independently proves the monotonic profile/session generation
allocator, fingerprinted cache with exact compare-remove, bounded controller,
reload/Config uncertain fences, tombstone lifecycle, classified cleanup, and
runtime shutdown. Reserve-before-spawn, coalesced control, one-profile/four-
global permits, exact event correlation, network-only two-second abort, and
durable mutation/export joins are part of that GREEN boundary. P2 does not
complete P6 native/RawInput/AccessKit or visual work, so T2/T3/T9 remain
Implementing rather than fully GREEN or Verified. P3 is still Not started and
Execute remains fail-closed.

## Target topology

```text
CLI commands --------------------------------------+
                                                   v
egui render -> bounded UiCommand ports -> runtime/controller -> ApplicationService
     ^                |                      |               |       |
     |                +-> control lane ------+               |       +-> config writer
     |                                                       +----------> secret store
     +<- bounded UiEvent lane <- correlated task registry <--+----------> session cache
                                                             |             /        \
                                                             |      MySQL prepared  Redis
                                                             +-> typed Catalog/Keyspace seams
                                                             +-> cooperative export worker
```

Required capacities and scheduling are contractual:

- serialized config mutation lane: 16;
- network work lane: 32;
- control lane for Cancel/Disconnect/Reconnect cleanup: 16;
- UI event lane: 128;
- one active network operation per profile generation, four process-wide;
- biased controller order: control → mutation → work;
- Shutdown has an independent watch signal.

## Identity, generations, and task ownership

The closed identity domains are not interchangeable:

- saved-profile work: `(ProfileId, ProfileGeneration, OperationId)`;
- draft work and Create: `(DraftId, OperationId)`;
- export: `(ResultId, OperationId)`;
- global load/shutdown: `OperationId`.

The registry shape is exact:

```rust
struct RegisteredTask {
    operation_id: OperationId,
    scope: TaskScope,
    cancel: CancellationToken,
    join: JoinHandle,
}

enum TaskScope {
    Profile {
        profile_id: ProfileId,
        profile_generation: ProfileGeneration,
        session_generation: Option<SessionGeneration>,
    },
    Draft { draft_id: DraftId },
    Export { result_id: ResultId },
    Global,
}
```

Only Profile scope contains profile/session generations. Runtime uses a
process-monotonic generation allocator; Delete publishes a tombstone and a
recreated id receives a greater generation. Cache entries are
`{profile_generation, session_generation, connection_fingerprint, handle}`.
Every eviction compare-matches both generations.

## Config and secret ownership

`config::load_path(&Path)` is the only lower-level loader. Entrypoints resolve
the path once using global `--config`, then `DBOTTER_CONFIG`, then the platform
default.

Version 1 is read-only input. Before the first confirmed v1→v2 mutation, the
writer creates fixed `<config>.v1.bak` with no-replace durability. Each
Create/Update/Delete then reloads the exact path, applies a typed mutation,
writes a 0600 same-directory temporary file, file-fsyncs, rechecks the input
fingerprint, renames at the commit point, parent-fsyncs, reloads, and reconciles
one of `NotCommitted`, `Committed`, or `CommittedDurabilityUnknown`.

`ConnectionProfile` contains only non-secret fields and persisted
`CredentialMode`. `SessionSecret` is non-serializable, redacted, and owned by
`HashMap<ProfileId, Arc<SessionSecret>>`. UI-only
`SessionCredentialIntent::{KeepCurrent, Replace, Forget}` maps to mutation-only
`SessionSecretUpdate::{Keep, Replace, Clear}` after the config commit point.
Environment mode stores a name only and exposes Available/Missing/Empty without
the value.

## Typed driver and resource seams

The target driver boundary is split by semantics:

```rust
trait ConnectionPing {
    async fn ping(&self, timeout: Duration) -> Result<(), DriverError>;
}

trait MySqlPreparedExecution {
    async fn execute_prepared(
        &self,
        request: &PreparedMySqlRequest,
    ) -> Result<QueryResult, DriverError>;
}

trait RedisExecution {
    async fn execute_command(
        &self,
        request: &RedisExecuteRequest,
    ) -> Result<QueryResult, DriverError>;
}

trait CatalogBrowser {
    async fn load_page(
        &self,
        request: &CatalogRequest,
    ) -> Result<CatalogPage, DriverError>;
}

trait KeyspaceBrowser {
    async fn scan_keys(
        &self,
        request: &RedisScanRequest,
    ) -> Result<RedisKeyPage, DriverError>;
    async fn inspect_key(
        &self,
        request: &RedisKeyInspectRequest,
    ) -> Result<RedisValuePreview, DriverError>;
}
```

MySQL user SQL has one entry: `PreparedMySqlRequest` through server
`COM_STMT_PREPARE` and `COM_STMT_EXECUTE`. SQLx's negotiated
`CLIENT_MULTI_STATEMENTS` capability is not a safety boundary. Source/trait
contracts reject user-text use of `sqlx::raw_sql`,
`Executor::execute(&str)`, `COM_QUERY`, or prepared-unsupported fallback.
Static/bound catalog statements are prepared too.

MySQL `CATALOG` and Redis `KEYSPACE_BROWSE` are independent capability bits.
Each stays planned until its hermetic and mandatory live contract turns green
in the same reviewed change. MongoDB remains a Planned descriptor with a
future document-native seam; it is never coerced into SQL.

## Runtime state and shutdown

`active_profiles`, tombstones, session cache, workspaces, secret store, and
task registry are runtime-owned. UI folds only events whose exact identity is
still current.

- Reload performs an id-keyed diff. Unchanged retains state; added is fresh;
  changed/removed fences then cancels, joins, evicts, and clears. An unreadable
  reload enters Config uncertain and permits only Reload/Shutdown.
- A runtime-neutral committed Edit may retag an idle proven handle. Active work
  or connection-affecting edits evict it after the new-generation fence.
- Cancel/timeout drops client waiting, joins, reports server state Unknown, and
  evicts only the exact used session generation.
- Async network work has a bounded abort grace. Blocking export checks
  cancellation per row/chunk and Shutdown waits for actual worker/temp cleanup.
- Registry/permit/temp cleanup precedes terminal event delivery, including
  panic/`JoinError` and full/closed event-lane cases.

The P2 source+test review is bound to snapshot
`e987bbf1d8a7f919cf53b95e882e0fa7b072d4226d7bb5e99e5e06d4dda65378`;
two independent reviewers each reported `NO P2 BLOCKER`. Its reproducible
production, test, and release-binary SHA-256 values are
`279757012280ab7bdcb90b547242114c80efcff3b64c26b7dcff4e3abb78fa9d`,
`467982ee06068fe8fee669cc20e43ca05b1a0f72129c69137743c70d3eecce1b`,
and `65ec73f1138587364005a1304fdd55006f85813283390fb3fd0f32f746183f3e`.
The checkpoint passed 188 regular tests and 12 doctests, strict locked/offline
Clippy, formatting, diff, release-contract, receipt, all-target/all-feature
tests, and the release build.

## UI architecture and accessibility

`UiModel` owns profile-generation workspaces, editor text, pending ids,
historical/current result snapshots, connection state, and public errors. It
owns no client or secret value.

The form distinguishes Create from Update and holds a `DraftId`. Test uses
temporary resources and has no path to config/cache/store/workspace mutation.
Create conflict recovery uses `ConnectionId`; draft recovery emits
`EditDraft`, while saved-profile recovery emits only safe ProfileId actions.

The editor exposes `editor.target`, `editor.row_limit`, and `editor.timeout`.
`FocusExecuteLimits` applies only to Execute. MySQL selection/caret extraction
and Redis physical-line extraction are pure before typed dispatch.

All P0 widgets have stable author ids, roles, names, focus order, keyboard
actions, enabled state, and non-color cues. Headless tests use egui 0.35
`Context::run_ui(RawInput, …)`, call `enable_accesskit()`, and inspect
`FullOutput.platform_output.accesskit_update`; installed automation verifies
the same ids as macOS AXIdentifier values.

## Public error and disclosure boundary

Internal errors convert through the exhaustive table in approved trace T8 to:

```rust
PublicOperationError {
    operation: OperationKind,
    category: ErrorCategory,
    code: PublicCode,
    summary: PublicSummary,
    recovery: NonEmpty<RecoveryAction>,
}
```

Unknown backend values become static InternalFailure. Backend prose and secrets
never cross the public boundary. User-owned editor/result/key/path values are
allowed only in their intended rendered/AX value node and, after explicit user
action, clipboard/export. Sensitive request types have manual redacted `Debug`
and no `Serialize`.

## Results, memory, and export

Every result carries `ResultProvenance` with profile/generation/operation.
Retained snapshot caps apply after driver decoding; transient row/RESP-frame
allocation is disclosed. The exact `Cell`, clipboard, TSV, CSV, and canonical
JSON mappings live in approved spec §9.

Export owns `Arc<ResultSnapshot>`, streams without a second whole-result byte
vector, uses a 0600 same-directory temporary file, file fsync, explicit
no-overwrite/confirmed-replace commit, and parent fsync. Reveal actions carry
safe ids, not paths. Runtime receipts contain no result digest or content.

## Distribution architecture

P8/P9 produce per-architecture signed `Dbotter Preview.app` bundles. Identity
is measured after signing and linked through typed source/build/artifact/release/
formula/install records. The installed CLI shim and exact launched PID must
resolve to the manifest's post-sign executable before AX input.

Binary identity and config compatibility are separate commands and schemas as
specified in `01-spec.md`. Preview and stable workflows share the verification
gate, but this task invokes preview only.

## Planned file ownership

Files listed below are expected by approved slices; absence before that slice is
not a deviation and presence in the historical demo is not completion proof.

| Slice | Primary ownership |
|---|---|
| P1 | `src/model.rs`, `src/config.rs`, `src/secrets.rs`, `src/public_error.rs`, `src/service.rs` |
| P2 | `src/service.rs`, `src/ui/{adapter,runtime,model}.rs`, controller/service tests |
| P3 | typed driver/resource traits, `PreparedMySqlRequest`, CLI/resource contracts |
| P4 | `src/drivers/mysql_catalog.rs`, catalog service/UI/CLI/live tests |
| P5 | Redis keyspace/TLS service/UI/CLI/live tests |
| P6 | native form/editor/explorer/result/recovery UI and RawInput/AccessKit tests |
| P7 | export encoders/filesystem policy and golden/failpoint tests |
| P8 | verification scripts/workflows/package/manifest/receipt contracts |
| P9 | reviewed merge, preview, tap, Brew install, installed proof |

The exact expected file map is maintained per slice in
`docs/usable-mvp/plan.md`. Do not claim a path exists or a capability is ready
without checking the tree and its trace evidence.

## Licensing boundary

dbotter is Apache-2.0. DBeaver is behavior/product research only. Do not copy
DBeaver PRO Redis or MongoDB implementation code.
