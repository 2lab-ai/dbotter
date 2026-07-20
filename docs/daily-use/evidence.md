# dbotter Daily-driver v1.2 — mutable evidence ledger

Normative contract: [`spec.md`](spec.md)

Normative vertical trace: [`trace.md`](trace.md)

Freeze tuple: [`../../04-patch-plan.md`](../../04-patch-plan.md)

This ledger is intentionally outside the frozen tuple. A status update cannot
weaken the normative contract. Exact commands and receipts must be retained
without credentials, query values, keys, cell values or imported/exported data.

| Journey | RED commit | GREEN commit(s) | Local/live/native evidence | Preview/xbrew/installed evidence | Status |
|---|---|---|---|---|---|
| J2 durable SQL workspace/history | `0e1e38d3d1bdf945a49595816d7946b21c2f97f9` through `6ade3eff0b370c1c3475311fc52dd38724ad54e0`; CI portability REDs `b689d352b1295ab42c56b28ee52eaee2e382aa1a`, `819a7c7d7878a793b127180992f0e55dd242565f`, `1fe4afdbabc80bd72e5985df9434030017fba213` and `608f1e4911e8c272b3749006e1bd702c6484c9dc`; installed-fixture REDs `ac93ab18bb788cca14ab9d4226332e8301d5b202` and `88e20761a5c9f9cb7d07eee52c4d09a00c2851c6`; stable-AX REDs `6533ca8d174d8b6a1af88787698e2a9872bdbd68` and atomic-replacement `e5b490ceff45ea172fe0110c68ff4b19c5b5d52d`; earlier exact ancestry retained at `03d8127` | `d674aa6984f2ceace1edd834eba5c0be7ca5797e` through `a66352b58e4183e4a98e3e1cbcf8caa13486bb6b`; ordered errors `4e21cb0098dbf076a3687a6371b23de6c1508fdb`; exact writer guard `b485f7b450ebb43c0b8bd837cd6a276ee0c0c906`; Unix stat portability `215b7386fdecce5b0b88ecf31dcf5aabe185b910` and `4da2f908610f760e2be139f1d8c6d9f1e453c8d4`; cross-target installed tests `4a18a0541e387025c5749bbd992e1939086f633b` and `863f95ae93875f86cf5cf3c8723589c376456aeb`; isolated MySQL entrypoint `577d70d860a9d0d6b7cb4f2a382a2819b2bd7fae`; inspectable tmpfs `1fc60773882b44912d91f29d013b3d86fad2380e`; stable AX path `59eb274767d7d9b064a362b1087b5e7b96b544a5`; per-phase pinned AX identity `5c54b6158dbac98e106129b89390940bf35bbcef` | See J2 local GREEN and repair-forward checkpoints below | Preview `29639223586` / `7bfae29d4e7094a66d4dd5462504618db6778470` and xbrew installation passed exact identity; installed six-step receipt is absent and a higher repair-forward Preview is required | local GREEN |
| J1 secure MySQL connection/Data | — | — | — | — | not started |
| J3 safe typed MySQL row edit | — | — | — | — | not started |
| J4 bounded export/CSV import | — | — | — | — | not started |
| J5 Redis browse/structured edit | — | — | — | — | not started |

## J2 local GREEN checkpoint

- Fixed implementation SHA: `577d70d860a9d0d6b7cb4f2a382a2819b2bd7fae`;
  clean tree and upstream divergence `0/0`.
- `just check` and `just check-all` pass. The all-feature run includes library
  254/254, installed J2 14/14, workspace model 8/8, renderer 10/10, store 34/34,
  result UI 7/7, all Doc-tests and receipt contracts.
- `cargo fmt --check`, all-target/all-feature Clippy `-D warnings`, `bash -n`,
  ShellCheck, release contract and `git diff --check` pass. The installed
  contract compiles and exercises a masked-argv Mach-O writer that the old
  argv-regex probe misses, while the text-vnode guard fails closed.
- Exact-source hermetic verification passed at the original documentation
  checkpoint and after each source fix through successor
  `656c18c7a5c4aeb14711fbad167b27397628a37f`. Preview run `29635298351`
  failed before publication because Ubuntu Unix stat nanoseconds are `u64`
  while macOS uses `i64`. RED `b689d352b1295ab42c56b28ee52eaee2e382aa1a`
  captured the missing checked conversion and GREEN
  `215b7386fdecce5b0b88ecf31dcf5aabe185b910` uses one generic fail-closed
  normalizer. Preview run `29636456372` then reached Ubuntu Rust 1.97 Clippy
  and found the same target-type issue for device, mode and link fields plus one
  redundant test borrow. RED `819a7c7d7878a793b127180992f0e55dd242565f`
  captured every remaining field and the toolchain lint; GREEN
  `4da2f908610f760e2be139f1d8c6d9f1e453c8d4` uses one generic checked
  normalizer for the target-dependent fingerprint fields. Preview run
  `29637246883` crossed those checks and found only three unconditional imports
  used by a macOS-only masked-argv test. RED
  `1fe4afdbabc80bd72e5985df9434030017fba213` captured the target boundary;
  GREEN `4a18a0541e387025c5749bbd992e1939086f633b` fully qualifies the macOS-only
  dependencies and passes both full local gate families plus read-only Linux
  arm64 Rust 1.97.1 all-target/all-feature Clippy with warnings denied.
- Preview run `29638136623` fixed exact source
  `cf156534076f4c7ec55a254a3c7d3cff1d799d08` and passed its GitHub hermetic
  job in 14m36s. It was deliberately canceled before publication after the
  canonical installed Compose failed locally: `MYSQL_PWD=dbotter-local-only`
  contaminated MySQL entrypoint root initialization and produced an
  authentication failure. RED
  `ac93ab18bb788cca14ab9d4226332e8301d5b202` captures the forbidden
  entrypoint environment; GREEN
  `577d70d860a9d0d6b7cb4f2a382a2819b2bd7fae` removes only that redundant
  variable. The exact `mysql:8.4` fixture then reached healthy and reported
  MySQL `8.4.10` with general logging enabled to `TABLE`; both full local gate
  families pass at the GREEN SHA.
- Independent fixed-hash reviews are Critical 0 / High 0 for both the installed
  writer guard and all three CI portability fixes, with no review-time source
  changes. The three failed Preview runs and deliberately canceled fourth run
  all stopped before publication.

## J2 Preview/install and repair-forward checkpoint

- Preview run `29639223586` fixed source
  `7bfae29d4e7094a66d4dd5462504618db6778470` and completed hermetic,
  macOS-package, live, all four target builds, publish and tap successfully.
  Its replacement-fenced prerelease is
  `preview-2026-07-18-094835-29639223586-1-7bfae29d4e70`, version
  `2026.07.18.094835.29639223586.1`, with manifest SHA-256
  `fb66e38353ba676153ab67357a5b227ba4a61559a44d4002468d561d3155e80e`.
- Tap proof binds formula commit
  `6509182a270184c2c7032e6b319a1e3f54819208`, formula blob
  `c9d4f41aab5056ac9bbb5c231bc64635ff24aac2` and tap run
  `29640160981` to that tag, version, source and manifest.
- xbrew installed `2lab-ai/tap/dbotter-preview` version
  `2026.07.18.094835.29639223586.1`. The linked app is signed, has bundle ID
  `ai.2lab.dbotter.preview`, declares `dbotter.icns`, and its host-arm64
  executable SHA-256 is
  `0689e17d166a24550ff8192656d9d3d6ac6c0d21b3bedd70e35bf077ef8c998d`,
  exactly matching the manifest.
- The installed six-step verifier produced no receipt. Its first preflight
  exposed that Compose short-form `tmpfs:` did not appear in Docker's
  inspectable `.Mounts[]` identity. RED
  `88e20761a5c9f9cb7d07eee52c4d09a00c2851c6` freezes the required mount and
  GREEN `1fc60773882b44912d91f29d013b3d86fad2380e` uses long-form `type: tmpfs`.
  The canonical fixture now reports healthy MySQL `8.4.10`, general log
  `TABLE`, exact loopback port `33316` and `/var/lib/mysql` as a tmpfs mount.
- The next preflight reached the native driver and stopped with macOS
  Accessibility permission required. The old verifier built that executable
  under a new `mktemp` directory on every run. RED
  `6533ca8d174d8b6a1af88787698e2a9872bdbd68` requires an opt-in stable path;
  GREEN `59eb274767d7d9b064a362b1087b5e7b96b544a5` introduced that path and
  compared it once with a fresh exact-source build. Independent review of
  successor `174b3cd6abef84b05d8145fe17141ef8c4892d4b` rejected it at
  Critical 0 / High 1 because each later phase reopened the pathname without
  revalidating the compared inode.
- Atomic-replacement RED
  `e5b490ceff45ea172fe0110c68ff4b19c5b5d52d` replaces the stable path with a
  distinct signed Mach-O inode carrying an observable execution marker.
  GREEN `5c54b6158dbac98e106129b89390940bf35bbcef` executes only the canonical
  realpath and pins device, inode, mode, uid, size, SHA-256 and CDHash. It
  validates the euid-owned non-writable `TMPDIR` parent chain and exact
  candidate bytes immediately before and after every one of the six AX phases.
  The focused installed-contract suite passes 16/16, including unchanged
  execution and fail-closed atomic replacement, plus release-contract tests,
  `bash -n`, source-following ShellCheck and `git diff --check`. A concurrently
  hostile same-euid process remains outside the pathname-guard threat boundary
  because macOS provides no `fexecve`-style execution of the verified Mach-O
  descriptor; the guard does not claim otherwise.
- These repair-forward commits postdate the published Preview, so the installed
  `7bfae29d4e70` binary cannot complete the final acceptance. J2 remains
  `local GREEN` until a higher exact Preview, xbrew update, the complete
  six-step receipt and independent file/MySQL readback all pass.
- Preview run `29721277916` fixed source
  `fbb9f438d831f4f07211465dca56e6aa39188e76` and stopped in Ubuntu hermetic
  before plan, build, publish or tap. `MetadataExt`, used only by the macOS
  atomic-replacement test, was imported for all Unix targets and failed Linux
  Clippy with warnings denied. RED
  `608f1e4911e8c272b3749006e1bd702c6484c9dc` extends the existing cross-target
  source contract; GREEN
  `863f95ae93875f86cf5cf3c8723589c376456aeb` scopes only that trait import to
  macOS. The focused source contract, installed J2 16/16 and local
  all-target/all-feature Clippy pass. No release or tag was published by the
  failed run; the succeeding exact Preview must provide the Linux proof.

## Status transition

`not started → RED → implementing → local GREEN → verified`

- RED names the pushed failing contract commit and retained expected failure.
- Local GREEN names all focused/hermetic/live/native commands at the exact SHA.
- Verified additionally names independent Critical/High review, Preview run/tag,
  artifact/tap identity, xbrew version/executable and installed black-box proof.

## Historical v1.1 evidence (not v1.2 completion)

The branch entered v1.2 at `03d6051`. Retained v1.1 foundations include:

- D3 RED commits `4da8a57`, `9e89fbc`, `36fbd76`; GREEN commits `4f047aa`,
  `89a9863`, `54961eb` for server-proven read-only execution and Run all.
- D11 RED/GREEN slices through `08da488` for object-to-data, result inspection,
  workspace split/collapse, history action and closable editor/result tabs.
- Preview run `29534925455`, source
  `8a22e1393134450025a275be19a97332d06317b7`, tag
  `preview-2026-07-16-213015-29534925455-1-8a22e1393134` and xbrew version
  `2026.07.16.213015.29534925455.1` proved packaging and installed executable
  identity. Native computer-use/AX state was unavailable, so it did not prove a
  complete installed journey.

At the reset, create/test/save/connect, catalog browse, generated bounded table
SELECT, read-only current/selection/all, in-memory editor/result tabs, result
inspect/copy/export and Redis SCAN/inspect had real call paths. Writes,
transactions, persistent workspace/history, secure-store/SSH, CSV import and
structured Redis mutations did not. UI labels that merely set status text or
stop the runtime are not accepted as reveal/restart implementations.

## Append rule

1. Record and push RED before production implementation.
2. Record GREEN only after the required gates pass at the named commit.
3. Do not publish user payloads or secrets in evidence.
4. Mark verified only when the installed acceptance and independent backend/file
   readback agree at the same source SHA.
