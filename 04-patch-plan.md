# dbotter — Daily-driver v1.2 freeze and patch gate

Status: **Frozen — Stage J2 RED next**

Reset source commit: `03d6051`

## Normative tuple

| File | SHA-256 |
|---|---|
| `docs/daily-use/research.md` | `d57917b25279cb392edbdc53ed897726e6bd16a752711d73c7091a78b19bc9f1` |
| `docs/daily-use/spec.md` | `88613020c6ddee095831a9d87ea10d12d26ad778b74a7899c2887aec0b039819` |
| `docs/daily-use/trace.md` | `ba08fcbc8867cb11a06c007f6f7661c256a43982a29c3993a755ca7d0ae581e6` |
| `docs/daily-use/plan.md` | `a7f04079daebd4a09276d1d040567df1248892cf958d8e85ced1ff52133f3a0b` |

The tuple becomes frozen only after two independent reviews report Critical 0
and High 0, findings are resolved, links/path checks pass and the final hashes
replace all pending values. `docs/daily-use/evidence.md` is mutable and excluded.

Final review record (`2026-07-17`):

- product-contract review: Critical 0 / High 0;
- safety-contract review: Critical 0 / High 0.

## Patch order

| Stage | Trace | Deliverable | Status |
|---|---|---|---|
| 0 | tuple | research/spec/trace/plan review and freeze | completed |
| 1 | J2 | durable SQL workspace/history/restart | RED |
| 2 | J1 | Keychain/TLS/SSH connection and typed Data | not started |
| 3 | J3 | managed MySQL transaction/typed row edit | not started |
| 4 | J4 | exact export and transactional CSV import | not started |
| 5 | J5 | Redis structured daily-driver loop | not started |

Every stage is independently usable and follows:

1. user-boundary and safety RED committed/pushed;
2. small GREEN slices with evidence-ledger updates;
3. focused/hermetic/live/native gates;
4. independent Critical/High review and fixes;
5. Preview at exact SHA, xbrew install/update, installed black-box acceptance;
6. only then `verified` and the next stage.

## Fixed gates

```sh
git diff --check
just check
just check-all
```

Driver/filesystem/UI slices additionally run their owning live, receipt and
actual-frame/AX checks. A missing fixture or installed observation is a failed
evidence class, not a waiver.

## Change gate

After freeze, a semantic edit to any tuple file requires DUV1 version bump,
synchronized tuple updates, new independent reviews and new hashes before
production work continues. Stable release creation remains forbidden without a
separate explicit user approval.
