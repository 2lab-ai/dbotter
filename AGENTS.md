# dbotter — agent guide

## Current phase pointer (update this section when phase/stage changes)

- **Active phase: Daily-use v1 (`DUV1`)** on branch `feat/daily-use-v1`
  (worktree `.worktrees/feat-daily-use-v1`), currently **Stage 1** of
  `docs/daily-use/plan.md` (D1 config-v3 + D11 workspace shell in flight).
- Phase 1 (usable MVP, T0–T10) is the delivered Preview baseline. Its
  ledger reconciliation landed on `main` (cherry-pick `cd65583` of branch
  commit `9781856`); all current status routes through
  `docs/daily-use/evidence.md`.
- The contract is **DUV1 v1.1** (2026-07-16 amendment, spec §9 log):
  reads run only on a proven server-enforced read-only session (no
  PureBuiltin allowlist; the MySQL 8.4-only gate is write-only), D7/D8
  are deferred to P1, and DU-05 has no TerminalProven fence. Work started
  from v1.0 semantics must rebase onto the v1.1 tuple first.
- Do not infer phase from branch names — read this pointer, then the
  phase tuple below.

## Read order (before any non-trivial change)

1. `01-spec.md` → `02-architecture.md` — repo-wide contract and
   architecture.
2. Active phase tuple: `docs/daily-use/{spec,trace,plan}.md` (frozen).
3. `docs/daily-use/evidence.md` — the mutable status ledger; the only
   authority on what is done.

If the ledger disagrees with git/CI reality, STOP and reconcile the
ledger first. Code lands only on top of a truthful ledger.

## Evidence discipline (the core law)

- Every trace row moves `Not started → RED → Implementing → GREEN →
  Verified`. Commit the RED contract test BEFORE implementation and
  record its SHA in `evidence.md` **in the same commit**. No
  implementation lands for a trace whose RED is not in the ledger.
- Keep uncommitted work small (≲300 lines): slice by trace, commit
  RED→GREEN pairs promptly, each GREEN commit updating `evidence.md`.
  A multi-thousand-line dirty tree is an incident, not a style choice.
- `Verified` requires the row's full evidence class (live / native /
  installed receipts), never just `just check`.

## Gates and shipping

- `just check` must pass before a commit. `just check-all` also
  compile-checks optional desktop and MongoDB seams.
- Merge to `main` at stage boundaries that are independently green and
  usable (gates + independent Critical/High review + CI/Preview green).
  No Stage-5 big-bang integration; no PR-and-stop.
- Every merged stage boundary ships a Preview release; the owner's
  daily-use friction reports outrank spec completeness.
- Never cut a stable (non-preview) tag or release without an explicit
  user instruction.

## Frozen specs and change control

- The active spec tuple is frozen. Never silently edit a frozen `DU-*`
  row to make a test pass; scope changes go through change control and
  are descoped BEFORE implementation, not after.
- The descope items previously listed here were executed on 2026-07-16
  as the **DUV1 v1.1 amendment** — see `docs/daily-use/spec.md` §9
  amendment log and the recomputed freeze tuple in `04-patch-plan.md`.
  v1.1 is subject to the owner's normal review/veto like any spec
  version; implement against the current tuple, and raise any objection
  as a new §9 change rather than a silent edit.

## Engineering invariants

- Keep pure/display state separate from live driver sessions.
- Never hold a lock across `.await`.
- No `unwrap()`/`expect()`/`panic!()`/`todo!()` in production paths.
- Errors are typed with `thiserror`; never log credentials or credential
  URIs.
- Config writes are atomic read-merge-write and profile-keyed.
- Worktrees live in `.worktrees/<branch-name>` and die with their
  PR/merge. A clean branch whose content has landed on `main` is a
  delete candidate, not a workspace.
