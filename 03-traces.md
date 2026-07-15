# dbotter — authoritative vertical trace index

Status: **P1, P2, P3, and P4 are independently reviewed GREEN. P4's MySQL
catalog mandatory live gate is GREEN; T5 remains Implementing for P6
native/installed accessibility evidence. T0 remains RED overall; T1–T5, T8,
and T9 are Implementing; T6, T7, and T10 are Not started.** Update this document before
changing cross-layer behavior.

The frozen normative trace is `docs/usable-mvp/trace.md` at SHA-256
`91bfbe89874e88e2c97c7252073cbf7348778192f2a6a349a68b903e1baceaa4`.
This file is its repository-facing ledger and routing index. Exact tables,
values, error mappings, bounds, and wire formats in the frozen trace are
incorporated by reference and must not be weakened here.

## Status rules

Allowed implementation states are `Not started`, `RED`, `Implementing`,
`GREEN`, and `Verified`.

- RED requires a failing contract derived from the trace's entry/input/flow/
  error/output requirements.
- GREEN requires the corresponding hermetic implementation tests.
- Verified additionally requires mandatory live evidence, trace conformance,
  disclosure checks, and file-map audit where the row calls for them.
- Existing historical demo behavior does not advance a row.
- A capability bit becomes ready only in the same reviewed change that supplies
  its mandatory live proof.

## Implementation ledger

| ID | User journey / scenario | Slice owner | Status | Required proof class |
|---|---|---|---|---|
| T0 | v1 read-only load, v2 migration/reload, first run | P1/P6 | RED (P1 config GREEN; P6 remains) | config/frozen-reader/RawInput |
| T1 | Create/Edit, credential intent, unsaved draft Test | P1/P6 | Implementing (P1 core GREEN; P6 remains) | matrix/draft isolation/AX |
| T2 | confirmed atomic profile delete | P1/P2/P6 | Implementing (P1/P2 core GREEN; P6 remains) | failpoint/order/tombstone/AX |
| T3 | controller, reload, connect/disconnect/reconnect/shutdown | P2/P6 | Implementing (P2 core GREEN; P6 native/AX remains; not fully GREEN/Verified) | state/cache/race/shutdown/AX |
| T4 | exact target, prepared-only execute, cancel | P3/P6 | Implementing (P3 hermetic core GREEN; P6 RawInput/AX and mandatory live proof remain) | scanner/source/live/RawInput |
| T5 | lazy paginated MySQL catalog | P4/P6 | Implementing (P4 review fixes and hermetic/live/CLI/UI core independently GREEN; P6 native/installed AX remains) | hermetic + mandatory live + CLI |
| T6 | Redis SCAN/inspect and verified TLS | P5/P6 | Not started (P3 shared typed seam only) | hermetic + auth/TLS live + CLI |
| T7 | result/copy/streaming atomic export | P7 | Not started | byte goldens/filesystem failpoints |
| T8 | static errors, total recovery, accessibility | P1/P6 | Implementing (P1 core GREEN; P6 remains) | Cartesian table/RawInput/AccessKit |
| T9 | restart and credential availability | P1/P2/P6 | Implementing (P1/P2 core GREEN; P6 remains) | restart contract + installed AX |
| T10 | CI/manifest/preview/tap/Brew/installed journey | P8/P9 | Not started | source/artifact/process/receipt chain |

## Approved vocabulary and correlation

Commands, events, `OperationKind`, `PublicSummary`, `ProfileFieldId`,
`RecoveryAction`, credential types, resource types, and export types are closed
exactly as listed in frozen trace §1 and approved spec §8.

Identity domains:

- profile command/event: `(ProfileId, ProfileGeneration, OperationId)`;
- Create and draft Test: `(DraftId, OperationId)`;
- export: `(ResultId, OperationId)`;
- global load/shutdown: `OperationId`.

`RegisteredTask { operation_id, scope, cancel, join }` uses
`TaskScope::{Profile, Draft, Export, Global}`. Only Profile scope carries
profile/session generations. Folds never borrow the currently selected profile
to repair missing identity.

## T0 — exact-path v1 normalization, v2 load, and first run

Status: **RED overall** — the P1 config portion is independently reviewed
GREEN; P6 first-run RawInput/AccessKit remains. Contract source: frozen trace
T0; slices P1/P6.

Entry resolves one config path: global `--config` → `DBOTTER_CONFIG` → default,
then calls `config::load_path`. Version 1 loads read-only and normalizes
credential mode in memory; missing path is a purposeful empty version-2 state;
legacy Redis Preferred remains visible and invalid. No startup write occurs.

The first confirmed Create/Edit/Delete performs fixed `.v1.bak` durability,
writes version 2, and reconciles the observed outcome. A frozen v1 reader must
reject v2 before service/network construction. First-run UI exposes New
MySQL/Redis and a disabled Planned MongoDB area.

P1 GREEN evidence:

- exact path precedence, v1 normalization/no-write, v2 load, missing vs invalid;
- fixed backup confirmation/cancel/failpoints and current-v1-reader rejection;
- exact independent `config-contract` JSON;

Remaining RED owner/evidence: P6 egui RawInput/AccessKit empty-state contract.

## T1 — Create/Edit, credential modes, and side-effect-free draft Test

Status: **Implementing** — P1 core evidence is GREEN; P6 native intent/AX work
remains. Contract source: frozen trace T1; slices P1/P6.

Create carries DraftId, chooses the lowest free suffix for automatic ids, and
maps an occupied explicit id to
`PROFILE_ID_CONFLICT → EditDraft(draft, ConnectionId)` /
`profile.connection_id`. Update carries immutable ProfileId plus expected
generation and can never recreate a deleted profile.

Credential source is explicit. KeepCurrent clones the existing secret Arc
under lock then unlocks; Replace creates one operation copy while retaining the
zeroizing form buffer; Forget/no secret returns draft credential recovery before
connector acquisition. Draft Test creates/pings/closes temporary resources and
has no config/cache/store/saved-state/workspace side effect or stored retry
recipe. Save maps Keep/Replace/Forget exactly after the config commit point.

P1 GREEN evidence covers config/credential matrices, Create versus Update
collision, draft buffer lifetime/invalidation, no-network/no-side-effect
assertions, atomic failpoints, and durability-unknown reconciliation. Remaining
P6 evidence is the complete native intent journey and AX ids.

## T2 — confirmed atomic profile delete

Status: **Implementing** — P1 atomic config-delete/reconciliation and P2
tombstone/controller lifecycle evidence are GREEN; P6 remains. Contract source:
frozen trace T2; slices P1/P2/P6.

Opening/cancelling confirmation is side-effect free. With active work the
dialog names static OperationKind and says dbotter stops waiting while the
server may continue. On observed commit: publish tombstone first, then cancel,
join, exact-session evict, secret/workspace clear, and correlated deletion with
server state Unknown. Pre-rename failure changes nothing; post-rename
durability uncertainty reloads and reconciles.

P1 GREEN evidence covers config failpoint barriers, unrelated-profile
preservation, and durability-unknown reconciliation. P2 GREEN evidence covers
tombstone publication, exact generation fences, active-work cancellation/join,
exact-session cleanup, correlated deletion, and known-versus-Unknown server
state. Remaining evidence is P6 dialog/AX coverage.

## T3 — controller, reload, connection lifecycle, and shutdown

Status: **Implementing** — P2 core is independently reviewed GREEN; P6 native/
AX work remains, so T3 is not fully GREEN or Verified. Contract source: frozen
trace T3 and §3; slices P2/P6.

The bounded mutation/work/control/event lanes, per-profile/global permits,
tagged task registry, cache generations, reload diff, Config uncertain barrier,
and exact state/cache table are normative. Session secret lookup unlocks before
await. Disconnect joins then evicts. Reconnect evicts first and allocates a new
session generation. Cancel/timeout reports Unknown and compare-removes only the
used generation. Shutdown drains secret-bearing queues and joins async,
mutation, and cooperative export work without detaching tasks.

P2 GREEN evidence proves:

- monotonic profile/session generations, fingerprinted cache identity, and
  exact generation/session compare-remove;
- bounded 32/16/16/128 work/mutation/control/event lanes, one profile and four
  global network permits, reserve-before-spawn, and coalesced control;
- tombstone/reload diff behavior and the Config uncertain barrier;
- exact cancel/timeout/panic/full/closed cleanup with correlated terminal
  events, including predecessor-event fences;
- network-only two-second abort, durable mutation/cooperative-export joins, no
  detached task, and actual `ui::run` shutdown.

P3 now supplies the independently reviewed typed execute/controller lifecycle,
including exact cancellation drop-before-close and one session disposition
through cache, event, and UI outcome. Remaining T3 evidence is P6 native
intent, RawInput/AccessKit, and installed AX coverage; no visual-style
implementation is claimed by this checkpoint.

## T4 — exact target, prepared-only execution, and cancel

Status: **Implementing** — the P3 hermetic core is independently reviewed
GREEN; P6 RawInput/AX and mandatory live proof remain. Contract source: frozen
trace T4; slices P3/P6.

Selection wins as the declared target; otherwise MySQL uses the exact scanner
and Redis uses one physical line. The MySQL scanner handles `#`, conditional
`--`, ordinary/version/hint comments, quotes/backticks, doubled/default
backslash rules, SQL-mode ambiguity, unterminated tokens, gaps, trailing
terminator, and UTF-8 boundaries. Unambiguous multiple statements reject.

Every accepted MySQL target enters only `PreparedMySqlRequest` through
`COM_STMT_PREPARE` → `COM_STMT_EXECUTE`. Negotiated
`CLIENT_MULTI_STATEMENTS` is not a safety control. User text has no raw/text
protocol or unsupported-prepared fallback. Prepared-unsupported produces
static UnsupportedFeature + FocusEditor/DismissError and retains a session only
when proven healthy.

Execute limit controls are `editor.row_limit` and `editor.timeout`;
`FocusExecuteLimits` is Execute-only. Cancel/timeout follows T3 and prior
results remain visibly historical.

P3 GREEN evidence:

- pure scanner normative table and profile A→B `editor.target` correlation;
- production-wide structural source/trait rejection of raw fallback;
- prepared-only MySQL and constructor-bound/rechecked Redis execution seams;
- pre-network bounds, bounded decode/retained snapshots, exact provenance, and
  typed session-disposition/cancel lifecycle contracts;
- stable headless execute/browse/inspect parser and JSON schemas; P4 has since
  made only MySQL `CATALOG` ready, while Redis keyspace remains planned.

Remaining P6/live owner/evidence:

- marker-table live negative for explicit-selection and current-target entry
  paths, asserting both markers absent;
- prepared-unsupported no-fallback and proven-session outcome;
- real editor selection/caret RawInput, keyboard, AccessKit, and installed AX.

## T5 — lazy paginated MySQL catalog

Status: **Implementing** — P4 code, review fixes, hermetic/mandatory-live/CLI/UI
core, and exact implementation/security reviews are GREEN. P6 native
RawInput/AccessKit plus installed AX remain. `CATALOG` is ready in the P4
implementation/review-fix chain that carries the live proof. Contract source:
frozen trace T5; slices P4/P6.

Typed `CatalogRequest::{Schemas, Relations, Columns}` flows through
`CatalogBrowser`. Each static/bound prepared information-schema query requests
`page_size + 1`, retains at most the requested page in deterministic binary
keyset order, and derives an opaque next token from the last retained key only
when the extra row exists. Per-level/count/4-MiB caps expose reachable Load
more, Clear catalog, and prefix narrowing. Failed refresh retains stale prior
state. Restricted-user omission is not fabricated Permission.

Review RED commit `31bd052f0d550e8c9e13e4f743f245ee4be6eba2` proves that a
public SHA-256 token rewrite/re-sign, catalog cancel/timeout lifecycle, mutable
Load more prefix, missing event session identity, and `#919191` ordinary text
were blockers. Fix commit `0aa007b3476a458bc83eeb241f30cc67e26e911d`
closes all five. Cross-process RED `ede07e766be198d1140d966667857092665cba70`
is closed by persistent-root fix `f51b3618f004b64e3601ca73f8072719ac273558`,
and same-path rewrite RED `7b622757b2405d6fb2859923d5a7bf868835630b`
is closed by connection-scope fix
`05ad72f20e415b44f2d90ce7d5971c3d7a75b520`. A lazily created private
0600 CSPRNG root sidecar is stable across processes, while HMAC-SHA256 domain
separation derives a redacted per-connection subkey from every
`ConnectionFingerprint` field. The scoped driver future drops before exact
session eviction; one typed generation/disposition reaches cache/event/UI;
continuation context is immutable; and ordinary text passes numerical WCAG AA.

P4 GREEN evidence includes public-SHA rewrite, tamper, cross-config and
cross-profile token rejection, unchanged-config CLI subprocess continuation,
and same-path connection rewrite rejection; cancel, outer-timeout, and
replacement races;
cache/event/UI identity equality; stale retention; exact Load more context;
three injected/static prepared plans, cap reachability, quoting,
successful-empty and Permission/stale contracts, real Explorer state, and the
headless JSON path. The isolated MySQL 8.4 fixture proves multi-page binary
ordering, table/view and wide columns, 2,000-relation and real 4 MiB metadata
caps with recovery, restricted omission, separate unauthorized Check/Execute
Permission, stale Retry, and CLI JSON. It passed 1/1 in both default and
all-features configurations. The hermetic checkpoint passed 249 regular tests
and 19 doctests. Two independent reviews of exact implementation commit
`05ad72f20e415b44f2d90ce7d5971c3d7a75b520` reported `NO P4 BLOCKER`
and `NO P4 SECURITY BLOCKER`. Remaining owner/evidence is P6
native/installed accessibility expansion.

```text
ac9abfd2b6434fec58e7280d4da958125737a342fed01b7a7db2c190860dc120  P4 review source+test input
718d90023bcaae1e1d70947d74de2fe2248bc5d79d7fca8bbf3b5586fbe414cf  production snapshot
d7a7f9b7d2032c4bdf4d1d77a9d6013d5053a04599fed1c23ac0872e950ac2e2  tests snapshot
4d4a8dd94668954b110946b6442a4ad7fca41c06bc85cd8ad831a1fd5ff616da  target/release/dbotter
```

## T6 — Redis SCAN/inspect and verified Required TLS

Status: **Not started** — P3 supplies only the independently reviewed typed
scan/inspect/raw-key/service/CLI seam and closed execute policy while keeping
`KEYSPACE_BROWSE` planned. P5/P6 still own verified TLS, live proof, and UI.
Contract source: frozen trace T6; slices P5/P6.

`RedisScanRequest` preserves LiteralPrefix versus Glob; raw bytes are identity
and display is separate. SCAN cursor `0` alone means complete. Inspect supports
the approved representative types, truthful TTL states, paging and bounded
previews without KEYS.

TLS exposes Disabled/Required only. Required uses verified TLS with OS roots or
a valid configured PEM CA. Invalid PEM/untrusted/wrong CA maps only to CA
edit/focus; hostname mismatch maps only to Host edit/focus and preserves CA.
Neither reaches plaintext. The closed execute classifier rejects all approved
blocking/subscription/replication/wait families before session acquisition.

RED owner/evidence: pure request/classifier/cap tests; binary key/race/TTL
tests; mandatory SCAN/type/mutation/auth live matrix on plaintext and verified
TLS; split CA/Host negative and plaintext-fallback counter; installed CLI
`browse redis`/`inspect redis`.

## T7 — profile result, exact copy, and streaming export

Status: **Not started**. Contract source: frozen trace T7 and approved spec §9;
slice P7.

Each profile generation owns its editor, pending state, historical/current
result, and error. `ResultSnapshot` is immutable and carries exact provenance.
Copy cell is byte-exact `clipboard_scalar` with no header/newline. Row/all copy
uses exact `tsv_field`, visible columns/order, one header, and one final LF.

CSV/TSV/JSON export streams from `Arc<ResultSnapshot>` to a 0600 temporary,
fsyncs, commits with DenyOverwrite or ReplaceConfirmed policy, then parent-
fsyncs. Runtime receipts contain no content/digest; only the external seeded
verifier records fixture/digests/verdict.

RED owner/evidence: every Cell/control/Unicode/truncation golden, duplicate
column/null/empty rows, allocation instrumentation, filesystem syscall order,
competition/symlink/failpoints/cancel, and installed byte-exact export.

## T8 — static public errors, total recovery, and accessibility

Status: **Implementing** — P1 public-error/recovery evidence is GREEN; P6
native accessibility and full dispatch journey remain. Contract source: frozen
trace T8 and approved spec §8; slices P1/P6.

Internal typed errors convert to allowlisted category/code, static summary, and
`NonEmpty<RecoveryAction>`. The exhaustive reachable
`OperationKind × PublicSummary` table is normative; unlisted pairs reject.
Create/draft actions use DraftId; saved-profile actions use ProfileId; Reveal
actions derive paths from safe registries; mutation Execute has no automatic
Retry. CA/Host actions and Execute-only limit focus remain disjoint.

Stable author ids, egui RawInput, AccessKit tree, installed AXIdentifier
readback, numerical contrast, disclosure presence/absence, protected secret
values, active Delete warning, and real recovery dispatch are all required.

P1 GREEN evidence covers the enum Cartesian table, unreachable rejection, and
typed recovery dispatcher. Remaining P6 evidence is the full action journey,
RawInput/AccessKit/contrast/disclosure suites, and installed recovery journey.

## T9 — restart and credential availability

Status: **Implementing** — P1 config/credential and P2 fresh-runtime lifecycle
foundations are GREEN; P6 remains. Contract source: frozen trace T9; slices
P1/P2/P6.

Restart reloads version 2 and creates fresh runtime generations/cache/store/
workspace. Profiles and Redis TLS fields persist; session secrets/results/
pending/retry recipes/tombstones do not. Session shows Needs credential with
Keep disabled, Replace default, Forget available. Environment shows
Available/Missing/Empty without value exposure.

P1 GREEN evidence covers persisted v2/TLS fields, migration backup/frozen-reader
behavior, and session/environment credential availability foundations. P2
GREEN evidence covers fresh runtime generations/cache and exact reconnect/
shutdown lifecycle. Remaining evidence is P6 exact credential-intent/AX states.

## T10 — gated preview publication, Brew install, and installed proof

Status: **Not started**. Contract source: frozen trace T10 and
`docs/release/{spec,trace}.md`; slices P8/P9.

One source-bound chain must prove exact six-field identity, independent exact
three-field config contract, four target builds, per-architecture signed macOS
bundles, manifest/hash relationships, monotonic preview tag/version, explicit
tap inputs, Homebrew upgrade, CLI shim identity, exact app-path/PID identity,
installed CLI operations, installed AX journey, and safe typed receipt.

Preview installs `Dbotter Preview.app` (`ai.2lab.dbotter.preview`). Identity is
measured after signing. `CFBundleShortVersionString`, `CFBundleVersion`, and
Homebrew version remain separate. Rollback publishes a new higher preview after
typed config-contract preflight. No stable publication occurs.

RED owner/evidence: workflow/manifest/receipt negative fixtures, live gates,
package/codesign/plutil, tap contract, installed CLI/AX journey, disclosure
scan, and final conformance audit.

## Journey, slice, and receipt routing

| Journey | Trace | Slice(s) | Receipt assertion family |
|---|---|---|---|
| U0 first run | T0 | P1/P6 | config/empty-state/AX |
| U1 create/test/edit | T1 | P1/P6 | credential/draft/ConnectionId |
| U2 delete | T2 | P1/P2/P6 | failpoint/tombstone/Unknown |
| U3 connection lifecycle | T3 | P2/P6 | controller/state/cache/shutdown |
| U4 execute | T4 | P3/P6 | scanner/prepared/marker/limits |
| U5 MySQL catalog | T5 | P4/P6 | page/cap/permission/CLI/live |
| U6 Redis browser/TLS | T6 | P5/P6 | SCAN/type/auth/TLS/CLI/live |
| U7 copy/export | T7 | P7 | exact bytes/filesystem/external digest |
| U8 errors/recovery | T8 | P1/P6 | total recovery/AX/disclosure |
| U9 restart | T9 | P1/P2/P6 | restart/credential availability |
| installed completion | T10 | P8/P9 | source→artifact→tap→PID→receipt |

## Verification command routing

Commands are fixed interfaces. The P1, P2, and P3 foundations passed their
recorded checkpoint subsets below; commands owned by later slices remain
planned and must not be interpreted as passes.

P0 document baseline:

```sh
shasum -a 256 docs/usable-mvp/spec.md docs/usable-mvp/trace.md docs/usable-mvp/plan.md
git diff --check
```

Historical P1 checkpoint (passed):

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

This produced 136 passing regular tests and 12 passing doctests (also 12/12 in
the separate doctest run), with production/test snapshot SHA-256 values
`6ccd3ded9a82384ce92b823914e1b5e9f518886460fc0df1c6455ed6d9a327a9`
and `dfacf608d773ca16dd4d25bdf0dc5bfb8f17926baf60d63bcadb1470ffb8114e`.

P2 checkpoint (passed):

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

This produced 188 passing regular tests and 12 passing doctests: lib 48/48,
controller 42/42, service 36/36, and source 4/4. Formatting, diff,
release-contract, receipt, strict Clippy, all-target/all-feature tests,
doctests, and release build passed. The final source+test review snapshot is
`e987bbf1d8a7f919cf53b95e882e0fa7b072d4226d7bb5e99e5e06d4dda65378`;
two independent reviewers each reported `NO P2 BLOCKER`.

```text
279757012280ab7bdcb90b547242114c80efcff3b64c26b7dcff4e3abb78fa9d  production snapshot (Cargo.toml, Cargo.lock, build.rs, src)
467982ee06068fe8fee669cc20e43ca05b1a0f72129c69137743c70d3eecce1b  tests snapshot (tests)
65ec73f1138587364005a1304fdd55006f85813283390fb3fd0f32f746183f3e  target/release/dbotter
```

P3 checkpoint (passed):

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

This produced 227 passing regular tests and 18 passing doctests: lib 51/51,
controller 46/46, service 37/37, source 6/6, execution 16/16, resource 10/10,
and prepared-only MySQL 3/3. Formatting, diff, release-contract, receipt,
strict Clippy, all-target/all-feature tests, doctests, and release build passed.
The final source+test review snapshot is
`599917d1507df767b5b873a6d52d914d9646b9135fa51671282b4f0b884d5ecb`;
two independent reviewers each reported `NO P3 BLOCKER`.

```text
59a348c8a5e7f4bc63a15631cdac7be14444aebc57c84fb34ebbcb795692fec7  production snapshot (Cargo.toml, Cargo.lock, build.rs, src)
1b7a9ca40dea4994126f101dfcab1fc33fa6019b773627699c77e24167ac5b95  tests snapshot (tests)
9e43c9732be5a642873063f91a75364f9ad7f310735b17accaa3c24be0f95556  target/release/dbotter
```

Full source/hermetic interface for later slice claims:

```sh
./scripts/check-release-contract.sh
./scripts/test-receipt-contract.sh
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-features --locked
```

P4 isolated mandatory live gate (passed):

```sh
docker compose -f docker-compose.yml \
  -f tests/fixtures/mysql-catalog/compose.yml -p dbotter-p4 up -d --wait mysql
DBOTTER_MYSQL_PASSWORD=dbotter-local-only \
  cargo test --locked --offline --all-features --test live_mysql -- --ignored
```

Remaining P5–P8 integrated mandatory live gate:

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

P8/P9 packaging, installed CLI, and AX commands are exact in
`04-patch-plan.md` and approved plan §5. Each command result is attached to its
trace row before that row may become Verified.

## Conformance record

P0 changed documentation only. The independently reviewed P1, P2, P3, and P4
foundations are GREEN, but no complete native journey is claimed
GREEN: T0 remains RED; T1/T8 remain Implementing for P6; T2/T3/T9 remain
Implementing for P6 native/AX evidence; T4 remains Implementing for P6
RawInput/AX plus execute live proof; and T5 remains Implementing for P6 native
AX. T6, T7, and T10, and slices P5–P9, remain Not
started.
Any production deviation is recorded here before code with an ADDED/MODIFIED/
REMOVED/RENAMED classification, affected trace ids, migration impact, and
contract evidence.
