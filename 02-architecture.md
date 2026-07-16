# dbotter — architecture

Status: **delivered usable-MVP baseline; Daily-use v1 extension is governed by D1–D12**

Current normative behavior lives in `docs/daily-use/{spec,trace,plan}.md`.
`docs/usable-mvp/` preserves the old P/T checkpoint evidence and hashes; it is
historical, not current scope or status authority.

## 1. Architectural decision

dbotter remains one Rust package with one library and one binary. The native UI
and headless CLI share `ApplicationService`; neither duplicates profile lookup,
credential resolution, capability validation, connection lifecycle, resource
browsing, execution classification or public-error conversion.

```text
native UI commands ----+
                        +-> bounded runtime/controller -> ApplicationService
installed CLI ----------+             |                       |
                                      |                       +-> config/secrets
                                      |                       +-> workspace store
                                      |                       +-> session resources
                                      +<- correlated events <-+-> MySQL / Redis
```

The UI owns display and input state. Live sessions, the transaction worker, task
registry, config/workspace writers, secret resolution and filesystem workers
stay behind typed service/runtime boundaries. No Rust mutex/RwLock or borrowed
in-process guard crosses `.await`. A synchronous component may retain the owned
file descriptor for the nonblocking OS advisory safety lease; it guards
cross-process ownership, not in-memory state awaited by a task.

## 2. Delivered baseline

At baseline commit `340133dca652a7bf51d652f06cdb7436b42bbc58` the
integrated source already provides:

- atomic v1/v2-era profile configuration and non-persisted
  None/Session/Environment credential modes;
- native MySQL/Redis profile lifecycle, connection recovery and exact
  operation correlation;
- prepared-protocol MySQL execution and policy-checked Redis command execution;
- lazy bounded MySQL schema/relation/column browsing and Redis SCAN/inspect;
- bounded result snapshots, copy and no-clobber CSV/TSV/JSON export;
- native accessibility identifiers plus Preview/package/tap/install
  verification machinery.

Those are foundations, not proof of Daily-use completion. In particular, the
baseline has a pooled MySQL auto-commit execution path, one memory-only
editor/result per profile, no table-row/Redis explorer mutations, no import and
no clean-install CLI profile bootstrap.

## 3. Identity and async ownership

Identity domains remain distinct:

- saved-profile work: `(ProfileId, ProfileInstanceId, ProfileGeneration, OperationId)`;
- draft profile work: `(DraftId, OperationId)`;
- editor/result work: `(ProfileInstanceId, EditorTabId, ResultTabId, OperationId)`;
- transaction work: `(ProfileInstanceId, ProfileGeneration, TransactionId, TransactionOperationId)`;
- staged/import/Redis reviews: their typed local ID plus profile generation and
  operation ID;
- Redis uncertain-mutation recovery: `(ProfileInstanceId, OperationId,
  HmacKeyToken)` without raw key/payload bytes;
- export: `(ResultId, OperationId)`.

Runtime uses monotonic generations and compare-matched session eviction. Every
completion is folded only when its complete identity is still current. A stale
or cancelled event cannot replace a newer tab, result, transaction, stage,
review or connection state.

The bounded task registry owns cancellation and joins. Control work remains
independent from ordinary network admission so Cancel/Disconnect/Shutdown can
make progress. A blocking file/parser worker checks cancellation at bounded
chunks and is joined before cleanup is reported complete.

## 4. Configuration, credentials and local durability

The config path is resolved once by the existing global precedence. Daily-use
D1 extends the writer to config v3:

- reads versions 1, 2 and 3, writes only version 3;
- legacy data normalizes in memory without startup mutation;
- first confirmed legacy mutation creates the exact source-version backup and
  uses the existing atomic/no-clobber durability boundary;
- environment/access posture is non-secret profile data;
- credential values remain only in credential channels and never serialize.

D4 owns `<full-config-path>.workspace-v1/`. Config v3 gives every profile an
immutable CSPRNG `ProfileInstanceId`; legacy profiles are Unclassified/read-only
until explicit migration assigns posture and instance IDs. Profile and orphan
shards are validated by embedded instance identity. `index.json` is a
derived cache. A single durable lifecycle journal orders duplicate,
delete→purge-or-orphan and clear operations across config/shards/index; startup
replays a valid intent idempotently and blocks destructive guesses for a corrupt
intent.

The per-instance safety union is driver-specific. MySQL Begin writes Active,
terminal intent writes Resolving, a proven server terminal writes
TerminalProven for replayable shard fanout, and uncertainty writes
OutcomeUnknown, all with the stable TransactionId. TerminalProven is the
Committed/RolledBack replay authority; OutcomeUnknown is independently the
Unknown shard/history/result/stage/import replay authority and cannot be
acknowledged or removed until its idempotent TransactionId fanout is fsynced
and any live state is folded. Redis mutation writes RedisApplying before wire
dispatch and response uncertainty writes RedisOutcomeUnknown with only an
HMAC key token and action class. These fences survive editor/history opt-out
and general workspace clearing. Startup conservatively converts in-flight
fences to their driver Unknown state; MySQL requires durable acknowledgement,
while Redis requires a matching-key fresh inspection plus explicit durable
acknowledgement. A Redis fence blocks profile update/duplicate/delete until
matching-key recovery or a separately typed unknown-recovery abandonment.
Neither driver automatically retries uncertain mutation.

All files use private permissions, same-directory atomic replacement,
file/parent durability and destination-fingerprint conflict detection. Unknown
versions, symlinks, corrupt identities and uncertain commit state fail closed.

## 5. Execution admission

D3 uses two pure admission phases. Before a lease or any network I/O, the
bounded source reader enforces total UTF-8/file/stream limits and the
mode-independent forbidden-comment scan. The service then acquires the exact
intended MySQL lease and performs only the static typed capability query. Its
sql_mode drives statement splitting and lexing; the pure mode-aware parser then
enforces per-target/token/depth limits and produces the complete closed typed
batch before user-target 1. Redis needs no capability query, so both phases
finish before its first network I/O.

MySQL P0 first proves an exact official Oracle MySQL 8.4 session with utf8mb4,
UTC and known sql_mode. The same typed capability result decodes
`@@GLOBAL.partial_revokes`; typed reads may remain available when it is ON, but
every mutation requires exact OFF plus complete direct global metadata
visibility. It admits only:

- SELECT whose recursive AST uses the frozen PureBuiltin table and references
  only catalog-proven base tables; and
- single-table INSERT/UPDATE/DELETE proved against trigger-free InnoDB catalog
  metadata after exact partial_revokes=OFF and direct non-role-only global
  SELECT/TRIGGER/REFERENCES visibility proof.

Views, raw SHOW/DESCRIBE/EXPLAIN/ANALYZE, REPLACE, raw transaction/session
controls, executable/hint/MariaDB comments, implicit-commit forms, DDL/admin,
unsupported functions, stored/loadable/UDF calls, multi-table or unbounded DML
and ambiguity are denied before user-target I/O. The intended connection's
exact version/family/charset/time-zone/sql_mode drives the lexer. A Read-only
profile may acquire that metadata-only lease, but mutation has zero user-target
dispatch. Run-all is fully parsed/classified and all relations/writable targets
are metadata-preflighted before target 1. Shared metadata locks are retained by
the active worker so ALTER or relevant inbound FK/trigger drift cannot
invalidate the complete proof.

Redis raw execution uses the exact command/arity/option table in DU-03.
Explorer writes are not raw editor text: they are typed DU-07 operations backed
by one-key static atomic scripts.

## 6. MySQL data and transaction architecture

D2 extends catalog ownership with index/engine/trigger/identity metadata and a
typed table-data request:

- identifiers come only from validated catalog identity and are quoted;
- filter values are typed parameters, never SQL fragments;
- page tokens bind profile/generation/relation/filter/sort/limit/cursor and
  catalog fingerprint;
- a usable key selects keyset paging; bounded keyless fallback is explicitly
  unstable and stops at its fixed offset cap.

D5 replaces pooled mutation semantics with one serialized connection worker per
active profile transaction and a stable CSPRNG TransactionId:

```text
AutoCommit
  -> ActiveClean
  -> ActivePending
  -> Resolving(Commit|Rollback)
  -> AutoCommit | OutcomeUnknown
```

The worker initializes utf8mb4/+00:00 and proves autocommit=1/in_transaction=0
before Begin. All editor reads/DML, table reads, row Apply and CSV import inside
an active transaction use that same worker/connection. DDL/session controls
remain outside the model. No GUI DML reaches an auto-commit connection.
Terminal requests use explicit AND NO CHAIN NO RELEASE and prove
in_transaction=0 even after a success response. Results/history distinguish
statement success from AppliedPendingTransaction. After a proven outcome the
worker persists TerminalProven, idempotently rewrites/fsyncs the profile shard
by TransactionId, folds memory, then removes the fence. Startup replays any
TerminalProven, so separate files are never assumed cross-file atomic; earlier
Resolving crashes become OutcomeUnknown. That fence similarly replays an
idempotent TransactionId fanout to durable Unknown plus the live-memory fold;
status, restart and acknowledgement rerun it, and acknowledgement cannot remove
the fence until durable agreement. A failure at any conversion, fanout or
removal phase remains blocking and replayable rather than leaving Pending
history without an authority.

D6 stages row changes locally using the closed lossless MysqlInputCell families
and applies one reviewed batch inside an internal operation savepoint.
Update/delete re-read and binary-compare/lock the retained identity/original on
the transaction connection before typed DML. Add supports only a reviewed
supplied usable identity or one numeric single-column AUTO_INCREMENT primary
key recovered from same-connection insert metadata. Generated columns remain
unwritable and Update cannot change its identity key. Any row-N
error/conflict/cancel/refresh failure proves rollback to the savepoint or enters
ApplyOutcomeUnknown; partial Apply is never presented as local/discardable.
Local/unknown/applied stage surfaces are lifecycle-guarded and non-evictable.
Apply never commits; only shared controls resolve AppliedPendingTransaction.

D8 parses/maps CSV through the same MysqlInputCell contract on a bounded
blocking lane and applies parameterized batches only to a completeness-proven,
trigger-free InnoDB table inside an operation savepoint on the same worker.
Error/cancel proves rollback to that savepoint or raises OutcomeUnknown; earlier
profile transaction changes are preserved.

## 7. Redis mutation architecture

D7 retains raw key/payload bytes only in memory and a Missing-or-present
inspection revision. Static one-key Lua inspection/mutation scripts run an O(1)
STRLEN or cardinality<=512 gate before capped exact MEMORY/DUMP proof, then
recheck revision/old member/value and apply exactly one frozen
String/Hash/List/Set/Sorted Set/TTL/DeleteKey action. Before target mutation,
redis.acl_check_cmd validates the exact action and recovery commands without a
second/probe key. Missing create/add paths use a no-DUMP revision.

Read-only rejects before dispatch. Review confirmation binds the exact decoded
payload, key/revision, profile/generation and expiry. The durable RedisApplying
fence precedes wire dispatch; response/restoration uncertainty becomes
RedisOutcomeUnknown with only an HMAC key token. Startup keeps all profile
mutation blocked until matching-key fresh inspection and explicit acknowledgement.
Lossy rendered text is never write identity.

## 8. Result, CLI and UI ownership

D9 replaces one overwritten result with bounded result tabs that share immutable
`Arc<ResultSnapshot>` data. Per-result and aggregate caps evict only inactive,
terminal, unprotected results; active or stage/review/unknown-owning surfaces
are never silently removed. Grid/record/value detail,
local filter/sort, copy and export operate only on retained data and disclose
truncation. Result target metadata is governed by editor-text persistence; when
that category is off/purged, no result target or rerun placeholder is stored.

D10 keeps profile commands, stdin/file/text target acquisition, output encoding
and numeric exits as thin adapters over shared model/service contracts. CLI DML
is one-process only: fully preflight, Begin, execute all, then prove the selected
whole transaction outcome. A partial failed batch can never Commit.

D11 uses the named local UI/UX OpenAI reference for visual language: white/black
opacity hierarchy, black-inverted primary action, square corners, no decorative
gradients/shadows, stable vector icons and restrained/reduced motion. DBeaver's
first four GitHub README screenshots provide the separate interaction-structure
floor: a persistent object navigator, object/editor and result tabs, bounded
resizable editor/result/detail panes, directly available daily action bars and
an always-readable connection/transaction/operation status strip. Per-profile
layout geometry and tab order are bounded durable state; invalid geometry resets
to safe defaults. Discrete actions retain 44-point targets while dense native
tree/grid collections use full-row keyboard-accessible selection. The
user-provided dbotter artwork remains the app/title icon. Every canonical flow
is operable at 1,440×900 and 840×560, with stable accessibility identity/state,
correlated background work and exact cancellation where possible. The public
wide/min screenshot matrix is captured only from the isolated tracked synthetic
fixture/temp config after AX allowlist/sentinel checks and raster metadata
stripping. GIS and ERD rendering remain P1 and no fake controls are shown.

## 9. Public error and privacy boundary

Backend prose and user data do not cross the static public-error boundary.
Sensitive request types have manual redacted `Debug` and no serialization.
Query/key/cell/CSV content appears only in the intended UI/clipboard/export or
the disclosed local editor/history store. Public visual evidence is the sole
exception and contains only exact tracked synthetic fixture values from an
isolated process; it never reads the user's config.

Credential channels are excluded from config/workspace/history/log/error/
evidence. Valid arbitrary editor/history literals may contain secrets or PII;
the first-run disclosure, private file boundary and independent per-profile
editor/history opt-outs are the protection. The exact conservative credential
form classifier redacts matched and malformed execution attempts.

## 10. Ownership and conformance

The expected source ownership table is authoritative in
`docs/daily-use/trace.md` §5; mutable RED/GREEN/live/native status belongs in
`docs/daily-use/evidence.md`. Cross-layer behavior changes update the frozen
trace before code. A file's presence never proves a D row.

D12 binds the reviewed source commit to CI, Preview tag/artifacts/checksums, tap
formula, xbrew install receipt and installed binary/AX identity. Preview is the
only authorized channel; stable publication requires a separate user approval.

dbotter remains Apache-2.0. Competitor products are behavior research only;
their implementation code is not copied.
