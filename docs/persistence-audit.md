# Persistent Storage Audit and SQLite Evaluation

Date: 2026-07-03

Scope: structured state persistence in the `double-helix`/`dhx` workspace. User-facing logs are explicitly out of scope for database migration: they should stay plain text files because they are immediately grabbable, tailable, and useful during failure.

## Status

- 2026-07-03: SQLite migration completed. The workspace MSRV is now Rust 1.95, and `helix-store` is a workspace member with SQLite state/cache databases, WAL/busy-timeout/foreign-key setup, a forward-only schema-version migration runner, drizzle-rs schema derives, and typed repository APIs. Assistant history/layout/permissions, file-picker frecency/query history, and package receipts now use SQLite first with one-release legacy import/fallback paths.
- 2026-07-03: Drizzle-rs native SQLite proof passed on Windows using `drizzle` 0.1.12 with the `rusqlite` bundled-SQLite backend. The integration test opens temp-file databases, applies migrations, verifies WAL, performs insert/select/update/delete round trips for assistant threads, frecency, and package receipts through drizzle query builders, and exercises two concurrent writer connections using repository transactions.

## Executive Recommendation

Introduce a small SQLite-backed persistence layer for structured, multi-record state, but do not make `drizzle-rs` the default access layer yet. The strongest first migrations are assistant history/feedback/layout and file-picker frecency/query history. Package receipts are a good later candidate. User config, language/theme files, plugin metadata, package manifests/locks, registry TOMLs, logs, and user documents should stay as files.

SQLite with WAL would remove the torn-file/lost-update class of bugs for state that is currently spread across JSON files, one aggregate layout JSON file, TOML rule files, and LMDB stores. A small crate such as `helix-store` should own the database path, migrations, connection options, transactions, and typed repository APIs. Callers should not issue ad hoc SQL across the editor.

`drizzle-rs` is worth tracking or dogfooding behind an experiment, especially because the owner authored it, but it has adoption and compatibility risks for this editor today: it is pre-1.0, newly released, has low public download counts, requires Rust 1.95 and edition 2024 in its published crate metadata, its normal native SQLite backend uses `rusqlite` with bundled SQLite, and its WASM support appears targeted at Cloudflare D1/Durable Object environments rather than a generic embedded wasm32 SQLite store.

## Inventory

Format key:

- `SQLITE` means a good candidate for the proposed structured-state database.
- `FILE` means should remain a normal file or is not Helix-owned structured state.
- `DEFER` means a database could help eventually, but not before higher-value sites.

| Site | Data | Current storage | Frequency and access pattern | Concurrency | Query needs | Size/growth | Robustness | Classification |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Assistant history | Persisted assistant threads: entries, turns, plan/draft/context/follow-run state, unread flag, mode/config/profile, feedback, review mode, scope, view, terminals. See `helix-view/src/assistant/history/local.rs:14`, `:23`, `:27`, `:83`, `:136`. | SQLite in `data_dir()/double-helix/state.sqlite3`; legacy JSON under `cache_dir()/assistant/history/{id}.json` is imported once and used only as fallback if the store path fails. | Debounced saves, flush on close/shutdown. Listing queries SQLite by scope; fallback scans legacy `*.json`. | Store writes use SQLite transactions. Legacy fallback retains per-file atomic writes. | Strong. History browser, scope filtering, ratings/notes/feedback, future search and retention all want indexed records. | Grows with assistant usage; records may include terminal/session context and can become large. | SQLite WAL/busy-timeout/foreign-key setup plus import marker. Corrupt legacy records are skipped during fallback/import. | SQLITE, landed. |
| Assistant layout | Persisted layout state per scope. See `helix-view/src/assistant/layout.rs:20`, `:54`, `:89`, `:145`. | SQLite in `data_dir()/double-helix/state.sqlite3`; legacy aggregate JSON at `cache_dir()/assistant/layout.json` is imported once and used only as fallback if the store path fails. | Read/write by scope through SQLite; fallback read-modify-write uses the legacy JSON file. | Store writes use SQLite transactions. The legacy fallback keeps the static async mutex. | Low by itself; keyed by scope. | Small. | SQLite WAL/busy-timeout/foreign-key setup plus import marker. Legacy fallback writes atomically. | SQLITE, landed. |
| Assistant permissions | Agent/tool permission rules. See `helix-view/src/assistant/permission.rs:249`, `:261`, `:275`. | SQLite in `data_dir()/double-helix/state.sqlite3`; legacy TOML at `cache_dir()/assistant/permissions.toml` is imported once and used only as fallback if the store path fails. | Read at startup or permission manager creation; written when rules change/reset. | Store writes replace the rule set transactionally. Legacy fallback writes the TOML file directly. | Moderate. Matching scans rules in memory, but rules are naturally tabular by agent/tool/choice. | Small. | SQLite WAL/busy-timeout/foreign-key setup plus import marker. Decode failures in legacy fallback use default rules. | SQLITE, landed. |
| Assistant profiles/config | Agent/profile definitions and assistant config. See `helix-view/src/assistant/profile.rs:8`, `helix-term/src/config.rs:22`, `:157`. | User/workspace TOML config files. | Read and merged at startup/reload. | User/editor file ownership, not generated state. | Low. | Small. | Normal config parse/merge behavior. | FILE. Human-edited configuration should stay TOML. |
| Editor config | `config.toml`, workspace `.double-helix/config.toml`, keys/editor/pkg/icons config. See `helix-loader/src/lib.rs:123`, `:152`, `helix-term/src/config.rs:157`. | User/workspace TOML. | Read at startup/reload. | User controlled. | Low. | Small. | Normal file semantics. | FILE. |
| Languages config | Built-in and user `languages.toml`. See `helix-loader/src/config.rs:3`, `:12`, `:20`. | TOML files from runtime/config dirs. | Read during loader initialization. | User controlled. | Low. | Small to medium. | Normal file semantics. | FILE. |
| Themes/runtime files | Runtime theme/query/grammar files loaded from runtime/config dirs. | File tree under runtime and user config dirs. | Read-only inputs. | User/package controlled. | Low. | Bounded by installed runtime assets. | Normal file semantics. | FILE. |
| Package receipts | Installed package metadata: name, kind, version, source, archive hash, bins/shims, file list, installed_at, native-manager info. See `helix-pkg/src/store.rs:52`, `:76`, `:93`, `:205`, `:231`. | SQLite in `data_dir()/double-helix/state.sqlite3`; legacy file-per-package TOML under `data_dir()/double-helix/pkg/receipts/{kind}-{name}.toml` is imported once and used only as fallback if the store path fails. | Written on install/upgrade, queried for list/status, read individually by loader compatibility paths. | Per-package advisory lock exists for install staging; receipt writes use SQLite when available. | Moderate. Installed package listing, lookup by kind/name, upgrade queries, stale receipt cleanup. | Grows with installed packages; usually small. | SQLite WAL/busy-timeout/foreign-key setup plus `pkg-receipts-toml-v1` import marker. Decode errors in legacy fallback surface to callers. | SQLITE, landed. |
| Package install lock files | Per-package OS advisory lock files. See `helix-pkg/src/store.rs:173`. | `staging/{kind}-{name}.lock` retained on disk. | Created/opened during package operations. | Uses `fs4::FileExt::try_lock`; stale files are harmless. | None. | Tiny. | OS advisory lock, not state. | FILE. Keep as lock files unless the package manager is fully redesigned. |
| Package manifest and lock | `pkg.toml` manifest and `pkg.lock` package lock. See `helix-pkg/src/lock.rs:12`, `:25`, `:95`, `:108`. | TOML files, likely project/user visible. | Read/write during package resolution/install. | User/project file ownership. | Low to moderate, but reviewability matters more. | Small. | Direct writes today. | FILE. A project lock should remain diffable, committable, and reviewable even if receipts move to SQLite. |
| Package registry cache/specs | Built-in registry TOMLs and configured registry source dirs. See `helix-pkg/src/registry.rs:40`, `:69`, `helix-pkg/src/config.rs:48`, `:100`. | TOML package specs in registry directories/caches. | Read/merged when building registry, then queried in memory. | External source/cache ownership. | Search/filter happens in memory after load. | Depends on registry size. | File-tree cache semantics. | FILE. Treat as source/cache content, not local app state. |
| Package artifacts/shims/runtimes | Installed package dirs, generated command shims, Node/Python runtime dirs. See `helix-pkg/src/store.rs:33`, `:297`, `:330`. | Directories and executable/script files. | Created during install/activation. | Package operation locks. | None as structured data. | Potentially large. | Artifact semantics, not database rows. | FILE. |
| Plugin metadata | Plugin discovery metadata. See `helix-plugin/src/lua/loader.rs:61`, `:119`. | `config_dir()/plugins/*/plugin.toml` plus `init.lua`. | Read during plugin discovery/load. | User/plugin controlled. | Low. | Small. | Normal file semantics. | FILE. |
| Plugin config | Plugin host/config limits and per-plugin JSON config values. See `helix-plugin/src/types.rs:78`. | User TOML config, with `serde_json::Value` inside config structures. | Read from user config. | User controlled. | Low. | Small. | Config parse/merge behavior. | FILE. |
| Plugin RPC codec | Plugin protocol messages. See `helix-plugin-api/src/codec.rs`. | MessagePack via `rmp-serde`, but only over RPC/wire. | Per plugin message. | Runtime protocol. | Not on disk. | N/A. | N/A. | FILE/none. Not persistent storage. |
| File-picker frecency | Path hash to timestamp deque for file open/access scoring. See `helix-term/src/fff.rs:369`, `vendor/fff-search/src/frecency.rs:25`, `:60`, `:132`, `:344`. | SQLite in `cache_dir()/cache.sqlite3`; legacy LMDB via `heed`, under `cache_dir()/fff/{workspace_hash}/frecency`, is imported once when present. | Updated on file open/access, read while ranking files from an in-memory workspace index loaded from SQLite. | SQLite handles durable cache updates; in-memory index serves the hot ranking path. | Strong. Ranking scans/scores many candidate files. | Grows with visited files per workspace. | SQLite WAL/busy-timeout setup plus `fff-cache-v1` import marker. Failed legacy LMDB import starts empty because the cache is rebuildable. | SQLITE, landed. |
| File-picker query tracker | Query-to-opened-file associations and bounded file/grep query histories. See `helix-term/src/fff.rs:385`, `vendor/fff-search/src/query_tracker.rs:30`, `:69`, `:137`, `:190`, `:292`. | SQLite in `cache_dir()/cache.sqlite3`; legacy LMDB via `heed`, under `cache_dir()/fff/{workspace_hash}/queries`, is imported once when present. | Updated when a query completes/open succeeds; queried for query history and ranking hints. | SQLite handles durable cache updates. | Strong. Query history and query-file associations are indexed state. | Bounded query history, association table grows with usage. | SQLite WAL/busy-timeout setup plus `fff-cache-v1` import marker. Failed legacy LMDB import starts empty because the cache is rebuildable. | SQLITE, landed. |
| Command/search history | Prompt/search/register history behavior. See `helix-view/src/register.rs:28`, `:75`. | In-memory registers and prompt state. | Runtime only. | Single editor process memory. | Would be useful if persisted, but not today. | N/A. | Lost on exit by design/current implementation. | None today. If persistence is added, use SQLite or a small history file depending on feature scope. |
| Registers | Named registers and clipboard-backed registers. See `helix-view/src/register.rs:28`, `:98`. | In-memory plus system clipboard. | Runtime editing. | In-process. | None. | Small. | Not persistent. | None. |
| Jumplist/history state | Document history and jump list. See `helix-view/src/history_state.rs:5`. | In-memory `VecDeque`, capacity 30. | Runtime navigation. | In-process. | None today. | Small. | Not persistent. | None. Future session restore could use SQLite. |
| Document session/workspace state | Per-document savepoints, version, focused_at, open state. See `helix-view/src/session_state.rs:1`. | In-memory. | Runtime only. | In-process. | Future workspace restore might query by workspace/document. | N/A today. | Not persistent. | None today; future SQLITE candidate if workspace restore is added. |
| User document IO | User buffers and files. See `helix-view/src/document.rs:623`, `:757`, `:766`, `:1179`; assistant accepted edits in `helix-view/src/assistant/host.rs` and `helix-view/src/editor/assistant/effect.rs`. | User files in their own formats. | Normal editor read/write. | Editor/user/tool coordination. | Not app state. | Arbitrary. | Existing editor save semantics. | FILE. Never move user documents into the store. |
| Logs and traces | Application log, benchmark/event logs, tracing output. See `helix-loader/src/lib.rs:167`, `helix-term/src/main.rs`, `helix-view/src/bench.rs`. | Plain text log files, often append-only. | Continuous/append during runtime. | Logging framework/file append semantics. | Tail/grep by humans and tools. | Can grow. | Log-file semantics. | FILE. Architect decision: logs stay `.txt`/plain files, no DB. |

## SQLite Candidates vs Files

Strong SQLite candidates:

- Assistant history and assistant feedback/ratings/notes: multi-record, naturally indexed by id/scope/time/feedback, currently scanned from JSON files.
- Assistant layout: not large, but belongs in the same transaction boundary as assistant state once a store exists.
- File-picker frecency and query tracker: already database-shaped and query-heavy. Migrating from LMDB to SQLite reduces storage engines and can simplify Windows/WASM strategy, though it should be benchmarked.
- Package receipts: generated local state, lookup/list/query needs, and direct TOML writes today.
- Assistant permissions: borderline, but a rules table would make updates transactional and avoid direct TOML writes. Keep as TOML only if human editing is an explicit goal.

Files that should stay files:

- Logs: plain text by architectural decision.
- User config, workspace config, language config, themes, runtime files: human-edited and reviewable.
- Plugin metadata/config: human/plugin-authored files.
- Package manifests and project locks: `pkg.toml` and `pkg.lock` should remain diffable and committable. A generated local receipt DB can coexist with a TOML project lock.
- Package registry specs/caches: source-like TOML trees and external caches.
- User documents and generated package artifacts/shims: not structured application state.
- Advisory lock files: keep simple OS lock files unless the package manager locking model changes.

The repeated pattern is that generated, multi-record local state benefits from SQLite; human-owned source/config content should remain ordinary files.

## `drizzle-rs` Evaluation

### Verified facts

The crate exists as `drizzle` on crates.io and docs.rs:

- Crate page/API: <https://crates.io/crates/drizzle>, <https://crates.io/api/v1/crates/drizzle>
- Docs: <https://docs.rs/drizzle/latest/drizzle/>
- Feature list: <https://docs.rs/crate/drizzle/latest/features>
- Published `Cargo.toml`: <https://docs.rs/crate/drizzle/latest/source/Cargo.toml>
- GitHub: <https://github.com/themixednuts/drizzle-rs>

As of the crates.io API response checked on 2026-07-03:

- Latest/default stable version: `0.1.11`.
- License: MIT.
- Last release/update: 2026-06-30T18:35:33Z.
- All-time downloads for `drizzle`: 273; recent downloads: 150; `0.1.11` downloads: 18.
- Related `drizzle-sqlite` crate: version `0.1.11`, MIT, all-time downloads 1,255, recent downloads 322. API: <https://crates.io/api/v1/crates/drizzle-sqlite>
- Published metadata says Rust version `1.95` and edition `2024`.

The docs describe it as "A type-safe SQL query builder and ORM for Rust, inspired by Drizzle ORM." The GitHub README has the same positioning and warns that the project is still evolving and to expect breaking changes.

### What it is

`drizzle` is a typed query builder/ORM with derive macros for schemas and tables, plus migration support. It is not just a low-level SQLite wrapper. The documented quick start defines a table with `#[SQLiteTable]`, derives a schema with `#[derive(SQLiteSchema)]`, opens a driver connection, creates schema, inserts typed rows, and queries typed selections.

Documented backend support includes:

- SQLite: `rusqlite`, `libsql`, `turso`
- PostgreSQL: `postgres-sync`, `tokio-postgres`
- WASM/edge-oriented SQLite-like support through features such as `d1` and `durable`

### Backend, build, and dependency implications

The published `Cargo.toml` shows:

- Default feature is `std`, which includes `drizzle-migrations`.
- `rusqlite` support is optional and depends on `rusqlite >=0.36,<0.40` with the `bundled` feature enabled.
- `libsql`, `turso`, `tokio`, `wasm-bindgen`, `worker`, and related dependencies are optional features.
- Core normal dependencies include `drizzle-core`, `drizzle-macros`, `drizzle-types`, `const_format`, `paste`, and `smallvec`.

For native SQLite on Windows, the `rusqlite` backend should be straightforward for users, because bundled SQLite avoids requiring a system SQLite installation. The tradeoff is a C build of SQLite through the rusqlite/libsqlite3-sys stack. That is usually acceptable for native editor builds, but it is not dependency-light and it is not a wasm story.

The crate has a no-std/alloc-shaped core, but the practical embedded SQLite backend matters more than the core crate. The `rusqlite` backend does not compile to generic wasm32 local SQLite. The WASM features shown in docs are `d1` and `durable`, which pull `wasm-bindgen`, `js-sys`, and Cloudflare `worker`-style dependencies. That is useful for Cloudflare Worker storage targets, not for a generic embeddable editor/plugin runtime that wants the same local SQLite layer in native and wasm.

### API ergonomics

This is the documented usage pattern from docs.rs, adapted to the assistant-history shape. It uses the real public derive/query style shown by the crate; exact generated insert/select names would be verified during implementation.

```rust
use drizzle::sqlite::prelude::*;
use drizzle::sqlite::rusqlite::Drizzle;

#[SQLiteTable(name = "AssistantThreads")]
struct AssistantThread {
    #[column(primary)]
    id: String,
    scope: Option<String>,
    updated_at: i64,
    title: Option<String>,
    // First migration can keep the full record as JSON while extracting indexed columns.
    record_json: String,
}

#[derive(SQLiteSchema)]
struct Schema {
    assistant_thread: AssistantThread,
}

let conn = rusqlite::Connection::open(db_path)?;
let (db, Schema { assistant_thread, .. }) = Drizzle::new(conn, Schema::new());

db.create()?;

db.insert(assistant_thread)
    .values([InsertAssistantThread::new(
        thread_id,
        scope,
        updated_at,
        title,
        record_json,
    )])
    .execute()?;

let rows: Vec<SelectAssistantThread> = db
    .select(())
    .from(assistant_thread)
    .all()?;
```

The ergonomics are promising for schema-heavy code: table definitions live in Rust, inserts/selects are typed, and migrations are part of the crate family. The risk is that a macro-heavy ORM becomes part of editor hot paths and storage migrations before its public API has stabilized.

### Risks

- Maturity: `0.1.11`, recent first public activity, low public download counts, README warning about breaking changes.
- Bus factor: the owner authored it. That is good for responsiveness, but also means project/product risk is correlated.
- MSRV/toolchain: published crate metadata says Rust `1.95` and edition `2024`. This workspace is edition 2021; the bigger concern is whether the editor is willing to raise MSRV to the crate's required compiler.
- WASM: native `rusqlite` backend is not a generic wasm32 embedded SQLite backend. The provided wasm features appear Cloudflare-specific.
- Dependency/build weight: `rusqlite` with bundled SQLite brings C compilation; `libsql`/`turso` add their own stacks; derive macros add compile-time cost.
- Production readiness: no evidence yet of broad production use in editor-like hot paths or long-lived local migrations.

Verdict: do not choose `drizzle-rs` as the default storage layer for this migration unless the owner explicitly wants to accept the MSRV, maturity, and wasm tradeoffs. It is a reasonable experimental branch or optional backend for native-only builds.

## Recommended Architecture

Create a small persistence crate, tentatively `helix-store`, with these boundaries:

- Own database path resolution and versioning.
- Enable WAL and busy timeout at connection open.
- Expose typed repositories such as `assistant_threads`, `assistant_layouts`, `assistant_permissions`, `frecency`, `query_history`, and later `pkg_receipts`.
- Keep transactions inside the crate. Callers pass domain DTOs and receive domain DTOs.
- Keep source/config files out of the crate except for one-time imports.
- Separate durable state from rebuildable cache if necessary:
  - Durable DB: `data_dir()/double-helix/state.sqlite3` for assistant history, feedback, permissions, package receipts.
  - Optional cache DB: `cache_dir()/double-helix/cache.sqlite3` for frecency/query ranking state.

Recommended access layer today:

- Native first: use `rusqlite` directly behind `helix-store`. It is mature, synchronous, and maps well to current editor code. Decide explicitly whether to use bundled SQLite for clean Windows builds or system SQLite for smaller builds.
- WASM later: hide the backend behind a small trait so a future wasm plugin/embeddable target can use an appropriate storage implementation. Do not force `rusqlite` through wasm.
- Re-evaluate `drizzle-rs` after it has more usage, a stable migration story, a compatible MSRV, and a confirmed generic wasm target if that remains required.

Alternatives:

- `sqlx`: strong typed SQL and async ecosystem, but heavier, adds runtime/build-time complexity, and is not obviously a better fit for local editor state.
- `libsql`: useful if remote/Turso sync becomes a product goal, but heavier than needed for local state.
- `turso`: interesting because of pure-Rust SQLite direction, but should be proven on Windows, wasm, and editor workloads before adoption.
- Keep LMDB/heed for FFF: viable short term because it already works transactionally, but it leaves the workspace with JSON/TOML/LMDB storage diversity.

## Migration Plan

Wave 0: storage crate skeleton and decisions only.

- Add `helix-store` with path resolution, connection setup, WAL, busy timeout, schema version table, and migration runner.
- Define DTOs and repository APIs for assistant history/layout first.
- Add import/export tests using fixtures copied from existing JSON/TOML shapes.

Wave 1: assistant state.

- On first open, import `cache_dir()/assistant/history/*.json` and `assistant/layout.json` into SQLite inside one transaction.
- Leave old files in place initially, or rename them only after a successful import marker is committed.
- During a compatibility window, read SQLite first and fall back to JSON if no database/marker exists.
- Store full assistant thread JSON initially plus indexed columns: thread id, scope, title, created/updated timestamps, feedback/rating flags. This limits migration risk while enabling indexed history UI.
- Move assistant layout into a table keyed by scope. This removes the aggregate JSON read-modify-write path.

Wave 2: frecency and query tracker.

- Add SQLite tables for workspace-scoped frecency timestamps and query associations/history.
- Import LMDB data when present. If import fails, treat it as rebuildable cache and start empty.
- Benchmark ranking and update paths before deleting the LMDB implementation.

Wave 3: package receipts.

- Import TOML receipts into a `pkg_receipts` table.
- Keep `pkg.toml`, `pkg.lock`, registry specs, artifacts, and lock files as files.
- Keep a compatibility path for loader code that currently reads grammar receipts from TOML, or update that code to go through `helix-store`.

Wave 4: optional permissions.

- Move generated assistant permission rules into the DB if the product wants transactional rule updates and no human editing.
- Otherwise keep `permissions.toml`, but make writes atomic using the existing atomic writer.

Rollback and compatibility:

- All imports should be one-time, idempotent, and transactional.
- Preserve old files through at least one release. Do not delete JSON/TOML/LMDB sources immediately after import.
- Add export/debug tooling or a documented schema so users are not locked out of their data.
- Keep logs as plain text and user config/project lock files as TOML throughout.

## Bottom Line

SQLite is the right direction for generated structured state, especially assistant history/feedback/layout and file-picker frecency/query history. It addresses the same concurrency and torn-write class of issue that was just fixed manually for assistant layout, while also enabling indexed history and ranking queries. The conservative implementation is a small `helix-store` crate on direct `rusqlite` for native builds, with a backend boundary for future wasm. `drizzle-rs` is promising but should not be the default dependency yet because its maturity, MSRV, bundled native SQLite backend, and Cloudflare-oriented WASM story do not line up cleanly with the editor's current portability goals.
