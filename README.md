# dbotter

dbotter is a local Rust desktop database client targeting a usable MySQL and
Redis preview. MongoDB remains honestly Planned.

## Current status

The approved contract and P1/P2/P3/P4 foundations are
complete at this branch point:

- P0 repository documentation baseline: complete;
- P1 config/profile/credential/public-error foundation: independently reviewed
  GREEN;
- P2 generations/cache/controller/reload/shutdown: independently reviewed
  GREEN;
- P3 typed prepared execution/resource/result/CLI seams: independently reviewed
  GREEN;
- P4 lazy paginated MySQL catalog: review fixes, hermetic gates, and mandatory
  live gates independently reviewed GREEN;
- T0 overall: RED because P6 first-run RawInput/AccessKit proof is missing; the
  P1 config portion is GREEN;
- T1 and T8: Implementing with P1 core GREEN and P6 remaining;
- T2: Implementing with P1/P2 core GREEN and P6 remaining;
- T3: Implementing with P2 core GREEN and P6 native/AX work remaining; it is
  not fully GREEN or Verified;
- T4: Implementing with the P3 hermetic core GREEN and P6 RawInput/AX plus
  mandatory live proof remaining;
- T5: Implementing with P4's catalog/CLI/Explorer/live review fixes GREEN
  and only P6 native/installed AX remaining;
- T6: Not started; P5 retains Redis keyspace/TLS capability and live-proof
  ownership;
- T9: Implementing with P1/P2 core GREEN and P6 remaining;
- T7 and T10, and slices P5–P9: Not started.

The P3 checkpoint is evidence for its hermetic typed execution/resource
foundation only. P4 keeps MySQL `CATALOG` ready with its mandatory live
proof and review-fix contracts; Redis `KEYSPACE_BROWSE` remains planned for P5.
The P4 Explorer applies the local OpenAI component reference, while P6 native
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

Original implementation commit `e4599152daf0ca066baf6619048dae89c43cc6e4`
adds the three static server-prepared catalog plans, retained count/4-MiB
recovery, exact quoting and bounded templates, shared CLI output, and the real
generation-scoped MySQL Explorer. Review RED commit
`31bd052f0d550e8c9e13e4f743f245ee4be6eba2` captures five blockers; fix commit
`0aa007b3476a458bc83eeb241f30cc67e26e911d` closes them. Cross-process RED
`ede07e766be198d1140d966667857092665cba70` is closed by persistent-root
fix `f51b3618f004b64e3601ca73f8072719ac273558`; same-path connection
rewrite RED `7b622757b2405d6fb2859923d5a7bf868835630b` is closed by final
fix `05ad72f20e415b44f2d90ce7d5971c3d7a75b520`. A private 0600 per-config
CSPRNG root sidecar persists across processes, and HMAC-SHA256 domain
separation derives a redacted per-connection signing subkey from every
`ConnectionFingerprint` field. Exact cancel/timeout compare-remove lifecycle,
one session disposition/generation through cache-event-UI, immutable Load more
context, and WCAG AA ordinary-text contrast remain enforced.

The strict formatting, Clippy, all-target/all-feature, doctest, release build,
release-contract, receipt secret-negative, credential-pattern, diff, UI skill,
and frozen-hash gates passed after the fixes. The hermetic run is 249/249,
doctests are 19/19, and the isolated MySQL 8.4 `dbotter-p4` mandatory live test
is 1/1 in both default and all-features configurations. That live test covers
multi-page binary order, table/view and wide columns, count and actual
metadata-byte caps, restricted visibility, separate unauthorized Permission,
stale Retry, cross-process CLI continuation, and same-path connection rewrite
rejection. Independent exact-commit reviews reported `NO P4 BLOCKER` and
`NO P4 SECURITY BLOCKER`; T5 remains Implementing only for P6 native/installed
accessibility evidence.

```text
ac9abfd2b6434fec58e7280d4da958125737a342fed01b7a7db2c190860dc120  P4 review source+test input
718d90023bcaae1e1d70947d74de2fe2248bc5d79d7fca8bbf3b5586fbe414cf  production snapshot
d7a7f9b7d2032c4bdf4d1d77a9d6013d5053a04599fed1c23ac0872e950ac2e2  tests snapshot
4d4a8dd94668954b110946b6442a4ad7fca41c06bc85cd8ad831a1fd5ff616da  target/release/dbotter
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
