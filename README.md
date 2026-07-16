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
- Browse MySQL schemas, tables/views and columns with bounded pagination.
- Scan/filter Redis keys and inspect bounded type, TTL and value previews, including binary key identity.
- Execute one selected/current MySQL statement through the server prepared protocol or one policy-checked Redis command.
- Cancel client waiting, inspect bounded result provenance, copy cells/rows and export retained CSV/TSV/JSON without overwriting an existing file.
- Use headless `check`, `exec`, MySQL catalog browse and Redis browse/inspect commands when a profile is already configured.

## Known daily-use gaps

The current Preview is useful for bounded inspection and one-off commands, but it is not yet the Daily-use v1 product:

- the editor and current result are in-memory single-workspace state;
- there are no persistent query tabs or searchable execution history;
- MySQL execution currently uses pooled auto-commit connections, with no managed Begin/Commit/Rollback UI;
- the MySQL explorer does not open/edit table rows; Redis type-aware mutation is deferred to P1 (DUV1 v1.1);
- there is no CSV import flow (deferred to P1 by DUV1 v1.1);
- the CLI cannot bootstrap profiles or accept a session credential safely from stdin;
- the long profile form and editor/result layout still lack the persistent navigator, resizable multi-pane context/status density and minimum-window reachability required by the Daily-use contract.

The independently reviewed frozen Daily-use v1 implementation contract is:

- [`docs/daily-use/spec.md`](docs/daily-use/spec.md) — product and safety contract;
- [`docs/daily-use/trace.md`](docs/daily-use/trace.md) — hash-frozen D1–D12 vertical trace;
- [`docs/daily-use/evidence.md`](docs/daily-use/evidence.md) — mutable implementation/live/native evidence ledger;
- [`docs/daily-use/plan.md`](docs/daily-use/plan.md) — staged RED/GREEN/Preview delivery plan.

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

Profile bootstrap, stdin credentials/targets and table/CSV/TSV output are frozen Daily-use v1 requirements under implementation, not current-release claims.

## Contracts

- [`01-spec.md`](01-spec.md) — current Daily-use v1 authority router and delivered-baseline summary.
- [`02-architecture.md`](02-architecture.md) — current ownership, typed seams and safety architecture.
- [`03-traces.md`](03-traces.md) — current D1–D12 status ledger and mapping from delivered T0–T10 evidence.
- [`04-patch-plan.md`](04-patch-plan.md) — current staged delivery/freeze gate and exact Daily-use artifact hashes.
- [`docs/release/spec.md`](docs/release/spec.md) and [`docs/release/trace.md`](docs/release/trace.md) — Preview packaging, tap and installed-evidence contracts.

## Security invariants

- Persist no value supplied through credential channels and never log credential-bearing URIs. Valid arbitrary editor/history text may itself contain literals; the app discloses that boundary and provides per-profile persistence opt-outs.
- Never expose backend prose at public error boundaries.
- Sensitive request types use redacted manual `Debug` and are not serializable.
- UI state owns no live client; no Rust mutex/RwLock or borrowed in-process guard crosses `await`. A synchronous owner may retain a nonblocking OS advisory-lock file descriptor across a safety-critical MySQL transaction.
- User SQL values never enter generated mutation text by string concatenation.
- No production `unwrap`, `expect`, `panic!` or `todo!`.
- A capability is not marked ready without its required live and installed proof.
