# dbotter — approved preview release contract

Status: **The source-bound daily-use Preview candidate is published and installed
through xbrew; the complete installed AX journey and final typed receipt remain
pending.** No stable release is authorized.

The baseline normative sources are `docs/usable-mvp/spec.md` D1/§10,
`docs/usable-mvp/trace.md` T10, and `docs/usable-mvp/plan.md` P8/P9/§5–§7.
The daily-use overlay is `docs/daily-use/spec.md` §5,
`docs/daily-use/trace.md` D1/D11, and `docs/daily-use/plan.md` D1/D11; where the
overlay tightens compatibility or installed evidence, it governs the next
Preview.

## Objective and scope

Publish one source-bound daily-use preview, update the Homebrew preview formula
with explicit immutable inputs, install it, and prove the exact installed app,
CLI, native accessibility journey, and safe receipt.

This task does **not** create a stable tag or stable release. Stable workflow
code may share the same reusable verification gate, but stable invocation
remains an explicit future operator action.

## Channel and bundle contract

- Formula: `2lab-ai/tap/dbotter-preview`.
- Installed app: `Dbotter Preview.app`.
- Bundle id: `ai.2lab.dbotter.preview`.
- Canonical executable: the post-sign
  `Dbotter Preview.app/Contents/MacOS/dbotter`.
- Homebrew's `dbotter` shim resolves to that same installed inode/bytes; no
  unsigned or pre-sign duplicate is installed.
- macOS arm64 and x86_64 each receive their matching signed app bundle. The
  reusable gate also builds the approved Linux targets; target/architecture
  identities are never swapped.

## Independent machine contracts

Binary identity comes only from:

```sh
dbotter version --format json
```

It returns exactly:

```text
{package_version,channel,build_id,source_sha,target,arch}
```

Config compatibility is a separate pure command:

```sh
dbotter config-contract --format json
```

It returns exactly:

```text
{read_versions:[1,2,3],write_version:3,migration_backup_suffixes:{"1":".v1.bak","2":".v2.bak"}}
```

No identity field appears in the config contract and no compatibility field
extends the six-field identity object. Source-built, packaged, shim-resolved,
and exact installed-app invocations must agree with their corresponding
manifest records.

## Bundle and Homebrew versions

- `CFBundleShortVersionString` is Cargo `package_version` and must be `x.y.z`.
- `CFBundleVersion` is exactly numeric `<run_id>.<run_attempt>`.
- Homebrew preview version is independently
  `YYYY.MM.DD.HHMMSS.<run_id>.<run_attempt>` and must be strictly greater than
  the current tap version.
- The preview tag contains UTC seconds, `run_id`, `run_attempt`, and short
  source SHA as fixed by the manifest/package-version contract.

`plutil`, manifest, and negative fixtures reject version-field conflation.

## Source, build, artifact, and manifest identity

Local evidence uses
`SourceIdentity::LocalAttached { commit, branch, clean: true }`; CI uses
`SourceIdentity::CiExpectedSha { commit, expected_sha, run_id, run_attempt }`
and requires `commit == expected_sha == candidate SHA`. Required inputs are
tracked and the checkout is clean before generated artifacts.

Build records source commit, target triple, profile/features, rustc/Cargo
versions, and workflow identity. Hashes link transformations rather than
pretending different bytes are equal:

```text
unsigned target binary
  -> package/sign
  -> post-sign embedded executable
  -> app/archive
  -> manifest artifact entry
  -> downloaded archive
  -> installed post-sign executable
```

`dbotter.preview-manifest.v1` contains the exact tag/source/version/package/
config-contract/run tuple and per-architecture artifact URL, size, archive hash,
post-sign embedded hash, bundle id, and bundle version fields. Equality is
asserted only for the same bytes at download/install boundaries.

## Verification and publish graph

Every CI/preview/stable publish path hard-depends on the same reusable verify
result. The preview graph is:

```text
candidate SHA
  -> identity + config/release contracts
  -> fmt/clippy/all-feature tests
  -> config/controller/export failpoints
  -> RawInput/AccessKit/contrast/disclosure tests
  -> mandatory MySQL/Redis live/auth/browse/execute/TLS/marker tests
  -> source/build receipt
  -> four target builds
  -> per-architecture package/sign/verify/hash
  -> manifest/security receipt
  -> immutable preview release
  -> explicit tap dispatch and validation
  -> brew update/upgrade
  -> installed CLI + exact-app AX journey
  -> installed receipt
```

Missing fixture, environment value, certificate, named assertion, config
contract, or receipt field is a non-zero failure. No publish/tap job may run
after a required failure.

## Tap contract

The source workflow dispatches explicit `{tag, source_sha, version,
manifest_url, manifest_sha256}`. The tap validates tag→source, manifest,
per-architecture URL/hash, version monotonicity, and exact config contract
before one atomic formula update. A missing token/dispatch is incomplete, not a
valid release waiting for a scheduled catch-up.

## Installed identity and AX proof

The verifier resolves:

```sh
APP_PATH="$(brew --prefix dbotter-preview)/Dbotter Preview.app"
```

It terminates or rejects stale same-bundle processes, launches that exact path
with an isolated `--config`, and before any AX input proves the new PID's
executable realpath, device, inode, SHA-256, and bundle id against the installed
manifest entry.

The same post-sign executable must pass identity, config-contract, check, exec,
MySQL browse, Redis browse, and Redis inspect. The installed AX journey then
proves T0–T10 behavior, including Create/credential/restart, prepared-only
MySQL marker safety, Execute-limit ids, catalog/keyspace, split TLS recovery,
total error recovery, Delete warning, exact clipboard/export, disclosure, and
stable AXIdentifier readback.

## Receipt boundary

The final typed receipt links source/build/artifact/release/formula/install
identities and records only safe codes, action/AX ids, counts, timings, verdicts,
and process/file metadata.

It contains no secret, backend prose, user SQL/Redis text, result/key/CA/export
path value, exported bytes, or result screenshot. Runtime receipts contain no
export-content digest. Only the isolated seeded-verifier subsection records a
non-sensitive fixture id plus expected/actual digest and verdict.

## Rollback

Rollback is repair-forward: publish a new strictly higher preview from a
last-known-good source only after its exact `config-contract` agrees with
manifest/release/tap preflight. The installer/rollback wrapper owns backup-
runbook presentation. Never move a tag, replace an asset, reuse metadata, lower
the formula version, or silently run an older binary. Direct old-binary
invocation only returns `UnsupportedVersion`.

## Required acceptance interfaces

The exact source/live/package/Homebrew/AX commands are maintained in
`04-patch-plan.md` and approved plan §5. P8/P9 remain Not started until those
commands and all negative receipt/workflow fixtures exist and pass against the
same candidate source.
