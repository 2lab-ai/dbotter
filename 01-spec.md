# dbotter — current product contract router

Status: **Daily-use v1 Stage 0 frozen — implementation starts from RED**

Baseline source commit: `340133dca652a7bf51d652f06cdb7436b42bbc58`

Current contract ID: `DUV1` v1.0

## Authority

For current work, read in this order:

1. [`docs/daily-use/spec.md`](docs/daily-use/spec.md) — frozen product, safety, privacy and release acceptance requirements `DU-01…DU-11`.
2. [`docs/daily-use/trace.md`](docs/daily-use/trace.md) — hash-frozen authoritative D1–D12 vertical trace and implementation order.
3. [`docs/daily-use/plan.md`](docs/daily-use/plan.md) — staged RED/GREEN/live/native/Preview delivery plan.
4. [`docs/daily-use/evidence.md`](docs/daily-use/evidence.md) — mutable commit/run/native status ledger outside the frozen tuple.
5. [`02-architecture.md`](02-architecture.md) — delivered ownership and safety architecture, extended by the Daily-use trace before any cross-layer change.

The SHA-256 tuple of the approved Daily-use artifacts is recorded in [`04-patch-plan.md`](04-patch-plan.md). A change to that tuple requires a new independent contract review before implementation continues.

The previous `docs/usable-mvp/{spec,trace,plan}.md` set is immutable historical baseline evidence. It no longer excludes query history, editable data, managed transactions, import or tabs from the current task. Its root-ledger checkpoint narrative was archived by this router because source `main` has already integrated the native UI, copy/export, release verification and public Preview work.

## Delivered baseline

At the baseline commit, dbotter truthfully provides:

- config v1 read/migrate and v2 write, atomic fail-closed profile mutation and non-persisted None/Session/Environment credential modes;
- native MySQL/Redis profile create/edit/test/connect/reconnect/disconnect/delete and static recovery states;
- exact selected/current target extraction, prepared-protocol MySQL execution, closed Redis execution policy, cancellation and bounded retained results;
- lazy bounded MySQL schema/relation/column browsing and Redis SCAN/type/TTL/value inspection;
- result provenance, cell/row copy and bounded no-clobber CSV/TSV/JSON export;
- native accessibility identifiers/harness, four-target Preview packaging, manifest/tap/install receipt verification and a public xbrew-installable Preview.

MongoDB remains visibly Planned. The delivered baseline is useful for one-off inspection, but it has the daily-use gaps listed in the README and D1–D12 audit.

## Daily-use v1 outcome

The current product is complete only when a user can finish this loop without losing work or bypassing a safety state:

> connect → locate data → inspect it → run work → review the result → make a safe change → commit or discard → find and reuse the work later

Release-blocking contracts:

| Contract | Outcome |
|---|---|
| DU-01 | config v3 profile environment/access posture, duplicate, migration and read-only zero user-target mutation dispatch |
| DU-02 | MySQL object-to-data navigation with typed filter/sort/paging; retained Redis browse |
| DU-03 | bounded persistent editor/result tabs, closed SQL/Redis admission and current/selection/all execution |
| DU-04 | atomic local drafts/history with disclosure, opt-outs, bounds and profile lifecycle |
| DU-05 | real profile-scoped MySQL transaction worker and explicit unknown recovery |
| DU-06 | lossless identifiable staged MySQL row editing with conflict detection |
| DU-07 | deferred to P1 by v1.1 — v1 keeps Redis read/browse/inspect; typed mutations/TTL/delete return in P1 |
| DU-08 | retained bounded export; relational CSV import deferred to P1 by v1.1 |
| DU-09 | bounded result tabs, local filter/sort and record/value detail |
| DU-10 | clean-install CLI profile bootstrap, safe stdin, structured output and exact exit codes |
| DU-11 | OpenAI visual language plus DBeaver-reference navigator/editor/result/status usability floor, wide/minimum reachability, keyboard/AX and exact cancellation |

D12 binds all product rows to the exact reviewed source commit, public Preview artifacts, tap version, xbrew installation and installed-app proof.

## Non-negotiable invariants

- Trace before code: update D1–D12 before changing a cross-layer command, event, state or persistence contract.
- Identity is explicit across async boundaries; stale/cancelled/mismatched events never overwrite newer state.
- No Rust mutex/RwLock or borrowed in-process guard crosses `await`; stateful MySQL transaction ownership uses a serialized async worker. The synchronous owner of the nonblocking OS advisory safety lease retains its file descriptor across an unresolved MySQL safety-fence lifetime without exposing guarded in-memory data to async code.
- Credential-channel values do not enter config/workspace/history/log/error/evidence data. Public UI screenshots use only the isolated tracked synthetic fixture and never the user's config/data.
- User-controlled identifiers are validated/quoted and generated mutation values use typed parameters.
- Unknown SQL/Redis operations, raw transaction/session controls and MySQL implicit-commit operations fail closed before user-target I/O; bounded static capability/catalog reads are not user-target dispatch.
- Read-only rejects every mutation before user-target/typed-mutation dispatch; a metadata-only lease may classify bounded source. Production requires the exact review/confirmation contract.
- MySQL mutation requires exact typed `@@GLOBAL.partial_revokes=OFF` plus direct non-role-only global SELECT/TRIGGER/REFERENCES visibility; otherwise reads may remain available but every mutation is disabled before target DML.
- A MySQL OutcomeUnknown fence is the TransactionId replay authority for the durable Unknown history/result/stage fold and cannot be acknowledged or removed before that fold agrees.
- Every retained list/file/value is bounded; driver transient-allocation limitations remain disclosed.
- File writes are permission-restricted, atomic and fail closed on uncertain durability.
- Public errors are closed/static and contain no backend prose or user data.
- No production `unwrap`, `expect`, `panic!` or `todo!`.
- Preview only; no stable tag/release without separate explicit approval.

## Change control

Implementation may split a trace into smaller RED/GREEN commits but may not weaken a `DU-*` row. A semantic scope change requires a spec version bump, trace update, new artifact hashes and independent contract review before production code.
