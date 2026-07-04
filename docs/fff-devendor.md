# FFF De-Vendor Research

Date: 2026-07-03

Decision marker: `FFF_DEVENDOR_STRATEGY_C`

## 2026-07-03 Rebase Status

The vendored crate remains in place, but its base has been rebased from
published `fff-search` `0.6.4` to stable `0.9.6`.

The three local blockers remain local extensions on top of the 0.9.6 source:

- `FFF_SCAN_OPTIONS_BLOCKER`: `FilePickerScanOptions` and `FilePickerOptions::scan`
  are reapplied through the 0.9.6 `scan.rs`/`FileSync::walk_filesystem` path.
- `FFF_UNSAVED_BUFFER_GREP_BLOCKER`: `ContentOverlay`, owned grep results,
  `FilePicker::grep_owned`, and `grep_bytes` are reapplied on top of 0.9.6 grep.
- `FFF_STORAGE_TRAITS_BLOCKER`: upstream `heed`/LMDB storage was stripped again;
  `FrecencyStore` and `QueryTrackerStore` remain the persistence boundary, with
  `helix-term` providing the SQLite-backed `helix-store` implementations.

Latest stable checked: `0.9.6`.
Latest nightly checked: `0.9.7-nightly.1cd8d31` (crates.io, 2026-07-02).

## Decision

Do not de-vendor `vendor/fff-search` in this pass.

The current published `fff-search` crate cannot preserve Helix's patched feature
contract through public APIs. In particular, published upstream does not expose
the unsaved-buffer grep overlay API, does not expose Helix-equivalent scan
configuration, and no longer exposes pluggable frecency/query storage traits.
The lower-level matching engine is published, but using it directly would mean
rebuilding a substantial file-picker, grep, watcher, and ranking layer in
`helix-term`, which is not a clean dependency swap.

Recommendation: keep the vendored crate for now. If the owner wants to track
new upstream releases, either update the vendored patch to the newest upstream
and reapply the local extension contract, or upstream the missing public APIs
before removing the vendor copy.

## Published Crates

`fff-core` is not published on crates.io. `cargo info fff-core` against the
registry returned no crate.

`fff-search` is published:

- Latest stable found: `0.9.6`
- Latest overall/nightly found: `0.9.7-nightly.fce72fa`
- Edition: `2024`
- License: `MIT`
- MSRV/rust-version: not declared in published metadata
- Links:
  - https://crates.io/crates/fff-search
  - https://docs.rs/fff-search/0.9.6/fff_search/
  - https://docs.rs/crate/fff-search/0.9.6/source/Cargo.toml.orig
  - https://docs.rs/fff-search/latest/src/fff_search/file_picker.rs.html

`fff-grep` and `fff-query-parser` are separately published support crates.
`fff-search` depends on them, but they do not provide the complete public
file-picker API Helix currently consumes.

The upstream repository is now `dmtrKovalenko/fff` rather than only
`dmtrKovalenko/fff.nvim`; its README describes the Rust core under
`crates/fff-search`, `crates/fff-grep`, and `crates/fff-query-parser`.

## Public API Check

### Scan Options

Marker: `FFF_SCAN_OPTIONS_BLOCKER`

Helix's vendored extension exposes:

- `FilePickerScanOptions`
- `FilePickerOptions::scan`
- fields for hidden files, parent ignore files, `.ignore`, git ignore/exclude/global, following symlinks, max depth, custom ignore files, and symlink deduplication

Published `fff-search` `0.9.6` and `0.9.7-nightly.fce72fa` do not expose
`FilePickerScanOptions`. `FilePickerOptions` exposes only higher-level controls
such as `base_path`, cache/indexing flags, `watch`, `follow_symlinks`,
`enable_fs_root_scanning`, and `enable_home_dir_scanning`.

The internal walker still hard-codes ignore behavior in `FileSync::walk_filesystem`:
`.hidden(!is_git_repo)`, `.git_ignore(true)`, `.git_exclude(true)`,
`.git_global(true)`, `.ignore(true)`, and `.follow_links(follow_symlinks)`.
There is no public max-depth or custom-ignore hook equivalent to Helix's current
file-picker config.

### Unsaved-Buffer Grep

Marker: `FFF_UNSAVED_BUFFER_GREP_BLOCKER`

Helix's vendored extension exposes:

- `ContentOverlay`
- `OwnedGrepMatch`
- `OwnedGrepResult`
- `FilePicker::grep_owned`
- `grep_bytes`

Published `fff-search` `0.9.6` and `0.9.7-nightly.fce72fa` do not contain those
symbols. The public grep API exposes `GrepMode`, `GrepMatch`, `GrepResult`, and
`GrepSearchOptions`, but it searches indexed files via `FilePicker::grep` and
does not provide a public overlay/byte-slice path that can merge unsaved editor
buffers with saved-file results.

This is the hard blocker. Helix's grep must search unsaved buffers without
writing them to disk.

### Frecency and Query Storage

Marker: `FFF_STORAGE_TRAITS_BLOCKER`

Helix's vendored extension exposes storage abstraction traits:

- `FrecencyStore`
- `QueryTrackerStore`

Published `fff-search` no longer exposes these traits. It provides LMDB-backed
trackers directly via `FrecencyTracker::open(db_path)` and
`QueryTracker::open(db_path)`, using `heed` internally. That conflicts with the
current Helix direction where persistent storage lives in `helix-store` SQLite
and the finder crate stays storage-agnostic.

## Engine Crate

Marker: `FFF_ENGINE_NEO_FRIZBEE`

The lower-level fuzzy matching engine is `neo_frizbee`.

- Latest found: `0.10.3`
- Edition: `2024`
- License: `MIT`
- MSRV/rust-version: not declared in published metadata
- Repository: https://github.com/saghen/frizbee
- Links:
  - https://crates.io/crates/neo_frizbee
  - https://docs.rs/neo_frizbee/0.10.3/neo_frizbee/

`fff-search` depends on `neo_frizbee`, and upstream docs describe path search as
using the frizbee-derived core. The crate is separately usable for SIMD
Smith-Waterman fuzzy matching over byte strings.

This does not by itself replace `fff-search`. It provides matching primitives,
not Helix's complete file index, ignore semantics, background watcher,
frecency/query ranking integration, or grep-over-unsaved-buffer behavior.

## Strategy Evaluation

### A. Depend on published `fff-search`

Rejected.

The public crate is missing all three Helix extension surfaces:

- no Helix-equivalent scan options
- no unsaved-buffer grep overlay / byte-slice API
- no pluggable storage traits

Using it directly would regress file-picker semantics, unsaved-buffer grep, and
SQLite-backed frecency/query tracking.

### B. Depend on `neo_frizbee` directly

Rejected for this pass.

This is technically possible only as a larger rewrite. We would need to build or
port a Helix-owned file picker around `ignore`, `notify`, `neo_frizbee`,
`fff-grep` or `grep-searcher`, and `helix-store`, including:

- path indexing and path-order fallback
- fuzzy ranking compatible enough with current behavior
- frecency and query-combo scoring
- saved-file grep and unsaved-buffer grep merging
- filesystem watcher behavior
- git status handling and binary/large-file filtering

That is not a clean dependency swap and has a high regression surface.

### C. Stop and report

Selected.

Keep `vendor/fff-search` until either upstream exposes the needed public APIs or
the project intentionally funds a Helix-owned replacement layer. The
pre-existing vendored `score::filename_bonus_tests` failure becomes moot only
after a real de-vendor; it is not addressed by this stop decision.

## Sources

- crates.io `fff-search`: https://crates.io/crates/fff-search
- docs.rs `fff-search` 0.9.6 crate docs: https://docs.rs/fff-search/0.9.6/fff_search/
- docs.rs `fff-search` 0.9.6 Cargo metadata: https://docs.rs/crate/fff-search/0.9.6/source/Cargo.toml.orig
- docs.rs `fff-search` latest file picker source: https://docs.rs/fff-search/latest/src/fff_search/file_picker.rs.html
- docs.rs `fff-search` latest grep source: https://docs.rs/fff-search/latest/src/fff_search/grep.rs.html
- upstream repository README: https://github.com/dmtrKovalenko/fff
- crates.io `neo_frizbee`: https://crates.io/crates/neo_frizbee
- docs.rs `neo_frizbee` 0.10.3: https://docs.rs/neo_frizbee/0.10.3/neo_frizbee/
