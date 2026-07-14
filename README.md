# dbotter

Local Rust database client MVP. The current executable surface connects to
MySQL, executes one SQL statement, connects to Redis, executes one Redis
command, and reports MongoDB honestly as planned. The desktop feature provides
profiles/status, Add/Edit connection forms, Test, an editor, Execute, and a
typed result table through a bounded background Tokio bridge. Profile forms
persist only an optional `secret_env` variable name, never a password value.
MongoDB profiles may be saved for future use, while Test and Execute remain
disabled. Catalog browsing is visibly deferred.

## Install

Stable channel:

```sh
brew install 2lab-ai/tap/dbotter
```

Rolling prerelease channel:

```sh
brew install 2lab-ai/tap/dbotter-preview
```

Both formulas install the `dbotter` executable, so only one should be linked
at a time. Switch explicitly:

```sh
brew uninstall dbotter
brew install 2lab-ai/tap/dbotter-preview

# Return to stable
brew uninstall dbotter-preview
brew install 2lab-ai/tap/dbotter
```

`dbotter --version` identifies both the Cargo version and the immutable build:

```text
dbotter 0.1.0 (preview 2026-07-14-0905-0123456789ab)
```

## Release method

- Every push to `main`/`master` runs CI and publishes a
  `preview-YYYY-MM-DD-HHMM-<sha12>` GitHub prerelease. The latest 15 previews
  are retained. Install or upgrade it with
  `brew upgrade 2lab-ai/tap/dbotter-preview` after the tap bump completes.
- A stable release is intentionally operator-triggered. Update the version in
  `Cargo.toml`, merge green CI, then create and push the exact matching tag
  (for example Cargo `0.1.1` requires tag `v0.1.1`). The stable workflow
  refuses a mismatch.
- Both channels publish four desktop-capable macOS/Linux binaries plus
  `SHA256SUMS`; release builds use all Cargo features, so `dbotter gui` is
  available.

The complete channel contract and end-to-end workflow are in
[`docs/release/spec.md`](docs/release/spec.md) and
[`docs/release/trace.md`](docs/release/trace.md).

```sh
cargo run -- drivers
DBOTTER_CONFIG=config/local.example.toml \
DBOTTER_MYSQL_PASSWORD=dbotter-local-only \
  cargo run -- check --profile mysql-local
```

Desktop client:

```sh
cargo run --features desktop -- gui
```

Local acceptance:

```sh
docker compose -p dbotter-e2e up -d --wait mysql redis
DBOTTER_MYSQL_PASSWORD=dbotter-local-only ./scripts/verify-local.sh
```

The verifier creates one unique `run_id`, then exercises that exact id through
dbotter and independently re-reads it through the official `mysql` and
`redis-cli` clients inside the Compose containers. MySQL covers create,
idempotent upsert, and select. Redis covers `SET ... EX`, `GET`, and `TTL`.

The resulting `artifacts/receipt.json` includes host, toolchain, Git, Docker,
Compose/service, sanitized profiles, named input fixtures with SHA-256
fingerprints, parsed results/timing, official-client readback, and per-backend
verdict evidence. Raw SQL, Redis commands, argv, and raw output streams are not
serialized. It is valid only from a clean attached commit when the script exits
zero and both backend verdicts are `pass`:

```sh
jq '{run_id, mysql: .mysql.verdict, redis: .redis.verdict, mongodb}' \
  artifacts/receipt.json
```

MongoDB remains opt-in and is not started by this acceptance path; the receipt
records it as `prepared-not-run`. To stop only the two fixture services after
inspection, without deleting project volumes or unrelated Compose resources:

```sh
docker compose -p dbotter-e2e stop mysql redis
```
