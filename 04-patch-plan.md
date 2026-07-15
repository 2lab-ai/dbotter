# dbotter — usable MVP implementation and conformance plan

Status: **P0 documentation baseline complete. P1, P2, P3, P4, and P5 are
independently reviewed GREEN, including both mandatory live slices. T0 remains
RED overall; T1–T6/T8/T9 are Implementing; P6–P9 and T7/T10 are Not started.**

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
| P2 | generations/cache/controller/reload/shutdown | T2, T3, T9 | GREEN (independently reviewed foundation) |
| P3 | typed prepared execution/resource/result/CLI seams | T4, shared T5/T6 | GREEN (independently reviewed foundation) |
| P4 | lazy paginated MySQL catalog | T5 | GREEN (independently reviewed; hermetic + mandatory live) |
| P5 | Redis SCAN/inspect and verified Required TLS | T6 | GREEN (mandatory Redis live receipt passed) |
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

At the historical P1 checkpoint, P1 provided only the service-level observed-
state/cache race foundations needed by its owned contracts; the full concurrent
controller remained assigned to P2. P2 has since reached the independently
reviewed GREEN checkpoint recorded below. T0 remains RED for P6 first-run
RawInput/AccessKit; T1/T8 remain Implementing for P6; and T2/T3/T9 remain
Implementing for P6 native/AX evidence.

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

The independently reviewed P2 checkpoint implemented and proved:

- monotonic profile/session generations and fingerprinted cache identity with
  exact profile/session-generation compare-remove;
- tagged `RegisteredTask` scopes; bounded 32/16/16/128 work/mutation/control/
  event lanes; one-profile/four-global network limits; reserve-before-spawn;
  control priority and coalescing;
- tombstones, exact reload diff, Config uncertain fencing, and fresh-runtime
  lifecycle;
- cancel/timeout/panic/full/closed cleanup with exact terminal correlation and
  no stale predecessor event overwriting newer pending/connected state;
- network-only two-second abort, durable mutation/cooperative-export join, no
  detached task, and actual `ui::run` shutdown.

P2 is GREEN only for that runtime foundation. T2, T3, and T9 remain
Implementing because P6 native/RawInput/AccessKit and installed AX evidence
remain. No P6 visual-style implementation is claimed. P3 is now independently
reviewed GREEN; P4 and P5 have since reached their local GREEN/live
checkpoints, while P6–P9 remain Not started.

Checkpoint gates passed:

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

Evidence: 188 regular tests plus 12 doctests passed; focused totals were lib
48/48, controller 42/42, service 36/36, and source 4/4. Formatting, diff,
release-contract, receipt, strict Clippy, all-target/all-feature tests,
doctests, and release build passed. The final source+test review snapshot was
`e987bbf1d8a7f919cf53b95e882e0fa7b072d4226d7bb5e99e5e06d4dda65378`;
two independent reviewers each returned `NO P2 BLOCKER`.

```text
279757012280ab7bdcb90b547242114c80efcff3b64c26b7dcff4e3abb78fa9d  production snapshot (Cargo.toml, Cargo.lock, build.rs, src)
467982ee06068fe8fee669cc20e43ca05b1a0f72129c69137743c70d3eecce1b  tests snapshot (tests)
65ec73f1138587364005a1304fdd55006f85813283390fb3fd0f32f746183f3e  target/release/dbotter
```

## P3 — typed execution/resource seam

The independently reviewed P3 checkpoint implemented and proved:

- split `ConnectionPing`, `MySqlPreparedExecution`, `RedisExecution`,
  `CatalogBrowser`, and `KeyspaceBrowser` seams with backend/resource-specific
  request and outcome types;
- pure exact-target MySQL and Redis parsing, a single server-prepared MySQL
  user-text entry, and a production-wide structural ban on raw/text fallback;
- constructor-bound and driver-rechecked closed Redis execute policy, including
  blocking option forms before I/O;
- bounded decode and retained result snapshots with exact provenance,
  truncation notices, raw Redis identity separation, and no serializable raw
  backend result/prose boundary;
- exact cancel drop-before-close, one typed session disposition through cache,
  event, and UI outcome, stale prior-page retention, and stable headless CLI
  parser/JSON schemas;
- independent planned `CATALOG` and `KEYSPACE_BROWSE` bits at the P3 boundary;
  P4 has since made only `CATALOG` ready with mandatory live proof.

T4 is Implementing because its P3 hermetic core is GREEN while P6 RawInput/AX
and mandatory execute proof remain. T5 is Implementing because P4 is
independently reviewed GREEN while P6 remains; T6 is Implementing because P5
is GREEN while P6 remains.

Checkpoint gates passed:

```sh
cargo fmt --all -- --check
git diff --check
./scripts/check-release-contract.sh
sh scripts/test-receipt-contract.sh
cargo clippy --locked --offline --all-targets --all-features -- -D warnings
cargo test --locked --offline --all-targets --all-features
cargo test --locked --offline --all-features --doc
cargo build --locked --offline --release --all-features
```

Evidence: 227 regular tests plus 18 doctests passed; focused totals were lib
51/51, controller 46/46, service 37/37, source 6/6, execution 16/16, resource
10/10, and prepared-only MySQL 3/3. Formatting, diff, release-contract,
receipt, strict Clippy, all-target/all-feature tests, doctests, and release
build passed. The final source+test review snapshot was
`599917d1507df767b5b873a6d52d914d9646b9135fa51671282b4f0b884d5ecb`;
two independent reviewers each returned `NO P3 BLOCKER`.

```text
59a348c8a5e7f4bc63a15631cdac7be14444aebc57c84fb34ebbcb795692fec7  production snapshot (Cargo.toml, Cargo.lock, build.rs, src)
1b7a9ca40dea4994126f101dfcab1fc33fa6019b773627699c77e24167ac5b95  tests snapshot (tests)
9e43c9732be5a642873063f91a75364f9ad7f310735b17accaa3c24be0f95556  target/release/dbotter
```

## P4 — MySQL catalog independently reviewed GREEN checkpoint

Original code commit `e4599152daf0ca066baf6619048dae89c43cc6e4`
implements T5's level-specific prepared information-schema queries, binary
keyset pagination, retained caps/recovery, restricted and denied permission
behavior, shared CLI, and the real OpenAI-reference Explorer. RED contract
commit `31bd052f0d550e8c9e13e4f743f245ee4be6eba2` captures five review
blockers; code commit `0aa007b3476a458bc83eeb241f30cc67e26e911d`
closes them. Cross-process continuation RED
`ede07e766be198d1140d966667857092665cba70` is closed by persistent-root
fix `f51b3618f004b64e3601ca73f8072719ac273558`; static failure mapping and
test-seam follow-ups are `7b11adb4d15f9a6406f58d9d94ee6325d30f9b80` and
`c7b607c62ef807cb62898d1c12cbdffd6964b867`. Same-path connection rewrite
RED `7b622757b2405d6fb2859923d5a7bf868835630b` is closed by final fix
`05ad72f20e415b44f2d90ce7d5971c3d7a75b520`:

- token integrity uses a lazily created, private 0600, race-safe per-config
  CSPRNG 32-byte root sidecar; HMAC-SHA256 domain separation derives a
  non-serializable/redacted per-connection subkey from a canonical digest of
  every `ConnectionFingerprint` field, with full page-context binding;
- unchanged same-config services and CLI subprocesses share valid
  continuations, while different config roots and same-path connection-field
  rewrites fail closed without raw profile values entering tokens, Debug,
  receipts, or the sidecar;
- cancel/outer-timeout drops the catalog driver future before exact acquired
  session compare-remove/close, retaining stale pages and protecting a
  replacement session;
- one typed session generation/disposition is identical through cache, event,
  and UI truth, including auto-connect outcomes;
- Load more reuses the exact prefix/parent/page-size/token context captured by
  the existing page; edited input affects Refresh/new expansion only;
- OpenAI-style ordinary text passes a numerical WCAG AA ratio of at least
  4.5:1 on white.

Checkpoint gates passed:

```sh
cargo fmt --all -- --check
git diff --check
./scripts/check-release-contract.sh
sh scripts/test-receipt-contract.sh
cargo clippy --locked --offline --all-targets --all-features -- -D warnings
cargo test --locked --offline --all-targets --all-features
cargo test --locked --offline --all-features --doc
cargo build --locked --offline --release --all-features
DBOTTER_MYSQL_PASSWORD=dbotter-local-only \
  cargo test --locked --offline --test live_mysql \
  p4_live_catalog_fixture_proves_pages_caps_permissions_and_cli -- --ignored --exact
DBOTTER_MYSQL_PASSWORD=dbotter-local-only \
  cargo test --locked --offline --all-features --test live_mysql \
  p4_live_catalog_fixture_proves_pages_caps_permissions_and_cli -- --ignored --exact
```

Evidence: 249 regular tests, 19 doctests, and the isolated `dbotter-p4`
mandatory MySQL live test 1/1 in both default and all-features configurations
passed. Focused review contracts cover public-SHA rewrite/re-sign and tamper,
cross-config/profile rejection, unchanged-config cross-process acceptance,
same-path connection rewrite rejection, exact lifecycle/replacement races,
cache/event/UI session identity, stale retention, immutable continuation, and
numerical contrast. The live fixture covers multi-page binary order,
table/view, wide columns, both count and real metadata-byte caps with
clear/prefix recovery, restricted omission, unauthorized-default Check/Execute
Permission, stale Retry, and CLI JSON. Two independent reviews of exact final
implementation commit `05ad72f20e415b44f2d90ce7d5971c3d7a75b520` reported
`NO P4 BLOCKER` and `NO P4 SECURITY BLOCKER`. P4 is independently reviewed
GREEN; T5 remains Implementing only because P6 native/installed accessibility
evidence is not yet complete.

```text
ac9abfd2b6434fec58e7280d4da958125737a342fed01b7a7db2c190860dc120  P4 review source+test input
718d90023bcaae1e1d70947d74de2fe2248bc5d79d7fca8bbf3b5586fbe414cf  production snapshot
d7a7f9b7d2032c4bdf4d1d77a9d6013d5053a04599fed1c23ac0872e950ac2e2  tests snapshot
4d4a8dd94668954b110946b6442a4ad7fca41c06bc85cd8ad831a1fd5ff616da  target/release/dbotter
```

## P5 — Redis resource slice independently reviewed GREEN checkpoint

- P5 implemented T6's SCAN/inspect/raw identity/TTL/bounds/classifier,
  component-local Redis UI, and verified Required TLS/auth matrix.
  `KEYSPACE_BROWSE` is ready with that proof. CA failure, Host failure, and
  plaintext-fallback counts are separate assertions.

P5 checkpoint gates passed:

```sh
cargo fmt --all -- --check
git diff --check
./scripts/check-release-contract.sh
./scripts/test-receipt-contract.sh
cargo clippy --locked --offline --all-targets --all-features -- -D warnings
cargo test --locked --offline --all-targets --all-features
cargo test --doc --locked --offline --all-features
cargo build --release --locked --offline --all-features
./scripts/verify-live-redis.sh
```

The checkpoint passed 257 regular tests, 18 doctests, and the mandatory Redis
Docker receipt 1/1. The source+test checkpoint snapshot is
`1f8d890b908e12c102dab40177e822add41102fbd3024a6aee2736dbd897e266`.

```text
2e8ea5b91f85b0a29fb6adedb42b82729ede12692d2591d9ad13fd1ee35a9acf  production snapshot (Cargo.toml, Cargo.lock, build.rs, src)
7ac4923aded6ca15200600877dc271ee9b85468f4bab6ceb7ccb817c97724621  tests snapshot (tests)
92ad9489b06892bae519b3ec2316b24c4dfed2a9c11e53b37ede5ee64ba3af0c  target/release/dbotter
```

T6 remains Implementing until P6 supplies the complete profile workspace
RawInput/AccessKit disclosure and installed AX journey. P5's OpenAI-style
component UI is not a substitute for that remaining proof.

## P6/P7 — installed-journey UI and output

P6 binds real service outcomes to profile-generation workspaces, exact scanner,
stable author ids, real recovery dispatch, RawInput/AccessKit, numerical
contrast, disclosure, Delete warning, and restart. Required ids include
`profile.connection_id`, `profile.host`, Redis CA controls, all Session intent
controls, `editor.target`, `editor.row_limit`, and `editor.timeout`.

P4–P7 UI authoring follows the local `ui-ux` OpenAI design reference translated
to egui: true white/black neutrals, black inverted primary actions, sharp
corners, no gradients or decorative shadows, generous whitespace, precise
alignment, visible keyboard focus, and field-local error/loading states. Color
never carries meaning alone. P4 and P5 apply these rules to their Explorer
components; this is not a P6 native/AX visual-completion claim.

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

The following commands are the full interfaces for later slice claims. P1,
P2, P3, P4, and P5 passed checkpoint subsets are recorded above. A missing
command or failure is evidence that its owning slice is not Verified, not
permission to weaken the contract.

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
