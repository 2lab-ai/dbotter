# dbotter

dbotter is a local native database client written in Rust. The public Preview supports MySQL and Redis on macOS; MongoDB is shown honestly as Planned.

## Install the Preview

With [xbrew](https://github.com/2lab-ai/xbrew):

```sh
~/.xbrew/bin/xbrew install 2lab-ai/tap/dbotter-preview
dbotter gui
```

Check the installed build:

```sh
~/.xbrew/bin/xbrew version 2lab-ai/tap/dbotter-preview
dbotter version --format json
```

This repository publishes only the Preview channel. It does not publish a stable release.

## What the current Preview can do

- Create, edit, test, connect, reconnect, disconnect and delete local MySQL/Redis profiles in the native app.
- Use no credential, a process-local Session credential, or an environment-variable credential without persisting the secret value in the profile.
- Browse MySQL schemas, tables/views and columns with bounded pagination, keep the selected object in context and open bounded base-table Data in a new editor tab.
- Scan/filter Redis keys and inspect bounded type, TTL and value previews, including binary key identity.
- Run the current selection/statement or a fully preflighted read-only script on one correlated operation and one session. MySQL uses the server prepared protocol and a proven read-only session; Redis admits only the exact bounded read allowlist.
- Keep multiple SQL editor tabs and searchable execution-history metadata across restart, with bounded private persistence, opt-out and clear controls.
- Retain ordered per-editor result and typed error tabs during the session, then inspect them in Grid/Record/Value modes with local filter/sort and closable outputs.
- Keep navigator, editor, results/history and status context visible in the resizable desktop workspace, with accessible collapse/restore controls and a compact fallback.
- Cancel the active run, inspect bounded result provenance, copy cells/rows and export retained CSV/TSV/JSON without overwriting an existing file.
- Use headless `check`, `exec`, MySQL catalog browse and Redis browse/inspect commands when a profile is already configured.

## Known daily-use gaps

The current Preview is useful for bounded inspection and one-off commands, but it is not yet the Daily-driver v1.2 product:

- result/error payload tabs are intentionally session-only and omitted after restart; durable editor state and history metadata are restored;
- Run all is read-only. There is no typed row-change review or managed Begin/Commit/Rollback UI yet;
- MySQL scripts whose boundaries depend on the exact session `sql_mode` can be rejected conservatively before execution until mode-aware pre-split is complete;
- the MySQL explorer opens bounded generated base-table Data but does not provide typed paging/filter/sort or edit rows;
- there is no CSV import flow or structured Redis core-type mutation;
- the CLI cannot bootstrap profiles or accept a session credential safely from stdin;
- secure Keychain credentials and SSH tunnelling are not implemented;
- J2's exact Preview/xbrew installed proof is pending; installed J1/J3/J4/J5 evidence remains pending.

The frozen Daily-driver v1.2 contract and current evidence are here:

- [`docs/daily-use/research.md`](docs/daily-use/research.md) — first-party feature research and priority basis;
- [`docs/daily-use/spec.md`](docs/daily-use/spec.md) — product and safety contract;
- [`docs/daily-use/trace.md`](docs/daily-use/trace.md) — J1–J5 vertical trace and installed acceptance;
- [`docs/daily-use/evidence.md`](docs/daily-use/evidence.md) — mutable implementation/live/native evidence ledger;
- [`docs/daily-use/plan.md`](docs/daily-use/plan.md) — per-journey RED/GREEN/Preview/xbrew plan.

## Run from source

Requirements: a current Rust toolchain and `just`.

```sh
just check
cargo run --features desktop -- gui
```

Run the full desktop/all-feature gate:

```sh
just check-all
```

The live MySQL/Redis verification fixtures use Docker and are intentionally separate from the hermetic gate:

```sh
./scripts/verify-live-redis.sh
./scripts/verify-live-contracts.sh \
  --config config/local.example.toml
```

See [`config/local.example.toml`](config/local.example.toml) for local fixture profiles. Never commit real credential values; the example refers only to environment variable names.

## CLI snapshot

The current released CLI supports:

```text
dbotter gui
dbotter version --format json
dbotter config-contract --format json
dbotter drivers
dbotter --config PATH check --profile ID --format json
dbotter --config PATH exec --profile ID --text 'SELECT 1' --format json
dbotter --config PATH browse mysql schemas --profile ID --format json
dbotter --config PATH browse redis keys --profile ID --format json
dbotter --config PATH inspect redis key --profile ID --key-base64 BASE64 --format json
```

These commands describe the released read/inspect baseline, not unimplemented v1.2 write, persistence or import claims.

## Contracts

- [`01-spec.md`](01-spec.md) — current Daily-driver v1.2 authority router and delivered-baseline summary.
- [`02-architecture.md`](02-architecture.md) — current ownership, typed seams and safety architecture.
- [`03-traces.md`](03-traces.md) — current J1–J5 status router.
- [`04-patch-plan.md`](04-patch-plan.md) — current journey delivery/freeze gate and exact tuple hashes.
- [`docs/release/spec.md`](docs/release/spec.md) and [`docs/release/trace.md`](docs/release/trace.md) — Preview packaging, tap and installed-evidence contracts.

## Security invariants

- Persist no value supplied through credential channels and never log credential-bearing URIs. Valid arbitrary editor/history text may itself contain literals; the app discloses that boundary and provides per-profile persistence opt-outs.
- Never expose backend prose at public error boundaries.
- Sensitive request types use redacted manual `Debug` and are not serializable.
- UI state owns no live client; no Rust mutex/RwLock or borrowed in-process guard crosses `await`. A synchronous owner may retain a nonblocking OS advisory-lock file descriptor across a safety-critical MySQL transaction.
- User SQL values never enter generated mutation text by string concatenation.
- No production `unwrap`, `expect`, `panic!` or `todo!`.
- A capability is not marked ready without its required live and installed proof.
