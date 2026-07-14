# dbotter release channels

## Objective

Ship the same desktop-capable `dbotter` binary through two Homebrew channels,
using GitHub releases as the immutable artifact boundary:

- `stable`: Homebrew formula `dbotter`, sourced from a `v*` GitHub release.
- `preview`: Homebrew formula `dbotter-preview`, sourced from the newest
  `preview-*` GitHub prerelease.

This repository owns source validation, cross-platform release assets, build
identity, and GitHub releases. `2lab-ai/homebrew-tap` owns the formulas and
their version/SHA updates.

## Release contract

### Build identity

`dbotter --version` prints:

```text
dbotter <cargo-version> (<channel> <build-id>)
```

| Build | Channel | Build ID |
|---|---|---|
| local | `dev` | `dev` |
| preview | `preview` | `YYYY-MM-DD-HHMM-<sha12>` |
| stable | `stable` | `v<cargo-version>-<sha12>` |

The workflow supplies `DBOTTER_BUILD_CHANNEL` and `DBOTTER_BUILD_ID` at
compile time. There is no runtime channel file and no mutable version lookup.

### Preview

A push to `main` or `master`, or a manual workflow dispatch, publishes one
GitHub prerelease with tag:

```text
preview-YYYY-MM-DD-HHMM-<sha12>
```

The release is `prerelease: true`, is not marked latest, and contains exactly
the four executable assets below plus `SHA256SUMS`:

- `dbotter-macos-aarch64`
- `dbotter-macos-x86_64`
- `dbotter-linux-aarch64`
- `dbotter-linux-x86_64`
- `SHA256SUMS`

Preview publishing retains the newest 15 `preview-*` prereleases and deletes
older preview releases and their tags. If `TAP_DISPATCH_TOKEN` is configured,
the workflow dispatches `2lab-ai/homebrew-tap`'s `bump.yml`; without the token,
the release remains valid and the tap's scheduled bump may catch up later.

### Stable

Pushing `v*` is the only stable release trigger. Before any build, the tag
without its leading `v` must equal the root `Cargo.toml` package version
exactly. A mismatch fails the workflow without publishing. A matching tag
publishes a normal GitHub release with the same five assets as preview.

Creating and pushing a stable `v*` tag remains an explicit operator action.

### Build features

Every released binary is built with `--all-features`. Therefore the `desktop`
feature is present and `dbotter gui` opens the native client; the optional
MongoDB adapter scaffold is compiled into the same artifact. CI also checks
all features on macOS and Linux before release.

## Homebrew channel contract

The tap formulas are outside this repository. They consume immutable release
assets by OS/architecture and verify the matching digest from `SHA256SUMS`.

```sh
# stable
brew install 2lab-ai/tap/dbotter

# rolling prerelease
brew install 2lab-ai/tap/dbotter-preview
```

Only one formula should be linked at a time because both install a `dbotter`
executable. Switching channels is an explicit uninstall/install operation.

## Acceptance

1. `cargo fmt --check` passes.
2. `cargo clippy --all-targets --all-features --locked -- -D warnings` passes.
3. `cargo test --all-features --locked` passes.
4. `scripts/check-release-contract.sh` passes and validates the workflow
   triggers, tag/version gates, asset names, features, checksum manifest,
   prerelease flag, and retention count.
5. A local dev build reports `(dev dev)`.
6. A compile with injected preview metadata reports `(preview <build-id>)`.

## Out of scope

- Editing or publishing Homebrew tap formulas in this source change.
- Code signing, notarization, or a `.app`/cask distribution.
- Stable tag creation or stable publication without an explicit operator
  action.
