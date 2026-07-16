# dbotter — current trace ledger

Status: **Daily-use v1 contract frozen; implementation rows not started**

Authoritative trace: [`docs/daily-use/trace.md`](docs/daily-use/trace.md)

## Ledger authority

D1–D12 replace the old root T0–T10 status ledger for current implementation decisions. The frozen T0–T10 details remain unchanged under `docs/usable-mvp/` as the delivered usable-MVP baseline. They must not be read as a current exclusion of tabs, history, transactions, editing or import.

State meanings:

- `Not started`: no trace-derived RED commit exists.
- `RED`: a failing contract proves the missing/incorrect behavior.
- `Implementing`: at least one owning layer is GREEN while required seams/evidence remain.
- `GREEN`: hermetic model/service/driver/UI contracts for the row pass.
- `Verified`: GREEN plus mandatory live, native/CLI, conformance/file-map and disclosure proof at the exact commit.

Only the mutable ledger in `docs/daily-use/evidence.md` changes these states.

## Delivered baseline routing

| Historical trace | Delivered baseline now present in source | Daily-use continuation |
|---|---|---|
| T0–T3, T8–T9 | config/profile/credential lifecycle, controller, recovery, native profile UI | D1, D4, D10, D11 |
| T4 | exact target, prepared MySQL/policy Redis execution, cancel, result provenance | D3, D5, D9, D10 |
| T5 | bounded lazy MySQL schemas/relations/columns | D2, D6, D8 |
| T6 | bounded Redis SCAN/inspect and Required TLS | D2, D7 |
| T7 | result copy and bounded no-clobber export | D8, D9 |
| T10 | CI/Preview/tap/package/install/AX receipt machinery | D12 |

The baseline was integrated through source commit `340133dca652a7bf51d652f06cdb7436b42bbc58`. This routing statement does not mark any new D row GREEN.

## Current D1–D12 ledger

| Trace | Primary user journey | Initial status | Required proof class |
|---|---|---|---|
| D1 | profile posture, duplicate and v1/v2→v3 migration | Not started | config/frozen-reader/controller/native |
| D2 | object-to-data typed browse/filter/sort/page | Not started | model/service/MySQL live/native |
| D3 | tabs, closed classifiers, current/selection/all and result tabs | Not started | parser/controller/native |
| D4 | durable drafts/history, privacy posture and profile lifecycle | Not started | filesystem failpoints/restart/native |
| D5 | one-connection MySQL transaction state machine | Not started | actor/service/live MySQL/native |
| D6 | lossless staged MySQL row add/update/delete | Not started | typed DML/conflict/live MySQL/native |
| D7 | type-aware Redis edit/TTL/delete | Not started | typed atomic mutation/live Redis/native |
| D8 | bounded reviewed CSV import and retained export | Not started | parser/transaction/live MySQL/native file |
| D9 | bounded result tabs, local inspect/filter/sort/copy | Not started | result model/controller/native |
| D10 | clean-install CLI bootstrap and automation | Not started | CLI contract/shell installed journey |
| D11 | OpenAI visual language with DBeaver-reference persistent navigator/editor/result/status usability, wide/min reachability/accessibility/cancel | Not started | RawInput/AccessKit/native AX/wide+min screenshots/external review |
| D12 | exact source→Preview→tap→xbrew→installed proof | Not started | CI/public release/tap/install receipt |

## Correlation and state ownership

The exact vocabulary lives in the Daily-use trace/spec. These ownership rules are global:

- profile network work carries stable user-facing `ProfileId`, immutable `ProfileInstanceId`, `ProfileGeneration`, `SessionGeneration` and `OperationId` where applicable;
- workspace drafts/history and durable MySQL Active/Resolving/TerminalProven/OutcomeUnknown or Redis RedisApplying/RedisOutcomeUnknown safety fences bind immutable `ProfileInstanceId`; in-memory result/stage/transaction state also binds the active profile generation;
- TerminalProven and OutcomeUnknown independently replay their TransactionId disposition through the durable profile shard and live state before their allowed cleanup; Unknown fanout failure retains the fence and global block;
- editor/result tab, history, table page, staged edit, Redis review and import plan each have their own stable local identity; every MySQL transaction carries a stable TransactionId and Redis unknown recovery carries only a private-root HMAC key token;
- export retains `(ResultId, OperationId)`; global load/shutdown/storage work retains its explicit operation identity;
- a fold never borrows the currently selected profile/tab/result to repair missing identity;
- transaction data work routes through the profile's worker while active; no UI object owns the live connection.
- MySQL mutation metadata completeness additionally requires exact typed `partial_revokes=OFF`; ON/unknown/restricted-schema cases are RED gates, while eligible typed reads remain available.

## Advancement rule

For each row:

1. add a trace-derived failing contract and record the RED commit;
2. implement the smallest vertical seam without weakening the test;
3. pass focused and full hermetic gates and record the GREEN commit;
4. run the row's mandatory live/native/CLI proof;
5. obtain independent conformance/security/UX review where assigned;
6. append exact commit, command/run and artifact evidence to `docs/daily-use/evidence.md`;
7. mark Verified only when all proof classes agree.

Historical commits, an older public release or an already-installed formula cannot advance a D row.
