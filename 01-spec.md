# dbotter — usable MVP product contract

Status: **P1 foundation independently reviewed GREEN. T0 remains RED overall;
T1, T2, T8, and T9 are Implementing; T3–T7 and T10 are Not started.** This file
describes the approved target and the bounded P1 implementation checkpoint.

## Authority and change control

The frozen approval set is normative:

- `docs/usable-mvp/spec.md` — product and wire contract;
- `docs/usable-mvp/trace.md` — cross-layer vocabulary, T0–T10 flows, and task
  ledger;
- `docs/usable-mvp/plan.md` — P0–P9 order, RED contracts, live gates, and
  delivery proof.

Their approved SHA-256 values are recorded in `04-patch-plan.md`. This document
reconciles that set into the repository entrypoint. If a summary here appears
less specific, the frozen approval set wins. Contract changes must update the
trace before production code.

## Product boundary

dbotter is a local, single-user Rust desktop database client. The usable MVP is
complete only when a developer can perform the full installed journey:

1. create or edit a non-secret MySQL or Redis profile;
2. choose an explicit credential source and test an unsaved draft;
3. connect, disconnect, reconnect, and delete truthfully;
4. browse MySQL schemas/relations/columns or Redis keys/values lazily;
5. execute one selected/current target, understand static failures, and recover;
6. copy or atomically export a bounded result;
7. restart and observe honest credential availability;
8. repeat the journey through the Homebrew-installed preview app and CLI with
   source, artifact, process, accessibility, and receipt proof.

MongoDB remains visibly Planned with no live connect/query path. DBeaver is
behavior/product research only; no DBeaver PRO Redis or MongoDB source is
copied.

## Implementation state

| Trace | Target | Current state |
|---|---|---|
| T0 | exact-path v1 read-only load, v2 migration/reload, first run | RED overall; P1 config GREEN, P6 RawInput/AX missing |
| T1 | Create/Edit, credential intent, side-effect-free draft test | Implementing; P1 core GREEN, P6 remains |
| T2 | confirmed atomic delete | Implementing; P1 core GREEN, P2/P6 remain |
| T3 | controller, reload, connect/disconnect/reconnect/shutdown | Not started |
| T4 | exact target, prepared-only MySQL/Redis execute, cancel | Not started |
| T5 | lazy paginated MySQL catalog | Not started |
| T6 | Redis SCAN/inspect and verified Required TLS | Not started |
| T7 | profile result, exact copy, streaming atomic export | Not started |
| T8 | static errors, total recovery, native accessibility | Implementing; P1 core GREEN, P6 remains |
| T9 | restart and credential availability | Implementing; P1 core GREEN, P2/P6 remain |
| T10 | gated CI/release/tap/Brew/installed golden journey | Not started |

Existing demo code or historical release artifacts are not evidence that a row
is GREEN or Verified. Status moves only through the ledger rules in
`03-traces.md` and the evidence gates in `04-patch-plan.md`.

## Product invariants

### Profiles, config, and secrets

- Config version 2 is the current write schema. The new binary reads version 1
  without writing, normalizes it in memory, and writes version 2 only on the
  first confirmed committed Create/Edit/Delete.
- Before that first mutation, dbotter creates the fixed
  `<config>.v1.bak` through a 0600 same-directory temporary file, file fsync,
  atomic no-replace rename, and parent-directory fsync. It never overwrites a
  non-identical backup.
- A frozen current-v1 reader rejects version 2 with `UnsupportedVersion(2)`
  before service or network construction. The installer/rollback wrapper owns
  the backup runbook; direct old-binary invocation only fails closed.
- `CredentialMode::{None, Session, Environment}` is explicit. Persisted
  `ConnectionProfile` contains no secret value.
- Session UX uses `SessionCredentialIntent::{KeepCurrent, Replace, Forget}`;
  accepted persistence uses `SessionSecretUpdate::{Keep, Replace, Clear}`.
  Replace owns a `Zeroizing<String>` form buffer, Keep clones an existing
  `Arc<SessionSecret>` under lock and unlocks before await, and Forget clears.
- `SessionSecretStore` is `HashMap<ProfileId, Arc<SessionSecret>>`. Secret and
  sensitive request types have redacted manual `Debug`, no `Serialize`, and no
  lock is held across `.await`.
- Create carries `(DraftId, OperationId)`. Explicit-id conflict
  `PROFILE_ID_CONFLICT` recovers with `EditDraft(draft, ConnectionId)` and
  focuses `profile.connection_id`; auto-slug collision allocates the lowest
  free suffix and is not an error. Edit uses immutable `ProfileId` plus expected
  `ProfileGeneration`.

### Runtime identity and lifecycle

- Every saved-profile command/event carries
  `(ProfileId, ProfileGeneration, OperationId)`. Draft, export, and global work
  carry only their own approved identities.
- The task registry is `RegisteredTask { operation_id, scope, cancel, join }`
  with closed `TaskScope::{Profile, Draft, Export, Global}`. Only Profile scope
  contains profile/session generations.
- The runtime has a capacity-16 serialized mutation lane, capacity-32 work
  lane, capacity-16 control lane, capacity-128 event lane, one active network
  operation per profile generation, and four process-wide.
- Cancel/timeout stops client waiting, reports server state Unknown, and
  compare-removes only the exact session generation. Disconnect/reconnect,
  committed Edit/Delete fences, tombstones, reload diff, Config uncertain, and
  shutdown follow T2/T3 exactly.
- Async network work may use a two-second abort grace. Blocking export is
  cooperative and must actually return and clean its temporary file. No task is
  detached.

### Drivers, execution, browsing, and bounds

- Ready capability bits are truthful and separate: MySQL `CATALOG`, Redis
  `KEYSPACE_BROWSE`. Neither becomes ready before its mandatory live contract
  passes in the same reviewed change. MongoDB remains Planned.
- MySQL user text enters only `MySqlPreparedExecution` and server
  `COM_STMT_PREPARE` → `COM_STMT_EXECUTE`. SQLx 0.9 may negotiate
  `CLIENT_MULTI_STATEMENTS`; that flag is not the safety proof. User text never
  reaches `sqlx::raw_sql`, `Executor::execute(&str)`, `COM_QUERY`, or a fallback
  after prepared rejection.
- Explicit selection is the user-declared target boundary. Without selection,
  the fixed scanner from T4 handles MySQL comments/quotes/SQL-mode ambiguity or
  one Redis physical line. Unambiguous multiple statements reject locally.
- Execute row/timeout controls are `editor.row_limit` and `editor.timeout`.
  `FocusExecuteLimits(ProfileId)` is Execute-only; browser and inspect recovery
  uses typed clear/restart/dismiss or generation-checked idempotent recipes.
- MySQL catalog uses static/bound prepared information-schema queries,
  deterministic keyset pagination, `page_size + 1`, and retained caps. Redis
  uses SCAN-family paging, raw key identity, bounded inspection, and the closed
  nonblocking command classifier from T6.
- Redis TLS UI offers only Disabled/Required. Required verifies certificate and
  hostname with OS roots or a valid PEM CA. CA failures edit/focus only the CA;
  hostname mismatch edits/focuses only Host; neither can fall back to plaintext.
- Driver frames/rows may allocate before dbotter applies retained caps. UI and
  receipts state this limitation rather than claiming universal allocation
  prevention.

### Errors, accessibility, copy, and export

- Public failures are
  `PublicOperationError { OperationKind, ErrorCategory, PublicCode,
  PublicSummary, NonEmpty<RecoveryAction> }`. Summaries/actions are closed and
  backend prose is never rendered.
- `recovery_for` is total over reachable `OperationKind × PublicSummary` and
  rejects unreachable pairs. Draft/Create field recovery uses `EditDraft` with
  DraftId; saved-profile actions never borrow a draft identity. Mutating Execute
  never receives automatic Retry.
- Stable author ids include `profile.connection_id`, credential-intent controls,
  `profile.host`, Redis CA controls, `editor.target`, `editor.row_limit`, and
  `editor.timeout`. RawInput/AccessKit tests prove macOS AXIdentifier readback,
  roles, names, focus, order, enabled state, keyboard use, and numerical
  contrast.
- Secrets and backend prose are absent from public strings, Debug, logs,
  receipts, clipboard, and AX. User-owned SQL/Redis/result/key/CA/export values
  may appear only in their intended value node; result data reaches clipboard
  or file only after explicit Copy/Export. Secret AX values remain protected.
- `clipboard_scalar(Cell)`, `tsv_field`, CSV/TSV/JSON wire formats, truncation
  markers, field order, and final newlines are exact as specified in
  `docs/usable-mvp/spec.md` §9.
- Export streams from `Arc<ResultSnapshot>` to a 0600 same-directory temporary
  file, fsyncs, commits under explicit no-overwrite/confirmed-replace policy,
  and fsyncs the parent directory. Runtime receipts contain no content digest;
  only the independent seeded verifier records expected/actual digest verdicts.

### Distribution identity

- Preview installs `Dbotter Preview.app` with bundle id
  `ai.2lab.dbotter.preview`. The canonical executable is the post-sign embedded
  file and Homebrew's `dbotter` shim resolves to it.
- `dbotter version --format json` returns exactly
  `{package_version,channel,build_id,source_sha,target,arch}`.
- `dbotter config-contract --format json` is independent and returns exactly
  `{read_versions:[1,2],write_version:2,migration_backup_suffix:".v1.bak"}`.
- Cargo `x.y.z` is `CFBundleShortVersionString`; numeric
  `<run_id>.<run_attempt>` is `CFBundleVersion`; the increasing Homebrew preview
  version is separate. This task creates no stable tag or stable release.

## Journey-to-trace map

| Journey | Trace owner | Planned slice | Required evidence |
|---|---|---|---|
| U0 first run | T0 | P1/P6 | config + RawInput/AX |
| U1 create/test/edit | T1 | P1/P6 | credential matrices + draft isolation |
| U2 delete | T2 | P1/P2/P6 | failpoints + tombstone/order/AX |
| U3 connect lifecycle | T3 | P2/P6 | state/cache table + controller races |
| U4 exact execute target | T4 | P3/P6 | scanner + prepared-only source/live tests |
| U5 MySQL catalog | T5 | P4/P6 | hermetic + mandatory live + CLI |
| U6 Redis browse/TLS | T6 | P5/P6 | hermetic + auth/TLS live + CLI |
| U7 copy/export | T7 | P7 | exact goldens + filesystem failpoints |
| U8 errors/recovery | T8 | P1/P6 | exhaustive table + RawInput/AccessKit |
| U9 restart | T9 | P1/P2/P6 | restart contract + installed AX |
| installed completion | T10 | P8/P9 | CI/manifest/tap/Brew/process/receipt chain |

## Exclusions

Live MongoDB, query history/recent statements, editable grids, transaction UI,
SSH/proxy tunnels, imports, ER diagrams, AI, multi-tab IDE behavior, keychain
persistence, guaranteed server cancellation, and multi-process writer safety
are out of scope. Stable publication is not part of this task.

## P1 checkpoint verification

The P1 foundation is independently reviewed GREEN after 136 regular tests and
12 doctests, strict Clippy, formatting, release build, source-contract, receipt-
contract, and diff checks passed. The recorded production and test snapshot
SHA-256 values are respectively:

```text
6ccd3ded9a82384ce92b823914e1b5e9f518886460fc0df1c6455ed6d9a327a9
dfacf608d773ca16dd4d25bdf0dc5bfb8f17926baf60d63bcadb1470ffb8114e
```

Frozen approval integrity remains checked with:

```sh
shasum -a 256 docs/usable-mvp/spec.md docs/usable-mvp/trace.md docs/usable-mvp/plan.md
git diff --check
git diff -- 01-spec.md 02-architecture.md 03-traces.md 04-patch-plan.md \
  docs/release/spec.md docs/release/trace.md README.md
```

Exact checkpoint commands and hashes are recorded in `04-patch-plan.md`.
Remaining implementation and delivery commands must not be reported green
until the corresponding trace row reaches its required state.
