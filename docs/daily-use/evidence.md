# dbotter Daily-driver v1.2 — mutable evidence ledger

Normative contract: [`spec.md`](spec.md)

Normative vertical trace: [`trace.md`](trace.md)

Freeze tuple: [`../../04-patch-plan.md`](../../04-patch-plan.md)

This ledger is intentionally outside the frozen tuple. A status update cannot
weaken the normative contract. Exact commands and receipts must be retained
without credentials, query values, keys, cell values or imported/exported data.

| Journey | RED commit | GREEN commit(s) | Local/live/native evidence | Preview/xbrew/installed evidence | Status |
|---|---|---|---|---|---|
| J2 durable SQL workspace/history | `0e1e38d3d1bdf945a49595816d7946b21c2f97f9` — UI RED; `143167074ed56b47696f11a0e8d52f9207b9b06b` — private store RED; `872b36fce5fb7158d67e6b1535293b21a130cf6c` — store safety RED; `9743956fb81b05a5f261e149989104f9aa094c75` — clear/quarantine RED; `6467d307e1f9cef445b39ff8f4a6f819e3b6906e` — installed restart RED; `d5be4dc2d6615cbaed641d91543274ba40add09c` — installed false-positive RED; `67592343b9154fc04dfff894bc5bdf8fa72c8dbd` — complete installed acceptance RED; `55977e2f03c8de94f702a62eb4d1f10ef36f611f` — remaining installed review gaps RED; `9ac440764fb21e3e90216ba0c4a794c350a60902` — ordered typed-error retention and fail-closed final-writer RED | `d674aa6984f2ceace1edd834eba5c0be7ca5797e` — GREEN A private atomic store; `9f4d066` + `876ff7d` — durable editor domain and bounded assistance; `891c8f4` + `cb2263f` + `7c45bd9` — serialized persistence, typed history and retention; `5b1e373` + `6847d4b` + `fd026bd` + `02d902e712719af9dab80ee161f67216c23813a4` — desktop restore, exact raw-byte accounting, global shrink-first retention and review closure; `ff7a4425ec99bd1236ed1d77f889f40032ed4efa` — opaque profile AX identity and restart verifier; `2f29e4383e2664e82961292534da4d92ff8aa81d` — exact installed readback; `efc88a03b1f4b702a1437684d5c0e0ad1879876d` — complete six-step installed verifier, isolated fixture and bounded private-workspace scanner; `58d2ad7` — production syntax status, full Value mode and typed result-error surface; `a66352b58e4183e4a98e3e1cbcf8caa13486bb6b` — exact/+1 workspace CLI and expanded installed privacy/interaction proof | At `d674aa6984f2ceace1edd834eba5c0be7ca5797e`: workspace contract 31/31; private marker/descriptor units 3/3; `cargo fmt --check`, all-target/all-feature Clippy `-D warnings`, `just check`, and `git diff --check` pass. At `02d902e712719af9dab80ee161f67216c23813a4`: `just check`, `just check-all`, `git diff --check`, all-feature library tests 253/253 and workspace-store contracts 34/34 pass. Focused regressions prove terminal history survives another workspace clear/save and same-instance retag, stale identity is discarded, raw noncanonical manifest bytes are reported exactly, mismatched commit byte ACK forces a baseline-only reload without overwriting local work, and generation 9→10 commits the single global shrink before growth without any intermediate 128 MiB breach. Independent fixed-hash review: Critical 0 / High 0, five focused Cargo commands pass, clean tree, no source changes. At `ff7a4425ec99bd1236ed1d77f889f40032ed4efa`: `just check`, `just check-all`, `git diff --check`, ShellCheck and release-contract checks pass; all-feature library tests 253/253, workspace-store contracts 34/34, installed J2 contracts 4/4 and renderer contracts 10/10 pass; the fixed tracked Swift source compiles twice to a byte-identical arm64 Mach-O driver. At `efc88a03b1f4b702a1437684d5c0e0ad1879876d`: focused installed J2 contracts 6/6, scanner clean/encoded-secret/symlink live cases, `cargo fmt --all -- --check`, `bash -n`, ShellCheck, release-contract and hermetic checks, `just check`, `just check-all`, and byte-identical tracked Swift driver builds pass. At `a66352b58e4183e4a98e3e1cbcf8caa13486bb6b`: Value keyboard/AX 7/7, installed J2 11/11, all-feature unit 254/254 and workspace-store 34/34 pass; `bash -n`, ShellCheck `-x`, tracked Swift optimized build, all-target/all-feature Clippy `-D warnings`, `just check`, `just check-all`, release contract and exact-SHA hermetic release-identity verification pass. Fixed-hash review at `ec909c87a8ed13c29954c3ebc8c783ae4bfe6fed` found Critical 0 / High 2: typed failures occupied one transient workspace slot instead of ordered selectable output tabs, and `stop_pid` could return with the final writer alive before the privacy scan. At `9ac440764fb21e3e90216ba0c4a794c350a60902`, the actual-frame RED fails at one output instead of ordered result+error, while installed contracts fail for missing later-result error reselection and fail-closed writer termination. Preview and installed black-box execution remain pending. | — | RED |
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
