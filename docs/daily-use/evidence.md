# dbotter Daily-driver v1.2 — mutable evidence ledger

Normative contract: [`spec.md`](spec.md)

Normative vertical trace: [`trace.md`](trace.md)

Freeze tuple: [`../../04-patch-plan.md`](../../04-patch-plan.md)

This ledger is intentionally outside the frozen tuple. A status update cannot
weaken the normative contract. Exact commands and receipts must be retained
without credentials, query values, keys, cell values or imported/exported data.

| Journey | RED commit | GREEN commit(s) | Local/live/native evidence | Preview/xbrew/installed evidence | Status |
|---|---|---|---|---|---|
| J2 durable SQL workspace/history | `0e1e38d3d1bdf945a49595816d7946b21c2f97f9` — UI RED; this commit — private store RED | — | RED UI contract fails 3/3 at `cargo test --all-features j2_red -- --nocapture`; store contract `cargo test --test daily_use_workspace_store_contract` must fail until exact manifest/shard reopen, single-writer read-only fallback, privacy/permissions, bounds and isolated corruption quarantine exist | — | RED |
| J1 secure MySQL connection/Data | — | — | — | — | not started |
| J3 safe typed MySQL row edit | — | — | — | — | not started |
| J4 bounded export/CSV import | — | — | — | — | not started |
| J5 Redis browse/structured edit | — | — | — | — | not started |

## Status transition

`not started → RED → implementing → local GREEN → verified`

- RED names the pushed failing contract commit and retained expected failure.
- Local GREEN names all focused/hermetic/live/native commands at the exact SHA.
- Verified additionally names independent Critical/High review, Preview run/tag,
  artifact/tap identity, xbrew version/executable and installed black-box proof.

## Historical v1.1 evidence (not v1.2 completion)

The branch entered v1.2 at `03d6051`. Retained v1.1 foundations include:

- D3 RED commits `4da8a57`, `9e89fbc`, `36fbd76`; GREEN commits `4f047aa`,
  `89a9863`, `54961eb` for server-proven read-only execution and Run all.
- D11 RED/GREEN slices through `08da488` for object-to-data, result inspection,
  workspace split/collapse, history action and closable editor/result tabs.
- Preview run `29534925455`, source
  `8a22e1393134450025a275be19a97332d06317b7`, tag
  `preview-2026-07-16-213015-29534925455-1-8a22e1393134` and xbrew version
  `2026.07.16.213015.29534925455.1` proved packaging and installed executable
  identity. Native computer-use/AX state was unavailable, so it did not prove a
  complete installed journey.

At the reset, create/test/save/connect, catalog browse, generated bounded table
SELECT, read-only current/selection/all, in-memory editor/result tabs, result
inspect/copy/export and Redis SCAN/inspect had real call paths. Writes,
transactions, persistent workspace/history, secure-store/SSH, CSV import and
structured Redis mutations did not. UI labels that merely set status text or
stop the runtime are not accepted as reveal/restart implementations.

## Append rule

1. Record and push RED before production implementation.
2. Record GREEN only after the required gates pass at the named commit.
3. Do not publish user payloads or secrets in evidence.
4. Mark verified only when the installed acceptance and independent backend/file
   readback agree at the same source SHA.
