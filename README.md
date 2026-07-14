# dbotter

dbotter is a local Rust desktop database client targeting a usable MySQL and
Redis preview. MongoDB remains honestly Planned.

## Current status

The approved contract is complete, but usable-MVP production implementation
has not begun on this branch:

- P0 repository documentation baseline: complete in the current uncommitted
  documentation diff;
- runtime T0 / implementation P1: RED;
- runtime T1–T10 / P2–P9: Not started.

The source tree contains historical demo code and release machinery. It may
compile or demonstrate older behavior, but it is **not** proof of the approved
usable MVP and must not be described as installed/verified. In particular, old
claims that session credentials, catalog browsing, cancellation/lifecycle,
copy/export, signed app packaging, or installed AX proof are deferred or already
complete are superseded by the approved contract and ledger.

## Contract map

- [`01-spec.md`](01-spec.md) — repository product contract and U0–U9 mapping.
- [`02-architecture.md`](02-architecture.md) — target ownership, typed seams,
  controller, security, export, and distribution architecture.
- [`03-traces.md`](03-traces.md) — authoritative T0–T10 ledger; T0 is RED.
- [`04-patch-plan.md`](04-patch-plan.md) — P0–P9 order and fixed verification
  command interfaces.
- [`docs/release/spec.md`](docs/release/spec.md) — preview release contract.
- [`docs/release/trace.md`](docs/release/trace.md) — T10.R1–R7 release traces.
- `docs/usable-mvp/{spec,trace,plan}.md` — frozen approval artifacts; do not
  edit during P0 reconciliation.

## Approved usable-MVP outcome

When T0–T10 are Verified, a developer can use the Homebrew-installed
`Dbotter Preview.app` to:

- create/edit a non-secret MySQL or Redis profile with explicit None, Session,
  or Environment credentials;
- test an unsaved draft without config/cache/store/workspace side effects;
- connect, disconnect, reconnect, delete, cancel, restart, and recover from
  static typed errors truthfully;
- browse paginated MySQL schemas/relations/columns and Redis keys/values;
- execute an exact selected/current target, with MySQL user SQL restricted to
  server prepared protocol and Redis commands restricted by the closed policy;
- copy exact cells/rows and atomically export bounded CSV/TSV/JSON;
- complete the same CLI and native AX journey with source/artifact/process/
  receipt proof.

Redis Required TLS verifies CA and hostname and never falls back to plaintext.
MongoDB stays disabled/Planned. Query history, editable grids, transactions,
SSH/proxy, import, ER diagrams, AI, keychain persistence, and stable publication
are not part of this task.

## P0 verification

This documentation task intentionally changes no Rust source, tests, scripts,
workflows, lockfiles, or approved artifacts. Verify the baseline with:

```sh
shasum -a 256 docs/usable-mvp/spec.md docs/usable-mvp/trace.md docs/usable-mvp/plan.md
git diff --check
git status --short --untracked-files=all
git diff -- 01-spec.md 02-architecture.md 03-traces.md 04-patch-plan.md \
  docs/release/spec.md docs/release/trace.md README.md
```

Expected frozen SHA-256 values:

```text
4c78aa0b957814d0dbaf46e8938a93701e2f85f0a6bb88772ef06b1b1da90cf3  docs/usable-mvp/spec.md
91bfbe89874e88e2c97c7252073cbf7348778192f2a6a349a68b903e1baceaa4  docs/usable-mvp/trace.md
ad649d256286f2e8dd8fa630bba8b64bb9f3ac5e6c5930f7ef432d85d0e8bd97  docs/usable-mvp/plan.md
```

## Implementation gates

P1 and later must start from failing trace-derived contracts. The planned
source/hermetic gate is:

```sh
./scripts/check-release-contract.sh
./scripts/test-receipt-contract.sh
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-features --locked
```

The mandatory live interface, once P4/P5/P8 implement it, is:

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

A missing command, fixture, certificate, environment value, or named assertion
means its slice is not Verified. Do not replace it with an older demo command.

## Preview install target

P8/P9 will publish and explicitly bump:

```sh
brew update
brew upgrade 2lab-ai/tap/dbotter-preview
```

The installed formula must contain `Dbotter Preview.app` with bundle id
`ai.2lab.dbotter.preview`; `dbotter` must resolve to its post-sign executable.
The installed proof uses separate exact commands:

```sh
dbotter version --format json
dbotter config-contract --format json
```

Do not treat a currently available historical formula or prerelease as this
outcome. Installation is complete only after the manifest-bound CLI and exact
app-path/PID AX journey in `04-patch-plan.md` passes. This task publishes no
stable release.

## Security and contribution rules

- Persist no password/token value and never log credential-bearing URIs.
- Never expose backend prose at public boundaries.
- Sensitive request types remain non-serializable with redacted manual Debug.
- UI state owns no live client; no lock crosses await; no production
  `unwrap`/`expect`/`panic!`/`todo!`.
- Update `03-traces.md` before changing cross-layer behavior.
- Do not mark a capability ready without its same-change mandatory live proof.
