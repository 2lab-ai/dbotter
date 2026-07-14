# dbotter

dbotter is a local Rust desktop database client targeting a usable MySQL and
Redis preview. MongoDB remains honestly Planned.

## Current status

The approved contract and P1 foundation are complete at this checkpoint:

- P0 repository documentation baseline: complete;
- P1 config/profile/credential/public-error foundation: independently reviewed
  GREEN;
- T0 overall: RED because P6 first-run RawInput/AccessKit proof is missing; the
  P1 config portion is GREEN;
- T1, T2, T8, and T9: Implementing with P1 core evidence complete and their
  remaining P2/P6 ownership explicit in `03-traces.md`;
- T3–T7, T10, and P2–P9: Not started.

The P1 checkpoint is evidence for that foundation only. Historical demo code
and release machinery remain **not** proof of the remaining usable MVP or of an
installed/verified preview.

## Contract map

- [`01-spec.md`](01-spec.md) — repository product contract and U0–U9 mapping.
- [`02-architecture.md`](02-architecture.md) — target ownership, typed seams,
  controller, security, export, and distribution architecture.
- [`03-traces.md`](03-traces.md) — authoritative T0–T10 ledger and partial-state
  ownership.
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

## P1 checkpoint verification

The independently reviewed P1 foundation passed the following local gates:

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

The full test run passed 136 regular tests and 12 doctests; the separate
doctest run passed 12/12. The source contract passed 1/1, and the receipt
contract, strict Clippy, formatting, and release build gates passed.

Checkpoint SHA-256 values:

```text
6ccd3ded9a82384ce92b823914e1b5e9f518886460fc0df1c6455ed6d9a327a9  production snapshot (Cargo.toml, Cargo.lock, build.rs, src)
dfacf608d773ca16dd4d25bdf0dc5bfb8f17926baf60d63bcadb1470ffb8114e  tests snapshot (tests)
80c8a75e35103a498fa845591c4418b038ac19c68b3d34aef50cf075dc765bb1  target/release/dbotter
```

The snapshots are reproducible with the tracked-plus-untracked file list in
each scope, sorted before hashing individual files and then hashing that list.
The frozen approval set remains unchanged:

```sh
shasum -a 256 docs/usable-mvp/spec.md docs/usable-mvp/trace.md docs/usable-mvp/plan.md
```

Expected frozen SHA-256 values:

```text
4c78aa0b957814d0dbaf46e8938a93701e2f85f0a6bb88772ef06b1b1da90cf3  docs/usable-mvp/spec.md
91bfbe89874e88e2c97c7252073cbf7348778192f2a6a349a68b903e1baceaa4  docs/usable-mvp/trace.md
ad649d256286f2e8dd8fa630bba8b64bb9f3ac5e6c5930f7ef432d85d0e8bd97  docs/usable-mvp/plan.md
```

## Implementation gates

Later slices must start from failing trace-derived contracts. Their full fixed
source/hermetic interface remains:

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
