# dbotter Daily-driver v1.2 — first-party research basis

Status: **Frozen**

Date: `2026-07-17`

This document records the product evidence used to set `DUV1` v1.2 scope. It
does not claim private telemetry or measured feature frequency. Frequency below
is an explicit inference from official quick starts, default product surfaces,
and the intersection of five SQL clients. Competitor source code was not used.

## SQL-client convergence

| User journey | First-party product evidence | Scope inference |
|---|---|---|
| Save, test and reopen a connection | [DBeaver](https://dbeaver.com/docs/dbeaver/Create-Connection/), [DataGrip](https://www.jetbrains.com/help/datagrip/data-sources-and-drivers-dialog.html), [TablePlus](https://docs.tableplus.com/gui-tools/manage-connections), [Beekeeper Studio](https://docs.beekeeperstudio.io/user_guide/connecting/connecting/), [MySQL Workbench](https://dev.mysql.com/doc/workbench/en/wb-mysql-connections-new.html) | Infrequent setup, but an absolute entry gate. Saved profiles, connection test, secure credentials and verified TLS are P0. SSH is a user-approved dbotter P0 product decision supported by these products, not a measured-frequency inference. |
| Find an object and open its data | [DBeaver Database Navigator](https://dbeaver.com/docs/dbeaver/Database-Navigator/), [DataGrip Database Explorer](https://www.jetbrains.com/help/datagrip/database-explorer.html), [TablePlus table view](https://docs.tableplus.com/gui-tools/working-with-table/table), [Beekeeper table view](https://docs.beekeeperstudio.io/user_guide/editing-data/), [Workbench Navigator](https://dev.mysql.com/doc/workbench/en/wb-sql-editor-navigator.html) | A default-session action and P0: searchable schema/table/view tree, one-action Data, structure, filter/sort/page and refresh. |
| Run current, selection or all | [DBeaver SQL execution](https://dbeaver.com/docs/dbeaver/SQL-Execution/), [DataGrip run queries](https://www.jetbrains.com/help/datagrip/run-a-query.html), [TablePlus editor](https://docs.tableplus.com/query-editor/untitled), [Beekeeper editor](https://docs.beekeeperstudio.io/user_guide/sql_editor/editor/), [Workbench query menu](https://dev.mysql.com/doc/workbench/en/wb-sql-editor-main-menu.html) | Repeated for nearly every SQL task and P0. The exact target must be visible; every statement keeps its own result/error and cancellation state. |
| Inspect and compare results | [DBeaver Data Editor](https://dbeaver.com/docs/dbeaver/Data-Editor/), [DataGrip data editor](https://www.jetbrains.com/help/datagrip/data-editor-and-viewer.html), [TablePlus rows](https://docs.tableplus.com/gui-tools/working-with-table/row), [Beekeeper table view](https://docs.beekeeperstudio.io/user_guide/editing-data/), [Workbench Result Grid](https://dev.mysql.com/doc/workbench/en/wb-develop-sql-editor-results.html) | Follows every query and P0: multiple result tabs, grid/record/value detail, local filter/sort, copy and bounded export. |
| Stage a row change, then commit or discard | [DBeaver editing](https://dbeaver.com/docs/dbeaver/Data-Viewing-and-Editing/), [DBeaver transaction mode](https://dbeaver.com/docs/dbeaver/Auto-and-Manual-Commit-Modes/), [DataGrip data editor](https://www.jetbrains.com/help/datagrip/data-editor-and-viewer.html), [TablePlus safe mode](https://docs.tableplus.com/gui-tools/code-review-and-safemode/safe-mode), [Beekeeper staged edits](https://docs.beekeeperstudio.io/user_guide/editing-data/), [Workbench Result Grid](https://dev.mysql.com/doc/workbench/en/wb-develop-sql-editor-results.html) | Medium frequency but highest error cost and P0: stable row identity, local stage, preview, Apply/Discard and explicit Commit/Rollback. |
| Resume and reuse work | [DBeaver Query Manager](https://dbeaver.com/docs/dbeaver/Query-Manager/), [DataGrip recent queries](https://www.jetbrains.com/help/datagrip/find-recent-queries-and-files.html), [TablePlus history](https://docs.tableplus.com/query-editor/query-history), [Beekeeper saved queries](https://docs.beekeeperstudio.io/user_guide/sql_editor/saving_queries/), [Workbench SQL editor preferences](https://dev.mysql.com/doc/workbench/en/wb-preferences-sql-editor.html) | P0. Every product has history, saved scripts or workspace recovery. A session-only editor/history is not a daily workspace. |
| Export and import tabular data | [DBeaver export](https://dbeaver.com/docs/dbeaver/Data-export/), [DBeaver import](https://dbeaver.com/docs/dbeaver/Data-import/), [DataGrip export](https://www.jetbrains.com/help/datagrip/export-data.html), [DataGrip import](https://www.jetbrains.com/help/datagrip/import-data.html), [TablePlus transfer](https://docs.tableplus.com/gui-tools/import-and-export), [Beekeeper export](https://docs.beekeeperstudio.io/user_guide/data-export/), [Beekeeper import](https://docs.beekeeperstudio.io/user_guide/importing-data-csv-json-etc/), [Workbench table transfer](https://dev.mysql.com/doc/workbench/en/wb-admin-export-import-table.html) | Export is a recurring handoff; import is episodic. Both are P0 for the approved v1 goal, with explicit scope, preview, progress/cancel and all-or-nothing import. |

## Redis Insight first-party benchmark

[Redis Insight's current overview](https://redis.io/docs/latest/develop/tools/insight/)
defines its Browser around connection management, key browse/filter, human-readable
formatters and CRUD for core data structures. Its
[3.6 production guard](https://redis.io/docs/latest/develop/tools/insight/release-notes/v.3.6.0/)
adds explicit environment labels and type-to-confirm for destructive production
actions. Therefore Redis browse-only behavior can be described only as a
read-only inspector; a Redis daily driver requires bounded browse plus structured
String/Hash/List/Set/Sorted-Set edits, TTL/PERSIST/delete, immediate-apply wording,
read-only enforcement and production confirmation.

Redis expiry conflict detection uses one cross-version contract. Redis documents
[`TIME` as permitted in read-only Lua scripts](https://redis.io/docs/latest/develop/programmability/eval-intro/),
while [`PEXPIRETIME` starts at Redis 7.0](https://redis.io/docs/latest/commands/expiretime/).
Because P0 includes Redis 6.2, dbotter derives an absolute expiry token from
atomically sampled server `TIME` plus `PTTL` instead of comparing a naturally
decreasing relative TTL.

## Interaction reference

The owner selected DBeaver's first-page screenshots as the minimum interaction
density and the local `ui-ux` OpenAI reference as the visual language. The local
design-system search was run for a professional data-dense desktop database
client. Its generic chromatic suggestion is overridden by the named OpenAI
reference: true white/black, opacity hierarchy, inverted primary emphasis,
square corners and no decorative gradient/shadow. DBeaver supplies layout and
workflow density, not copied branding. The owner-provided
`assets/dbotter-icon.png` is the title/app icon.

## Priority boundary

P0 is the intersection required to complete real work without switching tools:

1. secure saved connection and reconnect;
2. object-to-data navigation;
3. current/selection/all execution and multiple inspectable results;
4. durable drafts and searchable history;
5. safe MySQL generated row editing and transaction resolution;
6. bounded export and transaction-safe CSV import;
7. bounded Redis browse and structured core-type edits;
8. keyboard/AX operation in a persistent navigator/editor/result/status layout.

P1 contains saved-query libraries and parameters, DDL/schema editing, explain-plan
visualization, connection import/export, multi-table/XLSX transfer, Redis
Stream/JSON support and cluster/sentinel support. P2 contains ERD, GIS, charts,
schema compare, backup/scheduler, profiler/slow-log and AI/plugin ecosystems.

## Current dbotter truth at the v1.2 reset

At branch commit `03d6051`, direct MySQL/Redis profile create/test/connect,
schema/relation/column browse, bounded generated table SELECT, read-only
current/selection/all, in-memory editor/result tabs, result inspect/copy/export
and Redis SCAN/inspect have production call paths. The release-blocking gaps are:

- MySQL and Redis credentials have no OS secure-store mode and there is no SSH tunnel;
- MySQL Data is a generated `SELECT * ... LIMIT 500`, without typed paging/filter/sort;
- writable SQL and row editing return `MutationReviewUnavailable`; no transaction API exists;
- editor tabs and History are memory-only and explicitly clear on quit;
- CSV import and structured Redis mutations do not exist;
- global save/new/close/find/transaction shortcuts and installed black-box journeys are absent;
- two recovery labels (`Restart Dbotter`, reveal migration backup) do not perform their stated action.

This audit is why v1.2 completion is based on installed journeys rather than file
presence, unit checks, screenshots or a successful packaging pipeline.
