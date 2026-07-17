# dbotter — current trace/status router

Status authority: [`docs/daily-use/evidence.md`](docs/daily-use/evidence.md)

Normative trace: [`docs/daily-use/trace.md`](docs/daily-use/trace.md)

Reset source: `03d6051`

## v1.2 journey status

| Order | Trace | Scope | Current state |
|---|---|---|---|
| 1 | J2 | durable SQL workspace, results and searchable history | RED |
| 2 | J1 | secure MySQL onboarding and typed table/view Data | not started |
| 3 | J3 | managed typed MySQL row editing | not started |
| 4 | J4 | bounded export and transaction-safe CSV import | not started |
| 5 | J5 | Redis browse, typed inspect and structured core mutations | not started |

The source contains useful v1.1 read/browse/editor/result foundations, but no row
above inherits RED, GREEN or installed proof from the older D1–D12 structure.
Exact historical commits and the prior Preview receipt are preserved in the
mutable evidence ledger.

## Movement rule

A row moves only by the protocol in `docs/daily-use/trace.md`:

`not started → RED → implementing → local GREEN → verified`

RED is a pushed failing user-boundary contract before production changes.
Verified includes the installed xbrew app and independent backend/file readback,
not only source tests or packaging. Update this router whenever the active row
changes, and update the evidence ledger in every RED/GREEN/verification commit.
