# dbotter — agent guide

## Current phase pointer

- Active phase: `DUV1` v1.2 on `feat/daily-use-v1`.
- Active stage: Stage J2 RED; implementation order remains
  `J2 → J1 → J3 → J4 → J5`.
- v1.1 D1–D12 and usable-MVP T0–T10 are historical foundations. Current status
  comes only from `docs/daily-use/evidence.md`.
- Publish Preview at every verified journey boundary. Never create/move a stable
  tag or release without separate explicit user approval.

## Required read order

1. `01-spec.md` and `02-architecture.md`.
2. `docs/daily-use/{research,spec,trace,plan}.md`.
3. `docs/daily-use/evidence.md`.
4. The code/tests owned by the active J row.

If ledger and git/CI/installed reality disagree, reconcile the ledger before
claiming progress.

## Evidence discipline

- Every journey/slice moves `not started → RED → implementing → local GREEN →
  verified`.
- Commit and push the failing user-boundary RED before production code, and
  record its SHA in the same evidence-ledger commit.
- Keep changes reviewable (target roughly 300 changed lines where practical),
  push RED/GREEN pairs stepwise and preserve unrelated user-owned changes.
- Local GREEN requires focused checks plus `git diff --check`, `just check` and
  `just check-all`; add live/actual-frame checks for owning layers.
- Verified requires independent Critical 0/High 0 review, exact Preview/tap
  chain, xbrew install and installed action/readback proof.

## Engineering invariants

- Trace before cross-layer behavior change.
- Pure/display state is separate from live driver resources.
- No lock or borrowed guard crosses `.await`.
- Exact identity guards every async fold; stale work cannot overwrite current.
- Credential-channel values and user payloads never enter public errors, logs,
  evidence or unapproved persistence; persisted SQL text follows its disclosure
  and opt-out contract.
- Config/workspace writes are private, bounded, atomic and fail closed.
- Generated mutation uses quoted catalog identity and typed values.
- No `unwrap`, `expect`, `panic!` or `todo!` in production paths.
