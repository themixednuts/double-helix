# FFF Upstream Tracking

This directory vendors `fff-search`, the search crate from
`https://github.com/dmtrKovalenko/fff`.

## Current Mapping

- Upstream repo: `dmtrKovalenko/fff`
- Upstream crate path: `crates/fff-core` (published as crate `fff-search`)
- Vendored path: `vendor/fff-search`
- Current vendored crate version: `0.11.0` (upstream commit `95fd777c`)
- Previous vendored crate version: `0.9.6`
- Latest nightly listed: `0.11.1-nightly.e3f694a` (crates.io, 2026-09-21)

## Local Extension Contract

The vendored copy is not a pure upstream snapshot. Double Helix relies on these
local extension points. Each one is tagged with a grep-able marker:

- `FFF_SCAN_OPTIONS_BLOCKER`: `FilePickerScanOptions`, `SymlinkTargetScope`
  and `FilePickerOptions::scan` (`src/scan_options.rs`) keep Helix's
  ignore/hidden/depth/link semantics. They are threaded into
  `walk::walk_collect_files` and the watcher's `IgnoreFilter`.
- `FFF_UNSAVED_BUFFER_GREP_BLOCKER`: `ContentOverlay`, `OwnedGrepMatch`,
  `OwnedGrepResult`, `FilePicker::grep_owned`, `grep_bytes` and
  `grep_byte_sources_page` (`src/grep/overlay.rs`) search unsaved editor
  buffers without writing them to disk.
- `FFF_STORAGE_TRAITS_BLOCKER`: `FrecencyStore` and `QueryTrackerStore` keep
  fff-search storage-agnostic, and `helix-term` supplies the SQLite-backed
  `helix-store` implementations. Do not add `heed`/LMDB back to
  `vendor/fff-search`. Upstream's `dbs/lmdb.rs`, `dbs/env_pool.rs` and the LMDB
  tests are dropped, and `SharedDb` is sealed over the crate-private
  `SharedStore` trait instead of `LmdbStore`.
- Per-query `FuzzySearchOptions::abort_signal` (`ScoringContext::abort_signal`
  in `score.rs`) cancels superseded UI queries without stopping the shared
  picker or its watcher.

A few upstream tests also carry small local fixes: an explicit `scan.hidden`
where they assume upstream's hidden-file policy, a post-scan wait in two racy
binary-classification tests, and a writable handle for a Windows
`set_modified` call. `docs/fff-devendor.md` lists them.

If upstream gains equivalent APIs, delete the local extension and migrate the
adapter. If it does not, keep the patch small and isolated so it can be
rebased cleanly.

The vendored `Cargo.toml` also differs from the published manifest: the `heed`
dependency and LMDB test targets are removed, and a `[workspace]` table is
added so the crate can be tested on its own
(`cargo test --manifest-path vendor/fff-search/Cargo.toml`).

## Drift Check

Run:

```sh
cargo xtask fff-upstream
```

To inspect a published nightly tag:

```sh
cargo xtask fff-upstream --ref 0.11.1-nightly.e3f694a
```

For a scheduled/manual gate that fails when upstream drift exists:

```sh
cargo xtask fff-upstream --fail-on-drift
```

The check compares `vendor/fff-search/src` against upstream `crates/fff-core/src`
and reports which local extension symbols are absent upstream.

## Update Procedure

1. Run `cargo xtask fff-upstream --ref <tag-or-main>` and review the source drift.
2. Diff the pristine published crate for the current base against
   `vendor/fff-search` to recover the full local patch. Then copy the new
   upstream crate into `vendor/fff-search`.
3. Reapply only the local extension contract above, or replace it with upstream equivalents.
4. Run `cargo test --manifest-path vendor/fff-search/Cargo.toml`.
5. Run `cargo check -p helix-term --bin dhx --manifest-path E:/helix/helix-fork/Cargo.toml`.
6. Run `cargo test -p helix-term --manifest-path E:/helix/helix-fork/Cargo.toml --lib fff`.
7. Run `cargo clippy -p helix-term --lib --manifest-path E:/helix/helix-fork/Cargo.toml -- -D warnings`.
