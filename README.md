# dbotter

Local Rust database client MVP. The current executable surface connects to
MySQL, executes one SQL statement, connects to Redis, executes one Redis
command, and reports MongoDB honestly as planned. The desktop feature provides
profiles/status, Add/Edit connection forms, Test, an editor, Execute, and a
typed result table through a bounded background Tokio bridge. Profile forms
persist only an optional `secret_env` variable name, never a password value.
MongoDB profiles may be saved for future use, while Test and Execute remain
disabled. Catalog browsing is visibly deferred.

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
