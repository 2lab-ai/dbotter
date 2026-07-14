# dbotter — usable MVP implementation and conformance plan

Status: **P0 documentation baseline complete. P1 foundation is independently
reviewed GREEN. T0 remains RED overall; T1/T2/T8/T9 are Implementing; P2–P9
and T3–T7/T10 are Not started.**

This file routes implementation work. The full ordered plan is frozen at
`docs/usable-mvp/plan.md`; this repository-facing ledger must not weaken it.

## Frozen approval set

```text
4c78aa0b957814d0dbaf46e8938a93701e2f85f0a6bb88772ef06b1b1da90cf3  docs/usable-mvp/spec.md
91bfbe89874e88e2c97c7252073cbf7348778192f2a6a349a68b903e1baceaa4  docs/usable-mvp/trace.md
ad649d256286f2e8dd8fa630bba8b64bb9f3ac5e6c5930f7ef432d85d0e8bd97  docs/usable-mvp/plan.md
```

### G0 approval verdict record

The approved plan §2 follow-up gate was completed against exactly the frozen
three-file SHA tuple above:

- UX/product reviewer: `NO BLOCKER`;
- architecture/security reviewer: `NO BLOCKER`.

Both verdicts are bound jointly to those exact spec, trace, and plan hashes.
Changing any artifact or hash invalidates the record and requires both G0
reviews again before implementation proceeds.

P0 may edit only `01-spec.md`, `02-architecture.md`, `03-traces.md`, this file,
`docs/release/{spec,trace}.md`, and `README.md`. It does not modify production
source, tests, workflows, scripts, manifests, lockfiles, or the frozen approval
set.

## Status semantics

- `Complete (docs)` means repository contract reconciliation only.
- `RED` means a failing contract exists and production behavior is not green.
- `Implementing` means one owning slice is GREEN while required owners remain.
- `Not started` means no approved implementation work/evidence is claimed.
- `GREEN` and `Verified` require the evidence rules in `03-traces.md`.

Historical demo code, a prior release, or an existing Homebrew formula cannot
substitute for a slice's RED/GREEN/live/installed evidence.

## Dependency-ordered ledger

| Slice | Scope | Trace ownership | Status |
|---|---|---|---|
| P0 | approve/reconcile repository and release contracts | all routing | Complete (docs) |
| P1 | config/profile/credential/public-error foundation | T0, T1, T2, T8, T9 | GREEN (independently reviewed foundation) |
| P2 | generations/cache/controller/reload/shutdown | T2, T3, T9 | Not started |
| P3 | typed prepared execution/resource/result/CLI seams | T4, shared T5/T6 | Not started |
| P4 | lazy paginated MySQL catalog | T5 | Not started |
| P5 | Redis SCAN/inspect and verified Required TLS | T6 | Not started |
| P6 | profile-scoped native UI/recovery/accessibility | T0–T6, T8, T9 | Not started |
| P7 | exact copy and streaming atomic export | T7 | Not started |
| P8 | live gates/receipts/workflows/bundles/tap contract | T10 | Not started |
| P9 | review/merge/preview/Brew/installed proof | T10 | Not started |

Order is strict: P1 → P2 → P3. P4 and P5 may proceed in parallel only after P3
is integrated. P6 consumes the real P4/P5 outcomes; P7 follows P6; P8/P9 cannot
weaken any earlier gate.

## P0 — repository baseline reconciliation

Files:

- frozen/read-only: `docs/usable-mvp/{spec,trace,plan}.md`;
- reconciled: `01-spec.md`, `02-architecture.md`, `03-traces.md`,
  `04-patch-plan.md`, `docs/release/spec.md`, `docs/release/trace.md`,
  `README.md`.

Completed mapping requirements:

1. removed implemented/deferred claims that contradicted the approved usable
   MVP (session credentials, catalog, cancellation, delete/disconnect, export,
   installed distribution);
2. made config v1 read-only input and v2 the current write contract, with fixed
   backup/frozen-reader/config-contract behavior;
3. fixed Create DraftId/ConnectionId recovery, credential-intent ownership,
   generations, exact tagged task scopes, controller/shutdown, and total public
   recovery;
4. made MySQL user execution prepared-only, Execute-limit focus exact, catalog
   and Redis keyspace typed/bounded, and Redis TLS CA/Host recovery disjoint;
5. linked U0–U9 to T0–T10, P1–P9, RED evidence, commands, receipts, package,
   Brew, and installed AX proof;
6. marked the then-current runtime baseline honestly as T0 RED and later rows
   Not started; the current checkpoint state is recorded in the ledger above.

P0 acceptance commands:

```sh
shasum -a 256 docs/usable-mvp/spec.md docs/usable-mvp/trace.md docs/usable-mvp/plan.md
git diff --check
git status --short --untracked-files=all
git diff -- 01-spec.md 02-architecture.md 03-traces.md 04-patch-plan.md \
  docs/release/spec.md docs/release/trace.md README.md
```

P0 must finish with no production/source/workflow/test diff and no commit.

## P1 — GREEN foundation

The independently reviewed P1 checkpoint implemented and proved:

- exact-path v1 normalization/no write, fixed `.v1.bak`, first v2 mutation,
  frozen v1 reader rejection, and independent config-contract JSON;
- Create versus Update/Delete, DraftId/ConnectionId collision recovery,
  atomic failpoints, parent fsync, and durability-unknown reconciliation;
- credential/update/SessionCredentialIntent matrices, Arc lock lifetime,
  zeroizing Replace buffer, Forget/no-network, and restart availability;
- closed public errors and exhaustive
  `OperationKind × PublicSummary -> NonEmpty<RecoveryAction>`.

P1 provides only the service-level observed-state/cache race foundations needed
by its owned contracts; the full concurrent controller remains P2. T0 therefore
stays RED pending P6 first-run RawInput/AccessKit, while T1/T2/T8/T9 stay
Implementing pending their explicit P2/P6 owners.

Checkpoint gates passed:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --offline -- -D warnings
cargo test --all-features --offline
cargo test --doc --all-features --offline
cargo build --release --all-features --offline
cargo test --test source_contract --all-features --offline
sh scripts/test-receipt-contract.sh
git diff --check
```

Evidence: 136 regular tests plus 12 doctests passed; the separate doctest run
was 12/12; source contract was 1/1; strict Clippy, formatting, release build,
receipt contract, and diff checks passed.

```text
6ccd3ded9a82384ce92b823914e1b5e9f518886460fc0df1c6455ed6d9a327a9  production snapshot (Cargo.toml, Cargo.lock, build.rs, src)
dfacf608d773ca16dd4d25bdf0dc5bfb8f17926baf60d63bcadb1470ffb8114e  tests snapshot (tests)
80c8a75e35103a498fa845591c4418b038ac19c68b3d34aef50cf075dc765bb1  target/release/dbotter
```

## P2 — controller and lifecycle

Implement only after P1 integration. Required RED contracts include monotonic
profile/session generations, tombstones, exact state/cache table, tagged
`RegisteredTask` scopes, bounded lanes/permits, control priority, compare-remove
races, reload/Config uncertain, panic/event-lane cleanup, and async versus
blocking shutdown. No lock may cross await and no task may detach.

## P3 — typed execution/resource seam

Introduce the split `ConnectionPing`, `MySqlPreparedExecution`,
`RedisExecution`, `CatalogBrowser`, and `KeyspaceBrowser` seams plus bounded
snapshots and stable headless contracts. Remove any existing user-text
`sqlx::raw_sql`, `Executor::execute(&str)`, `COM_QUERY`, or prepared-unsupported
fallback. A source/trait test enforces the ban. Static/bound catalog statements
remain prepared.

## P4/P5 — live-gated resource slices

- P4 implements T5's level-specific prepared information-schema queries,
  keyset pagination, cap recovery, permission behavior, CLI, UI, and mandatory
  live fixture. `CATALOG` becomes ready only with that proof.
- P5 implements T6's SCAN/inspect/raw identity/TTL/bounds/classifier and
  verified Required TLS/auth matrix. `KEYSPACE_BROWSE` becomes ready only with
  that proof. CA failure, Host failure, and plaintext-fallback counts are
  separate assertions.

## P6/P7 — installed-journey UI and output

P6 binds real service outcomes to profile-generation workspaces, exact scanner,
stable author ids, real recovery dispatch, RawInput/AccessKit, numerical
contrast, disclosure, Delete warning, and restart. Required ids include
`profile.connection_id`, `profile.host`, Redis CA controls, all Session intent
controls, `editor.target`, `editor.row_limit`, and `editor.timeout`.

P7 implements exact `clipboard_scalar`, `tsv_field`, CSV/TSV/canonical JSON,
immutable provenance, streaming export, 0600/no-clobber/confirmed-replace,
fsync/rename/dir-fsync, cancellation, and independent seeded verification.

## P8/P9 — delivery proof

P8 adds the reusable verification graph, explicit live tests, typed receipts,
four target builds, per-architecture signed macOS bundles, manifest validation,
monotonic preview/tap inputs, app-path/PID checks, and negative fixtures.

P9 integrates, reviews, merges, publishes a new preview, explicitly bumps the
tap, upgrades Homebrew, runs installed CLI and AX journeys, and writes the final
safe receipt. This task does not create a stable tag/release. Rollback is a new
higher preview after exact config-contract preflight; tags/assets are immutable.

## Fixed verification interfaces

The following commands are the full interfaces for later slice claims. P1's
passed checkpoint subset is recorded above. A missing command or failure is
evidence that its owning slice is not Verified, not permission to weaken the
contract.

### Source and hermetic

```sh
./scripts/check-release-contract.sh
./scripts/test-receipt-contract.sh
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-features --locked
```

### Required live

```sh
docker compose -p dbotter-e2e up -d --wait mysql redis-auth redis-tls-auth
DBOTTER_MYSQL_PASSWORD=dbotter-local-only \
DBOTTER_REDIS_PASSWORD=dbotter-redis-local-only \
  ./scripts/verify-live-contracts.sh --config config/local.example.toml
DBOTTER_MYSQL_PASSWORD=dbotter-local-only \
DBOTTER_REDIS_PASSWORD=dbotter-redis-local-only \
  ./scripts/verify-local.sh --config config/local.example.toml
jq -e '.assertions.overall == true' artifacts/receipt.json
```

### Headless source contract

```sh
dbotter version --format json
dbotter config-contract --format json
dbotter --config config/local.example.toml browse mysql schemas \
  --profile mysql-local --page-size 50 --format json
dbotter --config config/local.example.toml browse mysql relations \
  --profile mysql-local --schema dbotter --page-size 50 --format json
dbotter --config config/local.example.toml browse mysql columns \
  --profile mysql-local --schema dbotter --relation receipt \
  --page-size 50 --format json
dbotter --config config/local.example.toml browse redis keys \
  --profile redis-local --filter-mode literal-prefix --filter receipt: \
  --cursor 0 --count 100 --format json
dbotter --config config/local.example.toml inspect redis key \
  --profile redis-local --key-base64 cmVjZWlwdDptYXJrZXI= --format json
```

### Per-architecture macOS package

```sh
./scripts/build-macos-app.sh --channel preview \
  --binary target/release/dbotter --output artifacts
codesign --verify --deep --strict "artifacts/Dbotter Preview.app"
"artifacts/Dbotter Preview.app/Contents/MacOS/dbotter" \
  config-contract --format json
./scripts/check-release-contract.sh --manifest artifacts/preview-manifest.json
```

### Homebrew-installed CLI

```sh
brew update
brew upgrade 2lab-ai/tap/dbotter-preview
brew list --versions dbotter-preview
dbotter version --format json
dbotter config-contract --format json
dbotter --config /tmp/dbotter-installed/config.toml check \
  --profile mysql-installed --format json
dbotter --config /tmp/dbotter-installed/config.toml exec \
  --profile mysql-installed --text 'SELECT 1 AS installed_path' --format json
dbotter --config /tmp/dbotter-installed/config.toml browse mysql schemas \
  --profile mysql-installed --page-size 50 --format json
dbotter --config /tmp/dbotter-installed/config.toml browse redis keys \
  --profile redis-installed --filter-mode literal-prefix --filter receipt: \
  --cursor 0 --count 100 --format json
dbotter --config /tmp/dbotter-installed/config.toml inspect redis key \
  --profile redis-installed --key-base64 cmVjZWlwdDptYXJrZXI= --format json
./scripts/verify-installed.sh --manifest artifacts/preview-manifest.json \
  --config /tmp/dbotter-installed/config.toml
```

### Installed native AX

```sh
APP_PATH="$(brew --prefix dbotter-preview)/Dbotter Preview.app"
./scripts/verify-installed-gui.sh \
  --app-path "$APP_PATH" \
  --config /tmp/dbotter-installed/gui-config.toml \
  --manifest artifacts/preview-manifest.json \
  --output artifacts/installed-gui-receipt.json
jq -e '.assertions.overall == true' artifacts/installed-gui-receipt.json
```

## Final conformance before completion

The audit must prove every T0–T10 state/evidence row; Create/Draft/Profile
identity separation; credential, migration, controller, scanner/prepared-only,
catalog/Redis/TLS, recovery, accessibility/disclosure, copy/export, and restart
contracts; exact identity/config commands; workflow/manifest/tap/Brew/PID chain;
and absence of stable publication. The detailed checklist is approved plan §7.
