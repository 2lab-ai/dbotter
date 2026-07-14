# dbotter — authoritative vertical trace index

Status: **P1 foundation independently reviewed GREEN. T0 remains RED overall;
T1, T2, T8, and T9 are Implementing; T3–T7 and T10 are Not started.** Update
this document before changing cross-layer behavior.

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
| T2 | confirmed atomic profile delete | P1/P2/P6 | Implementing (P1 core GREEN; P2/P6 remain) | failpoint/order/tombstone/AX |
| T3 | controller, reload, connect/disconnect/reconnect/shutdown | P2/P6 | Not started | state/cache/race/shutdown |
| T4 | exact target, prepared-only execute, cancel | P3/P6 | Not started | scanner/source/live/RawInput |
| T5 | lazy paginated MySQL catalog | P4/P6 | Not started | hermetic + mandatory live + CLI |
| T6 | Redis SCAN/inspect and verified TLS | P5/P6 | Not started | hermetic + auth/TLS live + CLI |
| T7 | result/copy/streaming atomic export | P7 | Not started | byte goldens/filesystem failpoints |
| T8 | static errors, total recovery, accessibility | P1/P6 | Implementing (P1 core GREEN; P6 remains) | Cartesian table/RawInput/AccessKit |
| T9 | restart and credential availability | P1/P2/P6 | Implementing (P1 core GREEN; P2/P6 remain) | restart contract + installed AX |
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

Status: **Implementing** — P1 atomic config-delete/reconciliation evidence is
GREEN; P2/P6 remain. Contract source: frozen trace T2; slices P1/P2/P6.

Opening/cancelling confirmation is side-effect free. With active work the
dialog names static OperationKind and says dbotter stops waiting while the
server may continue. On observed commit: publish tombstone first, then cancel,
join, exact-session evict, secret/workspace clear, and correlated deletion with
server state Unknown. Pre-rename failure changes nothing; post-rename
durability uncertainty reloads and reconciles.

P1 GREEN evidence covers config failpoint barriers, unrelated-profile
preservation, and durability-unknown reconciliation. Remaining evidence is P2
tombstone/controller ordering and restart behavior plus P6 dialog/AX coverage.

## T3 — controller, reload, connection lifecycle, and shutdown

Status: **Not started**. Contract source: frozen trace T3 and §3; slices P2/P6.

The bounded mutation/work/control/event lanes, per-profile/global permits,
tagged task registry, cache generations, reload diff, Config uncertain barrier,
and exact state/cache table are normative. Session secret lookup unlocks before
await. Disconnect joins then evicts. Reconnect evicts first and allocates a new
session generation. Cancel/timeout reports Unknown and compare-removes only the
used generation. Shutdown drains secret-bearing queues and joins async,
mutation, and cooperative export work without detaching tasks.

RED owner/evidence: deterministic barrier/model races, full lane behavior,
panic/JoinError, permit/registry cleanup, no-lock-across-await, reload cases,
cache table, and secret final-Arc drop.

## T4 — exact target, prepared-only execution, and cancel

Status: **Not started**. Contract source: frozen trace T4; slices P3/P6.

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

RED owner/evidence:

- pure scanner normative table and profile A→B `editor.target` correlation;
- source/trait rejection of raw fallback;
- marker-table live negative for explicit-selection and current-target entry
  paths, asserting both markers absent;
- prepared-unsupported no-fallback and proven-session outcome;
- Redis closed command classifier before session acquisition.

## T5 — lazy paginated MySQL catalog

Status: **Not started**. Contract source: frozen trace T5; slices P4/P6.

Typed `CatalogRequest::{Schemas, Relations, Columns}` flows through
`CatalogBrowser`. Each static/bound prepared information-schema query requests
`page_size + 1`, retains at most the requested page in deterministic binary
keyset order, and derives an opaque next token from the last retained key only
when the extra row exists. Per-level/count/4-MiB caps expose reachable Load
more, Clear catalog, and prefix narrowing. Failed refresh retains stale prior
state. Restricted-user omission is not fabricated Permission.

RED owner/evidence: injected typed seam, token integrity, cap reachability,
identifier quoting, permission cases, multi-page mandatory live fixture,
headless `browse mysql` JSON, then GUI/AX expansion.

## T6 — Redis SCAN/inspect and verified Required TLS

Status: **Not started**. Contract source: frozen trace T6; slices P5/P6.

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

Status: **Implementing** — P1 config/credential restart foundation is GREEN;
P2/P6 remain. Contract source: frozen trace T9; slices P1/P2/P6.

Restart reloads version 2 and creates fresh runtime generations/cache/store/
workspace. Profiles and Redis TLS fields persist; session secrets/results/
pending/retry recipes/tombstones do not. Session shows Needs credential with
Keep disabled, Replace default, Forget available. Environment shows
Available/Missing/Empty without value exposure.

P1 GREEN evidence covers persisted v2/TLS fields, migration backup/frozen-reader
behavior, and session/environment credential availability foundations.
Remaining evidence is the P2 fresh-runtime/reconnect lifecycle and P6 exact
credential-intent/AX states.

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

Commands are fixed interfaces. The P1 foundation passed the checkpoint subset
below; commands owned by later slices remain planned and must not be interpreted
as passes.

P0 document baseline:

```sh
shasum -a 256 docs/usable-mvp/spec.md docs/usable-mvp/trace.md docs/usable-mvp/plan.md
git diff --check
```

P1 checkpoint (passed):

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

Full source/hermetic interface for later slice claims:

```sh
./scripts/check-release-contract.sh
./scripts/test-receipt-contract.sh
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-features --locked
```

P4–P8 mandatory live gate:

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

P0 changed documentation only. The independently reviewed P1 foundation is
GREEN, but no complete runtime journey is claimed GREEN: T0 remains RED and
T1/T2/T8/T9 remain Implementing until their listed P2/P6 evidence lands. Any
production deviation is recorded here before code with an
ADDED/MODIFIED/REMOVED/RENAMED classification, affected trace ids, migration
impact, and contract evidence.
