# dbotter usable MVP — vertical trace

Status: follow-up review candidate. The third-pass findings are resolved in this trace. It becomes the cross-layer source of truth after remediation UX/product and architecture/security no-blocker verdicts and P0 legacy-document reconciliation.

## 1. Approved vocabulary

Names are contractual; implementation changes update this trace before code.

- Work/mutation commands: `RefreshProfiles`, `CreateProfile`, `UpdateProfile`, `DeleteProfile`, `TestDraftConnection`, `ConnectProfile`, `Execute`, `LoadMySqlSchemas`, `LoadMySqlRelations`, `LoadMySqlColumns`, `ScanRedisKeys`, `InspectRedisKey`, and `ExportResult`.
- Dedicated control commands: `CancelOperation`, `DisconnectProfile`, `ReconnectProfile`, and `ShutdownRuntime`. Reconnect performs control cleanup first, then enqueues a fresh Connect work recipe.
- Local recovery actions translate to work/control commands or closed UI actions: `SubmitSessionSecret`, `OpenCredentialPrompt`, `EditDraft(DraftId, ProfileFieldId)`, `EditProfile(ProfileId, ProfileFieldId)`, `Retry`, `FocusEditor`, `FocusExecuteLimits(ProfileId)`, `ReloadConfiguration`, `Reconnect`, `CancelOperation`, `ClearCatalog`, `RestartRedisScan`, `ChooseExportDestination`, `RevealExportDestination`, `RevealMigrationBackup`, `RestartApplication`, and `DismissError`. `ProfileFieldId` includes `ConnectionId` for the explicit Create id.
- Events: `ProfilesLoaded`, `DraftConnectionTested`, `ProfileCreated`, `ProfileUpdated`, `ProfileDeleted`, `ConnectionChanged`, `QueryFinished`, `MySqlPageLoaded`, `RedisKeysLoaded`, `RedisKeyInspected`, `OperationCancelled`, `ResultExported`, `RuntimeShutdown`, and `OperationFailed`.
- Identity/correlation types: `OperationId`, `OperationRecipeId`, `ProfileId`, `ProfileGeneration`, `SessionGeneration`, `DraftId`, `ResultId`, and `ResultProvenance`. Every profile-scoped async command/event carries `(ProfileId, ProfileGeneration, OperationId)`; draft carries `(DraftId, OperationId)`, export carries `(ResultId, OperationId)`, and global load/shutdown carries `OperationId`.
- Credential types: persisted `CredentialMode::{None, Session, Environment}`, non-serializable `SessionSecret`, `SessionSecretStore = HashMap<ProfileId, Arc<SessionSecret>>`, UI-only `SessionCredentialIntent::{KeepCurrent, Replace, Forget}`, mutation-only `SessionSecretUpdate::{Keep, Replace(Arc<SessionSecret>), Clear}`, and `EnvironmentAvailability::{Available, Missing, Empty}`.
- Resource types: `CatalogRequest::{Schemas, Relations, Columns}`, `CatalogPage`, `CatalogPageToken`, `RedisKeyFilter::{LiteralPrefix, Glob}`, `RedisScanRequest`, `RedisKeyId`, `RedisKeyPage`, `RedisKeyInspectRequest`, and `RedisValuePreview`.
- Capability bits: `CATALOG` is MySQL metadata browsing; `KEYSPACE_BROWSE` is Redis key browsing. They are never aliases.
- Errors: `PublicOperationError { operation: OperationKind, category, code, summary: PublicSummary, recovery: NonEmpty<RecoveryAction> }`; `OperationKind`, `ProfileFieldId`, `PublicSummary`, and `RecoveryAction` are closed enums from `spec.md`, never backend strings.
- Export: `Arc<ResultSnapshot>`, `ExportFormat::{Csv, Tsv, Json}`, and `OverwritePolicy::{DenyOverwrite, ReplaceConfirmed}`.

`SessionSecret` has no `Serialize`, a redacted manual `Debug`, and zeroizes dbotter-owned bytes on final Arc drop. `UiCommand`, `ExecuteRequest`, `RedisScanRequest`, `RedisKeyInspectRequest`, and `ExportResult` have manual redacted `Debug` and no `Serialize`. Secrets and backend prose never enter a public boundary. User-owned statement/key/result/CA-path/export-path values may appear only in their intended rendered field and AX value node, and result data may reach clipboard/file only after explicit Copy/Export; those values remain redacted from manual Debug/log/error/receipt and unrelated UI/AX nodes. Secret AX values remain protected. `ConnectionProfile` is the only persisted profile payload and contains no secret value. No contract claims zeroization of external copies.

## 2. Authoritative implementation ledger

| ID | Scenario | Contract source | Status | Dependency |
|---|---|---|---|---|
| T0 | v1 read-only load, v2 migration/reload, first-run | config/model/headless UI | Blocked(follow-up review) | remediation review, P0 reconciliation |
| T1 | Create/edit, credential mode, unsaved draft test | config/service/UI/restart | Blocked(follow-up review) | T0 |
| T2 | Confirmed atomic profile delete | config/service/UI/restart | Blocked(follow-up review) | T1 |
| T3 | Controller, reload, connect/disconnect/reconnect/shutdown | service/runtime/UI | Blocked(follow-up review) | T2 |
| T4 | Exact target/execute/policy/cancel | runtime/driver/UI | Blocked(follow-up review) | T3 |
| T5 | Lazy paginated MySQL catalog | typed seam/driver/live/CLI/UI | Blocked(follow-up review) | T4, resource seam |
| T6 | Redis SCAN/inspect/TLS/bounds | typed seam/driver/live/CLI/UI | Blocked(follow-up review) | T4, resource seam |
| T7 | Profile result/copy/streaming export | model/export/UI/filesystem | Blocked(follow-up review) | T4 |
| T8 | Static errors/recovery/accessibility | error/model/RawInput/AccessKit | Blocked(follow-up review) | T1–T7 |
| T9 | Restart and credential availability | restart/UI contracts | Blocked(follow-up review) | T1–T3, T8 |
| T10 | CI/manifest/preview/Brew/installed journey | workflow/receipt/package/AX | Blocked(follow-up review) | T0–T9 |

After the remediation verdict, P0 changes T0 to RED and every later row to Not started. Thereafter allowed statuses are `Not started`, `RED`, `Implementing`, `GREEN`, and `Verified`. A row reaches Verified only after contract results, required live evidence, trace conformance, and file-map audit.

## 3. Cross-cutting runtime contracts

### 3.1 Profile and session generations

- Runtime owns a process-monotonic `u64` allocator. Initial profiles receive generations during deterministic config load; every committed Create, Edit, and Delete consumes the next value.
- `active_profiles: HashMap<ProfileId, ProfileGeneration>` contains the current generation. Delete replaces the active entry with `tombstones[profile_id] = deleted_generation`; tombstones remain until runtime shutdown. Recreating a formerly deleted slug receives a generation greater than its tombstone.
- Every profile-scoped async command/event carries the exact tuple `(ProfileId, ProfileGeneration, OperationId)`. The fold applies it only when generation equals the active generation. Draft `(DraftId, OperationId)` and export `(ResultId, OperationId)` never borrow the currently selected profile; global load/shutdown uses OperationId only. A tombstone rejects every older profile event.
- A cache entry is `{ profile_generation, session_generation, connection_fingerprint, handle }`. `SessionGeneration` is monotonic and assigned before each new connect. Eviction uses compare-and-remove on both profile and session generation, so a late timeout/cancel/failure cannot remove a replacement handle.
- A committed edit first advances the active profile generation, fencing every old event. A runtime-neutral edit (display name only, same connection fingerprint, Session secret update Keep) retags the same idle session handle. When old-generation network work is active, the runtime cancels/joins it after the commit and evicts its session generation; it never retags a handle whose future was dropped. Every connection-affecting edit evicts the old handle.
- `ReloadConfiguration` performs an id-keyed diff. Identical profiles preserve generation/cache/secret/workspace. Added profiles receive a new generation, no cache/secret, and a fresh workspace. Any changed profile receives a new-generation fence, then cancel/join/evict; its prior Session secret is cleared before applying the new mode. Removed profiles receive tombstone→cancel/join→evict→secret/workspace clear. A later re-add receives a generation greater than the tombstone.
- An unreadable/ambiguous post-commit reload enters Config uncertain: fence every active generation, cancel/join every task, clear every cache/secret, and accept only ReloadConfiguration or ShutdownRuntime. No previous Connected state remains actionable. Barrier tests cover identical/changed/mode-change/removed/re-add/unreadable cases.

### 3.2 Operation controller and shutdown

- Config reads/mutations use a capacity-16 mutation lane and are processed serially; `RefreshProfiles` is ordered there with mutations.
- Network work is queued on a capacity-32 work lane. The controller permits one active network operation per active profile generation and four active network operations process-wide. Draft tests use `DraftId` for the per-draft slot and count toward the global four.
- Cancel, Disconnect, and the cleanup phase of Reconnect use a dedicated capacity-16 control lane; the UI coalesces duplicate controls per active operation/profile. Its capacity exceeds the maximum four active-operation cancellation/disconnect demand. Shutdown also uses a watch signal, so it cannot be blocked by any queue. The runtime uses biased polling in control → mutation → work order.
- Every task registers `RegisteredTask { operation_id, scope: TaskScope, cancel, join }` before it starts. `TaskScope` is the closed enum shown below; registration failure returns Busy and spawns nothing. The UI event lane has capacity 128; registry/permit cleanup happens before terminal delivery, so a full/closed lane cannot leak a permit or strand a task.

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

The Draft variant never contains profile or session generation data. Export and Global likewise carry only their declared scope data.
- Cancel or timeout cancels client waiting, drops the driver future, compare-and-removes the exact session generation used, awaits the task, and emits one correlated terminal event. Timeout and user cancellation remain distinct. Server state is always Unknown.
- Disconnect cancels and joins the target profile task before evicting its matching cache entry. Edit/Delete are transactional in the opposite order: a pre-rename failure leaves the operation untouched; an observed commit publishes the new-generation/tombstone fence first, then cancels, joins, and evicts old-generation work.
- Secret lookup clones `Arc<SessionSecret>` under the store lock and releases the lock before any await. Shutdown closes intake and drains/drops queued commands—including their secret Arcs—before completion.
- The two-second cancel/abort grace applies only to abortable async network tasks. An in-flight config mutation is joined through its classified commit outcome. Blocking export checks cancellation per row/chunk; Shutdown waits for the actual worker and temp-guard cleanup and never treats `spawn_blocking::abort` as cancellation.
- Async/export `JoinError` or panic maps to static InternalFailure, evicts the exact used session when present, cleans registry/permit/temp first, and attempts at most one terminal event. Shutdown tests use barriers for full event lane, panicking task/encoder, blocked encoder, mutation/network/export in flight, permit leakage, and queue drain.
- After those joins, shutdown evicts/drops sessions outside locks, clears the store, and emits `RuntimeShutdown`. No task is detached and no lock is held across `.await`.

### 3.3 Exact connection-state/cache outcomes

| Trigger and terminal outcome | Visible connection state | Cache outcome |
|---|---|---|
| Config load/Create | Disconnected, or Needs credential for Session without a secret | no entry |
| `TestDraftConnection`, any outcome | no saved-profile state change | ephemeral handle is closed; cache untouched |
| Runtime-neutral Edit committed, no active network work | prior Connected/Disconnected state retained under new profile generation | matching idle handle is retagged; session generation unchanged |
| Runtime-neutral Edit committed with active network work | Disconnected after old work is cancelled/joined | dropped future makes exact old session generation unsafe; it is evicted, not retagged |
| Connection-affecting Edit or credential Replace/Clear committed | Disconnected or Needs credential | old `{profile_generation, session_generation}` evicted |
| Delete committed | profile removed; tombstone only | exact entry evicted |
| Connect cache hit + ping success | Connected | same matching entry retained |
| Connect cache miss + connect/ping success | Connected | new profile/session-generation entry inserted |
| Credential unavailable before connect | Needs credential (Session) or Failed (Environment Missing/Empty) | unchanged; normally absent |
| Connect or ping auth/TLS/network/timeout failure | Failed | attempted/reused exact entry evicted |
| Execute/browse success, or safe syntax/constraint/permission failure | Connected | matching entry retained |
| Execute/browse network/protocol/internal-decode failure | Failed | exact used entry evicted |
| Execute/browse Cancel or timeout | Disconnected; operation separately Cancelled/Timed out | exact used entry evicted; server state Unknown |
| Disconnect completed | Disconnected or Needs credential if Session secret is absent | exact entry evicted after task join |
| Reconnect started/completed | Connecting → Connected or Failed | old entry evicted first; success inserts a new session generation |
| Reload unchanged | exact previous state | generation/cache/secret/workspace preserved |
| Reload added | Disconnected or Needs credential | new generation, no cache/secret |
| Reload changed | Disconnected or Needs credential | new fence; cancel/join; exact cache evicted; Session secret cleared |
| Reload removed/re-added | removed+tombstone, then fresh disconnected generation | clear all target state; re-add generation exceeds tombstone |
| Reload unreadable/ambiguous | Config uncertain; only Reload/Shutdown enabled | fence/cancel/join all; clear all cache/secret |
| Shutdown | Closing, then process exit | all entries joined/dropped; secret store cleared |

The driver-to-service error conversion carries a typed `SessionDisposition::{Keep, Evict}`. Syntax, constraint, permission, and other server rejections that prove the transport remains usable choose Keep; network, protocol, timeout, cancel, and uncertain driver failures choose Evict.

### 3.4 Typed driver/resource seam

The injectable service boundary is split rather than widened into one stringly method:

```rust
trait ConnectionPing {
    async fn ping(&self, timeout: Duration) -> Result<(), DriverError>;
}

trait MySqlPreparedExecution {
    async fn execute_prepared(&self, request: &PreparedMySqlRequest) -> Result<QueryResult, DriverError>;
}

trait RedisExecution {
    async fn execute_command(&self, request: &RedisExecuteRequest) -> Result<QueryResult, DriverError>;
}

trait CatalogBrowser {
    async fn load_page(&self, request: &CatalogRequest) -> Result<CatalogPage, DriverError>;
}

trait KeyspaceBrowser {
    async fn scan_keys(&self, request: &RedisScanRequest) -> Result<RedisKeyPage, DriverError>;
    async fn inspect_key(&self, request: &RedisKeyInspectRequest) -> Result<RedisValuePreview, DriverError>;
}

enum ConnectedResources {
    MySql { ping: Arc<dyn ConnectionPing>, execution: Arc<dyn MySqlPreparedExecution>, catalog: Arc<dyn CatalogBrowser> },
    Redis { ping: Arc<dyn ConnectionPing>, execution: Arc<dyn RedisExecution>, keyspace: Arc<dyn KeyspaceBrowser> },
}
```

The actual implementation may use concrete adapters, but tests inject these typed seams. `PreparedMySqlRequest` is the sole user-SQL driver entry and its implementation must issue server `COM_STMT_PREPARE` then `COM_STMT_EXECUTE`. Compile/source contracts reject `sqlx::raw_sql`, `Executor::execute(&str)`, `COM_QUERY`, and any prepared-unsupported fallback in the user-text path. `ApplicationService::{load_catalog_page, scan_redis_keys, inspect_redis_key}` validates driver/capability/request pairing before dispatch. MySQL cannot receive a keyspace request; Redis cannot receive a catalog request; MongoDB remains Planned and receives neither. Static/bound MySQL catalog statements also use prepared execution.

### 3.5 Resource-memory qualification

- MySQL browser queries and Redis string preview are server-bounded as described in T5/T6. SCAN/HSCAN/SSCAN COUNT is only a hint; list/zset/stream count limits bound item count but not individual server element bytes.
- redis-rs materializes one RESP frame before dbotter can traverse/cap it. SQLx can materialize the current row/cell. Arbitrary user SQL/Redis execute therefore has a transient-allocation caveat.
- After decoding, dbotter enforces the constants in `spec.md` on retained snapshots: 1,024 columns, 8 MiB total, 1 MiB shared cell cap (64 KiB for Redis), 32 static notices, plus row/item/depth caps. Oversized retained values carry preview/original-length/truncation metadata. Rendering, clipboard selection, and export consume only capped snapshots. No receipt claims that driver transient memory is universally bounded.

## T0 — exact-path v1 normalization, v2 load, and first run

### 1. Entry and input

- Native app startup and all CLI commands resolve config path once: explicit global `--config` → `DBOTTER_CONFIG` → platform default.
- `config::load_path(&Path)` is the only config loader beneath entrypoints. Missing path is an empty version-2 config.
- Version 2 is current. Version 1 loads read-only and normalizes absent credential mode: `secret_env` present → Environment, absent → None; Redis Preferred remains invalid/edit-required. No startup write occurs. The first confirmed committed mutation performs T1's backup and writes version 2.

### 2. Layer flow

`resolved config path` → `ApplicationService::load_path` → parse/version check/v1 read-only normalization/profile validation → deterministic profile-generation allocation → environment-name availability probe without value exposure → `ProfileSnapshot { wire_version, migration_required }` → `ProfilesLoaded` → model fold → profile navigation or `FirstRunEmptyState`.

MongoDB Planned descriptor → disabled planned area; no primary New action.

### 3. Side effects and errors

- Load and v1 normalization are read-only. A v1 snapshot shows a migration banner, fixed `<config>.v1.bak` path, and confirmation requirement; Cancel makes no file/runtime change.
- Parse/version/path error → static `InvalidInput`/`InternalFailure` summary plus `ReloadConfiguration`; no false empty state.
- Legacy Redis Preferred profile loads visibly with `REDIS_TLS_PREFERRED_LEGACY`, `EditProfile(profile, RedisTlsMode)`, and no network eligibility.
- Full work lane does not affect initial load; closed mutation/control runtime yields a visible InternalFailure.

### 4. Output and contracts

- Empty config shows New connection, MySQL/Redis choices, credential-mode explanation, and disabled MongoDB Planned.
- `dbotter config-contract --format json` is a pure capability command that constructs no service and returns exactly `{read_versions:[1,2],write_version:2,migration_backup_suffix:".v1.bak"}`. It is separate from the six-field identity command.
- Contracts cover `--config` precedence, v1 normalization without write, version-2 load, exact config-contract JSON, legacy TLS rejection, missing/invalid distinction, stable initial generations, and no secret/env value in snapshot or AccessKit tree. A frozen fixture using the current v1 reader rejects a version-2 file with `UnsupportedVersion(2)` before service/network construction.

## T1 — Create/Edit, credential mode, and side-effect-free draft test

### 1. Entry and input

- GUI New/Edit form exposes Test, Save, and Save & Connect.
- `CreateProfile` carries `(DraftId, OperationId)`, a validated draft, optional explicit id, and `SessionSecretUpdate`; `UpdateProfile` carries `(ProfileId, ProfileGeneration, OperationId)`, immutable id, draft, and update.
- Draft credential rules, `SessionCredentialIntent`, stable intent AX ids, and update matrix are exactly those in `spec.md` §4. Existing Session+Arc defaults KeepCurrent and shows only “set”; restart/no Arc and Create/enter-Session disable KeepCurrent and default Replace, with Forget available.

### 2. Draft-test flow

validated draft + intent → choose source before controller registration:

- KeepCurrent: clone the existing `Arc<SessionSecret>` under the profile-store lock, release the lock, then use that read-only Arc;
- Replace: copy the required `Zeroizing<String>` once into a one-shot Arc while retaining the form buffer;
- Forget or missing current Arc: return CredentialRequired whose `NonEmpty<RecoveryAction>` contains `EditDraft(draft_id, SessionCredential)` before connector/session acquisition;
- None/Environment: use the exact mode source with no Session intent.

An acquired source then flows through `TestDraftConnection { draft_id, operation_id, redacted draft, one_shot_or_read_only_secret }` → global controller slot → temporary resources → ping → close/drop/join → drop operation Arc → `DraftConnectionTested` or `OperationFailed`.

There is no arrow to config, active generations, cache mutation, store mutation, saved connection state, or workspace. Replace success/failure retains the form buffer; KeepCurrent leaves the stored Arc untouched. Editing driver/host/port/database/username/TLS/CA/credential mode/env/intent/password invalidates Test success. An edit-form Test does not evict/ping the saved handle. Accepted Save maps KeepCurrent→Keep, Replace→Replace, Forget→Clear. Only Replace moves/zeroizes the form buffer; a full queue leaves it untouched.

### 3. Create/Edit commit flow

Create slug preview uses normalized lowercase ASCII words and hyphens, fallback `connection`. In the serialized mutation lane, service calls `load_path`, then chooses the lowest free suffix for an auto id; that collision is allocation, not an error. An explicit occupied id returns `PROFILE_ID_CONFLICT` + `EditDraft(draft_id, ConnectionId)`, focuses `profile.connection_id`, and never constructs a ProfileId action. Create never calls update/replace.

Edit verifies active id + expected generation against latest loaded config. Id is immutable; missing/mismatched generation returns `PROFILE_STALE` and ReloadConfiguration.

Validated mutation → writer guard → `load_path(exact_path)` + fingerprint. For v1 after user confirmation: write exact original bytes through 0600 temp/file-fsync → atomic no-replace rename to fixed `<config>.v1.bak` → parent-fsync; existing identical backup is accepted, non-identical/failed backup aborts. Normalize v1 → apply mutation → serialize full `version = 2` → 0600 same-directory temp/write/flush/file-fsync → immediately re-read/fingerprint destination → mismatch returns `CONFIG_EXTERNAL_CHANGE`/NotCommitted → rename main config **commit point** → parent-fsync → classify → reload exact v2 → reconcile → event.

`SessionSecretUpdate` applies only after rename and only according to observed post-rename config. Pre-rename failure never changes service/cache/store/generation and any active operation continues. Once the observed config is committed, runtime publishes the new profile generation as a fence, then cancels/joins old-generation work. A runtime-neutral edit with an idle cache entry can retag it as in §3.3; active work or any connection-affecting edit evicts it.

### 4. Commit outcomes and failpoints

- Before rename: `NotCommitted`; old disk/service/cache/store remain authoritative; temp is cleaned.
- Rename + parent fsync success: `Committed`; new disk state becomes authoritative.
- Rename success + parent fsync failure: `CommittedDurabilityUnknown`; do not retry the mutation. Immediately `load_path`. If readable, reconcile runtime to observed content and emit static `CommittedDurabilityUnknown` + ReloadConfiguration. If unreadable, enter config-uncertain mode, disable further mutations, retain no unproven cache/session secret, and require successful ReloadConfiguration.
- Required deterministic failpoints exist for backup temp/create/write/file-fsync/no-replace rename/dir-fsync and main temp/create/write/file-fsync/pre-rename/post-rename/dir-fsync. Tests assert backup never overwrites, main temp cleanup, and exact authoritative state.

The writer guard covers one dbotter process only. Every mutation reloads and rechecks the disk fingerprint, so a known pre-rename external change fails with ReloadConfiguration. Another process can still race after that recheck; this remains explicitly unsupported and is not described as multi-process atomicity.

### 5. Output and contracts

- Save closes on Committed; Save & Connect enters T3 only after Committed. A successful Replace Test followed by Save & Connect needs no password re-entry; KeepCurrent needs no field; Forget/Clear opens the prompt.
- Test failure keeps the draft; the user follows `EditDraft` to correct the field and presses Test again. No operation recipe is stored for draft tests.
- Migration success reports version 2 and RevealMigrationBackup; confirmation cancel or backup failure never mutates main config. Release/installer rollback preflight rejects a source whose config-contract command is missing/mismatched; manual old-binary invocation only returns UnsupportedVersion.
- Contracts cover Create `(DraftId, OperationId)`, explicit collision → ConnectionId draft edit/no overwrite, auto-suffix allocation, immutable Edit id, every intent/credential/update path, intent AX ids, Keep Arc lock-release/read-only behavior, Replace retention/move, Forget no-network/Clear, final-Arc drop, manual Debug/no-Serialize, v1 read-only normalization, v2 bytes/current-v1-reader rejection, exact config contract, migration backup/failpoints, directory fsync, and single-process boundary.

## T2 — confirmed atomic profile delete

### 1. Entry and input

- Delete menu → confirmation naming display name + redacted endpoint. When an operation is active, it also names static `OperationKind` and displays: “Dbotter will stop waiting; the server operation may continue.”
- Before Confirm—including dialog open/cancel—there is no command, cancellation, generation/cache/secret/workspace change, or config write. Confirm carries `(ProfileId, ProfileGeneration, OperationId)`.

### 2. Flow

Confirm → serialized mutation lane → writer guard → `load_path` → verify id/generation → remove exactly target → version-2 temp/write/file-fsync/rename/dir-fsync from T1 → reload observed outcome. If committed: publish deletion generation+tombstone first → signal target cancellation → join → compare-and-remove exact cache → clear/zeroize secret → remove workspace → `ProfileDeleted { server_state: Unknown when work was active }` → select survivor/empty.

### 3. Errors and outcomes

- Dialog cancel: no command.
- Unknown/stale id: static InvalidInput + ReloadConfiguration; no write.
- Pre-rename failpoint: profile, active generation, cache, secret, selection, workspace, and active operation are unchanged; no premature cancel occurs.
- Post-rename fsync failure: CommittedDurabilityUnknown and exact-path reconciliation; UI does not promise the profile survived or was deleted until reload observes it.
- Tombstone rejects late target events. Unrelated profiles/sessions remain.

### 4. Contracts

Confirmation/AX identity and static active-operation warning/kind; pre-confirm zero side effects; post-confirm tombstone→cancel→join→evict order; Unknown UI/AX state; failpoints, restart, cleanup, unrelated preservation, tombstone recreation race, and CommittedDurabilityUnknown reconciliation.

## T3 — controller, connect/disconnect/reconnect, and shutdown

### 1. Entry and credential input

- Profile Connect/Disconnect/Reconnect; Save & Connect after T1; auto-connect from T4–T6. Every command/event carries `(ProfileId, ProfileGeneration, OperationId)`.
- None supplies no secret. Session requires current store value or `OpenCredentialPrompt`. Environment resolves only the named variable and reports Available/Missing/Empty without the value.
- Redis validation enforces Disabled/Required plus Redis-only `redis_tls.ca_file`. Required+blank uses OS roots; Required+path requires a readable regular valid PEM CA; Disabled clears the UI draft, hides/disables `profile.redis_tls.ca_file`/picker, and direct Disabled+value fails. Required builds `TcpTls { insecure:false }`; Preferred/non-Redis option fails before registration.

### 2. Connect flow

command tuple → controller permit → credential mode → under store lock clone `Arc<SessionSecret>` for Session then unlock → ready capability/cache lookup → new SessionGeneration on miss → typed connector/ping → race reconciliation → cache insert/reuse → correlated `ConnectionChanged` using §3.3. No store lock crosses await.

Prompt submit stores a `SessionSecret` only for Session mode, then retries the non-secret Connect recipe. Environment recovery edits the name/profile; no UI action copies an environment value.

### 3. Disconnect/reconnect/shutdown flow

Disconnect control command → cancel token → join profile task → compare-and-remove exact cache entry → drop handle outside lock → Disconnected/Needs credential.

Reconnect → same cancellation/join/evict → allocate new session generation → normal Connect. It never reuses the evicted handle.

Shutdown follows §3.2 exactly: drain queued secret commands; two-second abort only async work; join mutation and cooperative export; clean registry/permits/temp; drop sessions; clear final secret Arcs; terminal event.

### 4. Errors and contracts

- Missing Session secret → Needs credential and OpenCredentialPrompt before network.
- Missing/empty Environment → Authentication/Validation static summary + `EditProfile(profile, CredentialEnvironmentName)`; no network.
- `REDIS_TLS_CA_INVALID_PEM` and `REDIS_TLS_CA_UNTRUSTED_ISSUER` (including wrong CA) map only to the current identity domain's CA edit: `EditDraft(draft, RedisCaFile)` for Test or `EditProfile(profile, RedisCaFile)` for saved-profile operations, focusing `profile.redis_tls.ca_file`. `TLS_HOSTNAME_MISMATCH` likewise maps only to `EditDraft(draft, Host)` for Test or `EditProfile(profile, Host)` for saved-profile operations, focusing `profile.host`. The wrong-host recovery changes host to `localhost`, retains the CA, and succeeds; neither branch reaches TCP.
- Auth/TLS/network/timeout follows the exact state/cache table.
- Contracts cover mode-specific resolution (no precedence fallback), duplicate-connect reconciliation, global/per-profile limits, dedicated-control responsiveness, reconnect new generation, cancel/timeout compare-remove race, no lock across await, and complete shutdown cleanup.
- Mandatory auth contracts cover MySQL and Redis ACL/requirepass on plaintext and verified TLS. Each driver proves correct/wrong Session credentials, Environment Available/Missing/Empty, static Authentication code/action, and recovery to success. Missing fixture/env becomes a named false assertion and non-zero exit.
- Mandatory TLS live contract uses hostname `localhost`, test CA, SAN `localhost`, and Required. Invalid PEM is hermetic+RawInput; wrong CA asserts CA PublicCode/action/focus; wrong hostname asserts hostname PublicCode/Host focus, changes only host to `localhost`, retains CA, and reconnects. Plaintext fallback counters remain zero.

## T4 — execute selected/current work and cancel

### 1. Entry and input

- Execute button or Cmd+Enter/Ctrl+Enter; Cancel for the exact operation. `editor.target` is in that toolbar and shows profile/driver/redacted endpoint/database-or-Redis-db/TLS.
- Trimmed non-empty selection wins as a user-declared target boundary; invalid selection never falls back. Unambiguous multiple statements reject. Without selection, MySQL chooses the executable scanner span containing the UTF-8-safe caret, while Redis chooses only the caret physical line, trims it, and shell-parses it.
- Execute row default/cap is 500/10,000 at `editor.row_limit`; Execute timeout default/cap is 30/300 seconds at `editor.timeout`. Values are per-Execute UI state and are not persisted or reused as browser controls.

### 2. Flow

selected profile snapshot → render `editor.target` → selection/caret → pure extractor/validator → `Execute { profile_id, profile_generation, operation_id, language, text, row_limit, timeout }` → controller → T3 session/auto-connect → typed driver entry → capped result+provenance → exactly matching tuple workspace → `QueryFinished`.

For MySQL, SQLx 0.9 may negotiate `CLIENT_MULTI_STATEMENTS`; no safety claim depends on that handshake flag. Both explicit-selection and caret-derived targets become `PreparedMySqlRequest` and enter only `MySqlPreparedExecution::execute_prepared`: server `COM_STMT_PREPARE` must succeed before `COM_STMT_EXECUTE`. User text cannot reach `sqlx::raw_sql`, `Executor::execute(&str)`, `COM_QUERY`, or a fallback after prepared rejection. Prepared-unsupported returns a static UnsupportedFeature with FocusEditor + DismissError, never resubmits text, and keeps the session only when the typed outcome proves it healthy. For Redis, the typed Redis command path applies the closed policy below before session acquisition.

MySQL scanner uses the fixed P0 policy and never introspects SQL mode:

- `#` always begins a line comment. `--` begins one only when the byte following the second dash is whitespace/control; `SELECT 1--1;` is therefore not a comment.
- Ordinary `/*…*/` is semicolon-opaque and non-executable. `/*!…*/` is semicolon-opaque but executable even as a comment-only target. `/*+…*/` is semicolon-opaque and attaches to a containing statement, but is non-executable alone.
- Single/double/backtick spans are protected; doubled delimiters and MySQL-default backslash escapes are accepted. Double quotes remain protected under either ANSI_QUOTES setting. An odd backslash run immediately before a single/double quote makes caret inference return `AMBIGUOUS_SQL_MODE` and requests explicit selection; explicit selection provides the target boundary and is not expanded.
- Unterminated quote/block comment returns `UNTERMINATED_SQL_TOKEN` locally. Executable spans exclude surrounding whitespace/non-executable comments; one optional trailing terminator attaches to the prior span. Caret in a gap returns `NO_CURRENT_STATEMENT`. Character positions map to checked UTF-8 bytes.

Normative cases are executable contracts:

| Input/caret | Result |
|---|---|
| quoted `;`, doubled quote, default backslash escape, or double quotes in either ANSI_QUOTES mode | protected; one target |
| `SELECT 1# ;` then newline | `#` comment semicolon opaque |
| `SELECT 1--1;` | not a line comment; final terminator visible |
| `SELECT 1-- comment ;` then newline | line-comment semicolon opaque |
| `SELECT 1 /* ; */;` / standalone `/* ordinary ; */` | containing statement / `NO_CURRENT_STATEMENT` |
| standalone `/*!40101 SET @x=';' */;` | executable version-comment target |
| `SELECT /*+ hint; */ 1;` / standalone `/*+ hint; */` | one attached target / `NO_CURRENT_STATEMENT` |
| unterminated quote/backtick/block comment | local `UNTERMINATED_SQL_TOKEN`; no session |
| caret target with odd backslash before `'` or `"` | `AMBIGUOUS_SQL_MODE`; request exact selection |
| explicit selection after ambiguity | selected boundary only; no expansion/fallback; prepared-only MySQL entry |
| unambiguous selected `SELECT 1; SELECT 2;` | local multiple-statement rejection |
| statement unsupported by server prepared protocol | static UnsupportedFeature + FocusEditor + DismissError; no text fallback; proven-healthy session may remain |
| caret on trailing `;`, in an inter-statement gap, or after multibyte text | prior target including terminator / `NO_CURRENT_STATEMENT` / UTF-8-safe target |

Redis uses newline boundaries only. Blank/comment-only or shell parse failure returns `NO_CURRENT_STATEMENT`/Validation. Semicolon has no separator meaning. Input then passes the 65,536-byte/1,024-token/16-KiB-token caps and local ASCII-case-insensitive closed classifier before session acquisition: deny `SUBSCRIBE`, `PSUBSCRIBE`, `SSUBSCRIBE`, `UNSUBSCRIBE`, `PUNSUBSCRIBE`, `SUNSUBSCRIBE`, `MONITOR`, `SYNC`, `PSYNC`, `REPLCONF`, `WAIT`, `WAITAOF`, any `BL*`, explicit `BRPOP`/`BRPOPLPUSH`/`BZPOPMIN`/`BZPOPMAX`/`BZMPOP`, and `XREAD`/`XREADGROUP` with `BLOCK` before `STREAMS`; allow a key named BLOCK after STREAMS. Backend COMMAND metadata is never consulted.

Cancel → dedicated control lane → token cancel/drop future → compare-and-remove exact profile/session generation → join → `OperationCancelled { server_state: Unknown }`. Timeout performs the same session eviction but emits Timed out.

### 3. Side effects, errors, and boundedness

- Submitted SQL/Redis command can mutate the backend even after client cancel/timeout.
- Empty/multiple MySQL selection/target, empty/unparsable/denied Redis command, language mismatch, or invalid bounds fails in the pure service validator before session acquisition or network.
- Syntax/constraint/permission or prepared-unsupported failure keeps a session only when the typed driver outcome proves it healthy. Network/protocol/uncertain decode failure evicts it.
- A new start/failure/cancel never relabels the prior snapshot as current success.
- MySQL fetch retains at most 1,024 columns, 10,000 rows, 1 MiB/cell, and 8 MiB/snapshot; SQLx may allocate the current row/cell first. Redis conversion retains its stricter caps after redis-rs has materialized its RESP frame. These caveats use static `ResultNotice` values/receipt fields without backend text or data.

### 4. Contracts

Required pure/RawInput table includes every normative MySQL row above, selection priority/no fallback, executable version comments, standalone hints, ANSI_QUOTES conservative protection, NO_BACKSLASH_ESCAPES ambiguity, unterminated local rejection, and prepared-only dispatch despite negotiated `CLIENT_MULTI_STATEMENTS`; plus Redis quoted `a;b`, blank/comment line, every deny-family mixed case/quoted-argument edge, and XREAD BLOCK before vs key after STREAMS. A source/trait test rejects every raw or unsupported-prepared fallback. Profile A→B updates `editor.target` before B Execute and produces B's exact tuple. Also test `editor.row_limit`/`editor.timeout` focus, single-submit, Busy, cancel/timeout/races, retained caps, and honest server state.

## T5 — lazy paginated MySQL catalog and CLI browse

### 1. Entry and typed input

- GUI expands/refreshes schema, relation, or column node and uses Load more/Clear catalog.
- CLI uses the same seam:
  - `dbotter --config PATH browse mysql schemas --profile ID --page-size N [--page-token TOKEN]`
  - `dbotter --config PATH browse mysql relations --profile ID --schema NAME --page-size N [--page-token TOKEN]`
  - `dbotter --config PATH browse mysql columns --profile ID --schema NAME --relation NAME --page-size N [--page-token TOKEN]`
- Tagged `CatalogRequest::{Schemas, Relations, Columns}` carries `(ProfileId, ProfileGeneration, OperationId)`, parent identity, optional prefix, opaque keyset token, page size 1..=200 (default 50), and timeout 1..=30 s (default 5 s).

### 2. Layer flow

typed request → controller → service validation → T3 session → CatalogBrowser → level-specific bound information_schema query with deterministic binary-name/keyset order and `LIMIT page_size + 1` → retain at most page_size; extra row proves more and token derives from last retained sort key → `CatalogPage { nodes, next_token, retained_counts, retained_utf8_bytes, truncated, loaded_at }` → matching tuple event/state.

- Schemas query loads schema identity only.
- Relations query is scoped to one bound schema and loads table/view identity/type only.
- Columns query is scoped to one bound schema+relation and loads ordinal/name/type/nullability only.
- Configured database seeds the initial schema scope; no eager schema→relation→column fan-out exists.
- Identifiers used for query templates go through the tested MySQL backtick helper. Selecting a relation inserts `SELECT * FROM quoted_schema.quoted_relation LIMIT 500` and does not execute.

### 3. Caps and reachable recovery

- One page retains at most 200 nodes. Per profile: 200 schemas, 2,000 relations, 10,000 columns, 512 columns/relation, and 4 MiB total retained UTF-8 metadata bytes across names/type strings.
- `next_token` makes Load more reachable while under retained caps. Reaching a retained cap marks the exact level truncated and exposes `ClearCatalog(profile)` plus a prefix-filter field; clearing releases retained pages and the narrowed request can reach names beyond the prior cap.
- Failed refresh preserves the prior page, marks it stale, and exposes a typed Retry recipe for the exact idempotent request.

### 4. Errors, output, contracts

- Injected-driver hermetic tests map Permission and preserve/mark the old page stale. Empty page remains a successful empty page and is never fabricated as Permission.
- Live restricted metadata user proves only its allowed schema/relation/columns are present and a forbidden schema is absent; information_schema's omission is not called an error. A separate profile with an unauthorized default database (and a denied execute assertion) produces a real static Permission outcome.
- Stale profile generation/page token is rejected. Server identifiers are bound predicates, never interpolated.
- JSON CLI output includes request identity, ordered nodes, next token, retained counts, truncation/stale/timing, and no query text.
- Hermetic contracts cover dispatch, every mapping/order/token/filter, `page_size+1`, count+4-MiB cap recovery, injected Permission+stale, successful empty, quoting, and mismatch. Mandatory live covers pages/table/view/columns, cap/filter recovery, restricted allowed-visible/forbidden-absent behavior, plus separate unauthorized-default/check/execute real Permission.
- CATALOG becomes ready only in the reviewed commit whose hermetic and mandatory live contracts pass.

## T6 — Redis keyspace browse, inspect, TLS, and CLI

### 1. Entry and typed input

- GUI key explorer Refresh/Load more/filter/key selection.
- CLI uses the same seams:
  - `dbotter --config PATH browse redis keys --profile ID --filter-mode literal-prefix|glob --filter TEXT --cursor CURSOR --count N`
  - `dbotter --config PATH inspect redis key --profile ID --key-base64 BASE64`
- `RedisKeyFilter::LiteralPrefix` accepts at most 512 UTF-8 bytes, escapes `* ? [ ] \\` for Redis glob syntax, then appends `*`. `Glob` accepts at most 512 UTF-8 bytes and sends the validated pattern unchanged. UI defaults to Literal prefix and labels Glob as advanced Redis syntax.
- `RedisScanRequest` and `RedisKeyInspectRequest` carry `(ProfileId, ProfileGeneration, OperationId)`, manual redacted Debug, and no Serialize. Scan carries cursor/COUNT hint; inspect carries raw `RedisKeyId(Vec<u8>)`, never display text.

### 2. Scan flow and semantics

request → controller → Redis + KEYSPACE_BROWSE validation → T3 session → `KeyspaceBrowser::scan_keys` → exactly one `SCAN cursor MATCH pattern COUNT hint` → redis-rs RESP frame → raw byte keys → post-frame dedupe/caps → `RedisKeyPage { next_cursor, keys, skipped_oversize, retained_bytes, consistency: Weak }` → event.

COUNT is a hint: one SCAN can return zero, fewer, or more keys. Cursor zero means iteration complete only for that scan cycle; no total/snapshot count is claimed. Duplicate keys across pages are removed by raw bytes. Each retained key preserves exact raw bytes and has a separate lossy UTF-8 plus hex display. A display label is never accepted as command identity.

Retained key caps are 10,000 keys or 8 MiB raw bytes/profile and 4 KiB/key. Oversize keys are counted as unretained and never become a truncated selectable identity. `RestartRedisScan` clears page/dedupe state and starts cursor zero; a narrower filter is always reachable.

### 3. Inspect flow

raw `RedisKeyId` → TYPE + PTTL → type-specific size and preview:

- string: `STRLEN` then `GETRANGE 0 65535`, strictly server-bounded to 65,536 returned bytes;
- hash: `HLEN` plus one `HSCAN ... COUNT 100` representative page;
- list: `LLEN` plus `LRANGE 0 99`;
- set: `SCARD` plus one `SSCAN ... COUNT 100` representative page;
- zset: `ZCARD` plus `ZRANGE 0 99 WITHSCORES`;
- stream: `XLEN` plus `XRANGE - + COUNT 100`;
- module/unknown: metadata plus static Unsupported preview, without an arbitrary command.

Result retains at most 100 items, 1 MiB total, 64 KiB/cell, depth 8. String bytes are server-bounded. SCAN-family COUNT and list/zset/stream item counts do not bound individual element bytes; redis-rs can transiently allocate the whole returned frame before post-frame caps. Truncation and this qualification are explicit.

Generic Redis Execute uses the T4 65,536-byte/1,024-token/16-KiB-token input cap and closed blocking/streaming classifier, then 10,000 cells, 8 MiB retained, 64 KiB/cell, depth 8 after whole-frame materialization. It does not claim server-side response bounding.

### 4. TLS flow

Disabled → Tcp and no Redis CA field in payload. Required → redis-rs `tokio-rustls-comp` → `TcpTls { host,port,insecure:false,tls_params }`; blank `redis_tls.ca_file` uses OS roots, nonblank readable valid PEM populates root certificates. Invalid PEM stops before connect. CA-chain/untrusted-issuer failure maps to the CA code; hostname verification maps to the hostname code. Both terminate and have no edge to Tcp. Preferred terminates before registration.

### 5. Errors, output, contracts

- Invalid cursor/filter/limits/TLS is rejected before command. For saved-profile browse/inspect, invalid PEM/untrusted issuer/wrong CA exposes only `EditProfile(profile, RedisCaFile)`/`profile.redis_tls.ca_file`; hostname mismatch exposes only `EditProfile(profile, Host)`/`profile.host`. Draft Test uses the corresponding `EditDraft` actions instead. Wrong-host recovery changes host to `localhost`, retains CA, and succeeds. Scan/inspect failure keeps prior state stale. Disappearing/type-changing key is ResourceStale/NotFound, not fabricated Null.
- CLI JSON carries base64 raw identity, safe display, cursor, counts, type, TTL semantics, preview/truncation, and no key/value logs.
- Hermetic/RawInput/AX contracts cover CA hidden/clear/direct rejection, CA-vs-host PublicCode/action/focus isolation, `profile.host`, and corrected-field recovery; literal/glob, COUNT variability, raw identity, types/TTL/races/caps, and no KEYS/TLS fallback.
- Mandatory live Redis covers pages/binary key/types/TTL/string truncation/mutation plus ACL/requirepass correct+wrong Session and Environment Available/Missing/Empty on plaintext and verified TLS; wrong CA asserts CA-only recovery, wrong hostname asserts Host-only recovery and CA preservation, plain endpoint/fallback counter remains zero; all named assertions fail nonzero when unavailable.
- KEYSPACE_BROWSE becomes ready only in the reviewed commit whose hermetic and mandatory live contracts pass.

## T7 — profile result, copy, and streaming atomic export

### 1. Entry and snapshot

- Profile switch, cell/row selection, Copy cell/selected/all, Export CSV/TSV/JSON.
- Matching T4 success creates immutable capped `Arc<ResultSnapshot>` with ResultId and provenance; `completed_at` is UTC `YYYY-MM-DDTHH:MM:SS.mmmZ`. Export correlation is exactly `(ResultId, OperationId)`, while snapshot provenance retains its originating profile tuple.
- Workspaces contain only current editor/pending/result/error. Query history/recent statements do not exist.

### 2. Copy flow

Pure `clipboard_scalar(Cell)` is total: Null→empty; complete Text→literal; truncated Text→preview+`…[dbotter-truncated;original_len=N]`; Bool→`true|false`; Int/UInt/Decimal→canonical base 10; finite Float→shortest round-trip and non-finite→`nan|inf|-inf`; DateTime→normalized ISO-8601; Json→compact recursively key-sorted JSON; JsonPreview→`json-preview:<prefix>;truncated=true;original_len=N`; complete Bytes→`base64:<RFC4648>`; truncated Bytes→that preview plus `;truncated=true;original_len=N`.

Copy cell → exactly `clipboard_scalar`, preserving literal tab/CR/LF/backslash, with no header/newline. Pure `tsv_field` maps backslash/tab/CR/LF to `\\`/`\t`/`\r`/`\n` character-wise. Copy selected rows → `tsv_field` header for all visible schema columns + `tsv_field(clipboard_scalar)` cells, noncontiguous rows sorted by visible index, one final LF. Copy all → same header/field mapping + all visible rows in order + final LF. Duplicate columns remain positional; no selection/result disables action.

### 3. Export flow

`ExportResult { result_id, operation_id, Arc<ResultSnapshot>, format, destination, overwrite_policy }` (manual redacted Debug/no Serialize) → cooperative blocking worker → 0600 same-dir temp → incremental encoder → per-row/chunk cancel checks → flush/file-fsync → atomic policy commit → parent-fsync → `ResultExported` or typed failure.

There is no whole-result buffer. One export/ResultId and two process-wide; it does not consume the profile network slot. Its token/worker is registered, but the two-second async abort does not apply: Shutdown waits for real worker return/temp cleanup. Panic/JoinError cleans registry/permit/temp first and emits at most one static InternalFailure.

### 4. Exact schemas and file safety

- CSV/TSV/JSON follow `spec.md` §9. CSV+TSV always header when columns exist; JSON timestamp precision is exact. Golden cases cover every clipboard-scalar Cell variant, literal controls/backslash for Copy cell, escaped controls/backslash for row/all, duplicate/empty/Unicode/truncation, and every export Cell.
- DenyOverwrite uses macOS `renamex_np(RENAME_EXCL)` or Linux `renameat2(RENAME_NOREPLACE)`; competition barrier proves a rival destination is never clobbered.
- ReplaceConfirmed captures device/inode/size/mtime and rejects pre-commit mismatch. Pre-existing symlink/non-regular is rejected. A post-check entry swap remains an accepted local-model race; rename replaces the entry without following symlink target and no stronger claim is made.
- Pre-commit error/cancel cleans temp; post-commit cancel cannot roll back; parent-fsync may be CommittedDurabilityUnknown.
- Runtime event/log/receipt has format/count/bytes/mode/policy/outcome and no digest. Only seeded external verifier receipt stores fixture id + expected/actual digest + verdict; dynamic completed_at is independently encoded from observed provenance or normalized fixture, never by the app encoder under test.

### 5. Contracts

Pure golden bytes for every clipboard scalar and `tsv_field`, header/visible order/final LF, export timestamp; streaming; 0600; temp/failpoints; platform atomic no-clobber competition; confirmed-identity/race semantics; cancellation and blocked-worker/panic/shutdown barriers; no runtime digest; external seeded digest verdict; ResultId/OperationId isolation.

## T8 — static public errors, real recovery, and accessibility

### 1. Error flow

internal typed error + exact OperationKind/context → allowlisted conversion → total reachable-pair lookup → `PublicOperationError { category, PublicCode, PublicSummary, NonEmpty<RecoveryAction> }` → event/CLI JSON/UI. Raw source remains in the source chain and is never default-formatted at public boundaries.

`PublicSummary` variants and strings are static. MySQL errno/SQLSTATE and Redis error kind enter only validated `PublicCode`. `REDIS_TLS_CA_INVALID_PEM`/`REDIS_TLS_CA_UNTRUSTED_ISSUER` and `TLS_HOSTNAME_MISMATCH` are disjoint. Unknown values map to InternalFailure without raw text.

### 2. Recovery mapping

| PublicSummary | Reachable OperationKind/context | Non-empty actual dispatch |
|---|---|---|
| InvalidInput | Load/Reload/Migrate | ReloadConfiguration |
| InvalidInput | CreateProfile | `EditDraft(draft, exact field)`; `PROFILE_ID_CONFLICT` selects ConnectionId |
| InvalidInput | UpdateProfile | `EditProfile(profile, exact field)` or ReloadConfiguration by code |
| InvalidInput | DeleteProfile | ReloadConfiguration or DismissError by code |
| InvalidInput | TestDraftConnection | `EditDraft(draft, exact field)` |
| InvalidInput | ConnectProfile | `EditProfile(profile, exact field)` |
| InvalidInput | Execute statement/target | FocusEditor |
| InvalidInput | Execute row/timeout input | `FocusExecuteLimits(profile)` → `editor.row_limit` or `editor.timeout` by code |
| InvalidInput | BrowseMySql | ClearCatalog or DismissError by code |
| InvalidInput | BrowseRedis | RestartRedisScan or DismissError by code |
| InvalidInput | InspectRedis | DismissError |
| InvalidInput | ExportResult | ChooseExportDestination |
| CredentialRequired | TestDraftConnection | exactly one of `EditDraft(draft, SessionCredential)`, `EditDraft(draft, CredentialEnvironmentName)`, or `EditDraft(draft, Username)` by PublicCode; Forget/no-secret selects SessionCredential pre-network |
| CredentialRequired | Connect/Execute/Browse/Inspect | OpenCredentialPrompt + `EditProfile(profile, SessionCredential)` |
| AuthenticationFailed | TestDraftConnection | exactly one of `EditDraft(draft, SessionCredential)`, `EditDraft(draft, CredentialEnvironmentName)`, or `EditDraft(draft, Username)` by PublicCode |
| AuthenticationFailed | Connect/Execute/Browse/Inspect | Session prompt or typed CredentialEnvironmentName/Username profile edit; Retry only after changed state for an idempotent profile recipe |
| PermissionDenied | TestDraftConnection | `EditDraft(draft, Username)` or `EditDraft(draft, Database)` by PublicCode + DismissError |
| PermissionDenied | Connect/Browse/Inspect | Username/Database profile edit + DismissError |
| PermissionDenied | Execute | FocusEditor + DismissError; no Retry |
| NetworkUnavailable | TestDraftConnection | `EditDraft(draft, Host)` + DismissError |
| NetworkUnavailable | non-mutating saved-profile network kind | Host profile edit + Reconnect; optional exact idempotent Retry |
| NetworkUnavailable | ExecuteMutation | Host edit + Reconnect; no Retry |
| TlsVerificationFailed, CA code | TestDraftConnection | `EditDraft(draft, RedisCaFile)` → `profile.redis_tls.ca_file`; no fallback |
| TlsVerificationFailed, CA code | TLS-capable saved-profile kind | `EditProfile(profile, RedisCaFile)` → `profile.redis_tls.ca_file`; no fallback |
| TlsVerificationFailed, hostname code | TestDraftConnection | `EditDraft(draft, Host)` → `profile.host`; preserve CA; no fallback |
| TlsVerificationFailed, hostname code | TLS-capable saved-profile kind | `EditProfile(profile, Host)` → `profile.host`; preserve CA; no fallback |
| OperationTimedOut | TestDraftConnection | `EditDraft(draft, Host)` + DismissError |
| OperationTimedOut | ConnectProfile/ReconnectProfile | `EditProfile(profile, Host)` + Reconnect + DismissError |
| OperationTimedOut | Execute | `FocusExecuteLimits(profile)` → `editor.timeout` + Reconnect; mutation has no Retry |
| OperationTimedOut | Browse/Inspect | exact generation-checked idempotent Retry with auto-connect or Reconnect + DismissError |
| SyntaxRejected | Execute | FocusEditor; no Retry |
| ConstraintRejected | Execute | FocusEditor + DismissError; no Retry |
| UnsupportedFeature | legacy TLS TestDraftConnection | `EditDraft(draft, RedisTlsMode)` |
| UnsupportedFeature | legacy TLS ConnectProfile | `EditProfile(profile, RedisTlsMode)` |
| UnsupportedFeature | prepared-unsupported MySQL Execute | FocusEditor + DismissError; keep only a proven-healthy session; no text fallback |
| UnsupportedFeature | other Execute/Browse/Inspect | DismissError |
| OperationCancelled | TestDraftConnection | DismissError only |
| OperationCancelled | saved-profile network kind | Reconnect + DismissError |
| OperationCancelled | ExportResult | Choose/RevealExportDestination + DismissError |
| ResourceBusy | TestDraftConnection with safe active id | CancelOperation(active id) + DismissError |
| ResourceBusy | TestDraftConnection with no safe active id | DismissError(rejected operation id) |
| ResourceBusy | other enqueueable kind | CancelOperation(active id) + DismissError |
| ResourceBusy | other enqueueable kind with no safe active id | DismissError(rejected operation id) |
| ResourceStale | config/profile kind | ReloadConfiguration |
| ResourceStale | Browse/Inspect/idempotent Connect | Retry(exact generation-checked recipe) |
| ResourceStale | Execute | ReloadConfiguration + FocusEditor; no Retry |
| ConfigWriteNotCommitted | Migrate/Create/Update/Delete | ReloadConfiguration; migration may RevealMigrationBackup if present |
| CommittedDurabilityUnknown | config mutation | ReloadConfiguration; migration may RevealMigrationBackup |
| CommittedDurabilityUnknown | ExportResult | RevealExportDestination(ResultId) |
| ExportFailed | ExportResult | ChooseExportDestination; Reveal only for an observed committed destination |
| InternalFailure | TestDraftConnection | RestartApplication + DismissError |
| InternalFailure | every other reachable kind | RestartApplication + DismissError; config kinds also ReloadConfiguration |

`recovery_for(OperationKind, PublicSummary, PublicCode, SafeContext) -> NonEmpty<RecoveryAction>` implements this table. Draft-test SafeContext contains only `(DraftId, OperationId)` plus typed code context and can emit `EditDraft`, `CancelOperation`, `RestartApplication`, or `DismissError`; it stores no operation recipe and cannot construct saved-profile actions. Create SafeContext likewise contains `(DraftId, OperationId)` and uses `EditDraft`; `PROFILE_ID_CONFLICT` selects ConnectionId/`profile.connection_id`, while auto-slug collisions allocate a suffix. Every unlisted combination is rejected as unreachable and compile/table-tested. Mutation SQL/Redis commands never receive automatic Retry. Bounded retry recipes exist only for idempotent connect/browse/reload. `FocusExecuteLimits` is exclusive to Execute and focuses `editor.row_limit` or `editor.timeout`. Reveal actions derive paths from trusted config/export state and carry no path; RestartApplication orders shutdown then relaunch; DismissError changes only local visibility.

### 3. Accessibility flow and numerical gates

- Every P0 widget has a stable author id verified as macOS AXIdentifier. Required ids include the prior inventory plus `profile.connection_id`, `editor.target`, `editor.row_limit`, `editor.timeout`, `profile.host`, `profile.redis_tls.ca_file`, `profile.redis_tls.ca_file.pick`, `profile.credential.session.keep`, `profile.credential.session.replace`, `profile.credential.session.forget`, and `profile.delete.active_warning`.
- The first P6 RED step is a compile-only egui 0.35 harness using `Context::run_ui(RawInput, …)`. Every headless test calls `enable_accesskit()`, executes real frames, and inspects `FullOutput.platform_output.accesskit_update`.
- AccessKit assertions require role/name/author id/focus/order/enabled state. Installed automation reads each author id back as the same macOS AXIdentifier before using it.
- Palette tests calculate WCAG relative luminance and assert 4.5:1 normal text, 3.0:1 large text, 3.0:1 focus indicators, and 3.0:1 essential component boundaries for every actual state palette.

### 4. Security and recovery contracts

- Sentinel secrets and backend prose are absent from every manual Debug, log, public error/string, clipboard, AccessKit node, and receipt. User-owned SQL/Redis text, result values, Redis key display, CA path, and export destination must be present in exactly their intended rendered control and AX value node; result values may enter clipboard/file only after explicit Copy/Export. The same values are absent from manual Debug/log/error/receipt, unrelated rendered text, AX names/descriptions/live announcements, and unrelated nodes. Secret fields are masked/protected and their AX value never exposes the sentinel. Compile-fail tests prove no Serialize; final-Arc drop proves only dbotter-owned zeroization.
- RawInput/AccessKit tests cover first run; Create explicit-id collision → `EditDraft(draft, ConnectionId)`/`profile.connection_id` and auto-suffix success; every Session intent including restart/no-Arc; retained Replace Test→Save&Connect; Keep/Forget with Forget/no-secret `EditDraft(draft, SessionCredential)`; Test invalidation; CA-vs-host draft/profile code/action/focus/fix with `profile.host`; `FocusExecuteLimits` targeting only `editor.row_limit`/`editor.timeout`; exhaustive recovery table and draft-test identity isolation; Busy/Cancel; every clipboard scalar/export; intended-value presence/unrelated-node absence; disabled Mongo/legacy TLS; keyboard flow.

## T9 — restart and credential availability

### 1. Flow

Process A loads version 1 read-only → user confirms the fixed migration backup → the first committed Create/Edit/Delete writes the full config as version 2 and preserves the exact original bytes at `<config>.v1.bak` → controlled T3 shutdown → Process B resolves the same explicit `--config` path → T0 `load_path` reads version 2 → fresh runtime generations/cache/secret/workspaces → disconnected profile snapshots.

- Persisted profile fields, `CredentialMode`, and Redis `redis_tls.ca_file` survive exactly in version 2.
- Session secret, results, editor/workspace, pending operations, retry recipes, catalog/key pages, and tombstones have no persistence path.
- Session profile becomes Needs credential. KeepCurrent is disabled because no Arc exists; Replace is the default intent and Forget remains available. Connect opens the protected prompt.
- Environment profile probes only name availability and reports Available/Missing/Empty; it never displays or persists the value.
- None profile remains connectable without prompt.
- A frozen fixture of the current version-1 reader rejects the version-2 file with `UnsupportedVersion(2)` before service construction, secret resolution, cache lookup, or network access. It is a rejection contract, not an old-binary readability claim.

### 2. Errors and contracts

Temp artifact, invalid version, or unreadable path is a visible error, never empty config. CommittedDurabilityUnknown from process A is resolved by observed process-B bytes.

Temporary-path restart contracts cover version-1 read-only normalization, migration confirmation/cancel, exact no-replace backup bytes/mode/fsync, first-mutation version-2 bytes, frozen-reader rejection before network, exact config-contract output, all credential modes/intents, Redis CA persistence, Session Keep-disabled/Replace-default/Forget behavior, Environment availability transitions, no workspace/history persistence, temp cleanup, and shutdown joins. Manifest/release/tap rollback preflight rejects a missing/mismatched config contract. The installer/rollback wrapper owns backup-runbook presentation.

Direct old-binary invocation only returns UnsupportedVersion.

## T10 — CI, per-arch preview bundle, tap, installed CLI, and AX journey

### 1. Source and build identities

Verification accepts exactly one typed source identity:

- `SourceIdentity::LocalAttached { commit, branch, clean: true }`: physical repo root, attached symbolic branch, exact HEAD, tracked required inputs, and clean status including untracked files.
- `SourceIdentity::CiExpectedSha { commit, expected_sha, run_id, run_attempt }`: detached HEAD is allowed only in CI; `commit == expected_sha == workflow candidate SHA`, required inputs are tracked, and checkout is clean before generated artifacts.

Build identity records source commit, target triple, profile/features, rustc/Cargo versions, and workflow identity. Source equality is required across source commit, embedded build source, tag target, manifest `source_sha`, and tap-dispatch `source_sha`.

`dbotter version --format json` is the only machine-readable binary identity command. The source-built binary, packaged embedded executable, Homebrew shim target, and installed app executable each return exactly `{package_version,channel,build_id,source_sha,target,arch}`; missing, extra, renamed, or disagreeing fields fail verification.

Compatibility uses a separate pure command. Each of those binaries also runs `dbotter config-contract --format json` and must return exactly `{read_versions:[1,2],write_version:2,migration_backup_suffix:".v1.bak"}`. Its fields never enter or extend the six-field identity object.

Transformed hashes form links, not equality claims:

`unsigned target binary hash` → package/sign step → `post-sign embedded executable hash` → app/archive hash → manifest artifact entry hash → downloaded archive hash → installed app/archive extraction → installed post-sign executable hash.

The manifest asserts equality only for the same bytes at adjacent download/install checks. The unsigned hash is provenance input and is not asserted equal to the signed executable or archive hash.

### 2. Package and manifest flow

- Build separate macOS arm64 and x86_64 `Dbotter Preview.app` bundles with id `ai.2lab.dbotter.preview`, the approved icon, and stable AX ids. `CFBundleShortVersionString` is exactly Cargo `package_version` (`x.y.z`); `CFBundleVersion` is exactly numeric `<run_id>.<run_attempt>`. The long Homebrew version `YYYY.MM.DD.HHMMSS.<run_id>.<run_attempt>` is separate and never written into either bundle-version field.
- Sign each bundle, verify `codesign --verify --deep --strict`, then hash the post-sign embedded executable and archive. The release archive never installs an unsigned duplicate.
- `dbotter.preview-manifest.v1` contains `{tag, source_sha, version, package_version, config_contract, run_id, run_attempt, created_at, artifacts[]}`, where `config_contract` equals the exact command object. Each artifact entry contains `{target, arch, kind, url, bytes, sha256, embedded_executable_sha256, bundle_id, bundle_short_version, bundle_build_version}`. `plutil` and manifest validators reject a non-`x.y.z` short version, a build version other than the exact numeric two-component run tuple, use of the long Homebrew version in either field, or a missing/mismatched config contract.
- Preview version is `YYYY.MM.DD.HHMMSS.<run_id>.<run_attempt>` using UTC seconds and numeric GitHub ids. A publish validates it is strictly greater than the current tap preview version.
- Preview workflow dispatches the tap with explicit `{tag, source_sha, version, manifest_url, manifest_sha256}`. Tap verifies the tag resolves to source SHA, downloads/validates manifest/per-arch hashes, and executes/compares the typed config-contract during rollback preflight before atomically updating architecture-specific URLs/checksums/version.
- Formula installs the matching per-arch app and creates `bin/dbotter` as a shim/symlink to that app's post-sign `Contents/MacOS/dbotter`. Installed identity proves both paths resolve to the same inode/bytes.

### 3. Verification and publish graph

candidate source → exact identity + config-contract/release-contract/fmt/clippy/hermetic/RawInput/AccessKit/export/config failpoint/prepared-only source tests → Compose MySQL + Redis ACL/requirepass over plaintext and verified TLS → explicit MySQL/Redis live auth, browse/execute, and MySQL marker/no-fallback tests → source/build receipt → per-target builds → per-arch signed bundles → manifest/security receipt → preview release → explicit tap config-contract preflight/dispatch → Brew update/upgrade → installed CLI identity/config-contract/check/exec/browse/inspect → exact-app installed AX golden journey → installed receipt.

CI, preview, and stable workflows call the same reusable verify gate. Every build/publish/tap job has a hard dependency on successful required gates. Missing fixture/env is a non-zero live failure. Stable workflow is not invoked.

The live authentication matrix separately covers MySQL and Redis ACL/requirepass on plaintext and verified TLS: correct and wrong Session credentials, Environment Available/Missing/Empty, static Authentication code/action, and successful recovery. It does not infer Redis TLS authentication coverage from a certificate-only connection.

The mandatory MySQL safety matrix seeds an empty marker table. The UI locally rejects an explicit unambiguous two-statement selection; prepared-adapter variants sourced from explicit-selection and ambiguous/current-target entry points send `INSERT INTO marker VALUES ('first'); INSERT INTO marker VALUES ('second')` only to `COM_STMT_PREPARE`, require rejection, and assert both markers absent. A separately prepared-unsupported statement proves static recovery, no raw fallback, and proven-healthy-session retention only.

### 4. Installed CLI receipt

From Homebrew's actual shim and an isolated explicit config:

- `dbotter version --format json` returns the exact six-field identity and agrees with the package/manifest/current-architecture entry;
- `dbotter config-contract --format json` returns the exact independent three-field contract and agrees with source/manifest/release/tap records;
- `dbotter --config PATH check ...` and `exec ...` prove MySQL/Redis execution;
- T5 `browse mysql schemas/relations/columns` proves lazy catalog;
- T6 `browse redis keys` and `inspect redis key --key-base64` prove keyspace paths;
- receipt compares installed executable hash with the per-arch manifest's post-sign hash and proves the shim points to it.

### 5. Installed AX golden journey

The verifier requires an explicit `--app-path` equal to `$(brew --prefix dbotter-preview)/Dbotter Preview.app`; bundle id is an expected identity, never a standalone launch selector. It first terminates or rejects every stale process with bundle id `ai.2lab.dbotter.preview`, launches that exact path with an isolated `--config`, and before any AX input proves that the new PID's executable realpath, device, inode, SHA-256, and bundle id match the installed post-sign manifest entry. It then drives stable author ids and reads each back as the same macOS AXIdentifier:

1. Empty state → New MySQL Session: an occupied explicit id returns `PROFILE_ID_CONFLICT`, `EditDraft(draft, ConnectionId)`, and `profile.connection_id`; an auto id collision chooses the next suffix. KeepCurrent is unavailable, Replace is default, wrong Replace Test fails with `EditDraft(draft, SessionCredential)` and retains the masked buffer, correction succeeds, and Save & Connect requires no re-entry. Edit with an Arc defaults KeepCurrent and shows only “set”; its Test clones read-only. Replace and Forget paths prove required field vs hidden field, exact Save mappings, and Forget/no-secret CredentialRequired + `EditDraft(draft, SessionCredential)` before network. Restart proves Keep disabled/Replace default/Forget available. Repeat relevant auth assertions for Environment and Redis plaintext/TLS.
2. Browse schema → relation → columns over multiple pages; execute every normative MySQL case (`#`, conditional `--`, ordinary/version/hint comments, ANSI_QUOTES-protected double quotes, doubled/default backslash escapes, unterminated local rejection, AMBIGUOUS_SQL_MODE→explicit selection, unambiguous multi-statement rejection, UTF-8/trailing/gap). The fixture seeds an empty marker table: the UI rejects explicit unambiguous `INSERT INTO marker VALUES ('first'); INSERT INTO marker VALUES ('second')`; prepared-adapter cases sourced from explicit-selection and ambiguous/current-target paths require `COM_STMT_PREPARE` rejection and both marker rows absent, especially `second`. A prepared-unsupported statement returns static UnsupportedFeature + FocusEditor/DismissError and proves no fallback. Switch target A→B and assert `editor.target` plus exact Execute tuple in one frame; exercise invalid row/timeout and timeout recovery through `editor.row_limit`/`editor.timeout`; Cmd+Enter runs once.
3. For every Cell variant, controls/backslash, Unicode, and truncation, assert Copy cell equals exact `clipboard_scalar` with literal controls/no header/newline. Assert row/all `tsv_field` escaping, header/all visible columns, noncontiguous visible-index order, and final LF. Export CSV/TSV/JSON and let only the independent seeded verifier record digests. Assert mode 0600, no-clobber competition, deny overwrite, confirmed replacement, and symlink/non-regular rejection.
4. Start cancellable work; Cancel remains responsive; verify Unknown server state, exact-session eviction, then reconnect and continue.
5. Trigger each reachable PublicSummary row and assert `NonEmpty` real dispatch; reject unreachable combinations. For every reachable draft-test row assert the action carries the originating DraftId and no saved-profile action or stored operation recipe exists. Fix syntax/auth/network cases successfully. Assert secrets/backend prose absent everywhere; intended SQL/Redis/result/key/CA/export values present only in their rendered field and AX value node, absent from names/descriptions/live announcements/unrelated nodes and receipts; secret AX value remains protected.
6. Add Redis Disabled; SCAN Literal prefix and Glob over multiple pages, Load more, inspect string/hash/list/set/zset/stream TTL/previews, run a permitted mutation, and prove a locally denied streaming/blocking command acquires no session.
7. Add Redis Required against the hostname-valid TLS+auth fixture. Invalid PEM/untrusted issuer/wrong CA must emit CA code and focus `profile.redis_tls.ca_file`. In a separate case, wrong hostname emits `TLS_HOSTNAME_MISMATCH` and focuses `profile.host`; change only host to `localhost`, retain the original trust configuration, connect, and assert plaintext-fallback count zero.
8. Start active work, open Delete, and assert the dialog names the static OperationKind and warns that the server operation may continue. Cancel has no side effect; confirmed commit publishes the tombstone before cancel/join/evict and reports server state Unknown. Disconnect/reconnect another profile, then controlled shutdown and restart.
9. After restart, the version-2 config persists deletion and CA/mode fields; Session shows Needs credential with Keep disabled/Replace default/Forget available; Environment demonstrates Available/Missing/Empty without value exposure; reconnect after recovery. Migration backup, frozen v1-reader rejection, and exact config contract were already proved before publish.
10. Verify MongoDB and legacy Redis Preferred remain disabled/non-runnable.

The installed receipt records the requested/resolved app path, launch PID, executable identity, bundle/manifest artifact id, stale-process disposition, exact identity object, exact separate config contract, AX ids/action/verdicts (including Session intent, Host-vs-CA recovery, recovery-totality, and disclosure-boundary verdicts), formula/version, safe export metadata, safe error categories/codes, and timings. It records no user-owned field values. Runtime receipts contain no export content digest; only the external seeded-verifier subsection contains expected/actual digests and never bytes or dynamic user data.

### 6. Failure and rollback

Dirty/mismatched source, expected-SHA mismatch, test/live/contrast/accessibility failure, secret scan hit, wrong package/hash, non-monotonic version, tap input mismatch, Brew failure, installed CLI mismatch, or AX journey failure makes overall false and prevents completion.

Rollback creates a new preview only from a last-known-good source whose `dbotter config-contract --format json` output exactly matches manifest/release/tap preflight, with a new tag and strictly higher version. A missing/mismatched command is rejected before build/publish. The installer/rollback wrapper owns backup-runbook presentation. Rollback regenerates manifest/artifacts and dispatches normally; it never mutates/moves a tag, reuses an artifact under new metadata, or lowers formula version.

Direct old-binary invocation only fails closed with UnsupportedVersion.

## 4. Trace conformance record

No implementation deviation exists because production implementation has not started. This revision additionally makes Create recovery DraftId/ConnectionId-correct, fixes the registry to tagged `TaskScope`, replaces the dead generic limits action with Execute-only focus ids, and makes MySQL user execution prepared-only despite SQLx handshake capability. It also keeps TLS CA/Host recovery disjoint, defines every clipboard scalar/TSV field, separates the exact config contract from six-field identity, fixes the full MySQL boundary scanner/SQL-mode limitation, adds every SessionCredentialIntent path, makes recovery total with NonEmpty actions, and permits user-owned values only in intended rendered/AX value nodes. Prior v2 migration, shutdown, correlation, bounds, export, AccessKit, packaging, app-path, and rollback contracts remain. Any later change is recorded here before code as ADDED, MODIFIED, REMOVED, or RENAMED with migration and contract impact.
