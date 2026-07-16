# dbotter Daily-use v1 — mutable evidence ledger

Normative contract: [`spec.md`](spec.md)

Normative vertical trace: [`trace.md`](trace.md)

Freeze tuple: [`../../04-patch-plan.md`](../../04-patch-plan.md)

This file records implementation status and is intentionally not part of the
Stage 0 spec/trace/plan hash tuple. Every advancement names the exact commit and
retained command, run, artifact or native receipt. Updating status here cannot
weaken or reinterpret a frozen requirement. Rows deferred by the spec §9
v1.1 amendment are marked `deferred` rather than deleted.

| Trace | RED commit | GREEN commit | Local/live evidence | Native/installed evidence | Status |
|---|---|---|---|---|---|
| D1 | — | — | — | — | not started |
| D2 | — | — | — | — | not started |
| D3 | `4da8a57` — `test(d3): freeze server-enforced MySQL read session` | — | RED contract: `cargo test --features desktop --test daily_use_mysql_read_session_contract` must fail until the generic execution port is removed and same-connection read-only proof precedes general SELECT dispatch | — | RED |
| D4 | — | — | — | — | not started |
| D5 | — | — | — | — | not started |
| D6 | — | — | — | — | not started |
| D7 | — | — | — | — | deferred (P1, v1.1) |
| D8 | — | — | — | — | deferred import (P1, v1.1); export baseline retained |
| D9 | — | — | — | — | not started |
| D10 | — | — | — | — | not started |
| D11 | — | — | — | — | not started |
| D12 | — | — | — | — | not started |

## v1.1 branch reconciliation

The branch already contains useful v1.0-era foundations, but none is accepted
as a complete v1.1 trace row without the required RED-first contract and full
evidence class:

- D1: config-v3 wire/migration primitives and visible posture controls
  (`c424e4e` through `dc694c1`), while GUI/CLI legacy migration and duplicate
  remain incomplete.
- D3/D9: session-only editor/result tabs (`5dc4684` through `7c9530e`), while
  Run all, durable history, per-editor result ownership and the v1.1 read
  admission path remain incomplete.
- D11: persistent three-zone shell and geometry (`8341cb0` through `b121ed5`),
  while the canonical native journeys and complete action/recovery surfaces
  remain incomplete.

These commits are retained implementation foundations, not retroactive RED or
GREEN evidence. The rows therefore remain `not started` until a v1.1 RED
contract is committed and recorded below before the next production change.

## Append rule

1. Record the RED commit before implementation.
2. Record the GREEN commit only after focused and required full hermetic gates.
3. Link retained live/native/installed receipts without credential, query,
   key, cell or CSV payload content.
4. Mark `Verified` only when the frozen row's complete evidence class agrees.
