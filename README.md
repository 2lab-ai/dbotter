# dbotter

dbotter is a local Rust desktop database client targeting a usable MySQL and
Redis preview. MongoDB remains honestly Planned.

## Current status

The approved contract and P1/P2/P3 foundations plus the local P4 checkpoint are
complete at this branch point:

- P0 repository documentation baseline: complete;
- P1 config/profile/credential/public-error foundation: independently reviewed
  GREEN;
- P2 generations/cache/controller/reload/shutdown: independently reviewed
  GREEN;
- P3 typed prepared execution/resource/result/CLI seams: independently reviewed
  GREEN;
- P4 lazy paginated MySQL catalog: hermetic and mandatory live gates GREEN
  locally; independent review pending;
- T0 overall: RED because P6 first-run RawInput/AccessKit proof is missing; the
  P1 config portion is GREEN;
- T1 and T8: Implementing with P1 core GREEN and P6 remaining;
- T2: Implementing with P1/P2 core GREEN and P6 remaining;
- T3: Implementing with P2 core GREEN and P6 native/AX work remaining; it is
  not fully GREEN or Verified;
- T4: Implementing with the P3 hermetic core GREEN and P6 RawInput/AX plus
  mandatory live proof remaining;
- T5: Implementing with P4's catalog/CLI/Explorer/live core GREEN locally and
  P6 native AX plus independent review remaining;
- T6: Not started; P5 retains Redis keyspace/TLS capability and live-proof
  ownership;
- T9: Implementing with P1/P2 core GREEN and P6 remaining;
- T7 and T10, and slices P5–P9: Not started.

The P3 checkpoint is evidence for its hermetic typed execution/resource
foundation only. P4 now makes MySQL `CATALOG` ready in the same code commit as
its mandatory live proof; Redis `KEYSPACE_BROWSE` remains planned for P5. The
P4 Explorer applies the local OpenAI component reference, while P6 native
RawInput/AccessKit and installed AX work remain future work.
Historical demo code and release machinery remain **not** proof of the
remaining usable MVP or of an installed/verified preview.

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

## P3 checkpoint verification

The cumulative P1/P2/P3 checkpoint passed the following local gates:

```sh
cargo fmt --all -- --check
git diff --check
./scripts/check-release-contract.sh
sh scripts/test-receipt-contract.sh
cargo clippy --locked --offline --all-targets --all-features -- -D warnings
cargo test --locked --offline --all-targets --all-features
cargo test --doc --locked --offline --all-features
cargo build --release --locked --offline --all-features
```

The final run passed 227 regular tests and 18 doctests. Focused counts were lib
51/51, controller 46/46, service 37/37, source 6/6, execution 16/16, resource
10/10, and prepared-only MySQL 3/3. Formatting, diff, release-contract,
receipt, strict Clippy, all-target/all-feature tests, doctests, and the release
build passed. Two independent final reviewers each reported `NO P3 BLOCKER`
against source+test review snapshot
`599917d1507df767b5b873a6d52d914d9646b9135fa51671282b4f0b884d5ecb`.

Checkpoint SHA-256 values:

```text
59a348c8a5e7f4bc63a15631cdac7be14444aebc57c84fb34ebbcb795692fec7  production snapshot (Cargo.toml, Cargo.lock, build.rs, src)
1b7a9ca40dea4994126f101dfcab1fc33fa6019b773627699c77e24167ac5b95  tests snapshot (tests)
9e43c9732be5a642873063f91a75364f9ad7f310735b17accaa3c24be0f95556  target/release/dbotter
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

## P4 MySQL catalog checkpoint

Implementation commit `e4599152daf0ca066baf6619048dae89c43cc6e4`
adds three static server-prepared catalog plans, profile-bound opaque keyset
tokens, retained count/4-MiB recovery, exact quoting and bounded templates,
shared CLI output, and the real generation-scoped MySQL Explorer.

The strict formatting, Clippy, all-target/all-feature, doctest, release build,
release-contract, receipt secret-negative, diff, and frozen-hash gates passed.
The hermetic run is 236/236, doctests are 18/18, and the isolated MySQL 8.4
`dbotter-p4` mandatory live test is 1/1. That live test covers multi-page
binary order, table/view and wide columns, count and actual metadata-byte caps,
restricted visibility, separate unauthorized Permission, stale Retry, and CLI
JSON. Independent review is pending.

```text
359fc91428dc933cbfa36fcf88adf75968e9873d17040acf6abe44dc618adcda  P4 source+test review input
504a094cab732c58869fab629871e94800dc96efc0b1da88282f6b498afe7deb  production snapshot
34cdd805be0f09a722421fb8464b4dfac9f124e4415fada0cb6a17333020e063  tests snapshot
21f5c572daea43ee1d16d84defda704ab550e91afd45424fc2601a4bdd9bffe3  target/release/dbotter
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

P4's isolated live command is:

```sh
docker compose -f docker-compose.yml \
  -f tests/fixtures/mysql-catalog/compose.yml -p dbotter-p4 up -d --wait mysql
DBOTTER_MYSQL_PASSWORD=dbotter-local-only \
  cargo test --locked --offline --all-features --test live_mysql -- --ignored
```

The remaining integrated live interface for P5/P8 is:

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
