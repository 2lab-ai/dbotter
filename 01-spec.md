# dbotter — current product contract router

Status: **Daily-driver v1.2 frozen — J2 local GREEN awaiting verification**

Reset source commit: `03d6051`

Current contract ID: `DUV1` v1.2

## Authority

Read current work in this order:

1. [`docs/daily-use/research.md`](docs/daily-use/research.md) — first-party
   product evidence and P0/P1 inference.
2. [`docs/daily-use/spec.md`](docs/daily-use/spec.md) — product, safety, privacy,
   interaction and installed acceptance contract for J1–J5.
3. [`docs/daily-use/trace.md`](docs/daily-use/trace.md) — vertical ownership,
   RED boundary and evidence class.
4. [`docs/daily-use/plan.md`](docs/daily-use/plan.md) — journey-by-journey
   RED/GREEN/Preview/xbrew plan.
5. [`docs/daily-use/evidence.md`](docs/daily-use/evidence.md) — mutable status
   ledger; the only authority on what is actually complete.
6. [`02-architecture.md`](02-architecture.md) — shared ownership and safety
   architecture.

The first four files are the frozen tuple after review. Their SHA-256 values are
recorded in [`04-patch-plan.md`](04-patch-plan.md). A semantic change requires a
version bump, synchronized tuple and new independent contract review.

`docs/usable-mvp/` and DUV1 v1.0/v1.1 evidence are historical. They are valuable
foundations but cannot establish v1.2 journey completion.

## Truth at the reset commit

At `03d6051`, real production call paths exist for MySQL/Redis profile
create/test/save/connect, MySQL schema/relation/column browse, generated bounded
base-table SELECT, read-only current/selection/all execution, in-memory
editor/result tabs, result inspect/copy/export and Redis SCAN/inspect.

The app is not yet a daily driver: credentials lack Keychain storage and SSH,
table Data is not a typed pageable editor, SQL writes and managed transactions
are blocked, workspace/history disappear on quit, CSV import and structured
Redis mutation do not exist, and installed black-box user journeys have not been
proven. Labels that only set status text are not working recovery actions.

## Daily-driver v1.2 outcome

| Journey | Complete installed outcome |
|---|---|
| J2 first | Saved work survives force-kill/relaunch, history reopen has zero auto-run, and explicit rerun returns a fresh result without credential/result-payload persistence |
| J1 | clean-install secure MySQL onboarding reaches searchable table/view Data and reconnects from Keychain |
| J3 | identifiable typed MySQL row changes can be reviewed, rolled back or committed with conflict/read-only/production/unknown safety |
| J4 | exact-scope no-clobber export and previewed, mapped, transaction-safe CSV import work end to end |
| J5 | Redis bounded browse/typed inspect plus structured core-type/TTL/delete edits work with immediate-apply and production guards |

Each journey independently requires RED-first contracts, local/live/native
proof, independent Critical/High review, Preview publication, xbrew installation
and installed action/readback evidence. There is no final proxy-green stage.

## Non-negotiable invariants

- Trace before cross-layer behavior change; RED is committed and pushed before
  production implementation.
- Exact identity at every async boundary; stale/cancelled work cannot overwrite
  current state.
- No lock or borrowed guard crosses `.await`; stateful driver ownership is a
  serialized async worker.
- Credential-channel values stay out of config, workspace/history, logs, errors
  and evidence; persisted arbitrary SQL text has explicit disclosure and opt-out.
- Read-only blocks mutation before target dispatch; Production is visible and
  destructive actions require exact confirmation.
- Generated mutation uses quoted catalog identifiers and typed parameters.
- A payload-free durable intent precedes mutation wire dispatch; terminal
  transaction/mutation outcome is never guessed or auto-retried.
- Everything retained or transferred is bounded and visibly truncated.
- Private local writes are atomic and fail closed on corruption or uncertainty.
- No stable tag/release without separate explicit approval.

## Change control

Implementation may split a journey into small RED/GREEN pairs but cannot weaken
the installed observable or safety/privacy contract. A desired semantic change
returns the tuple to Review candidate before production code continues.
