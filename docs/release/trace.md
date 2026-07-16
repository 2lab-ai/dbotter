# dbotter — T10 preview release vertical trace

Status: **P8 release gates are GREEN; P9 has a public Preview/xbrew launch
checkpoint, while merge-source publication, installed AX and the final receipt
remain pending.** This trace records only the exact candidate and locally
observed installation evidence below.

Normative anchors: approved trace T10, approved plan P8/P9 and §5–§7, and
`docs/release/spec.md`.

## Release ledger

| ID | Scenario | Status | Required evidence |
|---|---|---|---|
| T10.R1 | reusable verification gates one candidate source | GREEN | run `29513008288` attempt 2; source/hermetic/live/package gates |
| T10.R2 | target builds become signed manifest-linked artifacts | GREEN | four target builds and immutable manifest-linked prerelease |
| T10.R3 | immutable preview dispatches an exact tap update | GREEN | release tag and tap commit `1fcb761a7baf` |
| T10.R4 | Homebrew shim and installed CLI prove exact executable | In progress | xbrew version, exact identity/config/bundle/shim proven; full check/exec/browse receipt pending |
| T10.R5 | exact installed app completes native AX journey | In progress | exact installed PID/bundle proven; AX automation unavailable on this host |
| T10.R6 | final typed receipt closes the source→install chain | Not started | schema/leak/digest/provenance verdicts |
| T10.R7 | repair-forward rollback preserves compatibility | Not started | higher version/config preflight/runbook |

## Current published checkpoint

- Source: `11a839fadadbbe6d380f516a37b1708ea4917cd1`.
- Preview run: `29513008288`, attempt 2; attempt 1 ended before verification
  because the hosted runner exhausted its device space.
- Prerelease: `preview-2026-07-16-161750-29513008288-2-11a839fadadb`.
- xbrew/Homebrew version: `2026.07.16.161750.29513008288.2`.
- Tap formula commit: `1fcb761a7baf181a1516c56ceb5c202e14606dcc`.
- Local installed identity: Preview channel, source above,
  `aarch64-apple-darwin`, bundle id `ai.2lab.dbotter.preview`; the Homebrew shim
  resolves into that formula installation.
- Local launch: the exact installed app was launched with the isolated visual
  fixture config and registered by LaunchServices. Computer-use AX readback
  failed with host automation authentication errors, so no visual or AX pass is
  claimed here.

## T10.R1 — reusable verification gate

### Entry and identity

CI receives a candidate SHA. Local preflight may use a clean attached commit;
CI detached checkout is allowed only through `CiExpectedSha` with exact
candidate equality. Required inputs are tracked and the checkout is clean
before generated artifacts.

### Flow

```text
candidate source
  -> exact six-field identity test
  -> independent exact config-contract test
  -> release/receipt contract tests
  -> fmt + all-feature clippy/test
  -> config/controller/export failpoints
  -> RawInput/AccessKit/contrast/disclosure
  -> mandatory Compose MySQL/Redis live matrix
  -> source/build receipt
```

The live matrix includes MySQL/Redis credential modes, MySQL prepared-only
marker/no-fallback safety, paginated MySQL catalog, Redis SCAN/inspect/auth on
plaintext and verified TLS, split CA/Host negatives, and zero plaintext
fallback. A missing fixture/env/cert/assertion is failure.

### Side effects, errors, output

Only build/test caches, Compose fixture data, and generated local receipts may
change. Any required failure blocks every build/publish/tap job. Output is a
source-bound reusable-verification result and safe receipt; no release exists.

## T10.R2 — per-target build, macOS package, and manifest

### Entry and input

R1 green candidate SHA plus target matrix, package version, run id/attempt,
signing context, exact config contract, and monotonic preview version.

### Flow

```text
candidate SHA -> four target builds
macOS target -> Dbotter Preview.app -> sign -> codesign verify
post-sign executable + archive -> independent hashes
all target records -> dbotter.preview-manifest.v1 -> schema/security validation
```

The packaged/shim/installed executable identity schema remains six fields. The
separate config contract remains exactly three fields. `plutil` verifies Cargo
`x.y.z`, numeric `<run_id>.<run_attempt>`, and separation from the Homebrew
version.

### Error and output

Target mismatch, missing/extra identity field, config-contract disagreement,
bad bundle id/version, unsigned bundle, swapped architecture, or false hash
equality blocks publication. Output is a signed per-architecture artifact set
and validated manifest linked to the candidate source.

## T10.R3 — immutable preview and explicit tap update

### Entry and input

Validated R2 manifest/artifacts and an increasing
`YYYY.MM.DD.HHMMSS.<run_id>.<run_attempt>` version. Tag inputs include UTC
seconds, run id/attempt, and short source SHA.

### Flow

```text
manifest + immutable assets -> GitHub preview release
{tag,source_sha,version,manifest_url,manifest_sha256} -> tap dispatch
tap: tag/source/manifest/arch/config-contract/version checks -> atomic formula update
```

### Errors, output, side effects

Incomplete assets, failed gate, non-increasing version, missing explicit
dispatch, tag/source disagreement, config-contract mismatch, or tap validation
failure leaves T10 incomplete. A valid output is one immutable preview plus one
explicitly validated formula commit. No stable tag/release is created.

## T10.R4 — Homebrew install and exact CLI proof

### Entry and input

Validated tap commit, `brew update`, preview upgrade, isolated explicit config,
and R2 manifest.

### Flow

```text
brew upgrade dbotter-preview
  -> installed Dbotter Preview.app
  -> bin/dbotter shim
  -> realpath/device/inode/hash match post-sign manifest executable
  -> version + config-contract
  -> check + exec + MySQL browse + Redis browse/inspect
```

### Errors and output

Wrong app/shim target, stale executable, identity/config mismatch, wrong
architecture, or any CLI contract failure blocks AX verification. Output is an
installed-CLI evidence block containing safe metadata and verdicts only.

## T10.R5 — exact-app installed AX golden journey

### Entry and process proof

Resolve:

```sh
APP_PATH="$(brew --prefix dbotter-preview)/Dbotter Preview.app"
```

Terminate or reject stale `ai.2lab.dbotter.preview` processes. Launch that exact
path with isolated config and prove PID executable realpath/device/inode/hash
and bundle id before the first AX action.

### Journey

The verifier reads each author id back as the same macOS AXIdentifier, then
drives:

1. first run, Create explicit-id ConnectionId recovery, auto-suffix, all
   credential intents, draft Test, Save & Connect, and restart availability;
2. MySQL catalog paging, exact scanner, prepared-only marker/no-fallback,
   profile A→B target, Execute-limit focus, and single-submit;
3. every Cell copy/TSV case and atomic CSV/TSV/JSON export with independent
   byte verification;
4. cancel/Unknown/exact eviction and reconnect;
5. every reachable PublicSummary recovery plus unreachable rejection and
   disclosure boundary;
6. Redis SCAN/inspect/types/TTL/mutation/classifier;
7. verified TLS CA versus Host recovery with CA preservation and no plaintext;
8. active-operation Delete warning, tombstone order, shutdown, and restart.

### Errors and output

Wrong PID/app, missing AX id, action without real dispatch, label-only recovery,
secret/backend prose leak, missing intended value node, protected-value leak,
or incomplete journey is failure. Output is safe AX/action/verdict metadata, not
user values or screenshots of result data.

## T10.R6 — final typed receipt

### Flow and schema

Source, build, artifact, release, formula, install, CLI, live, AX, and external
export-verifier evidence are linked into the approved typed receipt.

It records exact identity/config objects, manifest/artifact ids, process/file
metadata, safe codes/action/AX ids, counts, timings, and verdicts. It records no
secret, backend prose, SQL/Redis text, result/key/CA/export-path value, exported
bytes, or runtime content digest. Only the isolated seeded-verifier subsection
has fixture id and expected/actual digest verdict.

### Acceptance

Receipt schema/negative fixtures reject provenance mismatch, false clean state,
identity/config conflation, transformed-hash equality, missing live/AX/recovery/
disclosure assertion, value leak, or digest-boundary violation. Overall pass is
derived and all required verdicts must be true.

## T10.R7 — repair-forward rollback

### Entry and flow

Select last-known-good source → run exact `config-contract` → compare with
manifest/release/tap → build and verify a new strictly higher preview → publish
new immutable tag/assets/manifest → atomically update tap → reinstall and rerun
R4–R6.

The wrapper, not a direct old binary, presents the fixed backup runbook when
preflight rejects compatibility.

### Prohibited outcomes

No moved tag, replaced asset, reused artifact metadata, lowered formula version,
silent binary swap, or direct old-binary recovery. A direct older binary only
fails closed with `UnsupportedVersion`.

## Fixed command routing

Source/live/package/Homebrew/AX command blocks are exact in
`04-patch-plan.md`. Each block is attached to the corresponding R1–R6 evidence
record. Command absence or failure leaves the row Not started/RED; it does not
authorize a weaker trace.

## Trace deviations

- Attempt 1 of run `29513008288` failed before verification because the hosted
  runner exhausted device space; attempt 2 reused the same source SHA and passed
  the full Preview graph. No gate was skipped or weakened.
- This checkpoint was manually dispatched from `feat/daily-use-v1`, not a merge
  commit on `main`; P9 therefore remains incomplete even though the immutable
  prerelease, tap update and local xbrew launch succeeded.
- The local computer-use server rejected AX readback because its sender was not
  authenticated. Exact PID/path/bundle evidence was retained, but T10.R5 and
  T10.R6 remain incomplete and no visual pass is claimed.
