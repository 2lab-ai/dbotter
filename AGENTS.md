# dbotter — agent guide

Read `01-spec.md`, `02-architecture.md`, and `03-traces.md` before non-trivial
changes. The traces are the cross-layer source of truth.

- Keep pure/display state separate from live driver sessions.
- Never hold a lock across `.await`.
- No `unwrap()`/`expect()`/`panic!()`/`todo!()` in production paths.
- Errors are typed with `thiserror`; never log credentials or credential URIs.
- Config writes are atomic read-merge-write and profile-keyed.
- `just check` must pass before a commit. `just check-all` also compile-checks
  optional desktop and MongoDB seams.
