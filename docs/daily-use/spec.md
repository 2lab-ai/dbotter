# dbotter Daily-driver v1.2 — product contract

Status: **Frozen**

Contract ID: `DUV1` v1.2

Date: `2026-07-17`

This contract replaces DUV1 v1.1 for new work. Historical evidence remains in
[`evidence.md`](evidence.md), but cannot complete a v1.2 requirement. Scope is
based on [`research.md`](research.md). Requirement IDs below are normative and
map to evidence in [`trace.md`](trace.md).

## 1. Outcome and completion rule

dbotter v1.2 is a macOS Preview that can replace a second tool for the supported
MySQL and Redis work below:

> reconnect → find data → query or edit it safely → retain the work → exchange
> data → quit and continue later

Each journey ships independently. A journey is `Verified` only when the exact
source commit passes its RED-derived contracts, hermetic gates, required live
fixtures, independent Critical/High review, Preview publication, xbrew install
and the complete installed sequence in `trace.md` with independent backend/file
readback. Screenshots, file-presence tests, unit suites, CI green or package
receipts alone are not completion.

Only Preview releases are authorized. A stable tag or release requires a new,
explicit user approval.

## 2. Supported target (`S-*`)

- **S-1:** macOS `aarch64-apple-darwin` and `x86_64-apple-darwin`, using the
  bundle/CLI identity contract published by the Preview workflow.
- **S-2:** official MySQL 8.0 and 8.4 servers. Every Preview records the exact
  lowest/highest fixture version and immutable image digest. Compatible forks
  may work but are not claimed.
- **S-3:** verified standalone Redis targets are 6.2, 7.4 and the current
  supported 8.x release. Every Preview records exact endpoint versions and
  immutable image digests. Other minors may work but are not claimed. Cluster
  and Sentinel are P1 and must be labelled unsupported.
- **S-4:** P0 raw MySQL editor execution is read-only: closed `SELECT`, `SHOW`,
  `DESCRIBE`/`DESC` and `EXPLAIN SELECT`. Raw DML, DDL, administration, stored
  programs, `LOAD DATA`, raw transaction/session control and implicit-commit
  forms are P1 or fail closed. P0 writes exist only through reviewed typed row
  editing and CSV import.
- **S-5:** P0 Redis structured mutation covers String, Hash, List, Set and
  Sorted Set plus EXPIRE, PERSIST and whole-key delete. Stream/JSON inspection
  and mutation are P1; the P0 UI does not advertise them.

## 3. Cross-cutting safety, privacy and admission (`X-*`)

### X-1 — persistence/privacy matrix

| Data class | Allowed retention | Forbidden retention |
|---|---|---|
| credential-channel value from Keychain/session/environment field | Keychain or process memory only | config, workspace/history, log, public error, capture, receipt, evidence |
| environment variable name, Keychain item reference, TLS/SSH public settings | profile config | public capture/receipt when it identifies a user's environment |
| persistable SQL/Redis source text intentionally typed by the user | private draft/history after visible disclosure and per-profile opt-in/default setting | logs, public errors, receipts, public evidence/capture |
| result cell, Redis key/value and backend prose | bounded memory; explicit user export destination | workspace/history, logs, public errors, receipts, public evidence/capture |
| safe metadata: opaque IDs, typed code/status, timestamp, duration, row count, bounds | private history and sanitized receipts as specified | raw profile/query/key/value/path strings in public evidence |

Arbitrary source text can itself contain sensitive literals. The app does not
claim reliable content scanning; it discloses persistence and provides
per-profile disable/clear. Tests use distinct sentinels for each row rather than
calling all source text a credential.

### X-2 — posture and production

Read-only rejects every MySQL/Redis mutation before user-target dispatch.
Development/Production and Read-only/Read-write are always visible in text.
Production destructive actions require an action-specific review; Redis
overwrite/delete additionally requires the exact current key display token.

### X-3 — identity, cancellation and terminal truth

Every request, completion, tab, result, transaction, stage, transfer and Redis
review has explicit profile-instance/generation and operation identity.
Cancelled, stale or mismatched work cannot overwrite newer state. `Cancel
requested` is distinct from driver-confirmed cancellation; cancellation never
implies rollback or a known write outcome.

### X-4 — bounds

Every list, source, result, history, scan, file and retained surface has both
count and byte limits. Reaching a limit is visible; omitted/truncated content is
never presented as complete or safely replayable. Active drafts, transactions,
unknown outcomes and in-progress transfers are never silently evicted.

### X-5 — private durability

Local state uses restrictive permissions, same-directory temporary files,
file fsync, atomic rename/no-replace as applicable and parent-directory fsync.
Unknown versions, symlinks, corrupt identity/checksum and uncertain durability
fail closed. Corrupt content is quarantined only within a bounded private area
and surfaced without destroying valid profiles or other workspaces.

### X-6 — mutation intent/fence

Before MySQL Begin/terminal dispatch or any Redis immediate mutation, a
payload-free intent containing opaque identity, operation kind and request HMAC
is fsynced. Its durable state is:

```text
Prepared -> Dispatched -> Confirmed
                       \-> OutcomeUnknown
```

The `Dispatched` transition is durable before the first mutation byte is sent.
On startup, any non-terminal intent is conservatively folded to
`OutcomeUnknown`; it blocks further mutation and is never automatically
retried. The original payload is not persisted. Acknowledgement clears only the
local block after explicit user verification and never rewrites history as
Committed/RolledBack/Applied.

For MySQL Begin, `Confirmed` atomically becomes a durable `Active` transaction
fence rather than clearing the record. It remains non-terminal through every
applied DML/savepoint state and is cleared only by a confirmed Commit/Rollback.
Startup with Active/Resolving is therefore OutcomeUnknown even when the last
individual statement had a successful response.

### X-7 — generated mutation

Generated SQL uses quoted catalog-derived identifiers and typed bound values.
Editable MySQL rows require a primary key or non-null unique identity. An
unidentifiable row is visibly read-only.

### X-8 — closed raw read admission

MySQL source is bounded and fully parsed before user-target I/O. P0 admits only
the S-4 statement families. It explicitly denies executable comments,
multi-statement ambiguity, `INTO OUTFILE`/`DUMPFILE`, locking clauses,
`EXPLAIN ANALYZE`, user/session-variable assignment, advisory/sleep/file
functions and stored/UDF calls. A versioned built-in read-function allowlist
contains the supported aggregate/string/date/math/JSON functions; an unknown
function is locally denied. Execution also uses a server-proven read-only
session.

Redis raw execution uses a versioned exact command/arity/option allowlist.
Unknown/module commands and mutating variants such as `SORT ... STORE` are
denied before driver dispatch.

Typed View Data additionally requires `SafeViewProof`: recursively inspect at
most 8 nested views/128 relation nodes; require every view to be `SQL SECURITY
INVOKER`; parse each definition with the same sql_mode/read/function allowlist;
and reject missing definition privilege, stored/UDF call, unknown dependency or
bound overflow. On the exact read lease, acquire shared metadata locks for the
view/dependency set, re-read and fingerprint every definition under those locks,
then hold them through prepared execution. Drift, lock failure or unproved view
keeps Structure available but disables Data with an exact reason before user
query dispatch.

### X-9 — terminal outcome and implementation quality

Commit, rollback or immediate mutation is terminal only after server
confirmation. Lost/ambiguous response becomes `OutcomeUnknown`; no terminal
operation is automatically retried. Public errors are typed, bounded and redact
backend/user payloads. No production `unwrap`, `expect`, `panic!` or `todo!`.

## 4. J1 — secure MySQL connection to useful Data

- **J1-1:** From a clean install, create, edit, duplicate and delete a MySQL
  profile and choose Development/Production plus Read-only/Read-write posture.
  Delete is blocked by J3 transaction state, discloses retained draft/history
  counts and, after confirmation, durably purges that profile-instance workspace
  rather than leaving inaccessible source text.
- **J1-2:** Credential modes are None, Session, Environment-name-only and macOS
  Keychain. A Keychain item uses immutable profile-instance identity and a
  product-scoped service name. Rename preserves it; Duplicate never copies a
  secret and creates an independent item only after new input; Delete asks Keep
  or Remove. Config/Keychain partial failure is journaled without secret values,
  compensated when provable, otherwise blocks that profile with a repair action.

Profile Delete uses one payload-free journal containing immutable instance ID,
Keychain Keep/Remove decision and phase. It is fsynced before the first change
and advances idempotently through `Prepared → WorkspaceTombstoned →
ConfigDeleted → WorkspacePurged → KeychainResolved → Complete`. `ConfigDeleted`
is the user-visible commit point; the exact private tombstone makes source text
inaccessible first and is purged before completion. Startup resumes the recorded
phase, verifies an uncertain config commit by reload/fingerprint and never
guesses or deletes another instance. The journal is removed only after config,
workspace and Keychain decision are durably reconciled.
- **J1-3:** MySQL TLS defaults to verify-identity using OS roots or an explicit
  custom CA and the original database hostname. Encryption-only/Preferred and
  plaintext downgrade are not P0 connection modes. Wrong CA or hostname fails
  without fallback.
- **J1-4:** Optional SSH binds an ephemeral loopback port and connects only to
  the reviewed final database destination. Host identity uses strict
  `known_hosts` or an explicitly reviewed SHA-256 fingerprint; unknown keys
  require one-time review before saving and mismatches always fail. Private-key
  passphrases use a credential channel, never argv/environment/log. Database
  TLS still verifies the original database hostname/SNI through the tunnel.
- **J1-5:** Test and Connect share the ordered SSH → TCP → TLS → authentication
  → database pipeline and expose only a typed failed stage, safe recovery and
  zero secret/backend prose.
- **J1-6:** A saved Keychain profile reconnects after app restart without
  re-entering the password.
- **J1-7:** Search and refresh `schema → table/view → columns, primary/unique
  keys, indexes and foreign keys` while retaining selected object context.
- **J1-8:** Open table Data/Structure and view Structure in one action. View Data
  opens only after X-8 SafeViewProof; an unproved view has a truthful disabled
  Data action and exact reason rather than an unsafe fallback.
- **J1-9:** Data uses a typed request with explicit scope, limit, truncation,
  server filter, catalog-derived sort and page state. Stable keys use keyset
  paging; bounded keyless fallback is labelled unstable.
- **J1-10:** Data supports refresh, Grid/Record/full-value detail and cell/row
  copy without losing navigator/editor/result context.

## 5. J2 — durable SQL workspace/history with in-session multi-results

- **J2-1:** Per profile, create, rename, reorder, select, save and close multiple
  SQL tabs. Persistence is available only for classified v3 profiles with an
  immutable instance ID; a legacy profile explains and links to migration.
- **J2-2:** Editor has syntax highlighting and bounded catalog autocomplete.
- **J2-3:** Persist tab ID/title/text/language, profile/database binding,
  order/selection, cursor/selection and split geometry across quit/crash.
- **J2-4:** Execute non-empty selection, otherwise caret statement, or explicit
  whole script; display the target before dispatch.
- **J2-5:** Cancel active work and distinguish requested, cancelled, failed,
  successful and outcome-unknown operations.
- **J2-6:** During the current session retain every statement result or typed
  error in order with time, duration, returned/affected rows and truncation,
  without overwriting another editor/result.
- **J2-7:** Inspect Grid, Record and full Value; locally filter/sort retained
  rows; copy cells/rows; compare several result tabs.
- **J2-8:** Search private per-profile history by source/status/date and open an
  entry in a new editor with zero automatic network dispatch.
- **J2-9:** After quit or forced termination, relaunch, reconnect, reopen a
  history entry, explicitly Run it and obtain the expected new result.
- **J2-10:** Disable or clear draft/history persistence per profile. Clear never
  deletes profiles, Keychain items or mutation fences.

### J2 storage bounds and failure semantics

- editor source: 256 KiB/tab; 20 tabs/profile; 100 tabs total;
- history source: 64 KiB/entry; oversize source becomes visible non-reopenable
  metadata, never a truncated query; 2,000 entries/profile; 10,000 total;
- committed profile shard: 32 MiB; committed total store: 128 MiB;
- count and bytes are enforced together, evicting oldest terminal history first.

Result rows/cells and result tabs do not persist across restart. History stores
only source plus safe typed status/error code and metrics, never backend prose.

Autosave starts within 2 seconds of a change. `Unsaved`, `Saving`, `Saved` or
`Save failed` is visible. `Saved` means the profile manifest pointer, referenced
shard and parent directory are durable. `⌘S` flushes the current draft only.
Voluntary quit with pending/failed save is blocked until Retry or an explicit
Discard-local-changes confirmation. Forced termination after visible `Saved`
must restore that exact snapshot; visibly unsaved edits may be lost.

The store is single-writer. A second app instance opens workspace persistence
read-only and explains why. Each shard generation is written/fsynced under an
immutable generation name; an atomic fsynced manifest pointer plus checksum
chooses the committed generation. Orphan generations are bounded cleanup, not
implicit commits.

### J2 keyboard contract

`⌘Enter` current/selection, `⌘⇧Enter` all, `⌘T` new editor, `⌘S` flush draft,
`⌘W` close, `⌘F` context find/filter, `⌘⇧F` history search and `Esc`
cancel/dismiss. `⌘S` never applies a database mutation.

## 6. J3 — safe identifiable MySQL row editing

- **J3-1:** Begin one profile-scoped managed transaction on one physical
  connection/session ID. Raw transaction commands remain blocked.
- **J3-2:** Stage typed Add, Update and Delete locally with zero network write;
  show changed cells/rows and parameterized generated SQL; Discard is local.
- **J3-3:** Apply at most 100 reviewed rows through the transaction connection
  under one internal savepoint. Error, Conflict or cancel at the first, middle or
  last row requests rollback of the whole Apply. Only confirmed savepoint
  rollback returns every batch row to local-staged state; an ambiguous rollback
  becomes OutcomeUnknown and no row is shown as locally discardable. Partial
  Applied state is impossible. Apply never commits.
- **J3-4:** Update/Delete re-read and lock stable identity plus original values;
  any mismatch or affected-row count other than one is Conflict. Generated and
  read-only columns are not editable.
- **J3-5:** Rollback returns the separately observed original value; Commit makes
  the separately observed reviewed value durable.
- **J3-6:** Read-only has zero mutation dispatch. Production requires an exact
  change-scope confirmation before Apply and again before Commit.
- **J3-7:** Disconnect, profile edit/delete, update and voluntary quit are blocked
  while server transaction state is Active/Resolving/Unknown. Local-only stages
  may persist; a live server transaction is never claimed to survive restart.
- **J3-8:** Connection loss, forced termination, unproven cancel or unproven
  savepoint/terminal response enters Unknown and blocks further mutation.
- **J3-9:** Unknown UI shows opaque transaction ID and intended terminal action,
  supports read-only inspection and explicit acknowledge-after-external-check;
  acknowledgement does not rewrite the historical outcome.

Transaction state:

| State | Server resource | Allowed actions |
|---|---|---|
| AutoCommit | no owned transaction | Begin, reads, local stage |
| ActiveClean | owned physical session, no applied changes | reads, Apply, Commit, Rollback |
| ActivePending | same session, applied uncommitted changes | reads, Apply, Commit, Rollback |
| ResolvingCommit / ResolvingRollback | same session, terminal request in progress | wait/cancel-request only; no new work |
| OutcomeUnknown | session/outcome not safely known | read-only inspection, external verification, acknowledge; no mutation |

Begin, the persistent Active transaction and terminal requests use X-6 fences. Driver cancellation is not reported
Cancelled until server/session disposition and transaction state are known.
Forced process death with any nonterminal fence recovers as OutcomeUnknown.
Raw editor `INSERT`/`UPDATE`/`DELETE` remains P1.

## 7. J4 — bounded export and transaction-safe CSV import

- **J4-1:** Export exact current query result, current filtered table page or
  selected rows; name scope, included count/bound and truncation before path.
- **J4-2:** CSV/TSV/JSON are streamed with progress/cancel. A sibling private
  temp is fsynced and no-replace-renamed only when complete; destination is
  never overwritten and crash/cancel never leaves a normal-name partial file.
- **J4-3:** CSV exposes header, UTF-8, delimiter and NULL representation.
- **J4-4:** Import selects one table, parses configurable UTF-8 CSV and previews
  at least 20 data rows before dispatch.
- **J4-5:** Explicit column mapping validates required columns and every value
  against catalog types; error identifies source row without echoing payload.
- **J4-6:** Cap one import at 10,000 rows/32 MiB; show progress and cancel between
  bounded parameterized batches.
- **J4-7:** Execute on the J3 transaction connection under one savepoint. Parse,
  validation, server or cancel failure requests savepoint rollback. Report
  `Rolled back` only after confirmation; connection loss during batch/cancel/
  rollback becomes OutcomeUnknown and activates the mutation fence. Successful
  Apply still requires explicit Commit/Rollback.

## 8. J5 — Redis browse, inspect and structured edits

- **J5-1:** Test, save and reconnect a standalone Redis profile using J1's
  Keychain lifecycle and verify-identity/no-downgrade TLS contract.
- **J5-2:** Browse only by cursor SCAN with pattern/type filter, Load more,
  progress/cancel and explicit bounds — never `KEYS *`.
- **J5-3:** Show binary-safe key identity, type, TTL and bounded size/value;
  inspect String, Hash, List, Set and Sorted Set with typed pagination/search and
  Text/JSON/HEX presentation where applicable.
- **J5-4:** Structured operation matrix is fixed: String `SET`; Hash `HSET/HDEL`;
  List `LPUSH/RPUSH/LSET/LREM`; Set `SADD/SREM`; Sorted Set `ZADD/ZREM`; key
  create/delete; `EXPIRE/PERSIST`. Installed proof covers add/update/remove for
  every applicable type.
- **J5-5:** Apply uses one dedicated connection and `WATCH` plus operation-specific
  bounded read/compare, `MULTI` and `EXEC` so a concurrent key change yields
  Conflict. Raw key bytes, expected type and expected field/index/member/value
  are the identity. Whole-key overwrite/delete additionally compares an
  `ExpiryToken` and a `DUMP` digest capped at exactly 1 MiB. `ExpiryToken` is
  `Missing`, `Persistent` or `ExpiresAtUnixMs(i64)`. One embedded, static,
  read-only, O(1) Lua script atomically returns Redis server `TIME` and `PTTL`
  for exactly `KEYS[1]`; it accepts no user script or command and returns a fixed,
  bounded tuple. For `PTTL >= 0`, the token is
  `floor(seconds * 1000 + microseconds / 1000) + PTTL`; `-2` maps to Missing and
  `-1` to Persistent. The same script runs after `WATCH` on the apply connection
  and the exact token must match, so natural TTL passage does not conflict while
  competing `EXPIRE`/`PEXPIRE`/`PERSIST` does. A bounded RESP decoder rejects a
  declared DUMP bulk length over 1 MiB before body allocation, UNWATCHes and
  disables the action; exact-limit and limit+1 are tested. An indistinguishable
  delete/recreate cannot be called a distinct incarnation and is not claimed.
- **J5-6:** Redis changes are immediate and never show MySQL Commit/Rollback.
  X-6 Prepared is fsynced before WATCH and Dispatched before `MULTI` or a queued
  mutation command; lost `EXEC` response becomes OutcomeUnknown and never
  auto-retries.
- **J5-7:** Read-only and Production guards apply before WATCH/dispatch; missing,
  type-changed or concurrently changed keys recover without applying to another
  visible selection.
- **J5-8:** Closed read command editor uses X-8 allowlist, typed replies,
  multi-command results and J2 durable source/history. Arbitrary raw mutation
  commands remain blocked.

## 9. Interaction and visual contract (`UX-*`)

- **UX-1:** Use `assets/dbotter-icon.png` as product title/app icon.
- **UX-2:** Keep a DBeaver-density searchable navigator, multi-tab editor,
  multi-tab result/detail and operation/transaction status visible together;
  result/pane switching preserves context. Controls without a real effect path
  are absent.
- **UX-3:** Follow the named local OpenAI reference: true white/black,
  opacity hierarchy, inverted primary emphasis, square corners, no decorative
  gradients/shadows/chromatic state accent, readable sans/monospace typography.
- **UX-4:** Motion is restrained to 150–250 ms, interruptible and disabled by
  reduced motion. Every status also has text/icon, never color alone.
- **UX-5:** Full-row tree/grid selection may be dense; discrete targets are at
  least 44 points. All actions have visible focus, programmatic name/state/value
  and logical keyboard traversal. Primary journeys are mouse-free.
- **UX-6:** At 840×560 no required action is clipped/unreachable. A collapsed
  pane has a named restore action; wide layout keeps all four regions.
- **UX-7:** Mutation review traps focus. Its explicit Apply is activated by
  focused button or `⌘Enter` only while that modal is open; `⌘S` remains draft
  save and can never cause a database write.

## 10. Evidence contract (`E-*`)

- **E-1:** Before production code, commit/push the failing user-boundary and
  required safety tests; record the full RED SHA in `evidence.md`.
- **E-2:** Local GREEN requires focused tests, `git diff --check`, `just check`
  and `just check-all` at the exact SHA.
- **E-3:** Driver/filesystem changes require live fixtures and kill/failpoint
  evidence named in `trace.md`; UI changes require actual rendered frame plus
  keyboard/AX activation, not static label search.
- **E-4:** Independent review is Critical 0 and High 0 before publication.
- **E-5:** Preview source/artifacts/manifest/checksums/tap/xbrew executable all
  bind the same SHA; installed actions and independent backend/file readback
  complete every requirement mapped to that journey.
- **E-6:** Public proof uses only the tracked synthetic fixture and passes
  forbidden-sentinel, AX allowlist and metadata-strip gates.

## 11. P1/P2 exclusions

P1: raw DML and reviewed raw mutation batches, saved-query library/parameters/
snippets, DDL/schema UI, plan visualization, connection import/export/groups,
XLSX/multi-table transfer, Redis Stream/JSON, cluster/sentinel, shortcut
customization and detached result comparison.

P2: ERD/GIS/charts, schema compare/migration generation, backup/scheduler,
profiler/slow-log dashboards, Redis module-specific workspaces, AI/plugins and
additional database engines.

## 12. Change control

The frozen tuple is `research.md`, `spec.md`, `trace.md` and `plan.md`, with
SHA-256 values in [`../../04-patch-plan.md`](../../04-patch-plan.md). A semantic
change requires version bump, synchronized trace/plan, new tuple and independent
review before code proceeds. Implementation may choose internals but cannot
replace an installed observable with a proxy or weaken safety/privacy to make a
test pass.
