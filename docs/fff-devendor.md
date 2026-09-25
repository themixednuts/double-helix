# FFF De-Vendor Research

Date: 2026-09-24

Decision marker: `FFF_DEVENDOR_STRATEGY_C`

## 2026-09-24 Rebase Status

The vendored crate remains in place. Its base moved from published
`fff-search` `0.9.6` to stable `0.11.0` (crates.io, 2026-09-21, upstream
`dmtrKovalenko/fff` commit `95fd777c`).

All local extensions are still needed on top of 0.11.0. Where possible they now
sit in their own modules, which keeps future rebases small:

- `FFF_SCAN_OPTIONS_BLOCKER`: `FilePickerScanOptions`, `SymlinkTargetScope`
  and `FilePickerOptions::scan` now live in `src/scan_options.rs`, re-exported
  from `file_picker`. Upstream 0.11.0 moved filesystem walking into
  `walk::walk_collect_files` (ripgrep and zlob backends), so the options are
  threaded there. They also reach the watcher's `IgnoreFilter` and the new
  recursive `index_new_directory` walk. `max_depth` and symlink scope stay
  relative to the picker base when the watcher walks a new subdirectory.
- `FFF_UNSAVED_BUFFER_GREP_BLOCKER`: `ContentOverlay`, `OwnedGrepMatch`,
  `OwnedGrepResult`, `FilePicker::grep_owned`, `grep_bytes` and the
  `grep_byte_sources_page` / `ByteSourceGrep*` paging API now live in
  `src/grep/overlay.rs`. Upstream split `grep.rs` into a `grep/` module. The
  overlay code reuses its matcher and sink types (`NeedleFinder`,
  `PlainTextMatcher`, `PlainTextSink`, `RegexSink`), which were made
  `pub(super)` for this.
- `FFF_STORAGE_TRAITS_BLOCKER`: upstream heed/LMDB (`dbs/lmdb.rs`,
  `dbs/env_pool.rs`, the heed error variants and the LMDB-only tests) was
  stripped again. `FrecencyStore` and `QueryTrackerStore` are still the
  persistence boundary. Upstream 0.11.0 turned `SharedFrecency` and
  `SharedQueryTracker` into aliases of a generic `SharedDb<T: LmdbStore>`. The
  vendored copy keeps that shape but seals it over a crate-private
  `SharedStore` marker trait. Upstream's frecency changes were ported onto the
  trait-backed tracker: Windows-canonical path hashing and `copy_history`,
  which the watcher uses to carry frecency across renames. One local
  refinement: on Windows a path that no longer exists is keyed by its
  canonical parent plus file name, not the raw string. Without it, a rename
  source reported with `/` separators hashed differently from its tracked
  history, and the history was not carried to the new path (upstream's
  `rescan_tests::renaming_a_file_carries_frecency_and_keeps_the_old_entry`
  fails on Windows without this).
- Per-query `FuzzySearchOptions::abort_signal` (`ScoringContext::abort_signal`)
  is still reapplied in `score.rs`. Upstream now resolves candidates through
  frizbee's index-based resolver.

These local hunks from 0.9.6 are gone because upstream now covers them:

- Windows `/`-canonical index paths (`normalize_index_relative_path`).
  Upstream 0.11.0 stores relative paths with `/` on every platform
  (`path_utils::to_canonical_slashes`) and converts them back to native
  separators in `write_absolute_path`.
- The grep time budget firing before any match. Upstream added
  `GrepSearchOptions::enforce_time_budget`. `helix-term` and `helix-workspace`
  now set it next to `time_budget_ms: 40` to keep Helix's paging behavior.
- git2 0.21 `StatusEntry::path()` returning a `Result`. This is upstream now.

Behavior that comes with 0.11.0 and was kept as upstream intends:

- The git recency ranking boost (`GitRecencyConfig`, on by default, last 10
  non-merge commits) is enabled. Callers pass `GitRecencyConfig::default()`
  explicitly.
- The directory index now includes empty and pure-ancestor dirs, and dirs
  added by the watcher.
- Watcher changes: rename detection, a throttle on full rescans, and a
  serialized git-status worker thread.

Local test-only changes in the vendored suite:

- `rescan_tests::Fixture::index` and `tests/dotdir_glob_constraint_test.rs`
  set `scan.hidden` explicitly. The vendored `FilePickerScanOptions` default
  hides dot entries everywhere. These tests assume upstream's policy of walking
  dot entries inside git repos.
- `grep_integration`'s two large-binary classification tests now wait for
  `!is_post_scan_active()`. Upstream stops waiting once the bigram index is
  published, but the binary sniff runs after that, so the tests are racy. They
  also flake against pristine 0.11.0 on Windows.
- `rescan_tests::editing_an_indexed_file_is_never_mistaken_for_a_rename` opens
  its file writable before `set_modified`. Windows rejects the call on a
  read-only handle, so this test also fails against pristine 0.11.0.

Latest stable checked: `0.11.0`.
Latest nightly listed: `0.11.1-nightly.e3f694a` (crates.io, 2026-09-21). Its
public API was not audited.

### 2026-07-03 Rebase Status (previous)

The base moved from `0.6.4` to `0.9.6`, with the same three blockers reapplied
through the 0.9.6 `scan.rs`/`FileSync::walk_filesystem` path and the monolithic
`grep.rs`.

## Decision

Do not de-vendor `vendor/fff-search` in this pass.

The published `fff-search` crate still cannot provide Helix's patched feature
contract through its public API. Published 0.11.0 has no unsaved-buffer grep
overlay API and no Helix-equivalent scan configuration. Its storage is also
more closed than in 0.9.6: the frecency and query trackers are LMDB-only
behind a sealed, crate-private store trait. The lower-level matching engine is
published, but using it directly would mean rebuilding a large file-picker,
grep, watcher and ranking layer in `helix-term`. That is not a clean
dependency swap.

Recommendation: keep the vendored crate for now. To track new upstream
releases, either rebase the vendored copy onto the newest upstream and reapply
the local extension contract, or upstream the missing public APIs before
removing the vendored copy.

## Published Crates

`fff-core` is not published on crates.io (the registry API returns 404 for it
as of 2026-09-24).

`fff-search` is published:

- Latest stable found: `0.11.0`
- Latest overall/nightly found: `0.11.1-nightly.e3f694a`
- Edition: `2024`
- License: `MIT`
- MSRV/rust-version: not declared in the published metadata
- Default feature: `ripgrep` (the pure-Rust `ignore`/`globset` walker). The
  optional `zlob` backend needs a Zig toolchain. Helix builds the default.
- Links:
  - https://crates.io/crates/fff-search
  - https://docs.rs/fff-search/0.11.0/fff_search/
  - https://docs.rs/crate/fff-search/0.11.0/source/Cargo.toml.orig

`fff-grep` (`0.11.0`) and `fff-query-parser` (`0.11.0`) are published support
crates. `fff-search` depends on them, but they do not provide the complete
public file-picker API that Helix uses.

The upstream repository is `dmtrKovalenko/fff`. Its README places the Rust core
under `crates/fff-core` (published as `fff-search`), `crates/fff-grep` and
`crates/fff-query-parser`.

## Public API Check (0.11.0)

### Scan Options

Marker: `FFF_SCAN_OPTIONS_BLOCKER`. Still a blocker.

Helix's vendored extension exposes:

- `FilePickerScanOptions` and `SymlinkTargetScope`
- `FilePickerOptions::scan`
- fields for hidden files, parent ignore files, `.ignore`, git
  ignore/exclude/global, following symlinks, max depth, custom ignore files,
  symlink deduplication and symlink target scope

Published 0.11.0 does not expose any of these. `FilePickerOptions` adds only
`git_recency` to the 0.9.6 set (`base_path`, cache and indexing flags, `watch`,
`follow_symlinks`, `enable_fs_root_scanning`, `enable_home_dir_scanning`).

The ripgrep walker (`walk/ripgrep.rs`) still hard-codes `.hidden(!is_git_repo)`,
`.git_ignore(true)`, `.git_exclude(true)`, `.git_global(true)`, `.ignore(true)`
and `.follow_links(follow_symlinks)`. It has no max-depth, custom-ignore or
entry-filter hook.

### Unsaved-Buffer Grep

Marker: `FFF_UNSAVED_BUFFER_GREP_BLOCKER`. Still a blocker, and the hard one.

Helix's vendored extension exposes:

- `ContentOverlay`
- `OwnedGrepMatch`
- `OwnedGrepResult`
- `FilePicker::grep_owned`
- `grep_bytes`
- `grep_byte_sources_page`, `ByteSourceGrepCursor`, `ByteSourceGrepMatch`,
  `ByteSourceGrepPage` and `ByteSourceGrepError`

Published 0.11.0 contains none of these. Its public grep API is `GrepMode`,
`Casing`, `GrepMatch`, `GrepResult`, `GrepSearchOptions`, `parse_grep_query`
and `has_regex_metacharacters`, plus `FilePicker::grep` and
`FilePicker::multi_grep` over indexed files. The byte-slice search engine
(`grep_search`, the matcher and sink types) is `pub(crate)` or private. There
is no public path that searches in-memory bytes, so unsaved editor buffers
cannot be merged with saved-file results.

Helix's grep must search unsaved buffers without writing them to disk.

### Frecency and Query Storage

Marker: `FFF_STORAGE_TRAITS_BLOCKER`. Still a blocker, and more closed than in
0.9.6.

Helix's vendored extension exposes storage traits:

- `FrecencyStore`
- `QueryTrackerStore`

Published 0.11.0 does not expose them. `FrecencyTracker::open(db_path)` and
`QueryTracker::open(db_path)` are LMDB-backed through `heed`, with a
process-wide LMDB environment pool. `SharedFrecency` and `SharedQueryTracker`
are now `SharedDb<T>` aliases sealed over the crate-private `LmdbStore` trait,
so callers cannot plug in another backend. This conflicts with Helix keeping
persistent storage in `helix-store` SQLite and keeping the finder crate
storage-agnostic.

### Per-Query Fuzzy Cancellation

Not a named blocker marker, but a local extension listed in
`vendor/fff-search/UPSTREAM.md`.

Published 0.11.0 `FuzzySearchOptions` has no `abort_signal`.
`SharedFilePicker::cancel` exists, but it cancels the picker's background work,
not one superseded query. `GrepSearchOptions::abort_signal` covers grep only.

## Engine Crate

Marker: `FFF_ENGINE_NEO_FRIZBEE`

The lower-level fuzzy matching engine is `neo_frizbee`.

- Latest found: `0.13.3` (used by `fff-search` 0.11.0)
- Edition: `2024`
- License: `MIT`
- MSRV/rust-version: not declared in the published metadata
- Repository: https://github.com/saghen/frizbee
- Links:
  - https://crates.io/crates/neo_frizbee
  - https://docs.rs/neo_frizbee/0.13.3/neo_frizbee/

`fff-search` depends on `neo_frizbee`, and the upstream docs describe path
search as using the frizbee-derived core. The crate can be used on its own for
SIMD Smith-Waterman fuzzy matching over byte strings.

It does not replace `fff-search` by itself. It provides matching primitives,
not Helix's file index, ignore semantics, background watcher,
frecency/query ranking integration or grep over unsaved buffers.

## Strategy Evaluation

### A. Depend on published `fff-search`

Rejected.

The public crate is still missing all of the Helix extension surfaces:

- no Helix-equivalent scan options
- no unsaved-buffer grep overlay or byte-slice API
- no pluggable storage traits (LMDB-only and sealed as of 0.11.0)
- no per-query fuzzy cancellation

Using it directly would regress file-picker semantics, unsaved-buffer grep and
SQLite-backed frecency/query tracking.

### B. Depend on `neo_frizbee` directly

Rejected for this pass.

This is only possible as a larger rewrite. We would need to build or port a
Helix-owned file picker around `ignore`, `notify`, `neo_frizbee`, `fff-grep` or
`grep-searcher`, and `helix-store`, including:

- path indexing and path-order fallback
- fuzzy ranking close enough to the current behavior
- frecency and query-combo scoring
- saved-file grep and unsaved-buffer grep merging
- filesystem watcher behavior
- git status handling and binary/large-file filtering

That is not a clean dependency swap, and the regression risk is high.

### C. Stop and report

Selected.

Keep `vendor/fff-search` until upstream exposes the needed public APIs or the
project decides to fund a Helix-owned replacement layer.

## Sources

- crates.io `fff-search`: https://crates.io/crates/fff-search
- crates.io `fff-search` 0.11.0 crate archive: https://static.crates.io/crates/fff-search/fff-search-0.11.0.crate
- docs.rs `fff-search` 0.11.0 crate docs: https://docs.rs/fff-search/0.11.0/fff_search/
- docs.rs `fff-search` 0.11.0 Cargo metadata: https://docs.rs/crate/fff-search/0.11.0/source/Cargo.toml.orig
- upstream repository README: https://github.com/dmtrKovalenko/fff
- crates.io `neo_frizbee`: https://crates.io/crates/neo_frizbee
- docs.rs `neo_frizbee` 0.13.3: https://docs.rs/neo_frizbee/0.13.3/neo_frizbee/
