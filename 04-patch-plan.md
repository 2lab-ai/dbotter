# dbotter — current implementation and conformance plan

Status: **Stage 0 frozen; production implementation admitted only through trace-derived RED**

Branch: `feat/daily-use-v1`

Worktree: `.worktrees/feat-daily-use-v1`

Baseline: `340133dca652a7bf51d652f06cdb7436b42bbc58`

The detailed ordered plan is [`docs/daily-use/plan.md`](docs/daily-use/plan.md). The authoritative requirements and vertical traces are [`docs/daily-use/spec.md`](docs/daily-use/spec.md) and [`docs/daily-use/trace.md`](docs/daily-use/trace.md).

## Contract freeze gate

No production code starts until:

1. root `01-spec.md`, `02-architecture.md`, `03-traces.md`, this file and README route consistently to DUV1;
2. independent review reports no unresolved Critical/High finding;
3. `git diff --check` and documentation link/path checks pass;
4. the exact three-file SHA-256 tuple below replaces `PENDING` and is committed/pushed as the Stage 0 freeze.

```text
4d24f472de775c46f0e50f93a78ba0eb13734543c6229b2a12bcfc5360ce2324  docs/daily-use/spec.md
a7b5707b860c1b930c91d36c33e17b8b6b73081704b5a6a03da56449342074dc  docs/daily-use/trace.md
80e2829a18615ab4a1394712079e32779870976b64c0bd28dbb6064c12efcfbe  docs/daily-use/plan.md
```

Changing any frozen artifact invalidates the tuple and requires a new independent contract review before implementation resumes.

`docs/daily-use/evidence.md` is deliberately outside this tuple. Evidence-only
status/receipt updates there do not invalidate the normative hashes; any change
to spec, trace requirements or plan still does.

The old frozen usable-MVP hash set remains historical and unchanged under `docs/usable-mvp/`; it is not the current approval gate.

## Ordered delivery ledger

| Stage | Traces | Required checkpoint | Status |
|---|---|---|---|
| 0 | all routing | research/gap audit, closed spec/trace, independent review, exact hashes | Complete |
| 1 | D1, D3, D4, D9 foundation, D10, D11 | RED then GREEN durable safe workspace and CLI bootstrap | Not started |
| 2 | D2, D5, D9, D11 | RED then GREEN typed table data and real MySQL transaction worker | Not started |
| 3 | D6, D7, D8, D9, D10, D11 | RED/GREEN safe MySQL/Redis edits, CSV import and result detail | Not started |
| 4 | D1–D11 | full/live/native gates, independent conformance review and fixes | Not started |
| 5 | D12 | merge, Preview, public/tap proof, xbrew reinstall and installed use | Not started |

Each independently reviewable RED/GREEN unit is committed and pushed before the next unit. GREEN is not a stopping point: Stage 5 follows automatically after Stage 4 unless an external credential/authority gate is genuinely required.

## Fixed verification interfaces

Hermetic gates:

```sh
git diff --check
just check
just check-all
```

Existing live foundations, extended by the owning D row rather than replaced:

```sh
./scripts/verify-live-redis.sh
./scripts/verify-live-contracts.sh --config config/local.example.toml
./scripts/verify-local.sh --config config/local.example.toml
```

Release/install foundations:

```sh
./scripts/check-release-contract.sh
sh scripts/test-receipt-contract.sh
sh scripts/test-installed-verifier-contract.sh
```

Every command runs at the exact candidate commit. A missing fixture, environment value, named assertion, platform artifact or native observation is a failed gate, not a waiver.

## Worktree and integration policy

- Reuse the current Daily-use integration worktree; use smaller trace worktrees only when ownership is independent and merge order is explicit.
- Preserve user-owned dirty changes and old worktree artifacts; do not clean/reset them to make a gate pass.
- Update `docs/daily-use/trace.md` before a cross-layer behavior change.
- RED and GREEN commits name the D trace they advance and are pushed stepwise.
- Before integration, prove branch HEAD is clean and equals its origin.
- Merge reviewed work to `main`, rerun required gates at the exact merge commit and push.

## Release policy

- Publish Preview only.
- Never create or move a stable tag/release without separate explicit user approval.
- The Preview tag, source archive, per-architecture artifacts, manifest, checksums, tap formula and installed executable must all bind the same source commit.
- Wait for CI, Preview and tap completion; do not treat dispatch as success.
- Reinstall/update through xbrew, launch the installed app and retain CLI plus native proof from that artifact.
- Append the final receipt to the release evidence and zbrain workflow log without credentials or user data. Public screenshots are limited to the isolated tracked synthetic visual fixture after the AX allowlist/forbidden-sentinel and metadata-strip gate.

## Stop conditions

- A frozen requirement cannot be weakened to satisfy implementation.
- Read-only, transaction, production confirmation, privacy, bounds or identity uncertainty fails closed.
- Destructive external operations outside the authorized repository/release/install scope require a new user decision.
- A blocked live service does not erase completed local proof; report the exact missing gate and continue all safe independent work first.
