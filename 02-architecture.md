# dbotter — Daily-driver v1.2 architecture

Status: **Frozen-aligned architecture; delivered baseline plus J1–J5 seams**

Normative behavior lives in `docs/daily-use/{research,spec,trace,plan}.md`.
Architecture can refine implementation but cannot replace installed acceptance
or weaken product invariants.

## 1. Ownership model

dbotter remains one Rust package with one library and binary. Native UI and CLI
share `ApplicationService`; neither implements profile lookup, credential
resolution, admission, driver lifecycle, catalog, execution or error conversion
independently.

```text
native commands -----+                         +-- config / Keychain
                     +-> runtime/controller -->+-- workspace / mutation fence
CLI -----------------+          ^              +-- MySQL session/transaction
                                |              +-- Redis session/review
                                +-- typed, correlated events
```

UI owns text, selection, layout and render models, never live clients. Driver
resources and blocking filesystem/CSV/SSH work stay behind typed service
boundaries. No mutex/RwLock or borrowed guard crosses `.await`. Long work is
registered, cancellable, bounded and joined before cleanup is complete.

## 2. Identity and state fold

Saved-profile work binds profile ID, immutable instance ID, generation and
operation ID. Editor, result, history, physical MySQL session, transaction,
stage, transfer and Redis review add opaque IDs. Events fold only when every
relevant identity is current; close, profile mutation, reconnect and cancel
invalidate predecessors.

The controller is the sole fold point into UI state. Persistence receives
immutable snapshots, never driver objects. A stale save cannot mark a newer
snapshot Saved; it schedules a new bounded save.

## 3. J2 workspace/history durability

The resolved config has a private sibling workspace root. One nonblocking
single-writer lease spans the app lifetime. A second process may read the last
committed snapshot but cannot save/clear and receives an explicit status.

Each classified profile-instance shard is an envelope with schema version,
instance ID, monotonic generation, payload length and checksum. Commit is:

```text
encode bounded shard
  -> create private sibling generation temp
  -> write + fsync
  -> rename to immutable generation + fsync directory
  -> write/fsync atomic manifest pointer {generation, checksum}
  -> rename manifest + fsync directory
```

Load trusts only the manifest-referenced generation and checksum. A crash before
manifest replacement leaves the previous generation; after replacement it
loads the new one. Orphan generations are bounded garbage. Oversize, unknown
version, symlink or corrupt identity/checksum is moved only into a bounded
private quarantine; other shards remain available. Destination fingerprints
detect external rewrite.

Editor snapshots contain only contract fields. History contains source, target
kind, timestamp, typed status/code, duration and row metrics. Result rows/cells,
backend prose, credential-channel values, live handles and replay-on-open flags
cannot serialize. Distinct sentinel tests enforce the privacy matrix. SQL/Redis
source persistence is disclosed and has per-profile opt-out/clear.

Domain code enforces all count/byte/store limits before encoding and evicts only
oldest terminal history. Autosave is debounced no longer than two seconds;
explicit Save/close/quit flushes. `Saved` is emitted only after manifest and
directory durability. Failed voluntary quit is intercepted for Retry or an
explicit discard-local-changes decision.

## 4. J1 Keychain, TLS and SSH

Profile config stores no credential. Keychain service name is product/channel
scoped; item account derives from immutable instance ID and credential slot.
Rename does not change identity. Duplicate creates a new instance and never
copies a secret. Delete explicitly keeps or removes its item.

A payload-free profile-mutation journal orders Keychain, config and confirmed
profile-workspace purge. Delete fsyncs immutable instance ID, Keep/Remove choice
and phase before change, then idempotently advances WorkspaceTombstoned,
ConfigDeleted, WorkspacePurged, KeychainResolved and Complete. Startup reloads
and fingerprint-checks an uncertain config phase before resuming; the exact
instance tombstone prevents inaccessible source from being forgotten or another
instance from being purged. The operation holds prior/new secret capabilities only in memory, compensates a
provable partial failure, and leaves a blocking repair state if compensation or
observation is uncertain. Startup replays safe cleanup without ever serializing
a secret. Environment mode stores only a variable name.

Test and Connect share one pipeline. TLS uses verify-identity, OS/custom CA and
the original database hostname; no encryption-only or plaintext fallback path
exists. SSH uses strict known_hosts or an explicitly saved SHA-256 fingerprint,
an ephemeral loopback listener and the exact reviewed destination. Unknown keys
require review; mismatch fails. Passphrases flow directly from Keychain/session
memory to the SSH library, never argv/environment. DB TLS preserves original
hostname/SNI through the tunnel. Tunnel and child resources are session-owned,
cancelled/joined on failure, disconnect and shutdown.

## 5. Read admission and typed Data

Bounded source is fully split and parsed before user-target I/O. The MySQL
classifier accepts only the S-4 AST shapes and a versioned common built-in
read-function allowlist, explicitly denying file output, locking, analyze,
variable assignment, advisory/sleep/file and stored/UDF calls. It then executes
on a server-proven read-only session. Redis uses an exact read command/arity/
option table; mutating options are separate enum variants that P0 cannot build.

Catalog identity owns schema/relation/column/key/index/FK metadata. Typed Data
requests accept catalog identity, bound filters, catalog-derived sort and opaque
page tokens. Stable identity uses keyset paging; bounded keyless fallback is
explicitly unstable. View reads require bounded recursive definitions and
dependencies that are all INVOKER, closed-read/function proven, then shared
metadata-locked and fingerprint-rechecked on the exact read lease before locks
remain held through prepared execution. Failure disables view Data without
blocking Structure. View editing is not supported.

## 6. Durable mutation intent

The mutation store is separate from optional workspace/history and cannot be
cleared by their controls. Before MySQL Begin/terminal or Redis immediate EXEC,
it atomically fsyncs a payload-free Prepared intent. It then fsyncs Dispatched
before the driver is permitted to send the first mutation byte. Confirmation
atomically replaces the intent with terminal metadata; any ambiguous observation
becomes Unknown. A process death at any nonterminal point recovers as Unknown.

For MySQL, confirmed Begin transforms the intent into a durable Active fence
that remains through clean/pending/savepoint states. A confirmed terminal
operation clears it; startup Active/Resolving becomes Unknown. The request HMAC
is keyed by a private local key and supports correlation only;
it cannot recover or replay payload. Acknowledgement records that the user
cleared the local block after external verification and never changes historical
terminal truth.

## 7. J3 MySQL transaction and row stage

One serialized physical connection/session worker owns the profile transaction:

```text
AutoCommit -> ActiveClean -> ActivePending -> ResolvingCommit/Rollback
                                      \------> OutcomeUnknown
```

The worker records physical session ID and initializes/proves the required state.
All transaction reads, table reads and typed Apply use it. Voluntary lifecycle
operations block until confirmed terminal resolution. Cancel remains Requested
until the driver proves query/session/transaction disposition; connection loss
is Unknown. A live transaction is never represented as restart-persistent.

Row edits are local typed stages. Catalog identity and original values are
re-read/locked/compared before generated parameterized DML. One Apply uses an
internal savepoint and at most the frozen row bound. First/middle/last row error,
conflict or cancel requests whole-savepoint rollback; only proof restores every
row to local-staged state, otherwise the transaction becomes Unknown. Partial
Applied state cannot exist. Apply never commits. Raw editor DML does
not have a P0 command variant.

## 8. J4 transfer pipeline

Export owns an immutable retained scope and creates a private sibling temp.
Encoding runs on a bounded blocking worker with progress/cancel checkpoints.
Only a complete fsynced temp is no-replace-renamed and followed by directory
fsync; exact-operation cleanup never deletes a pre-existing destination.

Import separates bounded parse/preview/mapping from dispatch and converts cells
to the row editor's typed model. Reviewed batches run through the transaction
worker under one savepoint. Failure/cancel requests rollback and reports Rolled
back only after proof; any batch/cancel/rollback connection loss uses the same
Unknown fence. Success remains pending until explicit terminal resolution.

## 9. J5 Redis structured mutation

Browse uses cursor SCAN only. Binary keys remain opaque bytes; display text is
never identity. A review binds profile generation, raw key, observed type,
operation-specific expected value and operation ID.

Apply owns one dedicated connection:

```text
durable Prepared intent -> WATCH raw-key
  -> read/compare expected type and field/index/member/value
  -> durable Dispatched intent
  -> MULTI + one closed typed operation -> EXEC (nil => Conflict)
```

Whole-key overwrite/delete additionally compares an `ExpiryToken` and a DUMP
digest capped at exactly 1 MiB. The token is Missing, Persistent or an absolute
Redis-server expiry millisecond. One embedded static read-only O(1) Lua script,
with only `KEYS[1]` and a fixed bounded response, atomically samples `TIME` and
`PTTL`; non-negative PTTL becomes
`floor(seconds * 1000 + microseconds / 1000) + PTTL`. The review and watched
apply connection run that same script and require exact token equality. Natural
TTL decay therefore preserves identity, while a concurrent EXPIRE/PEXPIRE/
PERSIST changes it. No user script or arbitrary command reaches this path.

The DUMP RESP decoder rejects an oversized declared bulk before body allocation
and UNWATCHes. Oversize disables the action. This prevents an observable
concurrent change between review and apply. It does not claim to distinguish
delete/recreate with identical observable bytes and expiry. Lost EXEC response
becomes Unknown, never a retry. UI says immediate apply and never displays MySQL
transaction controls.

## 10. UI and evidence ownership

The native shell keeps navigator, editor, result/detail and status regions alive.
Render code consumes state and emits typed commands; a visible control without a
command/service/effect path is a defect. Shortcut/AX tests activate the actual
rendered control. `⌘S` maps only to draft flush; mutation Apply requires the
focus-trapped review.

Each journey owns its source→artifact→tap→xbrew chain. Installed action/AX logs
pair with independent backend/file/Keychain-metadata readback. Screenshots are
supplementary and use only the tracked synthetic fixture.
