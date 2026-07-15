# dbotter — usable MVP product contract

Status: **P1, P2, P3, and P4 are independently reviewed GREEN. P4's MySQL
catalog mandatory live gate is GREEN, while T5 remains Implementing for P6
native/installed accessibility evidence. T0 remains RED overall; T1–T5, T8,
and T9 are Implementing; T6, T7, and T10 are Not started.** This file describes the
approved target and the bounded P1–P4 implementation checkpoint.

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
| T2 | confirmed atomic delete | Implementing; P1/P2 core GREEN, P6 remains |
| T3 | controller, reload, connect/disconnect/reconnect/shutdown | Implementing; P2 core GREEN, P6 native/AX remains; not fully GREEN/Verified |
| T4 | exact target, prepared-only MySQL/Redis execute, cancel | Implementing; P3 hermetic core GREEN, P6 RawInput/AX and mandatory live proof remain |
| T5 | lazy paginated MySQL catalog | Implementing; P4 review fixes and hermetic/live/CLI/UI core independently GREEN, P6 native/installed AX remains |
| T6 | Redis SCAN/inspect and verified Required TLS | Not started; P3 shared typed seam only, P5 remains |
| T7 | profile result, exact copy, streaming atomic export | Not started |
| T8 | static errors, total recovery, native accessibility | Implementing; P1 core GREEN, P6 remains |
| T9 | restart and credential availability | Implementing; P1/P2 core GREEN, P6 remains |
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

- Ready capability bits are truthful and separate. MySQL `CATALOG` is
  ready in the P4 implementation/review-fix chain that includes its GREEN
  hermetic and mandatory live contracts. Redis `KEYSPACE_BROWSE` remains
  planned until P5 supplies the equivalent proof. MongoDB remains Planned.
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
  deterministic keyset pagination, `page_size + 1`, and retained caps. Its page
  token is authenticated by a non-serializable/redacted HMAC-SHA256 subkey
  derived from a private persistent per-config CSPRNG root and every
  connection-fingerprint field; the MAC binds the full profile/generation/
  level/parent/prefix/page-size/cursor/count/byte context.
  Redis uses SCAN-family paging, raw key identity, bounded inspection, and the
  closed nonblocking command classifier from T6.
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

## P3 checkpoint verification

P3 is independently reviewed GREEN for the typed prepared execution/resource/
result/headless foundation. It proves pure exact-target scanning, one
prepared-only MySQL user-text entry with no text fallback, a closed Redis
command policy enforced at construction and the driver boundary, typed catalog
and keyspace models with separate planned capability bits, bounded retained
snapshots and pre-snapshot decode budgets, exact provenance, stale-page retry
state, and cancellation cleanup before exact-session close. T4 remains
Implementing because P6 RawInput/AX and mandatory execute proof remain. T5 is
now Implementing with P4 independently reviewed GREEN; T6 remains Not started until P5
implements and live-proves its capability bit. No P6 native/AX completion is
claimed.

The final checkpoint passed formatting, diff, release-contract, receipt,
strict locked/offline Clippy, all-target/all-feature tests, doctests, and the
release build: 227 regular tests plus 18 doctests, including lib 51/51,
controller 46/46, service 37/37, source 6/6, execution 16/16, resource 10/10,
and prepared-only MySQL 3/3. Two independent reviewers each reported
`NO P3 BLOCKER` against source+test review snapshot
`599917d1507df767b5b873a6d52d914d9646b9135fa51671282b4f0b884d5ecb`.
The recorded production, test, and release-binary SHA-256 values are:

```text
59a348c8a5e7f4bc63a15631cdac7be14444aebc57c84fb34ebbcb795692fec7  production snapshot
1b7a9ca40dea4994126f101dfcab1fc33fa6019b773627699c77e24167ac5b95  tests snapshot
9e43c9732be5a642873063f91a75364f9ad7f310735b17accaa3c24be0f95556  target/release/dbotter
```

## P4 independently reviewed GREEN checkpoint

P4 code commit `e4599152daf0ca066baf6619048dae89c43cc6e4` implements
three level-specific static/bound server-prepared `information_schema` queries,
binary keyset pagination, all retained count and 4 MiB caps, prefix/Clear
recovery, identifier quoting, and the bounded non-executing SELECT template.
Review RED commit `31bd052f0d550e8c9e13e4f743f245ee4be6eba2` captures the
authentication, lifecycle, continuation-context, event-identity, and contrast
blockers; fix commit `0aa007b3476a458bc83eeb241f30cc67e26e911d` closes them.
Cross-process pagination RED commit
`ede07e766be198d1140d966667857092665cba70` is closed by persistent-root
fix `f51b3618f004b64e3601ca73f8072719ac273558`. Same-path connection
rewrite RED commit `7b622757b2405d6fb2859923d5a7bf868835630b` is closed by
connection-scoped derivation fix `05ad72f20e415b44f2d90ce7d5971c3d7a75b520`.
The shared CLI and real profile-generation-scoped egui Explorer continue to use
the same service path.

The fixed page token authenticates with a key derived from a lazily created,
private 0600, race-safe 32-byte CSPRNG root sidecar adjacent to the selected
config. HMAC-SHA256 domain separation derives a per-connection subkey from a
redacted canonical digest of every `ConnectionFingerprint` field; raw profile
values enter neither token, Debug output, receipt, nor sidecar. The token also
binds profile, generation, level, parent, prefix, page size, last retained key,
retained counts, and retained bytes. Unchanged same-config services and real
CLI subprocesses accept continuations across processes, while a different
config root or a same-path connection-field rewrite fails closed. Public
SHA-256 rewrite/re-sign, tamper, and cross-profile reuse also fail closed.
Catalog cancel/outer-timeout drops the driver future before exact
acquired-session compare-remove/close, and the same typed session
generation/disposition reaches cache, event, and UI. Stale pages remain
available, while a replacement session cannot be evicted or hidden by an old
terminal event. Load more reuses the page/token's captured prefix and context;
editing the prefix affects only Refresh or a new expansion.

The Explorer follows the requested local OpenAI UI reference: white/black
tokens, inverted black primary action, sharp corners, flat hairlines, no
decorative shadow/gradient, visible focus, loading spinner, and textual
stale/permission/cap states. Ordinary text now passes a numerical WCAG AA
contrast ratio of at least 4.5:1. This is P4 component evidence, not a claim
that P6 native RawInput/AccessKit and installed AX work is complete.

The review-fix checkpoint passed 249 regular all-target/all-feature tests and 19
doctests, strict locked/offline Clippy, formatting, diff, release-contract,
receipt secret-negative and credential-pattern checks, the UI skill
pre-delivery check, and the release build. The isolated `dbotter-p4` MySQL 8.4
live gate passed 1/1 in both default and all-features configurations and proves
multi-page schema/relation/column ordering, table/view identity, count and real
metadata-byte caps with recovery, restricted visibility, unauthorized-default
Check/Execute Permission, stale retry state, and headless CLI JSON. Two
independent reviews of exact implementation commit
`05ad72f20e415b44f2d90ce7d5971c3d7a75b520` reported `NO P4 BLOCKER`
and `NO P4 SECURITY BLOCKER`. T5 nevertheless stays Implementing rather than
Verified because P6 native/installed accessibility evidence remains.

```text
ac9abfd2b6434fec58e7280d4da958125737a342fed01b7a7db2c190860dc120  P4 review source+test input
718d90023bcaae1e1d70947d74de2fe2248bc5d79d7fca8bbf3b5586fbe414cf  production snapshot
d7a7f9b7d2032c4bdf4d1d77a9d6013d5053a04599fed1c23ac0872e950ac2e2  tests snapshot
4d4a8dd94668954b110946b6442a4ad7fca41c06bc85cd8ad831a1fd5ff616da  target/release/dbotter
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
