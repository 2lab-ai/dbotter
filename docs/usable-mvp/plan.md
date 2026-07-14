# dbotter usable MVP — implementation and verification plan

Status: follow-up review candidate; planning artifacts only. The third-pass findings are incorporated and D1/D2 remain accepted. Production implementation is blocked only until the remediation UX/product and architecture/security reviewers return no blocking finding. Branch `agent/usable-mvp`, worktree `.worktrees/agent-usable-mvp`.

## 1. Control rules

1. No production code changes before both remediation reviewers approve `spec.md`, `trace.md`, and this plan.
2. Their verdict is the only remaining authorization gate. D1, D2, credential/config shape, limits, error vocabulary, export formats, bundle identity, and release chain are settled decisions.
3. Immediately after approval, P0 reconciles `01-spec.md`, `02-architecture.md`, `03-traces.md`, `04-patch-plan.md`, and `docs/release/{spec,trace}.md` before production code.
4. `trace.md` is the cross-layer source of truth and task ledger. Each behavior starts with a failing contract derived from trace input/flow/error/output.
5. P0 → P1 → P2 → P3 is strictly sequential. P4 and P5 may proceed in parallel only after P3 is integrated. Later slices integrate in listed order.
6. Config mutations remain serialized; network work uses the bounded controller; control commands have a dedicated lane.
7. `ConnectionProfile` remains non-secret. Version 1 is read-only input; the first confirmed mutation creates the fixed backup and writes version 2. Six-field identity and three-field config compatibility use separate commands. Sensitive request types have manual redacted Debug/no Serialize; dbotter zeroizes only its own buffers/final secret Arc.
8. MySQL CATALOG and Redis KEYSPACE_BROWSE remain planned/absent until their own mandatory live contract is green in the same reviewed commit as the ready flag.
9. Every implementation slice receives independent code/security/UX review appropriate to its risk. These are defect checks inside approved scope, not new product-choice gates.
10. Completion means reviewed merge, green CI/live/source receipt, per-arch preview publication, explicit tap bump, Homebrew upgrade, installed CLI browse/inspect, installed AX journey, and final receipt. No stable release.

## 2. Follow-up-review gate

### G0 — required remediation verdicts

The UX/product reviewer checks:

- first-run, explicit credential modes and every SessionCredentialIntent, DraftId/ConnectionId-correct Create versus Edit, retained Replace Test→Save & Connect, active Delete warning/Unknown, connection truth, exact MySQL scanner/prepared-only target, Execute-limit controls, explorer pagination, every clipboard scalar/export, exhaustive recovery, split CA/Host TLS recovery, restart, disclosure boundaries, keyboard/AccessKit/contrast, and installed golden-journey testability;
- no label-only substitute for any P0 action and no query-history/recent-statements scope;
- stable AX identifiers and numerical accessibility gates are sufficient for deterministic automation.

The architecture/security reviewer checks:

- credential/update/Session-intent matrices, `Arc<SessionSecret>` ownership, manual Debug/no-Serialize and intended-value disclosure boundaries, and post-restart behavior;
- version-1 read-only normalization, fixed no-replace backup, version-2 first mutation, frozen current-v1-reader rejection, `load_path`, commit point, parent fsync, CommittedDurabilityUnknown reconciliation, failpoints, and single-process writer boundary;
- profile/session generations, reload diff/Config uncertain barrier, tombstones, exact correlation tuples/cache table/tagged task scopes, controller limits, control priority, cancel/timeout compare-remove races, event-lane cleanup, queue drain, panic handling, and async-versus-blocking shutdown;
- typed Execution/Catalog/Keyspace seams and separate capability bits;
- exact MySQL comment/quote/SQL-mode scanner plus server-prepared-only execution despite handshake capability, Redis paging/classifier, 4 MiB metadata/retained/input limits, transient-allocation caveats, disjoint Redis CA-vs-host validation/TLS/authentication/no downgrade;
- total `OperationKind × PublicSummary -> NonEmpty<RecoveryAction>`, exact clipboard scalar/TSV/export/no-clobber policy and external-only digests, separate identity/config-contract, bundle versions, installed app-path/process proof, tap inputs/versioning, and typed rollback preflight.

Both reviewers must return `NO BLOCKER` or cite exact sections. Blocking findings are corrected in all three artifacts and rechecked. Once both return `NO BLOCKER`, implementation starts without another decision request.

## 3. Dependency order

```text
P0 approved docs + legacy/release reconciliation
  |
  v
P1 config/profile/credential/error foundation
  |
  v
P2 generations/session cache/operation controller
  |
  v
P3 typed resource seams + bounded result/CLI contracts
  |\
  | +------------------+
  v                    v
P4 MySQL catalog    P5 Redis browser + verified TLS
  \                    /
   +---------+----------+
             v
P6 profile-scoped native UI + RawInput/AccessKit
             |
             v
P7 copy + streaming atomic export
             |
             v
P8 live receipts + workflows + per-arch bundles/tap contract
             |
             v
P9 review/merge/preview/Brew/installed golden receipt
```

P1 does not spawn the concurrent runtime before P2. P2 does not invent resource methods before P3. P4/P5 consume P3's merged typed seams. P6 binds real core outcomes, not divergent UI mocks. P8 cannot weaken tests added by any earlier slice.

## 4. Dependency-ordered slices

### P0 — approve and reconcile the contract

Purpose: establish one non-contradictory source of truth.

Files:

- `docs/usable-mvp/spec.md`, `docs/usable-mvp/trace.md`, `docs/usable-mvp/plan.md`
- after G0 only: `01-spec.md`, `02-architecture.md`, `03-traces.md`, `04-patch-plan.md`
- after G0 only: `docs/release/spec.md`, `docs/release/trace.md`, `README.md`

Actions:

1. Record both remediation-review no-blocker verdicts.
2. Reconcile legacy “deferred” claims for credentials, catalog, cancellation, delete/disconnect, export, and distribution; remove every additive-version-1 or old-reader-compatibility claim.
3. Add the exact approved vocabulary, Create ConnectionId recovery, tagged task scopes, Execute-limit ids, prepared-only MySQL path, D1/D2 resolution, v1-read/v2-write migration and config-contract outcomes, Session intents, scanner/copy/recovery/disclosure contracts, installed identity/app-path proof, and task ledger to authoritative docs.
4. Run a document cross-reference check: each U0–U9 maps to T0–T10, a slice, a RED contract, and a receipt assertion.

Acceptance:

- no unresolved reviewer decision or contradictory deferred claim;
- every behavior has one trace owner and one planned contract source;
- no production source diff exists before this acceptance.

Rollback: documentation-only revert.

### P1 — config, profile, credential, and public-error foundation

Purpose: make durable profile mutations and secret ownership exact before adding concurrency.

Expected files:

- `src/model.rs` — `CredentialMode`, `SessionCredentialIntent`, lifecycle ids/types, version-2 `redis_tls.ca_file`, `OperationKind`/closed `ProfileFieldId` including `ConnectionId`/`PublicCode` wire shapes, redacted manual Debug/no-Serialize boundaries.
- `src/config.rs` — read-only version-1 normalization, current version-2 wire, frozen-reader fixture, separate create/update/delete, migration backup, serialized writer, atomic commit outcomes, parent fsync, failpoints.
- `src/secrets.rs` — `SessionSecret`, `SessionSecretUpdate`, exact `HashMap<ProfileId, Arc<SessionSecret>>` store, final-Arc zeroization, mode validation.
- `src/public_error.rs` (new) — closed summaries/actions including `EditDraft(DraftId, ProfileFieldId)` and Execute-only `FocusExecuteLimits(ProfileId)`, exact TLS/scanner/Create codes, exhaustive NonEmpty recovery conversion.
- `src/service.rs` — exact-path construction, mutation orchestration, direct side-effect-free draft-test seam.
- `src/cli.rs`, `src/main.rs` — global `--config`, six-field identity, separate exact `config-contract`, safe error output.
- `src/lib.rs`; UI profile form types only where necessary to compile the contract.
- `tests/config_contract.rs`, `tests/profile_contract.rs`, `tests/restart_contract.rs`, `tests/public_error_contract.rs` (new), plus existing service/contracts tests.

RED contracts first:

1. Explicit `--config` wins over environment/default and every service mutation calls `load_path` on that exact path.
2. Version 1 loads read-only: absent mode normalizes to Environment when `secret_env` exists and None otherwise; no startup write occurs. The first confirmed mutation writes version 2. A frozen v1 reader rejects before network. Separately, `dbotter config-contract --format json` returns exactly `{read_versions:[1,2],write_version:2,migration_backup_suffix:".v1.bak"}` with no service construction or identity fields.
3. Before that mutation, confirmation creates exact original bytes at fixed `<config>.v1.bak` through a 0600 same-directory temp, file fsync, atomic no-replace rename, and parent fsync. Cancel changes nothing; an existing non-identical backup or backup failure aborts; successful migration exposes `RevealMigrationBackup`.
4. Redis defaults Disabled; Preferred remains invalid/edit-required. Required+blank uses OS roots; configured CA must be readable valid PEM; Disabled hides/clears it. CA-invalid/untrusted codes map only to the current identity domain's RedisCaFile edit and hostname codes only to its Host edit: `EditDraft(DraftId, _)` for TestDraft, `EditProfile(ProfileId, _)` for saved operations. Mapping is compile/table tested and neither falls back.
5. Create carries `(DraftId, OperationId)`. Auto-slug/suffix allocation is deterministic and a collision chooses the next suffix without error. An explicit collision returns `PROFILE_ID_CONFLICT` + `EditDraft(draft, ConnectionId)`, focuses `profile.connection_id`, never constructs EditProfile/ProfileId recovery, and never overwrites. Edit requires immutable id + expected generation and cannot recreate missing data.
6. Complete `CredentialMode × SessionCredentialIntent × SessionSecretUpdate` matrix: existing Session+Arc defaults KeepCurrent/show “set”; Create/enter/restart-no-Arc disable Keep and default Replace with Forget available. Stable ids are `profile.credential.session.keep`, `profile.credential.session.replace`, and `profile.credential.session.forget`. Save maps Keep→Keep, Replace→Replace, Forget→Clear exactly.
7. Keep Test clones the current Arc under lock then unlocks and is read-only; Replace Test makes one operation copy while retaining the `Zeroizing<String>` buffer; Forget/no secret returns CredentialRequired + `EditDraft(draft, SessionCredential)` before network. Only accepted Replace Save moves/zeroizes; Keep moves nothing, Forget hides/clears input; connection/intent edits invalidate success and a full queue preserves the draft.
8. `TestDraftConnection` create/ping/close succeeds or fails without config, cache, secret-store, saved connection-state, generation, or workspace mutation.
9. Atomic backup/main create/edit/delete fail at each failpoint. Pre-rename is NotCommitted with temp cleanup; rename is commit point; parent-fsync failure is CommittedDurabilityUnknown and reloads/reconciles exact bytes.
10. Concurrent writers inside one process serialize/reload and preserve unrelated profiles. `load_path` captures a destination fingerprint and a pre-rename recheck detects an injected external-content conflict and returns ReloadConfiguration; tests explicitly retain the post-recheck race as unsupported and do not claim multi-process safety.
11. `recovery_for(OperationKind, PublicSummary, PublicCode, SafeContext)` covers every reachable summary with `NonEmpty<RecoveryAction>` and rejects every other pair. Closed actions include `EditDraft(DraftId, ProfileFieldId)` separately from `EditProfile(ProfileId, ProfileFieldId)`, plus Execute-only `FocusExecuteLimits(ProfileId)`. Draft-test and Create SafeContext each have only `(DraftId, OperationId)` and cannot produce a saved-profile action or stored recipe. All actions dispatch real safe-id commands; Reveal actions derive paths, InternalFailure can RestartApplication, and mutating Execute never gets Retry.
12. Sensitive requests have redacted manual Debug and compile-fail no-Serialize proofs. Secrets/backend prose are absent everywhere public. User SQL/Redis/result/key/CA/export values are absent from Debug/log/error/receipt and unrelated UI/AX nodes but later must be present in their intended value nodes; secret AX values stay protected.

The recovery RED matrix explicitly enumerates every summary: InvalidInput, CredentialRequired, AuthenticationFailed, PermissionDenied, NetworkUnavailable, TlsVerificationFailed with CA/Host code branches, OperationTimedOut, SyntaxRejected, ConstraintRejected, UnsupportedFeature, OperationCancelled, ResourceBusy, ResourceStale, ConfigWriteNotCommitted, CommittedDurabilityUnknown for config/export, ExportFailed, and InternalFailure. Coverage fails if any enum variant or reachable OperationKind pair lacks an action, or if an unreachable pair converts.

Its expected dispatch rows are:

| Summary | Operation context → NonEmpty action |
|---|---|
| InvalidInput / CreateProfile | `EditDraft(draft, exact field)`; `PROFILE_ID_CONFLICT` → ConnectionId; auto collision is suffix allocation |
| InvalidInput / TestDraftConnection | `EditDraft(draft, exact field)` |
| InvalidInput / config/update/delete/connect | global Reload/Dismiss or typed profile Edit according to exact identity/code |
| InvalidInput / Execute statement or target | FocusEditor |
| InvalidInput / Execute row-limit or timeout input | `FocusExecuteLimits(profile)` → `editor.row_limit` or `editor.timeout` |
| InvalidInput / BrowseMySql | ClearCatalog or Dismiss |
| InvalidInput / BrowseRedis or InspectRedis | RestartRedisScan or Dismiss as typed by kind |
| InvalidInput / ExportResult | ChooseExportDestination |
| CredentialRequired / TestDraftConnection | exactly one of `EditDraft(draft, SessionCredential)`, `EditDraft(draft, CredentialEnvironmentName)`, or `EditDraft(draft, Username)` by code; Forget/no-secret selects SessionCredential |
| CredentialRequired / saved network | OpenCredentialPrompt + `EditProfile(profile, SessionCredential)` |
| AuthenticationFailed / TestDraftConnection | exactly one of `EditDraft(draft, SessionCredential)`, `EditDraft(draft, CredentialEnvironmentName)`, or `EditDraft(draft, Username)` by code |
| AuthenticationFailed / saved network | Session→prompt; Environment/username→typed profile Edit; idempotent Retry only after state change |
| PermissionDenied / TestDraftConnection | exactly one of `EditDraft(draft, Username)` or `EditDraft(draft, Database)` by code + Dismiss |
| PermissionDenied / saved network or Execute | profile Username/Database Edit + Dismiss; Execute→FocusEditor + Dismiss |
| NetworkUnavailable / TestDraftConnection | `EditDraft(draft, Host)` + Dismiss |
| NetworkUnavailable / saved network | profile Host Edit + Reconnect; idempotent non-mutation may Retry, mutation never does |
| TlsVerificationFailed CA code / TestDraftConnection | `EditDraft(draft, RedisCaFile)`; no fallback |
| TlsVerificationFailed hostname code / TestDraftConnection | `EditDraft(draft, Host)`; preserve CA; no fallback |
| TlsVerificationFailed / saved network | CA code→`EditProfile(profile, RedisCaFile)`; hostname→`EditProfile(profile, Host)`; no fallback |
| OperationTimedOut / TestDraftConnection | `EditDraft(draft, Host)` + Dismiss |
| OperationTimedOut / ConnectProfile or ReconnectProfile | `EditProfile(profile, Host)` + Reconnect + Dismiss |
| OperationTimedOut / Execute | `FocusExecuteLimits(profile)` → `editor.timeout` + Reconnect; mutation no Retry |
| OperationTimedOut / Browse or Inspect | exact generation-checked idempotent Retry with auto-connect or Reconnect + Dismiss |
| SyntaxRejected | FocusEditor only |
| ConstraintRejected | FocusEditor + Dismiss; no Retry |
| UnsupportedFeature / TestDraftConnection | legacy TLS→`EditDraft(draft, RedisTlsMode)` |
| UnsupportedFeature / other | legacy saved Connect→`EditProfile(profile, RedisTlsMode)`; prepared-unsupported MySQL Execute→FocusEditor + Dismiss with no text fallback; other execute/browse→Dismiss |
| OperationCancelled / TestDraftConnection | DismissError only |
| OperationCancelled / saved network or export | network→Reconnect + Dismiss; export→Choose/Reveal destination + Dismiss |
| ResourceBusy / TestDraftConnection with safe active id | CancelOperation(active id) + Dismiss |
| ResourceBusy / TestDraftConnection without safe active id | DismissError(rejected id) |
| ResourceBusy / other kind with/without safe active id | CancelOperation(active id) + Dismiss, otherwise DismissError(rejected id) |
| ResourceStale | config→Reload; idempotent browse/connect→exact Retry; Execute→Reload + Focus, no Retry |
| ConfigWriteNotCommitted | Reload; migration may RevealMigrationBackup |
| CommittedDurabilityUnknown | config→Reload/(migration backup reveal); export→RevealExportDestination |
| ExportFailed | ChooseExportDestination; Reveal only when committed destination exists |
| InternalFailure / TestDraftConnection | RestartApplication + Dismiss |
| InternalFailure / every other reachable kind | RestartApplication + Dismiss; config also Reload |

The exact OperationKind enum is LoadConfiguration, ReloadConfiguration, MigrateConfiguration, CreateProfile, UpdateProfile, DeleteProfile, TestDraftConnection, ConnectProfile, DisconnectProfile, ReconnectProfile, ExecuteRead, ExecuteMutation, BrowseMySql, BrowseRedis, InspectRedis, ExportResult, and ShutdownRuntime. Table-driven tests enumerate its Cartesian product with PublicSummary and require either the listed NonEmpty result or explicit unreachable rejection. The draft-test slice separately asserts every reachable row carries only its originating DraftId/OperationId-safe action and has no stored operation recipe. The Create slice asserts field errors use only EditDraft, ConnectionId is exact for explicit collision, auto-suffix collisions do not error, and other outcomes use only safe global actions.

Implementation notes:

- Version 2 is current. Version 1 is only an in-memory normalization input; it is never rewritten until the confirmed first mutation and is never claimed readable by the old binary.
- Backup/main temp is same-directory `create_new`, 0600, flushed and file-fsynced. Rename commits; parent directory is fsynced. Mutation result distinguishes NotCommitted, Committed, and CommittedDurabilityUnknown.
- `SessionSecretUpdate` is applied only after observed commit and is never part of a serializable event.
- P1 draft testing may call the service directly under hermetic tests; runtime registration and global limits arrive in P2.

Review/acceptance:

- security reviewer traces every secret owner/drop/format and failpoint state;
- all P1 contracts green, formatting/clippy/tests green, version 1 read-only normalization green, no production panic/todo/unwrap/expect.

Rollback: only a source whose separate config-contract command exactly matches manifest/release/tap preflight may publish. A missing/mismatched source is rejected, and the installer/rollback wrapper owns backup-runbook presentation.

Direct old-binary invocation only fails closed with `UnsupportedVersion(2)`.

### P2 — profile/session generations and bounded operation controller

Purpose: make network work responsive without stale-state or cleanup races.

Expected files:

- `Cargo.toml`, `Cargo.lock` — cancellation primitive (`tokio-util` or reviewed equivalent).
- `src/service.rs` — generation-tagged cache entries, connect/disconnect/reconnect, compare-and-remove eviction, `SessionDisposition`.
- `src/ui/adapter.rs` — separate bounded work/mutation/control ports and correlated events.
- `src/ui/runtime.rs` — exact `RegisteredTask { operation_id, scope, cancel, join }`/tagged `TaskScope`, per-profile/global permits, priority loop, cancellation/timeouts, task join/shutdown.
- `src/ui/model.rs` — active generations, tombstones, pending terminal fold, exact connection states.
- runtime/model tests and `tests/service_contract.rs`, `tests/controller_contract.rs` (new).

The required registry shape is:

```rust
struct RegisteredTask {
    operation_id: OperationId,
    scope: TaskScope,
    cancel: CancellationToken,
    join: JoinHandle,
}

enum TaskScope {
    Profile { profile_id: ProfileId, profile_generation: ProfileGeneration, session_generation: Option<SessionGeneration> },
    Draft { draft_id: DraftId },
    Export { result_id: ResultId },
    Global,
}
```

RED contracts first:

1. Process-monotonic ProfileGeneration for load/create/edit/delete; tombstone rejects late delete events and recreation receives a greater generation.
2. SessionGeneration is unique per new connect; cache hit requires profile generation + fingerprint; every eviction compare-matches profile/session generation.
3. Every profile command/event carries `(ProfileId, ProfileGeneration, OperationId)`; draft work carries `(DraftId, OperationId)`, export carries `(ResultId, OperationId)`, and global load/shutdown uses OperationId only. The tagged registry mirrors those exact scopes; only Profile contains generation fields. Folds never borrow the currently selected profile.
4. `ReloadConfiguration` covers an id-keyed exact diff: unchanged preserves generation/cache/secret/workspace; added starts fresh; changed fences then cancel/join/evict and clears old Session secret; removed tombstones then clears; re-add exceeds the tombstone. Unreadable/ambiguous reload enters Config uncertain, fences/cancels/clears all, and enables only Reload/Shutdown.
5. Edit/Delete never cancel before the config commit point. A committed edit/delete publishes its new generation/tombstone fence, then cancels/joins old work. Active Delete confirmation names static OperationKind and server-may-continue; cancel has no side effect; committed state reports Unknown. Runtime-neutral Edit retags an idle handle; active/connection-affecting edits evict exactly.
6. One active network operation/profile and four global; work/mutation/control/event channel capacities are 32/16/16/128; excess returns Busy and spawns nothing. Draft tests count globally by DraftId, with no profile/session generation stored in their scope.
7. A saturated work queue/long fake execute cannot delay Cancel or Disconnect on the coalesced control lane, Shutdown on its watch signal, RefreshProfiles on the ordered mutation lane, or another profile's allowed operation.
8. Cancel and timeout emit exactly one distinct terminal event, evict the exact used session, and cannot evict a concurrently replaced generation. Late success is ignored. Syntax/constraint/permission keeps a proven healthy session; network/protocol/uncertain failure evicts per the exact table.
9. Secret lookup clones the Arc under the store lock and releases it before await. Table tests construct and clean each Profile/Draft/Export/Global registry variant and reject optional-field hybrids. Terminal registry/permit cleanup precedes event delivery, so full/closed UI lanes cannot strand a task. Disconnect/Edit/Delete cancel and join target work without affecting unrelated profiles.
10. Shutdown closes intake and drains queued commands including secret Arcs. The two-second grace/abort applies only to abortable async network tasks. Config mutation is joined to its classified outcome; blocking export cooperatively checks each row/chunk and Shutdown waits for actual worker/temp cleanup, never trusting `spawn_blocking::abort`.
11. Async/export panic or JoinError maps to static InternalFailure after exact-session/registry/permit/temp cleanup and attempts at most one terminal event. Barriers cover full event lane, panic/blocked encoder, mutation/network/export in flight, queue drain, permit leaks, and no detached tasks.
12. No lock is held across await; Loom-style/model race tests or deterministic barriers cover connect/cancel/replacement/reload interleavings.

Review/acceptance:

- concurrency reviewer checks task ownership, channel capacity/priority, all exact state/cache rows, race barriers, and shutdown leak instrumentation;
- P2 does not add catalog/keyspace behavior or ready flags.

Rollback: no config change. Reverting P2 returns to the P1 runtime while the version-2 config and P1 contracts remain valid.

### P3 — typed prepared execution/resource seams, retained snapshots, and headless command contract

Purpose: define backend-specific browsing and make the only MySQL user-text seam server-prepared-only.

Expected files:

- `src/model.rs` — tagged Catalog/Redis requests/pages, bounds, raw key id, `ResultSnapshot`, provenance and truncation types.
- `src/drivers/mod.rs` — `ConnectionPing`, `MySqlPreparedExecution`, `RedisExecution`, `CatalogBrowser`, `KeyspaceBrowser`, `ConnectedResources`, `CATALOG`, `KEYSPACE_BROWSE`.
- `src/drivers/mysql.rs` — remove any current `sqlx::raw_sql`/`Executor::execute(&str)`/`COM_QUERY` user-text path and every unsupported-prepared fallback; implement only `COM_STMT_PREPARE` → `COM_STMT_EXECUTE` for `PreparedMySqlRequest`.
- `src/service.rs` — typed validation/dispatch, `PreparedMySqlRequest`, and static prepared-unsupported capability errors.
- `src/cli.rs` — exact `browse mysql …`, `browse redis keys`, and `inspect redis key` parsers/JSON schema.
- `src/ui/adapter.rs`, `src/ui/runtime.rs`, `src/ui/model.rs` — typed commands/events/state without rendered widgets.
- `tests/resource_contract.rs`, `tests/mysql_prepared_contract.rs` (new), CLI parser/snapshot/service/source-contract tests.

RED contracts first:

1. MySQL user text enters only `MySqlPreparedExecution::execute_prepared(PreparedMySqlRequest)`; SQLx 0.9 handshake `CLIENT_MULTI_STATEMENTS` is acknowledged but never used as the safety proof. MySQL browsing accepts only CatalogRequest; Redis accepts only its typed execute/scan/inspect seams; MongoDB and mismatches fail before connector/resource invocation.
2. CATALOG and KEYSPACE_BROWSE serialize/report independently; neither flips ready.
3. The exact profile tuple `(ProfileId, ProfileGeneration, OperationId)`, draft tuple `(DraftId, OperationId)`, export tuple `(ResultId, OperationId)`, and page identity survive CLI/UI → service → typed trait → outcome without consulting selection state.
4. Bounds validate before network. Failed refresh retains old page and marks it stale.
5. ResultSnapshot enforces retained row/cell/byte/depth caps after decoding and records transient-allocation qualification; tests do not claim whole-frame prevention.
6. Headless CLI command forms and JSON outputs are stable, use global `--config`, and invoke the same service seam as GUI.
7. Compile-fail tests prove `UiCommand`, `ExecuteRequest`, Redis scan/inspect requests, and export commands do not implement Serialize; sentinel manual Debug/log tests prove redaction.
8. Source/trait contracts fail if user SQL can reach `sqlx::raw_sql`, `Executor::execute(&str)`, `COM_QUERY`, or any fallback after prepared rejection. Prepared-unsupported maps to static UnsupportedFeature + FocusEditor/DismissError, resubmits nothing, and retains only a proven-healthy session. Static/bound catalog statements also use prepared execution.

Review/acceptance:

- architecture reviewer confirms no generic `load_resource(String)`, no Catalog/Redis coercion into user `QueryResult`/SQL, and no raw MySQL user-text escape hatch;
- P3 is merged before either driver slice begins.

Rollback: additive config-neutral types; capabilities remain planned.

### P4 — lazy paginated MySQL catalog

Purpose: browse schema→relation→column pages with reachable truncation recovery.

Expected files:

- `src/drivers/mysql.rs`, `src/drivers/mysql_catalog.rs` (new), `src/drivers/mod.rs`.
- `src/model.rs`, `src/service.rs` for final opaque token/mapping details.
- `src/ui/explorer.rs` (new) model/controller seam; CLI dispatch from P3.
- `tests/mysql_catalog_contract.rs`, `tests/live_mysql.rs`; Compose restricted-user/catalog fixtures.

RED contracts first:

1. Schemas, relations, and columns are distinct static/bound prepared information_schema queries; expansion never eager-loads descendants and no catalog statement uses text-protocol execution.
2. Page default 50/max 200 and timeout default 5/max 30 seconds validate locally. Each query requests `page_size + 1`, retains at most page_size in deterministic binary keyset order, and emits an opaque token from the last retained key only when the extra row exists; configured database scopes initial schemas.
3. Per-profile retained caps are 200 schemas, 2,000 relations, 10,000 columns, 512 columns/relation, and 4 MiB UTF-8 metadata bytes across names/type strings. Every next token reaches Load more; any count/byte cap reaches Clear catalog + prefix filter.
4. Hermetic injected-driver Permission preserves/marks a stale prior page; a successful empty page remains empty and is never fabricated as Permission.
5. The live restricted user proves allowed schemas/relations/columns visible and forbidden schema absent without calling omission a Permission error. A separate unauthorized-default-database profile plus denied check/execute produces a real static Permission result.
6. Bound schema/relation predicates and correct backtick escaping, including embedded backtick; SELECT template is bounded to 500 and never auto-executes.
7. Mandatory live fixture proves >1 page, table/view, >1 column page, ordering, count+4-MiB cap/filter recovery, stale behavior, restricted visibility, separate unauthorized Permission, and headless CLI JSON. Missing fixture is failure.

Capability gate: CATALOG becomes ready only with green hermetic + mandatory live tests in the same reviewed commit.

Review/rollback:

- MySQL reviewer inspects binding, keyset token integrity, permissions, identifier quoting, and retained caps.
- Regression rollback reverts implementation and ready flag together.

### P5 — Redis SCAN/inspect and verified Required TLS

Purpose: browse/inspect real keys with explicit semantics and never downgrade transport.

Expected files:

- `Cargo.toml`, `Cargo.lock` — redis-rs 1.3 `tokio-rustls-comp`.
- `src/drivers/redis.rs`, `src/drivers/redis_browser.rs` (new), `src/drivers/mod.rs`.
- `src/model.rs`, `src/service.rs`, `src/ui/explorer.rs`, `src/ui/profile_form.rs`; CLI dispatch from P3.
- `tests/redis_contract.rs`, `tests/live_redis.rs`.
- `docker-compose.yml`, `scripts/generate-redis-tls-fixture.sh` (new), and ignored `artifacts/redis-tls/` output containing a generated test CA and server cert/key with SAN `localhost`.

RED contracts first:

1. New Redis default Disabled; UI/wire validation exposes Disabled/Required; legacy Preferred fails before network with typed RedisTlsMode edit. Required shows CA field/picker; blank uses OS roots, nonblank must be readable valid PEM, and Disabled clears/hides/rejects a direct value.
2. Required maps to verified `TcpTls` with no Tcp retry. Invalid PEM/untrusted issuer/wrong CA emits CA-only code/action and focuses `profile.redis_tls.ca_file`; hostname mismatch emits `TLS_HOSTNAME_MISMATCH`, focuses `profile.host`, and preserves CA. PublicCode/action cross-mapping is impossible.
3. The live runner starts authenticated plaintext and hostname-valid TLS fixtures. Correct CA+`localhost` succeeds. Wrong CA proves CA-only recovery. Wrong host proves Host-only recovery, changes only host to `localhost`, retains CA, succeeds, and leaves plaintext-fallback counter zero.
4. LiteralPrefix escapes `* ? [ ] \\` then appends `*`; Glob is explicit/validated and unchanged; neither path uses KEYS.
5. SCAN COUNT default 100/max 1,000 is handled as a hint: zero/fewer/more reply counts, cursor zero, duplicate raw-byte dedupe, no total/snapshot claim.
6. Raw binary key round-trip uses base64 CLI identity and never lossy display. Retain 10,000 keys or 8 MiB/profile, 4 KiB/key; oversize keys are counted unretained; Restart scan/filter recovery is reachable.
7. String uses STRLEN+GETRANGE 0..65535. Hash/list/set/zset/stream use the exact representative commands in T6. TTL persistent/expiring/missing and disappearance/type races are typed.
8. Preview retains 100 items, 1 MiB, 64 KiB/cell, depth 8. Generic execute accepts at most 65,536 UTF-8 bytes, 1,024 shell tokens, and 16 KiB/token, then retains 10,000 cells/8 MiB/64 KiB per cell/depth 8. Tests and UI disclose redis-rs whole-frame transient allocation and COUNT/element-size qualifications.
9. Before session acquisition, an ASCII-case-insensitive closed classifier rejects `SUBSCRIBE`, `PSUBSCRIBE`, `SSUBSCRIBE`, `UNSUBSCRIBE`, `PUNSUBSCRIBE`, `SUNSUBSCRIBE`, `MONITOR`, `SYNC`, `PSYNC`, `REPLCONF`, `WAIT`, `WAITAOF`, every `BL*`, explicit `BRPOP`/`BRPOPLPUSH`/`BZPOPMIN`/`BZPOPMAX`/`BZMPOP`, and `XREAD`/`XREADGROUP` with BLOCK before STREAMS while allowing a key named BLOCK after STREAMS; it never consults backend COMMAND metadata.
10. Mandatory live fixture proves >1 SCAN page, binary/oversize key behavior, all representative types, string truncation, mutation/readback, split TLS code/action/focus/recovery, and Redis ACL/requirepass correct/wrong Session plus Environment Available/Missing/Empty on plaintext and TLS. Missing fixture is failure.

Capability gate: KEYSPACE_BROWSE becomes ready only with green hermetic + mandatory live tests in the same reviewed commit.

Review/rollback:

- Redis/security reviewer inspects command construction, raw identity, memory claims, disjoint CA/Host codes and focus ids, CA preservation on host fix, and proof that TLS failure cannot reach TCP.
- Regression rollback reverts implementation/TLS feature/ready flag together; it never maps Required to Disabled.

### P6 — profile-scoped native UI, execution UX, recovery, and accessibility

Purpose: bind U0–U6/U8/U9 to real P1–P5 outcomes with deterministic native interaction.

Expected files:

- `src/ui/model.rs` — profile-generation workspaces, connection/error states, tombstones, result provenance.
- `src/ui/profile_form.rs` — Create/Edit, `profile.connection_id`, all Session intents/AX ids, Keep/Replace/Forget Test+Save, environment availability, typed Redis CA/Host recovery.
- `src/ui/editor.rs` (new) — exact fixed-policy MySQL scanner, Redis-line extraction, `editor.target`, Execute-only `editor.row_limit`/`editor.timeout`, and shortcut.
- `src/ui/explorer.rs`, `src/ui/result_view.rs` (new), `src/ui/app.rs` — responsive layout, lifecycle/browser/result states and recovery buttons.
- `src/ui/adapter.rs`, `src/ui/runtime.rs` — final operation/control/retry wiring.
- `tests/ui_raw_input.rs`, `tests/ui_accesskit.rs`, `tests/ui_contrast.rs` (new) and model tests.

RED contracts first:

1. First-run/Create/Edit plus all Session intents: occupied explicit Create id proves `PROFILE_ID_CONFLICT` → `EditDraft(draft, ConnectionId)`/`profile.connection_id` with no saved-profile recovery; auto collision proves next-suffix success. Existing Arc defaults Keep/show “set”; Create/enter/restart-no-Arc disable Keep/default Replace/allow Forget. RawInput proves Keep read-only clone/unlock, Replace buffer retention→move/zeroize, Forget hidden/CredentialRequired + `EditDraft(draft, SessionCredential)`/no-network/Clear, exact Save mapping, and all stable intent ids.
2. Every exact connection-state/cache outcome renders the right non-color state and control availability; Session prompt and Environment Available/Missing/Empty after restart.
3. Profile switch restores only that generation's editor/pending/result/error; query history/recent statements do not exist. Failure/cancel keeps prior result visibly historical.
4. A selection wins/no fallback and unambiguous multiple statements reject locally. The pure MySQL RED table covers `#` always-comment, conditional `--`, ordinary/version/hint block comments, doubled/default-backslash delimiters, double quotes under both ANSI_QUOTES modes, odd-backslash `AMBIGUOUS_SQL_MODE`→explicit selection, unterminated local rejection, trailing/gap/UTF-8; every accepted selection/current target still enters the prepared-only trait despite SQLx negotiating `CLIENT_MULTI_STATEMENTS`. Redis remains physical-line shell parsing.
5. Lazy MySQL and Redis explorer Load more/stale/truncation/filter/clear/restart recovery; raw Redis display is never identity.
6. Profile A→B updates `editor.target` before B Execute and creates B's exact `(ProfileId, ProfileGeneration, OperationId)` in one RawInput frame sequence. Cmd/Ctrl+Enter submits once; invalid Execute bounds dispatch `FocusExecuteLimits` to `editor.row_limit` or `editor.timeout`; timeout does the same then reconnects. Browser/inspect invalid/timeout paths use only their typed Clear/Restart/Dismiss or generation-checked recipe/reconnect actions. Busy/Cancel remain responsive.
7. Active-operation Delete confirmation names the static OperationKind and says the server may continue. Opening/cancelling has no side effect; confirmed commit publishes tombstone then cancel/join/evict and renders Unknown.
8. Every reachable `OperationKind × PublicSummary` row returns NonEmpty real actions; unreachable pairs reject. The draft-test Cartesian slice covers InvalidInput, CredentialRequired, AuthenticationFailed, PermissionDenied, NetworkUnavailable, both TLS codes, OperationTimedOut, legacy UnsupportedFeature, OperationCancelled, both ResourceBusy contexts, and InternalFailure with exact `EditDraft(DraftId, field)`/OperationId-safe dispatch and no stored recipe or saved-profile action. The Create slice proves DraftId-only field actions and global-only config outcomes. CA codes focus only CA, hostname code only `profile.host`, Execute limit focus never appears for Connect/Browse/Inspect, every other summary has its table action, Reveal carries no path, and mutating Execute never receives Retry.
9. The first RED artifact is a minimal compile-only egui 0.35 harness using `Context::run_ui(RawInput, …)`. All headless tests call `enable_accesskit()`, run real frames, and inspect `FullOutput.platform_output.accesskit_update`; no framework-conditional omissions.
10. AccessKit asserts `profile.connection_id`, `editor.target`, `editor.row_limit`, `editor.timeout`, `profile.host`, CA controls, all Session intents, active Delete warning, role/name/focus/order/enabled, and author id→AXIdentifier; palette ratios remain numerical.
11. Secrets/backend prose are absent everywhere public. SQL/Redis/result/key/CA/export values must appear in exactly the intended rendered field and AX value node, with absence from names/descriptions/live announcements/unrelated nodes and Debug/log/error/receipt. Secret input stays masked/protected and never exposes its AX value.

Required MySQL RED cases:

| Case | Expected |
|---|---|
| quoted `;`, doubled delimiter, default backslash escape, double quotes under either ANSI_QUOTES mode | protected one target |
| `SELECT 1# ;` + newline | `#` comment semicolon opaque |
| `SELECT 1--1;` / `SELECT 1-- comment ;` + newline | not-comment / whitespace-triggered comment |
| `SELECT 1 /* ; */;` / standalone `/* ordinary ; */` | containing target / `NO_CURRENT_STATEMENT` |
| standalone `/*!40101 SET @x=';' */;` | executable version-comment target |
| `SELECT /*+ hint; */ 1;` / standalone `/*+ hint; */` | attached target / `NO_CURRENT_STATEMENT` |
| unterminated quote/backtick/block comment | local `UNTERMINATED_SQL_TOKEN`, no session |
| odd backslash before `'`/`"` in caret target | `AMBIGUOUS_SQL_MODE`, request explicit selection |
| explicit selection after ambiguity | selected boundary, no expansion/fallback; prepared-only entry |
| unambiguous `SELECT 1; SELECT 2;` selection | local rejection |
| prepared-unsupported statement | static UnsupportedFeature + FocusEditor/DismissError, no raw fallback, retain only proven-healthy session |
| trailing terminator / inter-statement gap / multibyte prefix | attached terminator / `NO_CURRENT_STATEMENT` / UTF-8-safe mapping |

Review/acceptance:

- UX/accessibility reviewer runs all U0–U9 flows on a local app and attaches interaction/AccessKit results; code-only visual approval is insufficient.
- P6 uses actual typed driver/service events and no divergent mock-only path.
- Mandatory MySQL live safety seeds an empty marker table. UI explicit selection locally rejects `INSERT INTO marker VALUES ('first'); INSERT INTO marker VALUES ('second')`; direct prepared-adapter cases sourced from explicit-selection and ambiguous/current-target entry paths require `COM_STMT_PREPARE` rejection and assert both markers absent, especially `second`. A separately prepared-unsupported statement proves static UnsupportedFeature + FocusEditor/DismissError, no raw fallback, and healthy-session retention only when proven.

Rollback: UI state is in-memory. No user-data migration.

### P7 — copy and background streaming atomic export

Purpose: provide exact usable output without a second whole-result allocation or unsafe file replacement.

Expected files:

- `src/export.rs` (new) — incremental CSV/TSV/JSON encoders and canonical scalar/JSON functions.
- `src/export_file.rs` (new) — destination policy, 0600 temp, fsync/rename/dir-fsync outcomes.
- `src/ui/result_view.rs`, `src/ui/app.rs`, `src/ui/model.rs`, `src/ui/runtime.rs`, `src/ui/adapter.rs`.
- `src/lib.rs`, reviewed file-dialog dependency.
- `tests/export_golden.rs`, `tests/export_file_contract.rs`, extended RawInput/AccessKit tests.

RED contracts first:

1. CSV RFC4180/CRLF, TSV escaped/LF, and JSON `dbotter.result.v1` exactly match `spec.md` for every Cell variant, canonical nested JSON, duplicate names, null, empty rows, Unicode, controls, non-finite float text, and truncated bytes.
2. Pure `clipboard_scalar(Cell)` maps Null→empty; complete Text→literal; truncated Text→preview+marker; Bool→`true|false`; Int/UInt/Decimal→canonical base 10; finite Float→shortest round-trip and non-finite→`nan|inf|-inf`; DateTime→normalized ISO; Json→compact key-sorted; JsonPreview→canonical preview marker; Bytes→base64 with truncation marker when needed. Copy cell writes it byte-for-byte with literal controls/backslash and no header/newline.
3. Pure `tsv_field` maps backslash/tab/CR/LF to `\\`/`\t`/`\r`/`\n`. Copy selected/all applies it to visible headers and clipboard scalars, preserves all visible columns, sorts noncontiguous selected rows by visible index, and writes exactly one final LF. Goldens cover every Cell/control/Unicode/truncation/duplicate/empty case.
4. Export streams from `Arc<ResultSnapshot>` with bounded encoder buffers; allocation instrumentation rejects a whole-result `Vec<u8>`. One export/ResultId and two process-wide are permitted without consuming a profile network slot; excess is Busy.
5. Same-directory `create_new` temp is 0600 before write; file fsync, rename commit point, and parent fsync occur in order. Final mode remains 0600. JSON timestamp is exact UTC millisecond; CSV/TSV always include a header when columns exist.
6. DenyOverwrite uses platform atomic no-replace and preserves a competing destination. ReplaceConfirmed captures identity and rejects mismatch; symlink/non-regular destinations fail; the documented post-check entry-swap race remains.
7. Blocking export checks cancellation per row/chunk and Shutdown waits for actual cleanup; abort is never treated as cancellation. Failpoints preserve destination/commit truth.
8. Runtime UI/events/logs/receipts show safe metadata but no content/digest. Only an independent seeded verifier records fixture/expected/actual/verdict after byte-exact comparison.

Review/acceptance:

- export/security reviewer compares golden files and filesystem syscall order/failpoints;
- all three formats and file policies are mandatory on macOS installed journey.

Rollback: export files already committed are user artifacts and are never deleted by application rollback. Feature removal does not touch them.

### P8 — live gates, typed receipts, workflows, per-arch bundle, and tap contract

Purpose: make “usable” and “installed” one source-bound chain.

Expected files:

- live tests from P4/P5 plus `tests/restart_contract.rs`, `tests/ui_*`, export/config tests.
- `scripts/verify-live-contracts.sh`, extended `scripts/verify-local.sh`, receipt security/jq contract and negative fixtures.
- `scripts/check-release-contract.sh`, `scripts/package-version.sh`, `scripts/build-macos-app.sh`, `scripts/verify-installed.sh`, `scripts/verify-installed-gui.sh`.
- `.github/workflows/verify.yml`, refactored `ci.yml`, `preview.yml`, `release.yml`.
- `packaging/macos/Info.plist`, icon assets/conversion, manifest schema/validator.
- `docs/release/spec.md`, `docs/release/trace.md`, `README.md`.
- external reviewed integration: `2lab-ai/homebrew-tap` preview formula and dispatch workflow.

Required gate design:

1. Reusable verify runs exact identity + separate config-contract, release contract, fmt/clippy/test, config/export/controller failpoints, tagged-scope/Create-recovery/prepared-only source contracts, RawInput/AccessKit/contrast/disclosure, receipt negatives, Compose MySQL plus authenticated plaintext/TLS Redis, explicit live auth/browse/execute/marker-no-fallback/TLS-recovery tests, and source receipt.
2. Live runner invokes required ignored-test names explicitly. Missing env/service/cert is non-zero failure, never early-return success.
3. Local source uses `LocalAttached`; CI uses `CiExpectedSha` and verifies HEAD equals candidate SHA. Detached CI is valid only in that typed variant.
4. Build records source/target/toolchain/features. Four target builds complete. macOS arm64/x86_64 each package/sign/verify before canonical embedded-executable and archive hashes.
5. `dbotter version --format json` remains exactly six identity fields.
   Every source/package/shim/installed binary separately returns `dbotter config-contract --format json` as exactly `{read_versions:[1,2],write_version:2,migration_backup_suffix:".v1.bak"}`. Manifest/release/tap/receipt compare it without extending identity.
6. Preview version is `YYYY.MM.DD.HHMMSS.<run_id>.<run_attempt>`, strictly greater than current tap. Tag also contains UTC seconds/run id/attempt/short source SHA.
7. Tap dispatch carries explicit tag/SHA/version/manifest inputs; tap validates tag/SHA/artifacts and the manifest config contract, and rollback preflight executes/compares the candidate command before one atomic formula change.
8. Formula installs `Dbotter Preview.app` id `ai.2lab.dbotter.preview`; CLI shim points to the post-sign executable. `CFBundleShortVersionString` equals Cargo `x.y.z`, `CFBundleVersion` equals numeric `<run_id>.<run_attempt>`, and the long Homebrew version stays separate; plutil/manifest negatives enforce all three.
9. Installed GUI verification receives exact `--app-path` from `brew --prefix dbotter-preview`, rejects/terminates stale same-bundle processes, launches that path, and verifies PID executable realpath/device/inode/SHA plus bundle id against the manifest before AX input.
10. CI/preview/stable publish jobs all hard-need the reusable verification result. Stable remains operator-only and is not invoked.

RED workflow/receipt contracts:

- reject local detached/dirty/untracked and CI expected-SHA mismatch independently;
- reject source/tag/manifest/tap disagreement, swapped arch, bad embedded/archive hash, unsigned/invalid bundle, wrong bundle id, CLI copy mismatch;
- reject identity/config-contract conflation, wrong six-field identity, missing/extra/mismatched three-field config contract, bundle version conflation, stale process, wrong app path, or PID identity mismatch;
- reject missing live/accessibility/contrast/recovery-totality/clipboard/disclosure assertion; reject secret/backend prose anywhere public and user values in receipts or unrelated UI/AX nodes, while also rejecting missing intended AX value-node content;
- reject equal-hash claims across different transformations;
- reject duplicate/non-increasing version and incomplete tap dispatch;
- simulate rollback and require a higher version/tag/manifest whose exact config-contract command matches manifest/release/tap; reject a missing command and verify wrapper-owned backup-runbook presentation;
- verify direct old-binary invocation only returns UnsupportedVersion.

Review/acceptance:

- release/security reviewer checks workflow dependency graph, permission scopes, manifest parser, negative receipts, codesign, four targets, formula layout, and absence of stable mutation.

Rollback: publish a new higher preview only after exact config-contract preflight. Reject a missing/mismatched source before publish; the installer/rollback wrapper owns backup-runbook presentation. Never move a tag, replace an asset, reuse metadata, or lower formula version.

Direct old-binary invocation only returns UnsupportedVersion.

### P9 — integrate, review, merge, publish, install, and prove

Purpose: finish at a user-visible receipt.

Steps:

1. Integrate reviewed slices in dependency order; P4/P5 merge only after P3, and P6 consumes both.
2. Reconcile any trace correction before its code. Run full file-map and contract-source audit.
3. From a clean attached commit, run all source/hermetic/live/package commands below and produce LocalAttached receipt.
4. Independent final code/conformance review compares every T0–T10 row, exact state/cache table, error/file/security boundaries, tests, capability flags, and diff file map.
5. Push PR; require CiExpectedSha verification, live fixtures, target builds, signed bundle/manifest dry validation, and all review threads green.
6. Merge. Preview workflow publishes exact merge source under the monotonic tag/version and explicitly dispatches tap inputs.
7. Verify tap commit inputs/manifest, `brew update`, and upgrade preview. Prove CLI shim resolves to manifest's post-sign executable.
8. Run installed six-field identity, separate config-contract, check/exec/MySQL browse/Redis browse+inspect using explicit config.
9. Launch exact Brew app/PID identity, then complete T10: Create ConnectionId collision/auto-suffix; every Session intent/restart case; full MySQL scanner/prepared-only marker/no-fallback table; Execute-limit AX focus; every clipboard scalar/TSV control; total recovery table; intended-value AX disclosure; active Delete; split CA-vs-Host recovery where wrong host changes only to `localhost` and preserves CA; live auth/no fallback.
10. Write final typed receipt with separate identity/config contract, safe recovery codes/action ids and disclosure verdicts, no user values, no runtime export digest, and isolated external verifier digests. Confirm no stable release.
11. Clean the worktree only after merge commit, release/tag/tap reachability, installed receipt, and clean status are verified.

Any post-publish failure keeps the task incomplete. Repair forward with a new higher preview or perform the defined higher-version rollback, then rerun installed proof.

## 5. Acceptance commands

Interfaces are fixed enough for receipts; implementation may add non-weakening diagnostic flags.

### Source and hermetic gates

```sh
./scripts/check-release-contract.sh
./scripts/test-receipt-contract.sh
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-features --locked
```

The Cargo suite includes exact identity/config-contract, Create DraftId/ConnectionId recovery, tagged task scopes, config/controller/resource, MySQL scanner/prepared-only source and trait contracts, Session-intent, recovery-totality, clipboard/export, RawInput/AccessKit/disclosure/contrast tests. Live tests can be ignored Rust tests, but only explicit named execution counts.

### Required live gates

```sh
docker compose -p dbotter-e2e up -d --wait mysql redis-auth redis-tls-auth
DBOTTER_MYSQL_PASSWORD=dbotter-local-only DBOTTER_REDIS_PASSWORD=dbotter-redis-local-only ./scripts/verify-live-contracts.sh --config config/local.example.toml
DBOTTER_MYSQL_PASSWORD=dbotter-local-only DBOTTER_REDIS_PASSWORD=dbotter-redis-local-only ./scripts/verify-local.sh --config config/local.example.toml
jq -e '.assertions.overall == true' artifacts/receipt.json
```

The live receipt proves MySQL paging/mutation/permission evidence and the prepared-only marker negative: explicit-selection UI rejection plus prepared-adapter selection/current-entry cases require rejection for `INSERT INTO marker VALUES ('first'); INSERT INTO marker VALUES ('second')`, both rows absent, and an unsupported-prepared case with no fallback. It also proves Redis SCAN/types/auth. TLS assertions are separate: invalid/wrong CA emits CA code/action/focus; wrong host emits hostname code/Host focus, changes only host to `localhost`, preserves CA, succeeds, and never increments plaintext fallback. Every named missing fixture/env/assertion fails non-zero.

### Headless CLI contract from source build

```sh
dbotter version --format json
dbotter config-contract --format json
dbotter --config config/local.example.toml browse mysql schemas --profile mysql-local --page-size 50 --format json
dbotter --config config/local.example.toml browse mysql relations --profile mysql-local --schema dbotter --page-size 50 --format json
dbotter --config config/local.example.toml browse mysql columns --profile mysql-local --schema dbotter --relation receipt --page-size 50 --format json
dbotter --config config/local.example.toml browse redis keys --profile redis-local --filter-mode literal-prefix --filter receipt: --cursor 0 --count 100 --format json
dbotter --config config/local.example.toml inspect redis key --profile redis-local --key-base64 cmVjZWlwdDptYXJrZXI= --format json
```

### Per-architecture macOS package gate

On each macOS architecture runner:

```sh
./scripts/build-macos-app.sh --channel preview --binary target/release/dbotter --output artifacts
codesign --verify --deep --strict "artifacts/Dbotter Preview.app"
"artifacts/Dbotter Preview.app/Contents/MacOS/dbotter" config-contract --format json
./scripts/check-release-contract.sh --manifest artifacts/preview-manifest.json
```

Package receipt records bundle id/version, architecture, unsigned provenance hash, post-sign embedded hash, archive hash, codesign verdict, icon, stable AX inventory, and manifest link without asserting transformed hashes equal.
It also verifies with `plutil` that `CFBundleShortVersionString` equals Cargo `x.y.z` and `CFBundleVersion` equals `<run_id>.<run_attempt>`; negative fixtures reject either bundle field containing the independent long Homebrew version.

### Homebrew-installed CLI gate

```sh
brew update
brew upgrade 2lab-ai/tap/dbotter-preview
brew list --versions dbotter-preview
dbotter version --format json
dbotter config-contract --format json
dbotter --config /tmp/dbotter-installed/config.toml check --profile mysql-installed --format json
dbotter --config /tmp/dbotter-installed/config.toml exec --profile mysql-installed --text 'SELECT 1 AS installed_path' --format json
dbotter --config /tmp/dbotter-installed/config.toml browse mysql schemas --profile mysql-installed --page-size 50 --format json
dbotter --config /tmp/dbotter-installed/config.toml browse redis keys --profile redis-installed --filter-mode literal-prefix --filter receipt: --cursor 0 --count 100 --format json
dbotter --config /tmp/dbotter-installed/config.toml inspect redis key --profile redis-installed --key-base64 cmVjZWlwdDptYXJrZXI= --format json
./scripts/verify-installed.sh --manifest artifacts/preview-manifest.json --config /tmp/dbotter-installed/config.toml
```

The verifier asserts exact six-field identity and separate exact three-field config contract, proves the CLI shim resolves to the post-sign executable, and matches both against the manifest/release/tap records.

### Installed native AX journey

```sh
APP_PATH="$(brew --prefix dbotter-preview)/Dbotter Preview.app"
./scripts/verify-installed-gui.sh \
  --app-path "$APP_PATH" \
  --config /tmp/dbotter-installed/gui-config.toml \
  --manifest artifacts/preview-manifest.json \
  --output artifacts/installed-gui-receipt.json
jq -e '.assertions.overall == true' artifacts/installed-gui-receipt.json
```

The script proves exact process identity, then drives Create `profile.connection_id` recovery/auto suffix, every Session intent, every MySQL scanner/prepared-only marker row, `editor.row_limit`/`editor.timeout`, every PublicSummary recovery row, split CA/Host focus/fix, and every clipboard scalar/TSV field. AX disclosure tests require user values in intended value nodes and absent from names/descriptions/live announcements/unrelated nodes; secrets stay protected. External seeded export verification remains isolated. Every recovery ends successfully, not at an error label.

## 6. Receipt schemas and provenance checks

The source receipt contains exactly one source variant:

- local: physical root, attached branch, commit, clean status and tracked inputs;
- CI: commit, expected SHA, run id/attempt, clean checkout and tracked inputs.

The preview manifest and installed receipt record:

- source SHA equality at source/build/tag/manifest/tap semantic identity points;
- exact six-field identity objects and separate exact three-field config contracts for source/package/shim/installed executable, with manifest/release/tap comparisons;
- unsigned binary provenance hash, post-sign embedded hash, bundle/archive hash, download hash, installed embedded hash as distinct typed fields;
- release tag/version/run id/attempt/manifest URL+hash;
- Cargo package version, exact bundle short/build versions, and independent long Homebrew version;
- formula repository/commit/version/per-arch URL+hash;
- requested/resolved installed app path, stale-process disposition, launch PID, executable realpath/device/inode/hash, bundle id, CLI shim target/inode/hash, and codesign verdict;
- CLI/live/AX/export assertions including Create/ConnectionId, tagged scope, Session intents, prepared-only scanner/marker/no-fallback cases, Execute-limit ids, exhaustive recovery, split TLS codes/actions, and intended-value disclosure verdicts, plus timestamps/durations;
- derived credential/content leak verdict over the entire candidate.

No secrets, backend prose, user SQL/Redis text, result/key/CA/export-path values, exported bytes, or result screenshots enter a receipt. Runtime receipts contain no export-content digest. Only the independent seeded-verifier subsection records non-sensitive fixture id, expected/actual digest, and verdict.

## 7. Final conformance audit

Before completion:

- mark each T0–T10 Verified only with its contract and required live/installed evidence;
- compare `git diff --name-only` to every slice file map and explain every deviation;
- run assumption-injection, scope-creep, direction-drift, missing-core, and over-engineering checks;
- search production paths for secret formatting and `panic!`/`todo!`/`unwrap()`/`expect()`;
- prove version-1 read-only normalization, fixed backup, first-mutation v2, frozen-reader rejection, exact separate config-contract through manifest/release/tap, wrapper-only runbook, and every credential/Session-intent row after restart;
- prove Create DraftId recovery, `ConnectionId`/`profile.connection_id`, explicit collision versus automatic suffix allocation, and zero saved-profile Create field actions;
- prove reload diff/Config uncertain behavior, exact `RegisteredTask`/tagged Profile/Draft/Export/Global scopes, every correlation/state/cache row, cancel/session-generation race, shutdown queue drain, panic, and async-versus-blocking joins;
- prove catalog/keyspace capability bits equal live readiness, prepared static/bound MySQL catalog statements, MySQL `page_size+1`/4-MiB behavior and permission evidence are exact, and Redis input/classifier/memory claims are precise;
- prove full MySQL scanner/target A→B, prepared-only source/trait and marker/no-fallback evidence despite handshake capability, every Session intent, Execute-only limit focus ids, active Delete, CA-vs-Host code/action/focus/CA preservation, recovery NonEmpty totality, AX ids/readback, intended-value presence/unrelated-node absence/protected secret, every clipboard scalar/TSV field, and exact export goldens;
- prove CI/local source, separate exact identity/config commands, bundle version split, workflow graph/hash chain, monotonic tap inputs, typed higher rollback preflight, wrapper-only runbook, installed shim, and exact app-path/PID identity;
- prove no stable tag/release was created.

## 8. Scope pressure order

If implementation risk threatens the approved P0, reduce only presentation breadth in this order:

1. visual animation and nonessential decoration;
2. catalog metadata beyond schema/relation/column/type/nullability;
3. rich preview formatting beyond the required representative Redis values.

Do not remove side-effect-free draft testing, explicit credential modes, Create/Edit collision safety, delete/disconnect/restart behavior, controller/cancellation/shutdown, MySQL pagination/recovery, Redis SCAN/raw identity/TTL/verified TLS, static recoverable errors, profile result provenance, copy and exact streaming export, RawInput/AccessKit/contrast, live/source/package gates, monotonic preview/tap/rollback, installed CLI browse/inspect, or installed AX proof. Query history/recent statements are excluded and are not a scope lever.
