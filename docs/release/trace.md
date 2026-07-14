# dbotter release vertical traces

The trace is the source of truth for the release implementation.

## Implementation status

| Scenario | Status | Contract evidence |
|---|---|---|
| T1 CI validates a source commit | Implemented | `.github/workflows/ci.yml`, release contract script |
| T2 preview commit becomes Homebrew-consumable prerelease | Implemented | `.github/workflows/preview.yml`, release contract script |
| T3 stable tag becomes version-matched release | Implemented | `.github/workflows/release.yml`, release contract script |
| T4 binary exposes immutable build identity | Implemented | `src/build_info.rs`, Rust tests |

## T1 — CI validates a source commit

### 1. API Entry

GitHub Actions receives `pull_request` or a push to `main`/`master`. Repository
read permission is sufficient.

### 2. Input

Input is a checked-out commit containing `Cargo.lock`. Dependencies must resolve
under `--locked`; the workflows and release contract must remain internally
consistent.

### 3. Layer Flow

`GitHub event.sha → actions/checkout ref → Cargo source tree → fmt/clippy/test`

`workflow files → scripts/check-release-contract.sh → release invariants`

The matrix runs on macOS and Ubuntu. All Rust gates use all features, so the
same GUI/MongoDB feature surface shipped later is compiled and tested here.

### 4. Side Effects

Cargo build/test caches may be updated. No release, tag, or tap state changes.

### 5. Error Paths

Formatting drift, a warning, a test failure, a lockfile mismatch, or a release
contract mismatch fails the job. No publish job exists in the CI workflow.

### 6. Output

Both matrix jobs finish successfully and GitHub records a green CI result.

### 7. Observability

Each gate has its own named workflow step and command output.

## T2 — preview commit becomes a Homebrew-consumable prerelease

### 1. API Entry

GitHub Actions receives a push to `main`/`master` or `workflow_dispatch`.

### 2. Input

The checked-out commit must be buildable with `Cargo.lock` and all Cargo
features. Current UTC time and the full commit SHA are required.

### 3. Layer Flow

`event commit → git rev-parse HEAD → full commit`

`UTC now → YYYY-MM-DD-HHMM + sha12 → build_id`

`build_id → preview-<build_id> → Git tag/release tag`

`DBOTTER_BUILD_CHANNEL=preview + DBOTTER_BUILD_ID=<build_id> → option_env! → dbotter --version`

`matrix.target → cargo --all-features binary → canonical asset name → release asset`

`release assets → sha256sum → SHA256SUMS → GitHub prerelease`

### 4. Side Effects

Four build artifacts and one checksum manifest are uploaded to a GitHub
prerelease. Preview releases beyond the newest 15 are deleted with their tags.
When the optional tap token exists, `2lab-ai/homebrew-tap` `bump.yml` is
dispatched after the release is published.

### 5. Error Paths

Any target build, artifact assembly, checksum, or release publication failure
fails the workflow and prevents tap dispatch. A missing tap token only skips
the immediate dispatch; it does not invalidate the published prerelease.

### 6. Output

The GitHub release is marked prerelease, not latest, targets the exact input
commit, and exposes all canonical assets plus `SHA256SUMS`.

### 7. Observability

Release notes contain the build id, full commit, installation command, and the
checksum manifest. Workflow logs show pruning and optional tap dispatch.

## T3 — stable tag becomes a version-matched release

### 1. API Entry

GitHub Actions receives a pushed tag matching `v*`.

### 2. Input

`GITHUB_REF_NAME` must be `v<version>`. `<version>` must exactly equal the root
package version returned by `cargo metadata --no-deps`.

### 3. Layer Flow

`GITHUB_REF_NAME → strip v → tag_version`

`Cargo.toml → cargo metadata root package → cargo_version`

`tag_version == cargo_version → verified commit → four --all-features builds`

`stable + v<cargo_version>-<sha12> → compile-time build identity`

`matrix.target → canonical asset → SHA256SUMS → normal GitHub release`

### 4. Side Effects

A normal GitHub release is created for the already-pushed stable tag. No
preview pruning occurs and no tap repository is directly changed.

### 5. Error Paths

Version mismatch fails the preflight before the build matrix. Build, checksum,
or publication failure prevents a complete stable release.

### 6. Output

The `v*` GitHub release has generated notes, four canonical binaries, and one
checksum manifest. It is not marked prerelease.

### 7. Observability

Preflight logs both compared versions. Build jobs expose target and asset name;
the publish job prints `SHA256SUMS`.

## T4 — binary exposes immutable build identity

### 1. API Entry

The user runs `dbotter --version`.

### 2. Input

Compile-time Cargo version and optional compile-time environment variables
`DBOTTER_BUILD_CHANNEL` and `DBOTTER_BUILD_ID`.

### 3. Layer Flow

`CARGO_PKG_VERSION → VERSION`

`DBOTTER_BUILD_CHANNEL | absent → BUILD_CHANNEL | dev`

`DBOTTER_BUILD_ID | absent → BUILD_ID | dev`

`VERSION + BUILD_CHANNEL + BUILD_ID → clap version → stdout`

### 4. Side Effects

None.

### 5. Error Paths

Absent build variables resolve deterministically to `dev`; no runtime lookup
can fail.

### 6. Output

One line: `dbotter <version> (<channel> <build-id>)` and exit status zero.

### 7. Observability

The version line itself is the build provenance marker used by operators.

## File map

- `docs/release/spec.md`
- `docs/release/trace.md`
- `.github/workflows/ci.yml`
- `.github/workflows/preview.yml`
- `.github/workflows/release.yml`
- `scripts/check-release-contract.sh`
- `scripts/package-version.sh`
- `src/build_info.rs`
- `src/lib.rs`
- `src/cli.rs`
- `Cargo.toml`
- `README.md`

## Trace deviations

None.
