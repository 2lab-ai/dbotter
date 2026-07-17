# dbotter Daily-driver v1.2 — execution plan

Status: **Frozen**

Normative tuple: [`research.md`](research.md), [`spec.md`](spec.md),
[`trace.md`](trace.md), this file.

## Stage 0 — freeze the useful product

- Reconcile code, installed binary and evidence truth.
- Record first-party research without claiming measured frequency.
- Freeze requirement IDs S/X/J/UX/E and their focused/live/installed mapping.
- Resolve independent reviews to Critical 0 and High 0 twice.
- Check links/paths, `git diff --check`, `just check`, `just check-all`.
- Compute the four-file SHA-256 tuple in `04-patch-plan.md`, commit and push.

Exit: frozen tuple and clean branch equals origin.

## Stage J2 — durable work before more surface area

1. RED: exact Saved → force-kill → restore → reconnect → zero-run history open
   → explicit rerun journey; store privacy/bounds/corruption/single-writer and
   session-only result disclosure.
2. GREEN A: profile-instance snapshot types and generation/checksum manifest
   store with private atomic durability, 2-second RPO, opt-out/clear/quarantine.
3. GREEN B: editor create/rename/reorder/save state, syntax highlighting and
   bounded catalog autocomplete.
4. GREEN C: durable bounded status/code history, search/date/status filters and
   reopen-as-new-draft with zero dispatch.
5. GREEN D: persistence/status/shortcut/recovery UI and actual-frame/AX journey.
6. Full gates, independent review, Preview, xbrew install and all six installed
   J2 trace steps.

Exit: the installed app can be force-killed, reopened and explicitly used again
without losing the last visibly Saved SQL work.

## Stage J1 — verified connection to typed Data

1. RED: Keychain lifecycle/partial failure, TLS verify-identity/no-downgrade,
   strict SSH host key and full installed navigator/Data sequence.
2. GREEN A: immutable profile-instance Keychain adapter and nonsecret mutation
   journal/repair.
3. GREEN B: verify-identity TLS plus strict host-key loopback SSH pipeline and
   typed test stages.
4. GREEN C: searchable structure metadata, SafeViewProof/locked fingerprint and
   typed proven-view/table page/filter/sort.
5. GREEN D: Grid/Record/value/copy/context UI and actual-frame/AX flow.
6. MySQL 8.0/8.4 live matrix, review, Preview, xbrew and full installed J1 trace.

Exit: a clean installed app securely reaches, explores and revisits real data.

## Stage J3 — one trustworthy MySQL write loop

1. RED: Add/Update/Delete stage/review/rollback/commit/conflict/lifecycle guards
   plus every durable fence kill point.
2. GREEN A: payload-free Prepared/Dispatched/Confirmed/Unknown fence and startup
   fold/acknowledgement.
3. GREEN B: serialized physical-session transaction worker and full state/loss/
   cancel/lifecycle behavior.
4. GREEN C: stable-identity typed row stage, parameterized review, savepoint
   Apply/Discard and conflict detection.
5. GREEN D: focus-trapped review, transaction status and keyboard/AX surfaces.
6. Live independent readback, review, Preview, xbrew and installed J3 trace.

Exit: identifiable rows can be changed and resolved without raw DML or guessed
terminal state.

## Stage J4 — data handoff

1. RED: all scope/format export, no-clobber/crash cleanup and previewed mapped
   all-or-nothing CSV import including connection-loss Unknown.
2. GREEN A: exact export scope UX and sibling-temp streaming worker.
3. GREEN B: bounded CSV parser, 20-row preview, mapping and typed validation.
4. GREEN C: parameterized batch/savepoint import with progress/cancel and
   confirmed-rollback-or-Unknown truth.
5. Full live/file/failpoint/AX gates, review, Preview, xbrew and installed J4.

Exit: data enters/leaves the installed app with exact scope and outcome.

## Stage J5 — Redis daily driver

1. RED: TLS/SCAN, five-type operation matrix, competing-client conflict,
   posture guards and every immediate-mutation fence kill point.
2. GREEN A: bounded filter/paging and typed five-type details.
3. GREEN B: payload-free Redis intent store and startup Unknown fold.
4. GREEN C: dedicated `WATCH`/compare/`MULTI`/`EXEC` structured mutation,
   server-absolute expiry token, bounded whole-key digest and typed
   Conflict/Unknown.
5. GREEN D: immediate-apply review/status, production token and read-history UI.
6. Redis 6.2/7.4/current-8.x live matrix, review, Preview, xbrew and installed J5.

Exit: Redis is truthfully a structured daily driver, not a read-only demo.

## Per-slice gate

```sh
git diff --check
just check
just check-all
```

Run the mapped live/failpoint/actual-frame checks for the owning requirement.
Commits stay reviewable (roughly 300 changed lines where practical), RED and
GREEN are pushed stepwise, and unrelated user-owned changes are preserved.

## Release loop for every J stage

1. branch HEAD is clean, equals origin and all gates pass at that SHA;
2. independent review reports Critical 0 and High 0;
3. dispatch Preview and wait for hermetic/live/build/publish/tap terminal success;
4. verify artifact manifest/checksums/source SHA/tap version;
5. xbrew install/update, verify CLI/bundle/code-sign identity and launch exact
   Cellar app;
6. perform every installed step mapped to the active J row with sanitized
   action/AX log and independent backend/file/Keychain metadata readback;
7. update evidence and README claims only after the complete sequence passes.

Stable remains forbidden without separate explicit approval.
