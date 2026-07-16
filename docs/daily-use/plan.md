# dbotter Daily-use v1 — delivery plan

The frozen product contract is [`spec.md`](spec.md) (v1.1 — the 2026-07-16 amendment defers D7/D8 to P1 and moves read admission to server-enforced read-only sessions; see spec §9), with [`trace.md`](trace.md) as the vertical source of truth. Work proceeds as reviewable, pushed slices. A green unit is not a release: integration continues through Preview installation and installed-app proof.

## Stage 0 — research and freeze

- Audit current code against daily-use workflows.
- Retain first-party research links and interpretation limits.
- Reconcile the root capability ledger and README with the already-integrated native UI/export/release work.
- Freeze `DUV1` spec and D1–D11 traces.
- Obtain an independent spec/trace review.
- Commit and push the documentation-only freeze.

Exit: no unresolved Critical/High ambiguity; spec and trace cross-reference exactly.

## Stage 1 — durable safe workspace and CLI bootstrap (D1, D3, D4, D9 foundation, D10, D11)

- Start from RED contracts for backward-compatible profile posture, read-only rejection, tabs, batch parsing and workspace/history durability.
- Add profile duplicate/environment/access state.
- Add versioned atomic workspace/history storage and bounds/redaction.
- Replace the single editor/result state with retained editor and result tabs.
- Add current/selection/all execution and history reopen/rerun.
- Add clean-install profile CLI, stdin credential/target input, structured output and stable exits.
- Build the DBeaver-reference persistent navigator/editor/result/status composition with bounded split geometry, tab ordering and the 840×560 named-drawer fallback; apply OpenAI-reference layout/accessibility contracts to every control.
- Commit/push RED and GREEN checkpoints separately.

Exit: restart proof retains multiple tabs and searchable history; after only the permitted bounded static capability reads, read-only has zero user-target or typed-mutation dispatch.

## Stage 2 — data navigation and transaction safety (D2, D5, D9, D11)

- Start from RED table-data, statement identity and transaction state-machine contracts.
- Add index/identity metadata and bounded table data paging/filter/sort.
- Add serialized MySQL connection worker with begin/commit/rollback and rollback-on-close semantics.
- Add fail-closed `partial_revokes=OFF` plus direct global metadata-visibility proof and live restricted-schema negative fixtures.
- Enforce Safe writes and production posture before driver work.
- Add pending-change and OutcomeUnknown UI states with a durable Unknown fence and idempotent fold/retry before acknowledgement.
- Commit/push RED and GREEN checkpoints separately.

Exit: live MySQL proves rollback and commit across tabs, plus disconnect rollback; an in-transaction ReadOnly statement with a function call or view reference is denied before I/O.

## Stage 3 — safe MySQL edits and result detail (D6, D9, D10, D11 — D7/D8 deferred to P1 by v1.1)

- Start from RED typed mutation, conflict and confirmation contracts.
- Add staged identifiable MySQL row add/update/delete and review/apply/discard.
- Complete record/value detail and explicit local filter/sort.
- Commit/push each driver slice and the integrated UI slice.

Exit: live MySQL matrices pass; no stage auto-commits.

## Stage 4 — independent review and full gates (D1–D11)

- Run format, lint, unit, contract, controller and full checks.
- Run live MySQL and Redis tests with retained receipts.
- Run native accessibility/keyboard/copy/export journeys at wide and minimum window sizes.
- Launch the installed app with only the disposable synthetic visual fixture, verify the AX allowlist/forbidden sentinel, strip raster metadata, then retain the exact DU-11 four-journey screenshot matrix at 1,440×900 and 840×560; have an external visual/UX reviewer check context preservation, density, overlap, clipping, status clarity and action reachability.
- Assign an independent senior review for safety, privacy, UX and spec/trace/code conformance.
- Fix all Critical/High findings and rerun affected plus full gates.
- Commit/push review fixes and mutable `evidence.md` updates.

Exit: D1–D11 evidence rows are green, exact branch HEAD is clean and matches origin.

## Stage 5 — merge, Preview, xbrew and installed proof (D12)

- Integrate the reviewed branch to `main` without losing unrelated work.
- Push exact `main` commit.
- Publish only the Preview channel and wait for CI/release/tap completion.
- Verify public source, tag, assets, checksums and tap formula point at the same commit/artifact.
- Reinstall/upgrade with xbrew.
- Launch the installed app and run the selected MySQL/Redis native smoke journeys.
- Append the release/install receipt to the release docs and zbrain workflow log.

Exit: installed artifact evidence proves D12 and all required work is complete.

## Stop conditions

- Never create a stable tag/release without a separate explicit user instruction.
- Never weaken a frozen `DU-*` row to make a test pass.
- Stop before a destructive external action not already authorized by the task.
- If a credential or service is unavailable, continue every offline and local proof first; report the exact remaining live gate rather than claiming completion.
