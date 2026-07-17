# dbotter Daily-driver v1.2 — vertical trace

Status: **Frozen**

Normative product contract: [`spec.md`](spec.md)

Research basis: [`research.md`](research.md)

This file maps every normative requirement to evidence. Update it before a
cross-layer command, event, state or persistence behavior changes.

## Fixed order

Implementation order is `J2 → J1 → J3 → J4 → J5`. J2 uses an existing
classified Environment-credential fixture, so it can prove restart and explicit
reuse before Keychain/SSH arrive. Preparation may land earlier only when it does
not claim a different journey complete.

## Requirement-to-evidence map

Coverage inventory (must match `spec.md`): `S-1 S-2 S-3 S-4 S-5`;
`X-1 X-2 X-3 X-4 X-5 X-6 X-7 X-8 X-9`;
`J1-1 J1-2 J1-3 J1-4 J1-5 J1-6 J1-7 J1-8 J1-9 J1-10`;
`J2-1 J2-2 J2-3 J2-4 J2-5 J2-6 J2-7 J2-8 J2-9 J2-10`;
`J3-1 J3-2 J3-3 J3-4 J3-5 J3-6 J3-7 J3-8 J3-9`;
`J4-1 J4-2 J4-3 J4-4 J4-5 J4-6 J4-7`;
`J5-1 J5-2 J5-3 J5-4 J5-5 J5-6 J5-7 J5-8`;
`UX-1 UX-2 UX-3 UX-4 UX-5 UX-6 UX-7`; `E-1 E-2 E-3 E-4 E-5 E-6`.

| Requirements | Focused/hermetic proof | Live/failpoint proof | Actual-frame/installed proof |
|---|---|---|---|
| S-1, E-4..6 | release/manifest/receipt schemas, target/arch negative fixtures | four-target build and signed bundle chain | host-architecture exact xbrew binary/app identity; both macOS artifacts bind the same source |
| S-2, J1-3..5 | typed transport stages, verify-identity/no-downgrade and secret-redaction tests | exact MySQL 8.0/8.4 image digests; good, wrong-CA, wrong-hostname, plaintext-fallback and SSH host-key matrix | J1 installed transport sequence and safe stage/AX readback |
| S-3, J5-1 | Redis version/TLS/Keychain contracts | exact Redis 6.2/7.4/current-8.x image digests and TLS negatives | J5 installed reconnect and server-version status |
| S-4, X-8, J2-4..5 | versioned MySQL AST/function allowlist; every denied side-effect form zero-dispatch; bounded SafeViewProof | server-proven read-only session; nested INVOKER safe view; definer/UDF/missing-definition/depth/drift/lock negatives | J2 installed current/selection/all and explicit rerun; denied target remains local; J1 installed safe/unsafe view Data states |
| S-5, J5-3..5 | exact type/operation matrix and bound/+1 tests | two-version type CRUD and concurrency matrix | J5 installed matrix/readback |
| X-1 | four distinct sentinel classes across config/workspace/history/log/error/receipt; opt-out/clear | credential resolver never yields to persistence/log ports | installed private-store inspection and public capture/receipt scan |
| X-2..3 | posture, identity, stale/cancel and confirmation model tests | driver counters prove zero target dispatch; delayed/stale completions | actual controls/AX state plus installed negative sequence in owning J row |
| X-4..5, J2 bounds | every count/byte limit and +1; checksum/version/symlink/quarantine/single-writer/fingerprint; crash-point store matrix | temp/file/rename/directory fsync failpoints | J2 Saved/kill/restore, visible oversize omission, second-instance and one-shard corruption sequence |
| X-6, X-9, J3-8..9, J5-6 | Prepared/Dispatched/Confirmed/Active/Unknown transition and replay/ack tests; no payload serialization | kill before/after wire/response/terminal persistence plus ActiveClean and ActivePending | installed verifier kills exact app at every mapped state, relaunches and observes Unknown without auto-retry |
| X-7, J3-2..4, J4-5..7 | quoting, typed values, identity/original comparison, savepoint/import mapping | MySQL rollback/commit/conflict/connection-loss matrix | installed J3/J4 action log and separate MySQL readback |
| J1-1..2 | profile/Keychain immutable identity, CRUD/duplicate/delete and partial-failure journal | fake Keychain failpoints, delete-phase kill matrix and real macOS Keychain integration | clean installed lifecycle including independent duplicate, Keep/Remove and replayed purge |
| J1-6..10 | catalog/table request/page/filter/sort/detail/copy and stale generation tests | known table/view fixture including keyless page | installed full navigator/Data sequence and independent SQL readback |
| J2-1..3, J2-8..10 | snapshot/domain/store round-trip; syntax/autocomplete; zero-run reopen; opt-out/clear | save debounce/flush/crash failpoints | installed forced-restart/reuse sequence below |
| J2-6..7 | ordered result/error ownership, bounds, grid/record/value/filter/sort/copy | delayed/error batch fixture | installed two-result/error inspection; restart visibly omits result payloads |
| J3-1, J3-5..7 | physical-session state machine and lifecycle guards | separate-connection before/rollback/commit plus disconnect/cancel | installed Add/Update/Delete/production/read-only sequence |
| J4-1..4 | exact scope/options, stream/no-clobber/temp cleanup, CSV parse/preview | filesystem crash/cancel and parser bounds | installed all scopes/formats plus 20-row preview |
| J5-2, J5-7..8 | SCAN-only, binary identity, raw read allowlist/history and stale selection | sparse SCAN, no-KEYS counter, missing/type-change/conflict | installed browse/history/guard sequence |
| UX-1..7 | icon decode, theme tokens, reduced motion, layout/contrast, focus/AX/44pt, modal shortcut contracts | — | mouse-free installed action log at 840×560 and wide layout; visible state readback |
| E-1..3 | ledger, RED ancestry and exact gate receipt validation | owning live/failpoint scripts | owning actual-frame test before publication |

No normative ID may be removed from this map without change control.

## User-boundary RED and installed acceptance

### J2 — durable SQL workspace/history with in-session results

RED first proves that the existing session-only app fails this sequence:

1. isolated classified v3 Environment-credential profile; persistence disclosure
   accepted;
2. create, rename and reorder two tabs; edit both; exercise highlight and
   autocomplete; run current/selection/all including one error and two retained
   results; inspect/filter/sort/copy;
3. verify searchable history status/metrics, 64 KiB/+1 omission, tab/shard bounds,
   opt-out/clear, store privacy and zero-run reopen;
4. wait for visible `Saved`, record exact manifest generation, force-kill the
   exact app, relaunch it, and restore title/text/order/selection/cursor/split;
   result payload/tabs are visibly not restored;
5. reconnect explicitly using the Environment fixture, search/open history with
   a zero-dispatch counter, explicitly Run, and verify the expected fresh result
   through an independent MySQL connection;
6. open a second app instance and observe read-only persistence; corrupt one
   bounded profile shard and prove quarantine plus another profile still works.

Installed acceptance repeats all six steps through the exact xbrew app. The
forced kill occurs only after `Saved`; no crash-loss claim is made for visible
`Unsaved` state.

### J1 — secure MySQL connection to useful Data

RED and installed acceptance use a clean config and perform:

1. Create with posture and Keychain; prove wrong CA, wrong hostname, plaintext
   fallback, unknown-unapproved SSH key and mismatched saved host key all fail
   at the typed stage with no credential/backend disclosure;
2. approve the known SSH fingerprint, tunnel through loopback while verifying
   original DB hostname, Test, Save and Connect; restart and reconnect without
   password entry;
3. rename (same Keychain identity), duplicate (no copied secret; independent
   input/item), edit, and later Delete once with Keychain Keep and once with
   Remove; kill before/after every journal phase and prove idempotent startup
   completion, exact-instance workspace purge and no unrelated profile/item loss;
4. search/refresh schema tree; open table Data/Structure and safe nested INVOKER
   view Data/Structure; prove definer/UDF/unreadable/drifted views expose Structure
   but disabled Data with zero user-query dispatch; apply typed filter, sort,
   next page and unstable keyless fallback; use Grid, Record, full Value and copy;
   preserve navigator/editor/result context;
5. independently read fixture rows and Keychain item metadata; switch profile to
   Read-only and prove a mutation action has zero target dispatch.

MySQL 8.0 and 8.4 run the live transport/data matrix; installed proof uses the
host-selected version and records its exact server version.

### J3 — safe identifiable MySQL row editing

RED and installed acceptance perform on a disposable table:

1. Begin; stage Add/Update/Delete and show zero write before Apply plus exact
   parameterized review;
2. Apply/Discard local stage, then Apply under the same physical session;
   Rollback and verify original rows from a separate connection;
3. inject first/middle/last row error, Conflict and cancel into multi-row Apply;
   prove confirmed whole-savepoint rollback, every row local-staged, and no
   partial change on the transaction session or separate connection;
4. repeat and Commit; verify added/updated/deleted rows separately;
5. inject concurrent original-value change and affected-row mismatch; both stop
   as Conflict; Read-only is zero-dispatch and Production requires both reviews;
6. attempt disconnect/profile edit/delete/update/quit while Active; all block
   until resolution;
7. kill after confirmed Begin in ActiveClean and after confirmed Apply in
   ActivePending before any terminal request; also kill at every X-6 terminal
   wire/response/record point. Relaunch to durable Unknown/no retry, independently
   verify DB state and acknowledge without relabelling historical outcome.

### J4 — bounded export and CSV import

RED and installed acceptance perform:

1. export query result, filtered table page and selected rows; exercise CSV,
   TSV and JSON, CSV header/delimiter/NULL options, exact file contents/hash,
   no-clobber, cancel and crash cleanup;
2. select a target table; preview at least 20 rows; map columns; show required/
   type/64-bit row errors with zero dispatch and no payload echo;
3. import multiple batches, cancel and inject server failure; independently
   prove confirmed whole savepoint rollback;
4. lose the connection during batch, cancel and savepoint rollback; observe
   OutcomeUnknown/fence rather than false rollback;
5. run valid import, Apply, Commit, and verify exact rows through a separate
   connection. Repeat successful Apply with Rollback and verify absence.

### J5 — Redis browse, inspect and structured edits

RED and installed acceptance perform:

1. wrong-CA/hostname no-downgrade negatives, then Keychain-backed TLS reconnect
   to a recorded supported version;
2. sparse cursor SCAN with pattern/type/Load more/cancel, binary/text keys and a
   wire counter proving no `KEYS`; inspect type/TTL/size/value for all five types;
3. execute String SET; Hash HSET/HDEL; List LPUSH/RPUSH/LSET/LREM; Set
   SADD/SREM; Sorted Set ZADD/ZREM; key create/delete; EXPIRE/PERSIST, covering
   add/update/remove and independent exact readback;
4. replace/type-change the watched key from a competing client between inspect
   and EXEC; observe Conflict and no apply to the replacement. For whole-key
   actions, advance natural TTL time between review/apply and observe success,
   then race EXPIRE, PEXPIRE and PERSIST from a competing client and observe
   Conflict. Assert the absolute token and sentinel mapping on Redis 6.2, 7.4
   and current 8.x. Exact-1-MiB DUMP remains enabled; declared limit+1 is rejected
   before allocation and disables overwrite/delete;
5. prove Read-only zero dispatch and Production exact-token review; reopen one
   durable raw-read history entry with zero auto-dispatch and explicitly run it;
6. kill the installed app at every X-6 Redis intent/wire/response record point;
   relaunch to Unknown with zero auto-retry, independently verify the key, then
   acknowledge without rewriting the outcome.

Redis 6.2, 7.4 and the recorded current 8.x run the live matrix; installed proof
uses and displays the exact host-selected version.

## RED/GREEN protocol

1. Add the smallest failing installed/user-boundary contract and its safety
   contracts; retain the expected failure.
2. Commit and push RED; record full SHA in `evidence.md` in that commit.
3. Implement without weakening the frozen row.
4. Run focused, hermetic, live/failpoint and actual-frame evidence as mapped.
5. Commit/push GREEN with evidence-ledger update.
6. Obtain independent Critical/High review and fix findings.
7. Publish Preview, wait for all terminal jobs, install/update through xbrew and
   run the complete installed sequence before `Verified`.

A journey may use small RED/GREEN pairs; none may inherit installed proof from a
different journey or a prior contract version.
