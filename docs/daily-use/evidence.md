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
| D3 | `4da8a57` — `test(d3): freeze server-enforced MySQL read session` | `4f047aa` — `feat(d3): enforce server-proven MySQL reads` | `just check`; `just check-all`; exact-source `./scripts/verify-live-contracts.sh --config config/local.example.toml --expected-sha 4f047aa35e1330a6b3173d5fd674c6e3cebfc765` → `live contracts: ok`. Local receipt `artifacts/live-contract-receipt.json`, SHA-256 `66b300cd72b969af059467e57dcfdab213819640fc39294da25e167895b55243`, records all three suites and MySQL safety `statements_executed=8`, `server_side_effect_denied_attempts=1`, `raw_fallback_attempts=0`. | — | GREEN read-admission slice; complete D3 native/installed journeys pending |
| D4 | — | — | — | — | not started |
| D5 | — | — | — | — | not started |
| D6 | — | — | — | — | not started |
| D7 | — | — | — | — | deferred (P1, v1.1) |
| D8 | — | — | — | — | deferred import (P1, v1.1); export baseline retained |
| D9 | — | — | — | — | not started |
| D10 | — | — | — | — | not started |
| D11 | `96d708f` — `test(d11): require object-to-data execution`; `8ecc0c7` — `test(d11): require selected object editor flow`; `8d297b1` — `test(d11): require result inspection modes`; `c259bbf` — `test(d11): require rendered workspace splitter`; `5e6d8dc` — `test(d11): require direct editor history action`; `1a16417` — `test(d11): require closable result tabs`; `02605a8` — `test(d11): require selected result status metrics` | `3b573a2` — `feat(d11): execute table data action`; `164346b` — `feat(d11): retain selected object context`; `740e483` — `feat(d11): add result inspection modes`; `c013659` — `feat(d11): render accessible workspace splitter`; `521a986` — `feat(d11): add direct editor history action`; `89a335e` — `feat(d11): close retained result tabs`; `6bf23b0` — `feat(d11): show selected result status metrics` | Object/editor slices: focused all-features contracts, `just check`, `just check-all`. Result inspection: focused unit/AX contracts plus full gates. Rendered splitter: focused actual-frame test and `ui_layout_contract` (11/11), then full gates. Editor action: focused actual AccessKit interaction proves `Run current` and direct `History` preserve the draft; `scripts/check-release-contract.sh`, `just check`, and `just check-all` pass. Result-tab close: focused model, renderer and actual AccessKit keyboard activation prove adjacent selection, empty-last state and a 44-point close action; active export keeps close disabled. Selected-result status: focused actual-frame AX contract proves duration, returned/affected rows and truncation state are scoped to the selected workspace and clear with the last result; `ui_contrast`, `ui_layout_contract`, `just check`, and `just check-all` pass. | — | GREEN selected-result-status slice; prior object/editor/result/splitter GREENs retained; complete D11 native journeys pending |
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
