# dbotter usable MVP — product specification

Status: follow-up review candidate. The third-pass UX/product and architecture/security findings are incorporated, decisions D1 and D2 remain accepted, and no product decision remains open. Production implementation is blocked only until the remediation reviewers return no blocking finding.

## 1. Objective and boundary

Turn the current installable preview into a database client that a local developer can use for a complete, truthful MySQL or Redis task without editing configuration files, guessing hidden state, or copying results out by hand.

“Usable MVP” means the user can create a connection, test an unsaved draft, choose an explicit credential source, connect, browse primary backend objects, run work, understand and recover from failures, copy or export a bounded result, disconnect, delete the profile, restart, and repeat the flow from the Homebrew-installed application. It does not mean DBeaver feature parity.

The existing runtime contract remains in `01-spec.md`, `02-architecture.md`, `03-traces.md`, and `04-patch-plan.md` until the remediation review approves these artifacts. Immediately after that verdict and before production code, P0 reconciles those files and `docs/release/{spec,trace}.md` with this approved trace. There is no other approval or design gate.

## 2. Resolved reviewer decisions

### D1 — accepted macOS preview bundle

- Homebrew installs a per-architecture `Dbotter Preview.app` with bundle id `ai.2lab.dbotter.preview`.
- The app exposes stable author ids that are verified as the same macOS `AXIdentifier` values for every golden-journey control.
- The CLI has a global `--config <path>` argument, which takes precedence over `DBOTTER_CONFIG` and the default path, so installed automation never depends on shell-global state.
- Each macOS bundle is signed before its canonical identity is measured. The canonical executable is the post-sign file at `Dbotter Preview.app/Contents/MacOS/dbotter`; Homebrew's `dbotter` shim points to that file and no unsigned/pre-sign copy is installed.
- The package and receipt use typed source, build, artifact, release, formula, and install identities. Source SHAs are compared only where identity is expected; different transformed artifacts have their own SHA-256 values and are linked through the release manifest rather than falsely asserted equal.
- `dbotter version --format json` is the only machine identity command. Source-built, packaged, and installed invocations return exactly `{package_version,channel,build_id,source_sha,target,arch}`.
- Config compatibility is deliberately separate from binary identity. `dbotter config-contract --format json` returns exactly `{read_versions:[1,2],write_version:2,migration_backup_suffix:".v1.bak"}` with no identity or extra fields; source, manifest, release contract, tap rollback preflight, and installed receipts record and verify that typed value.
- `CFBundleShortVersionString` equals Cargo `package_version` (`x.y.z`). `CFBundleVersion` is exactly the numeric two-component `<run_id>.<run_attempt>`; the separate Homebrew version remains `YYYY.MM.DD.HHMMSS.<run_id>.<run_attempt>`.

### D2 — accepted Redis TLS contract

- New Redis profiles default to `TlsMode::Disabled`; Redis UI offers only Disabled and Required.
- A persisted legacy Redis `Preferred` value remains visible as an invalid profile requiring edit and is rejected before network access. It is never interpreted as Disabled or Required.
- Required uses redis-rs 1.3 with `tokio-rustls-comp`, a `ConnectionAddr::TcpTls { insecure: false, .. }`, hostname verification, and OS roots or an explicitly configured CA PEM file.
- The mandatory live fixture uses a real test CA and server certificate whose SAN matches the configured hostname. Wrong-CA and wrong-hostname negative tests fail. No branch retries plaintext after a TLS failure.
- Redis Required shows `profile.redis_tls.ca_file` and `profile.redis_tls.ca_file.pick`. Blank uses OS roots; a value must be a readable valid PEM CA file. Disabled clears the draft CA field, hides/disables it, and rejects a non-empty direct payload. Non-Redis profiles reject Redis TLS options.
- Invalid PEM, untrusted issuer, and wrong CA produce allowlisted CA codes and focus `profile.redis_tls.ca_file`: draft Test emits `EditDraft(draft, RedisCaFile)`, while saved-profile operations emit `EditProfile(profile, RedisCaFile)`.
- Hostname mismatch produces `TLS_HOSTNAME_MISMATCH` and focuses `profile.host`: draft Test emits `EditDraft(draft, Host)`, while saved-profile operations emit `EditProfile(profile, Host)`. Neither recovery changes transport mode or falls back to plaintext.

## 3. Product invariants

1. The UI never owns a live database client. Database work crosses the background bridge and shared `ApplicationService`.
2. Persisted profiles remain non-secret. A password/token is never serializable and never enters config, events, logs, clipboard, exports, or receipts. Zeroization claims cover only dbotter-owned buffers/stores, not environment, OS, allocator, driver, TLS, kernel, or server copies.
3. Credential source is explicit through persisted `CredentialMode::{None, Session, Environment}`. There is no implicit session-to-environment precedence.
4. Testing an unsaved draft is side-effect-free: no config write, session-cache insertion, session-secret-store update, connection-state change, or workspace creation.
5. Profile ids are immutable. Create and Edit are different mutations; Create can never overwrite an existing profile.
6. Every profile lifecycle has a runtime-monotonic `ProfileGeneration`; deleted generations leave tombstones until shutdown. Every cached handle also has a `SessionGeneration`.
7. Every profile-scoped asynchronous command and event carries `(ProfileId, ProfileGeneration, OperationId)`. Draft work instead carries `(DraftId, OperationId)` and export carries `(ResultId, OperationId)`; global load/shutdown events carry `OperationId` only. A result always carries the profile, generation, and operation that produced it.
8. Driver capabilities are truthful and separately typed: MySQL uses `CATALOG`; Redis uses `KEYSPACE_BROWSE`. Neither is ready before its live contract passes.
9. Cancellation is honest client-side cancellation. Cancel or timeout evicts only the exact session generation used by the operation; the UI never claims the server stopped work.
10. One network operation may run per profile, at most four globally. A dedicated control lane keeps Cancel, Disconnect, and Shutdown responsive.
11. Browser commands use server-side bounds where the backend supports them and retained/rendered/exported state is always capped. Arbitrary user SQL or Redis execution can still transiently materialize a driver row or RESP frame before dbotter applies retained-state caps; the UI and receipt state this limitation honestly.
12. User-visible errors contain static summaries and typed recovery actions, not backend-controlled prose.
13. Preview and stable publication are downstream of the same verification gate. This task publishes preview only.
14. Secret/sensitive request types (`UiCommand`, `ExecuteRequest`, Redis browse/inspect requests, and export commands) have manual redacted `Debug` and no `Serialize` implementation.
15. Secrets and backend-controlled prose are forbidden at every public boundary. User-owned SQL/Redis text, result data, Redis key display, CA path, and export destination may appear only in their intended editor/result/path field and that control's AX value node. Result data reaches clipboard only after explicit Copy and a file only after explicit Export. Those values remain absent from manual Debug, logs, public errors, receipts, unrelated rendered text, AX names/descriptions/live announcements, and unrelated nodes. Secret editors stay masked/protected and never expose the secret in their AX value.
16. SQLx 0.9 may negotiate MySQL `CLIENT_MULTI_STATEMENTS`; that handshake state is not a safety control. Every user-provided MySQL target uses only server prepared protocol (`COM_STMT_PREPARE` then `COM_STMT_EXECUTE`), with no text-protocol fallback.

## 4. Config migration, profiles, and credentials

The approved wire schema is config version 2. The new binary reads version 1 and normalizes it in memory without writing: `secret_env = Some` becomes Environment, otherwise None; Redis Preferred remains invalid/edit-required. The first successfully committed Create/Edit/Delete writes the whole config as version 2 atomically. A frozen fixture of the current version-1 reader must reject that file with `UnsupportedVersion(2)` before service construction, secret resolution, cache lookup, or network access.

Before the first v1→v2 mutation, the UI displays a migration confirmation and the fixed backup path `<config>.v1.bak`. After confirmation, the writer creates that backup with same-directory temp/create-new, mode 0600, file fsync, atomic no-replace rename, and parent-directory fsync. An existing non-identical backup or backup failure aborts the main mutation. The backup is never overwritten. Successful migration shows the backup path and a `RevealMigrationBackup` action; cancelling confirmation changes nothing.

Release rollback may use only a source whose exact `dbotter config-contract --format json` value declares reads `[1,2]`, write `2`, and suffix `.v1.bak`. The installer/rollback wrapper preflights that value and owns presentation of the backup runbook when rejecting it.

Direct invocation of an older binary only fails closed with `UnsupportedVersion(2)`.

`CredentialMode` has exact behavior:

| Mode | Persisted fields | Test/connect source | State after restart |
|---|---|---|---|
| None | `credential_mode = "none"`; no `secret_env` | no secret is supplied | connect remains available without a prompt |
| Session | `credential_mode = "session"`; no `secret_env` | current process's `SessionSecret` | Disconnected/Needs credential; opening Connect presents the masked credential prompt |
| Environment | `credential_mode = "environment"`; required valid `secret_env` name | the named non-empty environment value | UI reports Available, Missing, or Empty without revealing the value; Connect is available only when Available |

Persisted mutations carry the non-serializable `SessionSecretUpdate::{Keep, Replace(Arc<SessionSecret>), Clear}` with these rules:

| Mutation | Mode | Allowed update | Result after config commit |
|---|---|---|---|
| Create | None or Environment | Clear only | no session secret exists |
| Create | Session | Replace or Clear | Replace stores it; Clear saves a profile that will prompt before connect |
| Edit, Session → Session | Session | Keep, Replace, or Clear | retain, replace, or remove exactly that profile's secret |
| Edit, None/Environment → Session | Session | Replace or Clear | no secret from an older mode can be resurrected |
| Edit, Session → None/Environment | destination mode | Clear only | old session secret is removed |
| Edit within None/Environment | destination mode | Clear only | session-secret store remains empty |

`Keep` is invalid for Create or when entering Session from another mode. None rejects `secret_env`; Session rejects `secret_env`; Environment requires it. Replace is invalid outside Session. Save applies the secret update only after the config commit point. Save & Connect then runs the normal Connect path; when Session was saved with Clear it opens the credential prompt instead of attempting a secretless connection.

Existing Session-profile UX uses the non-serializable `SessionCredentialIntent::{KeepCurrent, Replace, Forget}`:

| Context | Allowed/default intent | Test source | Save mapping |
|---|---|---|---|
| Edit Session with a current store Arc | KeepCurrent (default), Replace, Forget | KeepCurrent clones the existing Arc read-only under lock then unlocks; Replace uses one form copy; Forget returns CredentialRequired + `EditDraft(draft, SessionCredential)` before network | KeepCurrent → Keep; Replace → Replace; Forget → Clear |
| Edit Session after restart/no Arc | Replace (default) or Forget; KeepCurrent disabled | Replace uses one form copy; Forget returns CredentialRequired + `EditDraft(draft, SessionCredential)` before network | Replace → Replace; Forget → Clear |
| Create Session or enter Session from None/Environment | Replace (default) or Forget; KeepCurrent invalid | Replace uses one form copy; Forget returns CredentialRequired + `EditDraft(draft, SessionCredential)` before network | Replace → Replace; Forget → Clear |
| None or Environment mode | no Session intent controls | mode-specific source | Clear |

The stable intent controls are `profile.credential.session.keep`, `profile.credential.session.replace`, and `profile.credential.session.forget`. KeepCurrent displays only the static state “set”, never the value. Replace alone reveals a required masked `Zeroizing<String>` input. Forget hides and clears that draft input. Switching away from Session selects Clear; switching into Session cannot resurrect an old value.

`SessionSecretStore` is exactly `HashMap<ProfileId, Arc<SessionSecret>>`. Service code clones the `Arc` while holding the store lock, drops the lock, and only then awaits. The final dbotter-owned `Arc` drop zeroizes `SessionSecret`; no stronger external-copy guarantee is made.

The Replace password field is `Zeroizing<String>`. Replace Test validates the draft and makes one one-shot `Arc<SessionSecret>` copy for temporary connect/ping/close; success and failure drop that copy while preserving the form buffer. KeepCurrent Test clones the existing Arc under the store lock, releases the lock before await, and never mutates the store. Forget/no current secret yields CredentialRequired plus `EditDraft(draft, SessionCredential)` without connector/session acquisition. Test never writes config/cache/store/state/workspace. Editing driver, host, port, database, username, TLS mode/CA, credential mode/name, intent, or replacement password invalidates displayed Test success. Only accepted Save/Save & Connect with Replace moves the form value into `SessionSecretUpdate::Replace(Arc<SessionSecret>)`, then empties/zeroizes it; KeepCurrent moves nothing, and Forget maps Clear. Replace Test success can flow directly to Save & Connect without re-entry.

Version 2 stores Redis-only `redis_tls.ca_file`. Required + blank uses OS roots; Required + value requires a readable regular file containing valid PEM CA certificates before network. Switching the form to Disabled clears the draft value and hides/disables its field/picker; a direct Disabled+value or non-Redis Redis-TLS payload is rejected. Invalid PEM/untrusted issuer/wrong CA returns a CA-specific PublicCode and focuses `profile.redis_tls.ca_file`, using `EditDraft(draft, RedisCaFile)` for Test and `EditProfile(profile, RedisCaFile)` only for saved-profile operations. Hostname mismatch returns `TLS_HOSTNAME_MISMATCH` and focuses `profile.host`, using `EditDraft(draft, Host)` for Test and `EditProfile(profile, Host)` only for saved-profile operations; the wrong-host golden changes only host to `localhost`, retains the CA, and then succeeds without plaintext fallback.

## 5. P0 user journeys

### U0 — first run

- A missing config opens a purposeful empty state with one primary New connection action and short MySQL/Redis choices.
- MongoDB appears only in a disabled Planned area.
- The app explains None, This app session, and Environment variable credential modes.

### U1 — create, test, and edit

- New profile defaults are driver-specific: MySQL port 3306, Redis port 6379 and TLS Disabled.
- Display name produces a suggested slug id. At Create commit, the serialized writer chooses the lowest unused suffix (`local`, `local-2`, …); an auto-slug collision is therefore allocation, not an error. Create carries `(DraftId, OperationId)`. An occupied user-supplied id returns `PROFILE_ID_CONFLICT` with `EditDraft(draft, ConnectionId)`, focuses `profile.connection_id`, and never overwrites or emits a saved-profile recovery.
- Edit carries the immutable original id and expected profile generation; it cannot change id or recreate a deleted profile.
- Editing an existing Session profile with a current Arc defaults to KeepCurrent and reveals only “set”; Replace reveals the required masked field and Forget hides it. Create/enter-Session and restart-without-Arc disable KeepCurrent and default to Replace, with Forget available.
- Test validates and tests the unsaved draft without any profile, cache, credential-store, or workspace side effect. KeepCurrent clones the stored Arc read-only; Replace uses one form copy; Forget/no secret fails CredentialRequired with `EditDraft(draft, SessionCredential)` before network. Its success badge remains only while connection-relevant fields and intent are unchanged.
- Replace Test success retains the masked `Zeroizing<String>` buffer. Save & Connect moves it once into Replace, zeroizes the form buffer, commits version 2, and connects without password re-entry. KeepCurrent maps Keep without moving a buffer; Forget maps Clear.

### U2 — delete

- Delete requires confirmation naming the profile and redacted endpoint. When work is active, the dialog also names the static `OperationKind` and says “Dbotter will stop waiting; the server operation may continue.” Opening/cancelling the dialog has no side effect.
- A committed delete evicts the exact cached generation, clears the session credential and workspace, records a tombstone, and selects a surviving profile or the empty state.
- After confirmation and commit, the runtime publishes the tombstone before Cancel → Join → exact-session eviction. UI/AX reports client stopped waiting and server state Unknown.
- A failure before rename changes no authoritative state. A parent-directory fsync failure after rename reports Committed durability unknown, reloads the exact path, and reconciles to what is actually visible on disk instead of claiming rollback.

### U3 — connect, disconnect, and reconnect

- Connect follows the selected `CredentialMode`, establishes or reuses only a matching profile/session generation, pings, and reports elapsed time.
- Session mode with no secret opens the credential prompt. Environment mode reports name availability without displaying its value.
- Disconnect cancels and joins that profile's operation, evicts its exact session generation, and reports Disconnected without deleting the profile or a Session credential.
- Reconnect always evicts first and creates a verified new session generation.
- Execute or browse auto-connect uses the same path and updates visible connection state.

### U4 — choose and run an exact target responsively

- A trimmed non-empty selection always wins and becomes a user-declared single target; invalid selection never falls back to the caret. Unambiguous multiple top-level MySQL statements or multiple Redis commands reject locally.
- Without a selection, MySQL uses a fixed P0 boundary scanner. `#` always starts a line comment. `--` starts one only when the byte after the second dash is whitespace/control; `SELECT 1--1;` therefore is not a comment. Ordinary `/*…*/` is non-executable and semicolon-opaque. Executable version comment `/*!…*/` is semicolon-opaque to the client but is an executable target even alone. Optimizer hint `/*+…*/` is semicolon-opaque and attaches to a containing statement; alone it is non-executable.
- The scanner protects single/double/backtick delimiters, accepts doubled delimiters and MySQL-default backslash escapes, and treats double quotes as protected under both ANSI_QUOTES modes; it never introspects session SQL mode. In caret extraction, an odd backslash run immediately before a single/double quote returns `AMBIGUOUS_SQL_MODE` and asks the user to select the exact target. Explicit selection supplies that boundary and is never expanded. An unterminated quote/block comment returns local `UNTERMINATED_SQL_TOKEN` Validation before session acquisition.
- SQLx 0.9 may enable `CLIENT_MULTI_STATEMENTS` during its handshake, so handshake flags are explicitly excluded from the safety proof. Selection and caret targets both enter a typed prepared-only MySQL adapter that sends `COM_STMT_PREPARE` and, only on success, `COM_STMT_EXECUTE`. User text is forbidden from `sqlx::raw_sql`, `Executor::execute(&str)`, `COM_QUERY`, or any unsupported-prepared fallback. A statement unsupported by server prepared protocol returns static UnsupportedFeature plus FocusEditor and DismissError; a session is kept only when the typed driver outcome proves it healthy, and the text is never retried through a raw path. Internal catalog queries are static/bound and also prepared.
- The scanner selects only the executable span containing the UTF-8-safe caret; a caret in whitespace/non-executable comment between separators returns `NO_CURRENT_STATEMENT`. One optional trailing terminator belongs to the preceding span. This intentionally documents the NO_BACKSLASH_ESCAPES limitation instead of guessing session mode.
- Without a selection, Redis takes only the caret's physical line, trims it, parses it with `shell_words`, and rejects a blank/comment-only or unparsable line. A semicolon is not a Redis line separator.
- Cmd+Enter on macOS and Ctrl+Enter elsewhere invoke the same action exactly once.
- The `editor.target` toolbar appears beside Execute and always shows selected profile, driver, redacted endpoint, database (MySQL) or Redis DB, and TLS mode. Switching profile A→B updates this target before B's Execute command is created; target display and command correlation are asserted in one RawInput frame sequence.
- Execute row limit and timeout are visible at `editor.row_limit` and `editor.timeout`, validated, and adjustable for the current Execute operation; they are not persisted settings in P0. Browser paging/timeouts use their own typed controls and never dispatch Execute-limit focus.
- Cancel is always routed through the dedicated control lane, stops client waiting promptly, evicts the operation's session generation, and reports server state Unknown.
- A profile has at most one active network operation and the app has at most four. Excess work returns Busy without spawning.

Required extractor cases are normative:

| Case | Expected target |
|---|---|
| trimmed non-empty selection with caret elsewhere | selection, followed by single-command validation |
| `SELECT ';'` or quoted identifier containing `;` | one MySQL statement |
| `SELECT 'it''s;ok'`, `SELECT "a"";b"`, or default-mode `SELECT 'a\' ; b'` | doubled/default-backslash delimiter remains protected |
| `SELECT "a;b"` under either ANSI_QUOTES setting | double-quoted span is protected without SQL-mode introspection |
| `SELECT 1# ;` then newline | `#` always starts a line comment; its semicolon is opaque |
| `SELECT 1--1;` | `--1` is not a comment; final semicolon is the terminator |
| `SELECT 1-- comment ;` then newline | whitespace after the second dash makes a line comment; its semicolon is opaque |
| `SELECT 1 /* ; */;` | ordinary block-comment semicolon is opaque and the comment attaches to the statement |
| caret in standalone `/* ordinary ; */` | `NO_CURRENT_STATEMENT` |
| caret in standalone `/*!40101 SET @x=';' */;` | executable version-comment target, including optional terminator |
| `SELECT /*+ INDEX(t i); */ * FROM t;` | hint semicolon is opaque and the hint attaches to the statement |
| caret in standalone `/*+ hint; */` | `NO_CURRENT_STATEMENT` |
| unterminated `'`, `"`, backtick, or `/*` | local Validation; no session/network |
| caret-derived target containing odd backslash before `'`/`"` | `AMBIGUOUS_SQL_MODE`; ask explicit selection |
| explicit selection after `AMBIGUOUS_SQL_MODE` | selection is the exact boundary; no caret expansion/fallback; prepared-only execution |
| caret on the optional trailing `;` | preceding MySQL statement including the terminator |
| caret in whitespace/comment between `SELECT 1;` and `SELECT 2;` | `NO_CURRENT_STATEMENT` |
| unambiguous selected `SELECT 1; SELECT 2;` | local multiple-statement rejection |
| one statement unsupported by server prepared protocol | static UnsupportedFeature, FocusEditor + DismissError, no raw fallback; retain the session only when proven healthy |
| multibyte UTF-8 text before/inside target | checked character→byte mapping, never split a code point |
| Redis caret line `SET k 'a;b'` | one shell-parsed command on that physical line |
| Redis caret on a blank/comment-only line | `NO_CURRENT_STATEMENT` |

### U5 — browse MySQL lazily

- The explorer loads one bounded page at a time: schemas, then relations for an expanded schema, then columns for an expanded relation.
- The configured database scopes the initial schema view when present. Every level supports Load more; when a retained cap is reached, Clear cached catalog and a narrower filter are reachable recovery actions.
- Refresh failure preserves the prior page, marks it stale, and offers Retry.
- Selecting a table/view inserts a correctly quoted bounded SELECT template into that profile's editor and never executes it.

### U6 — browse Redis honestly

- Refresh and Load more issue cursor-based SCAN, never KEYS. COUNT is labelled and treated as a server hint, not an exact page size or total.
- Search mode is explicit: Literal prefix (default) escapes Redis glob metacharacters before appending `*`; Redis glob sends a validated glob unchanged.
- Retained key identity is raw bytes (`RedisKeyId`); lossy text/hex is display-only and is never sent back to Redis.
- Selecting a key loads TYPE, PTTL, size/count, and a representative bounded preview. Strings use `STRLEN` plus `GETRANGE 0 65535`; other types use count-limited server commands and post-frame retained caps.
- SCAN weak consistency, duplicates, disappearing keys, truncation, and redis-rs whole-frame transient allocation are explained truthfully.
- Required TLS verifies the certificate and hostname; it never retries TCP.
- Required shows the CA path field/picker; blank uses OS roots. Invalid PEM/untrusted issuer/wrong CA returns the CA code and focuses `profile.redis_tls.ca_file`; Test uses `EditDraft(draft, RedisCaFile)` and saved-profile work uses `EditProfile(profile, RedisCaFile)`. Hostname mismatch returns `TLS_HOSTNAME_MISMATCH` and focuses `profile.host`; Test uses `EditDraft(draft, Host)` and saved-profile work uses `EditProfile(profile, Host)`. The golden changes only host to `localhost`, retains the CA, and connects. Neither path retries plaintext.

### U7 — understand, copy, and export results

- Each profile generation has its own editor, pending state, latest historical/current result, and error. Query history/recent statements are not part of P0.
- Result provenance shows profile, driver, completed time, duration, row/affected count, truncation, and operation id.
- Copy cell emits exactly `clipboard_scalar(cell)`, with no header or trailing newline and with literal tab/CR/LF/backslash unescaped. Copy selected rows applies `tsv_field(clipboard_scalar(cell))`, emits one escaped TSV header plus all visible schema columns for noncontiguous rows sorted by visible row index, and ends with one LF. Copy all uses the same header/field mapping plus every visible row in visible order and ends with one LF.
- CSV, TSV, and JSON exports stream from an immutable `Arc<ResultSnapshot>` on a background task. They do not build a second whole-result byte vector.
- Runtime UI/events/logs/receipts expose format, counts, mode, overwrite policy, and commit outcome but no content or digest. Only the seeded external verifier may record `{fixture_id, expected_digest, actual_digest, verdict}` while comparing exact bytes.

### U8 — understand and recover from errors

- Empty, loading, busy, cancelled, stale, success, Needs credential, and error states are distinct without relying on color.
- Errors use `ErrorCategory`, `PublicCode`, static `PublicSummary`, and one or more typed `RecoveryAction` values. Each displayed recovery button dispatches a real command or local focus/open action.
- Auth recovery opens credential entry/edit; draft Test always edits its `DraftId`, while saved-profile operations may open a profile prompt/edit. An idempotent saved-profile recipe may offer Retry only after credential state changes, while draft Test and mutating Execute never do. Syntax recovery focuses the editor; network/TLS recovery opens the exact typed draft/profile field; stale browser recovery reruns the typed browse command; durability-unknown recovery follows the operation-specific config/export row.
- Recovery construction is total over the closed reachable `OperationKind × PublicSummary` table in §8 and returns `NonEmpty<RecoveryAction>`; an unlisted combination is rejected in the internal-to-public conversion rather than rendered without an action.
- Raw SQLx/Redis text, credentials, credential URIs, SQL text, Redis args/values, and export contents never render through an error, log, or receipt.

### U9 — accessible installed restart

- Every P0 control has a stable AX identifier, role, accessible name, deterministic focus order, keyboard activation, and non-color state cue. Required ids include `profile.connection_id`, `profile.host`, the Redis CA controls, all three Session intent controls, `editor.row_limit`, and `editor.timeout`.
- Normal text contrast is at least 4.5:1; large text, focus indicators, and essential UI component boundaries are at least 3.0:1, verified numerically from the actual palette.
- After restart, only normalized/persisted profile fields and `CredentialMode` remain. Results, pending work, workspaces, and session credentials do not.
- A Session profile opens Needs credential and a prompt before reconnect. An Environment profile shows Available/Missing/Empty for the name without revealing its value.
- AX tests require user-owned SQL/Redis text, result cells, Redis key display, CA path, and export destination to be present in only the intended control's value node. They are absent from names/descriptions/live announcements and unrelated nodes. The masked secret input has a protected value and never exposes secret bytes.

## 6. Reload, task, and shutdown contract

`ReloadConfiguration` diffs the last authoritative snapshot against the newly loaded version-2 snapshot by immutable ProfileId:

| Diff | Generation/session/secret/workspace outcome |
|---|---|
| unchanged | preserve ProfileGeneration, cache, SessionSecret Arc, connection state, and workspace |
| added | allocate a new monotonic generation; no cache/secret; fresh Disconnected or Needs credential workspace |
| changed | publish a new-generation fence, cancel/join old work, evict exact session, clear the old Session secret, apply the new credential mode, and retain editor/result only as clearly historical under the new target |
| removed | publish tombstone, cancel/join, evict, clear secret/workspace, and reject every late event |

A removed id later re-added receives a generation greater than its tombstone. An unreadable/ambiguous post-commit config enters Config uncertain: fence every active generation, cancel/join all operations, clear every cache/secret, disable every action except Reload configuration and Shutdown, and display no prior connection as usable.

The task registry is exactly a tagged record, never a bag of optional profile/draft fields:

```rust
struct RegisteredTask {
    operation_id: OperationId,
    scope: TaskScope,
    cancel: CancellationToken,
    join: JoinHandle,
}

enum TaskScope {
    Profile { profile_id: ProfileId, profile_generation: ProfileGeneration, session_generation: Option<SessionGeneration> },
    Draft { draft_id: DraftId },
    Export { result_id: ResultId },
    Global,
}
```

Only `TaskScope::Profile` carries profile/session generations. Draft, Export, and Global cleanup/correlation use only the ids in their own variants.

Async network tasks and blocking export workers have different shutdown contracts. The two-second grace/abort applies only to abortable async tasks. Export checks cancellation at every row/chunk, owns a temp cleanup guard, and Shutdown waits for the actual worker to return and clean the temp; it never calls abort on `spawn_blocking`. An in-flight config mutation is allowed to reach and report its commit outcome. Queued commands—including secret-bearing ones—are drained/dropped before `RuntimeShutdown`.

Any async/blocking task panic (`JoinError`) maps to static Internal failure. Registry and permit cleanup happens first, the exact used session is evicted, export temp cleanup completes, and at most one terminal event is attempted even with a full/closed event lane. Tests cover full event lane, task/encoder panic, blocked encoder, shutdown during mutation/network/export, permit leakage, and secret-bearing queue drain.

## 7. Resource and response limits

These are P0 constants, not implementation suggestions:

| Resource | Default | Hard/retained cap | Recovery/qualification |
|---|---:|---:|---|
| Network operations | 1/profile | 4 process-wide | Busy; Cancel/Disconnect remain available |
| Runtime channels | — | work 32, mutation 16, control 16, UI events 128 | control capacity exceeds the four active-operation cancel/disconnect demand; Shutdown also has a watch signal |
| Export workers | 1/result | 2 process-wide | Busy; export cancellation and Shutdown still join/clean temp files |
| Execute rows | 500 | 10,000 retained | result is marked truncated |
| Execute timeout | 30 s | 300 s | timeout evicts exact session generation |
| Shared result snapshot | — | 1,024 columns, 8 MiB total, 1 MiB/text-or-binary cell, 32 static notices of 512 bytes | oversized cell is an explicit preview with original length; later rows are omitted/truncated |
| MySQL catalog page | 50 nodes | 200 nodes/page | Load more with opaque keyset token |
| MySQL retained catalog | — | 200 schemas, 2,000 relations, 10,000 columns, 512 columns/relation, and 4 MiB UTF-8 metadata/profile | Clear cached catalog and narrow filter |
| MySQL catalog timeout | 5 s | 30 s/page | preserve prior page as stale |
| Redis SCAN COUNT | 100 hint | 1,000 hint | actual frame may differ; retain within caps |
| Redis retained keys | — | 10,000 keys or 8 MiB raw-key bytes/profile; 4 KiB/key | oversize/unretained counts are explicit; clear/restart scan |
| Redis preview | 100 items | 1 MiB retained, 64 KiB/cell, depth 8 | truncation explicit; string range is server-bounded to 65,536 bytes |
| Generic Redis result | request row limit | 10,000 cells within the shared 8 MiB cap, 64 KiB/cell, depth 8 | redis-rs may allocate the whole RESP frame first |
| MySQL result | request row limit | 10,000 rows within the shared snapshot cap | SQLx may allocate the current row/cell first |
| Redis command input | — | 65,536 UTF-8 bytes, 1,024 shell tokens, 16 KiB/token | pure local classifier rejects streaming/blocking families before session acquisition |

App-controlled browse commands use server bounds where available. Caps after driver decoding protect dbotter's retained, rendered, copied, and exported state; they are not a claim that arbitrary server responses cannot transiently consume more driver memory.

Result notices are a closed `ResultNotice` enum with static text; backend-controlled warning text is not copied into retained/public snapshots. A truncated Text/Bytes/Json cell records retained preview bytes, original length when the driver exposes it, and `truncated = true`; copy/export preserves that metadata rather than presenting the preview as complete.

Every MySQL catalog page query asks the server for `page_size + 1`, retains at most `page_size`, and derives `next_token` from the last retained deterministic sort key only when the extra row exists. The 4 MiB metadata budget counts retained UTF-8 bytes for schema/relation/column names and type strings; reaching any count/byte cap exposes Clear catalog + narrower prefix rather than an unreachable token.

Redis Execute parses/classifies before session acquisition. Input is capped as above, command matching is ASCII case-insensitive, and the local closed classifier never consults backend `COMMAND` metadata. It always rejects Pub/Sub/monitor/replication/wait commands `SUBSCRIBE`, `PSUBSCRIBE`, `SSUBSCRIBE`, `UNSUBSCRIBE`, `PUNSUBSCRIBE`, `SUNSUBSCRIBE`, `MONITOR`, `SYNC`, `PSYNC`, `REPLCONF`, `WAIT`, and `WAITAOF`; every command token beginning `BL`; and `BZPOPMIN`, `BZPOPMAX`, `BZMPOP`, `BRPOP`, and `BRPOPLPUSH`. (`BR*`/`BZ*` entries are explicit because they do not begin `BL`.) `XREAD`/`XREADGROUP` is rejected when a `BLOCK` option occurs before `STREAMS`; a key literally named BLOCK after `STREAMS` is allowed. Parse/length/policy failure sends no Redis command.

## 8. Public error and recovery vocabulary

`ErrorCategory` is a closed enum: `Validation`, `Authentication`, `Permission`, `Network`, `Tls`, `Timeout`, `Syntax`, `Constraint`, `Unsupported`, `Cancelled`, `Busy`, `Stale`, `Io`, and `Internal`. `PublicCode` is a tagged allowlist of dbotter stable codes, validated MySQL errno/SQLSTATE pairs, and stable redis-rs error kinds; it has no free-form backend-message variant.

TLS/scanner codes include `REDIS_TLS_CA_INVALID_PEM`, `REDIS_TLS_CA_UNTRUSTED_ISSUER` (including a wrong CA), `TLS_HOSTNAME_MISMATCH`, `AMBIGUOUS_SQL_MODE`, and `UNTERMINATED_SQL_TOKEN`. The CA codes can never dispatch Host recovery, and the hostname code can never dispatch CA recovery.

`PublicSummary` is a closed enum with static display strings: `InvalidInput`, `CredentialRequired`, `AuthenticationFailed`, `PermissionDenied`, `NetworkUnavailable`, `TlsVerificationFailed`, `OperationTimedOut`, `SyntaxRejected`, `ConstraintRejected`, `UnsupportedFeature`, `OperationCancelled`, `ResourceBusy`, `ResourceStale`, `ConfigWriteNotCommitted`, `CommittedDurabilityUnknown`, `ExportFailed`, and `InternalFailure`.

`OperationKind` is closed: `LoadConfiguration`, `ReloadConfiguration`, `MigrateConfiguration`, `CreateProfile`, `UpdateProfile`, `DeleteProfile`, `TestDraftConnection`, `ConnectProfile`, `DisconnectProfile`, `ReconnectProfile`, `ExecuteRead`, `ExecuteMutation`, `BrowseMySql`, `BrowseRedis`, `InspectRedis`, `ExportResult`, and `ShutdownRuntime`.

`ProfileFieldId` is closed: `ConnectionId`, `DisplayName`, `Host`, `Port`, `Database`, `Username`, `CredentialMode`, `CredentialEnvironmentName`, `SessionCredential`, `RedisTlsMode`, and `RedisCaFile`. `RecoveryAction` carries only safe ids: `OpenCredentialPrompt(ProfileId)`, `EditDraft(DraftId, ProfileFieldId)`, `EditProfile(ProfileId, ProfileFieldId)`, `Retry(OperationRecipeId)`, `FocusEditor(ProfileId)`, `FocusExecuteLimits(ProfileId)`, `ReloadConfiguration`, `Reconnect(ProfileId)`, `CancelOperation(OperationId)`, `ClearCatalog(ProfileId)`, `RestartRedisScan(ProfileId)`, `ChooseExportDestination(ResultId)`, `RevealExportDestination(ResultId)`, `RevealMigrationBackup`, `RestartApplication`, and `DismissError(OperationId)`.

The conversion function is exhaustive over reachable pairs and returns `NonEmpty<RecoveryAction>`. In the table, Execute expands to `ExecuteRead|ExecuteMutation` and Browse expands to `BrowseMySql|BrowseRedis`. `TestDraftConnection` is never folded into a saved-profile row:

| PublicSummary | Reachable OperationKind/context | Required non-empty recovery |
|---|---|---|
| InvalidInput | Load/Reload/Migrate | ReloadConfiguration |
| InvalidInput | CreateProfile | EditDraft(draft, exact validated field); `PROFILE_ID_CONFLICT` maps to ConnectionId |
| InvalidInput | UpdateProfile | EditProfile(profile, exact validated field) or ReloadConfiguration as selected by code |
| InvalidInput | DeleteProfile | ReloadConfiguration or DismissError as selected by code |
| InvalidInput | TestDraftConnection | EditDraft(draft, exact validated field id) |
| InvalidInput | ConnectProfile/ReconnectProfile | EditProfile(profile, exact validated field id) |
| InvalidInput | ExecuteRead/ExecuteMutation, statement/target code | FocusEditor |
| InvalidInput | ExecuteRead/ExecuteMutation, row-limit/timeout-input code | FocusExecuteLimits(profile), focusing `editor.row_limit` or `editor.timeout` by code |
| InvalidInput | BrowseMySql | ClearCatalog or DismissError as selected by code |
| InvalidInput | BrowseRedis | RestartRedisScan or DismissError as selected by code |
| InvalidInput | InspectRedis | DismissError |
| InvalidInput | ExportResult | ChooseExportDestination |
| CredentialRequired | TestDraftConnection | Exactly one of EditDraft(draft, SessionCredential), EditDraft(draft, CredentialEnvironmentName), or EditDraft(draft, Username), selected by PublicCode; Forget/no-secret selects SessionCredential before network |
| CredentialRequired | Connect/Execute/Browse/Inspect | OpenCredentialPrompt and EditProfile(profile, SessionCredential) |
| AuthenticationFailed | TestDraftConnection | Exactly one of EditDraft(draft, SessionCredential), EditDraft(draft, CredentialEnvironmentName), or EditDraft(draft, Username), selected by PublicCode |
| AuthenticationFailed | Connect/Execute/Browse/Inspect | OpenCredentialPrompt for Session, otherwise EditProfile(profile, CredentialEnvironmentName or Username); Retry only after changed state for an idempotent profile recipe |
| PermissionDenied | TestDraftConnection | Exactly one of EditDraft(draft, Username) or EditDraft(draft, Database), selected by PublicCode, and DismissError |
| PermissionDenied | Connect/Browse/Inspect | EditProfile(profile, Username or Database) and DismissError |
| PermissionDenied | ExecuteRead/ExecuteMutation | FocusEditor and DismissError; never automatic Retry |
| NetworkUnavailable | TestDraftConnection | EditDraft(draft, Host) and DismissError |
| NetworkUnavailable | Connect/Reconnect/Browse/Inspect/ExecuteRead | EditProfile(profile, Host) and Reconnect; an idempotent connect/browse recipe may additionally offer Retry |
| NetworkUnavailable | ExecuteMutation | EditProfile(profile, Host) and Reconnect; no Retry |
| TlsVerificationFailed + CA code | TestDraftConnection | EditDraft(draft, RedisCaFile), focusing `profile.redis_tls.ca_file`; no transport fallback |
| TlsVerificationFailed + CA code | Connect/Browse/Inspect/Execute | EditProfile(profile, RedisCaFile), focusing `profile.redis_tls.ca_file`; no transport fallback |
| TlsVerificationFailed + hostname code | TestDraftConnection | EditDraft(draft, Host), focusing `profile.host`; CA remains unchanged and there is no transport fallback |
| TlsVerificationFailed + hostname code | Connect/Browse/Inspect/Execute | EditProfile(profile, Host), focusing `profile.host`; CA remains unchanged and there is no transport fallback |
| OperationTimedOut | TestDraftConnection | EditDraft(draft, Host) and DismissError |
| OperationTimedOut | ConnectProfile/ReconnectProfile | EditProfile(profile, Host), Reconnect, and DismissError |
| OperationTimedOut | ExecuteRead/ExecuteMutation | FocusExecuteLimits(profile), focusing `editor.timeout`, and Reconnect; ExecuteMutation has no Retry |
| OperationTimedOut | BrowseMySql/BrowseRedis/InspectRedis | Retry(exact generation-checked idempotent recipe that auto-connects) or Reconnect(profile), plus DismissError |
| SyntaxRejected | ExecuteRead/ExecuteMutation | FocusEditor; no automatic Retry |
| ConstraintRejected | ExecuteRead/ExecuteMutation | FocusEditor and DismissError; no automatic Retry |
| UnsupportedFeature | TestDraftConnection with legacy Redis TLS | EditDraft(draft, RedisTlsMode) |
| UnsupportedFeature | ConnectProfile with legacy Redis TLS | EditProfile(profile, RedisTlsMode) |
| UnsupportedFeature | ExecuteRead/ExecuteMutation, prepared-unsupported MySQL statement | FocusEditor and DismissError; retain only a proven-healthy session and never fall back to text protocol |
| UnsupportedFeature | Other Execute/Browse/Inspect | DismissError |
| OperationCancelled | TestDraftConnection | DismissError only |
| OperationCancelled | Connect/Execute/Browse/Inspect | Reconnect and DismissError |
| OperationCancelled | ExportResult | ChooseExportDestination or RevealExportDestination, plus DismissError |
| ResourceBusy | TestDraftConnection with a known active operation | CancelOperation(active OperationId) and DismissError |
| ResourceBusy | TestDraftConnection without a safe active operation id | DismissError(rejected OperationId) |
| ResourceBusy | Any other enqueueable kind with a known active operation | CancelOperation(active OperationId) and DismissError |
| ResourceBusy | Any other enqueueable kind without a safe active operation id | DismissError(rejected OperationId) |
| ResourceStale | Reload/Create/Update/Delete | ReloadConfiguration |
| ResourceStale | Browse/Inspect or idempotent Connect | Retry(exact generation-checked recipe) |
| ResourceStale | ExecuteRead/ExecuteMutation | ReloadConfiguration and FocusEditor; no automatic Retry |
| ConfigWriteNotCommitted | Migrate/Create/Update/Delete | ReloadConfiguration; Migrate may additionally RevealMigrationBackup when the fixed backup exists |
| CommittedDurabilityUnknown | Migrate/Create/Update/Delete | ReloadConfiguration; Migrate additionally offers RevealMigrationBackup when present |
| CommittedDurabilityUnknown | ExportResult | RevealExportDestination(ResultId) |
| ExportFailed | ExportResult | ChooseExportDestination; add RevealExportDestination only when a committed destination exists |
| InternalFailure | TestDraftConnection | RestartApplication and DismissError |
| InternalFailure | Any other reachable kind | RestartApplication and DismissError; config kinds additionally offer ReloadConfiguration |

Every other `OperationKind × PublicSummary` combination is unreachable and the internal-to-public conversion rejects it in a total table test. Draft-test recovery is constructed only from `(DraftId, OperationId, PublicCode)` and may emit `EditDraft`, `CancelOperation`, `RestartApplication`, or `DismissError`; no stored operation recipe exists. Saved-profile prompt/edit/reconnect actions are excluded from that scope. Create recovery is likewise constructed from `(DraftId, OperationId, PublicCode)`; its only field action is `EditDraft`, while config/durability outcomes may use global Reload/Dismiss actions. `PROFILE_ID_CONFLICT` maps exactly to `EditDraft(draft, ConnectionId)`/`profile.connection_id`; auto-slug suffix allocation never produces that error. The controller maintains bounded non-secret retry recipes only for idempotent connect/browse/reload operations; mutating SQL/Redis Execute never receives automatic Retry. `EditDraft(_, RedisCaFile|Host)` and `EditProfile(_, RedisCaFile|Host)` are the typed CA/host focus actions for their respective identity domains. `FocusExecuteLimits` targets only `editor.row_limit`/`editor.timeout` for Execute. `RevealMigrationBackup` derives the fixed path from trusted config state, and `RevealExportDestination(ResultId)` derives the path from the export registry; neither carries path data. `RestartApplication` performs orderly shutdown then requests relaunch, while `DismissError` changes only local error visibility.

Safe MySQL errno/SQLSTATE and allowlisted Redis error kinds may populate typed `PublicCode`; backend messages never populate a summary. Every variant-to-command mapping is contract-tested.

## 9. Export wire contract and file safety

All export formats preserve column order and duplicate names. CSV and TSV are enabled only for tabular results; JSON also represents mutation-only results. CSV and TSV always emit one header row when columns exist, including a zero-row result. TSV uses the same escaped header fields and LF records as data.

`clipboard_scalar(Cell)` is a pure total function and is also the canonical CSV/TSV scalar before format-specific quoting:

| Cell variant | Exact clipboard scalar |
|---|---|
| Null | empty string |
| complete Text | literal UTF-8 text |
| truncated Text | UTF-8-boundary preview + `…[dbotter-truncated;original_len=N]` |
| Bool | `true` or `false` |
| Int / UInt / Decimal | canonical base-10 string |
| finite Float | shortest round-trip decimal |
| non-finite Float | `nan`, `inf`, or `-inf` |
| DateTime | normalized ISO-8601 text |
| complete Json | compact recursively key-sorted JSON |
| JsonPreview | `json-preview:<UTF-8-prefix>;truncated=true;original_len=N` |
| complete Bytes | `base64:<RFC4648>` |
| truncated Bytes | `base64:<RFC4648-preview>;truncated=true;original_len=N` |

Copy cell writes that scalar byte-for-byte: tab, CR, LF, and backslash remain literal, and no header or trailing newline is added. `tsv_field(s)` is a pure character-wise transform: backslash → `\\`, tab → `\t`, CR → `\r`, and LF → `\n`; every other character is unchanged. Copy selected/all applies `tsv_field` to each visible column name and `tsv_field(clipboard_scalar(cell))` to each positional field. Selected noncontiguous rows are sorted by visible row index; all visible schema columns remain in visible order; both forms write one header and exactly one final LF. Golden tests cover every table row above, embedded controls/backslash, Unicode, duplicate names, null/empty rows, and truncation.

- CSV is UTF-8 without BOM, always writes a header for non-empty columns, uses RFC 4180 quoting and CRLF records. Null is an empty field; complete text is literal; truncated text is its UTF-8-boundary preview followed by `…[dbotter-truncated;original_len=N]`; bool is `true`/`false`; Int/UInt/Decimal use canonical base-10 strings; finite Float uses shortest round-trip decimal, with `nan`, `inf`, and `-inf` for non-finite values; DateTime uses normalized ISO-8601 text; complete JSON uses compact recursively key-sorted JSON; a JSON preview is `json-preview:<UTF-8-prefix>;truncated=true;original_len=N`; bytes use `base64:<RFC4648>` and append `;truncated=true;original_len=N` when incomplete. Every `original_len` is the original byte length.
- TSV is UTF-8 with LF records and the same scalar mapping. Backslash, tab, CR, and LF inside a field become `\\`, `\t`, `\r`, and `\n`.
- JSON is UTF-8 compact canonical JSON with keys emitted in this exact top-level order and schema `dbotter.result.v1`: `{schema, provenance, columns, rows, affected_rows, last_insert_id, truncated}`. `provenance` is `{operation_id,profile_id,profile_generation,driver,completed_at,elapsed_ms}`; `completed_at` is UTC exactly `YYYY-MM-DDTHH:MM:SS.mmmZ` with millisecond precision. `columns` is an ordered array of `{index,name,type_name}`. `rows` is an array of positional cell arrays, so duplicate column names cannot overwrite. Null is `{type:"null"}`. Int/UInt/Decimal/Float are `{type,value}` with the canonical strings above; Bool has a JSON boolean; complete Text/DateTime have string values; truncated Text additionally has `original_len` then `truncated:true`; complete Json has a recursively key-sorted JSON value; truncated JSON uses type `json_preview`, a UTF-8 prefix string, `original_len`, then `truncated:true`; Bytes is `{type:"bytes",value:{base64,original_len,truncated}}` in that key order.

Export creates a same-directory random temp with `create_new` and mode 0600, streams and flushes, and `sync_all`s it. `DenyOverwrite` commits atomically with macOS `renamex_np(RENAME_EXCL)` or Linux `renameat2(RENAME_NOREPLACE)`; a competing destination created at the barrier wins and remains untouched. `ReplaceConfirmed` captures the confirmed regular file's device/inode/size/mtime identity and rejects a mismatch immediately before ordinary rename. A pre-existing symlink/non-regular destination is rejected. A post-check directory-entry swap can still occur in the explicitly local single-user threat model; POSIX rename replaces that final directory entry and does not follow a symlink target, but dbotter does not claim the check closes that race. Rename is the commit point and parent-directory fsync follows. Pre-rename failure/cancel removes temp; post-rename cancel cannot claim rollback. Parent-fsync failure reports Committed durability unknown.

Runtime export events/logs/receipts contain no content SHA/digest. Installed seeded verification computes expected bytes with an independent reference encoder; for dynamic JSON it supplies the observed `completed_at` to that encoder (or normalizes it to the fixed fixture timestamp before both encodings). Only its external receipt records fixture id, expected digest, actual digest, and verdict—never bytes.

## 10. Verification and delivery acceptance

P0 completion requires all of the following:

1. Unit/contract tests cover read-only v1 normalization, atomic v1→v2 migration/backup, frozen current-v1-reader rejection, exact config-contract JSON, Create DraftId recovery/ConnectionId collision versus auto-suffix allocation, credential/update/SessionCredentialIntent matrices, draft buffer retention/invalidation/move, tagged `RegisteredTask`/`TaskScope` shapes, reload diffs, mutation failpoints, generations/tombstones, controller shutdown/races, typed seams, exact MySQL tokenizer and prepared-only source/trait cases, Redis command policy, bounds, exhaustive recovery totality including TestDraft DraftId-only dispatch, every clipboard scalar/TSV field, and exact export/no-clobber behavior.
2. Mandatory egui 0.35 headless tests use `Context::run_ui(RawInput, …)`, call `enable_accesskit()`, inspect `FullOutput.platform_output.accesskit_update`, and prove author id → installed macOS AXIdentifier readback, including `profile.connection_id`, `editor.row_limit`, and `editor.timeout`; stable names/roles/focus/shortcuts/target correlation/Session intents/field-specific TLS and Execute-limit recovery/disabled/error paths; numerical contrast; intended user-content value-node presence; and absence from names/descriptions/live announcements/unrelated nodes. Secret AX values remain protected.
3. Required live auth covers MySQL plus Redis ACL/requirepass over plaintext and verified TLS: correct/wrong Session secrets, Environment Available/Missing/Empty, static auth code/action, and successful recovery. The MySQL safety fixture seeds an empty marker table. UI selection rejects the unambiguous `INSERT INTO marker VALUES ('first'); INSERT INTO marker VALUES ('second')` locally; prepared-adapter live cases derived from explicit-selection and ambiguous/current-target entry points submit that exact two-statement text to `COM_STMT_PREPARE`, require server prepare rejection, and assert both markers absent, especially `second`. A separately prepared-unsupported statement proves static UnsupportedFeature, FocusEditor/DismissError, and no raw fallback while retaining only a proven-healthy session. Redis TLS negatives separately assert CA-code→CA focus and hostname-code→`profile.host`; wrong-host recovery changes only host to `localhost`, retains CA, succeeds, and never reaches plaintext. Every missing fixture/env is a named false assertion and non-zero exit.
4. Headless CLI `browse` and `inspect` commands exercise the same typed service seams and are run from the installed package with explicit `--config`.
5. A source-bound receipt uses `SourceIdentity::LocalAttached` locally or `SourceIdentity::CiExpectedSha` in CI plus typed build/artifact/release/formula/install identities. Binary identity comes only from `dbotter version --format json` with the exact six-field schema.
6. Compatibility independently comes from `dbotter config-contract --format json` with exactly three fields `{read_versions:[1,2],write_version:2,migration_backup_suffix:".v1.bak"}` and is linked through manifest/release/tap/install receipts.
7. CI verification gates preview and stable workflows. No publish job can run when a required job or receipt fails.
8. Every preview version is strictly increasing and includes UTC seconds, GitHub `run_id`, and `run_attempt`. Tap dispatch receives explicit tag, source SHA, version, and manifest.
9. The installed GUI verifier requires `--app-path` resolved from `brew --prefix dbotter-preview`, terminates/rejects stale same-bundle processes, launches that exact app, and before AX input proves PID executable realpath/device/inode/SHA and bundle id against the manifest. The same post-sign executable passes version/check/exec/browse/inspect.
10. The golden journey proves Create explicit-id collision → ConnectionId draft focus and auto-slug suffix success; all Session intents; unsaved draft-test failure/recovery with only EditDraft/OperationId-safe actions and no saved-profile action; restart credential prompt/env availability; exact MySQL scanner and prepared-only marker/no-fallback cases; `editor.row_limit`/`editor.timeout` recovery; split CA/host TLS recovery; all clipboard scalar/control/truncation cases; recoverable errors; and byte-exact CSV/TSV/JSON exports with mode 0600 and expected overwrite/symlink behavior.
11. Rollback publishes a new higher preview only after manifest/release/tap preflight verifies the exact config-contract from the last-known-good source. A missing/mismatched command is rejected before publish, and the installer/rollback wrapper owns backup-runbook presentation. Rollback never moves/reuses a tag, lowers the formula version, or silently swaps an artifact.
12. Direct older-binary invocation only returns UnsupportedVersion.
13. `plutil` and manifest negative tests enforce Cargo `x.y.z` as `CFBundleShortVersionString`, numeric `<run_id>.<run_attempt>` as `CFBundleVersion`, and the independent long Homebrew version.
14. No stable tag or stable release is created by this task.

## 11. Explicit exclusions

- Live MongoDB.
- Editable result grids, transaction UI, SSH tunnels, imports, ER diagrams, AI, or multi-tab IDE behavior.
- Query history or recent statements, persisted or in memory.
- Keychain credential persistence.
- Guaranteed server-side cancellation.
- Multi-process config-writer coordination. Every mutation reloads the exact path, fingerprints it, rechecks that fingerprint immediately before rename, and serializes all writers inside one dbotter process. A detected external change returns Reload configuration; an external write racing after the recheck remains unsupported and no stronger safety claim is made.

## 12. Follow-up-review approval criterion

Implementation becomes authorized when the remediation UX/product and architecture/security reviewers both confirm that this spec, `trace.md`, and `plan.md` are mutually consistent and have no blocking finding. D1, D2, config version 2/migration backup, limits, formats, error vocabulary, and delivery identity are resolved decisions and are not awaiting another choice.
