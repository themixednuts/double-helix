# Performance Notes

Date: 2026-07-02

## Methodology

Benchmarks used the existing headless harness:

- `cargo run -p helix-term --bin hx-bench --features "integration bench" --release -- --fixture giant-lines-render --lines 100 --bytes-per-line 18500 --renders 8 --width 120 --height 50`
- `cargo run -p helix-term --bin hx-bench --features "integration bench" --release -- --fixture document-change-fanout --lines 2000 --renders 6 --width 120 --height 50`

Initial baseline used the workspace `target` on `E:`. During verification `E:` reached 0 free bytes, so A/B checks used `CARGO_TARGET_DIR=C:\Users\jonfo\AppData\Local\Temp\helix-fork-target-perf`.

## Baseline

| Fixture | Metric | Baseline |
| --- | ---: | ---: |
| giant-lines-render | first render | 9455 us |
| giant-lines-render | cached renders avg, renders 2-8 | 583 us |
| giant-lines-render | `render_document/main_loop` | 3339 us |
| document-change-fanout | pre-mutation first render | 14677 us |
| document-change-fanout | pre-mutation cached avg, renders 2-6 | 1068 us |
| document-change-fanout | mutation | 1044 us |
| document-change-fanout | post-mutation first render | 13744 us |
| document-change-fanout | post-mutation cached avg, renders 2-6 | 1050 us |

## Candidate Results

| Candidate | Fixture/metric | Before | After | Delta | Decision |
| --- | ---: | ---: | ---: | ---: | --- |
| Pre-size document line-map and live syntax-style vectors | fanout pre first render | 13420 us | 18809 us | +40.2% | Reverted |
| Pre-size document line-map and live syntax-style vectors | fanout post first render | 11717 us | 16219 us | +38.4% | Reverted |
| Reuse `CacheStore::compose_batch` pending/output buffers | giant cached avg | 976 us | 668 us | -31.6% | Reverted: mixed |
| Reuse `CacheStore::compose_batch` pending/output buffers | fanout pre first render | 16037 us | 13420 us | -16.3% | Reverted: mixed |
| Reuse `CacheStore::compose_batch` pending/output buffers | fanout post first render | 10988 us | 11717 us | +6.6% | Reverted |
| Single-item `compose_batch` fast path | fanout pre first render | 16037 us | 16154 us | +0.7% | Reverted |
| Single-item `compose_batch` fast path | fanout post first render | 10988 us | 12857 us | +17.0% | Reverted |

No code optimization was kept. The measured allocation-removal candidates did not produce a consistent win across relevant render fixtures, and the regressions were above the 2% cutoff.

## Notes

- `cargo test -p helix-term render::tests:: --features "integration bench"` passed with the alternate `CARGO_TARGET_DIR`.
- Default-target verification was blocked because `E:` had 0 free bytes; `target/debug` was about 47 GB.
- Existing warnings observed before and after the pass: unused `BenchState` imports in `helix-term/src/commands/typed.rs` and `helix-term/src/bin/hx_bench.rs`.

## Future Candidates

- Move `VisualLineInfo::horizontal_checkpoints` from `Vec` to a small inline collection if `helix-view` ownership allows it; current rendering allocates tiny checkpoint vectors per visible line.
- Add an allocation counter or Criterion micro-bench for `CacheStore::compose_batch` with one-view and multi-view statusline batches before retrying buffer reuse.
- Audit `TextRenderer` whitespace marker storage; it still builds several tiny `String`s per full document render, but needs isolated measurement before changing representation.

## Round 2: Allocation-count-driven retry

Date: 2026-07-02

### Methodology

Added a `hx-bench` global allocator wrapper that reports per-phase:

- `alloc_calls`
- `alloc_bytes`
- `realloc_calls`
- `realloc_bytes`
- `dealloc_calls`
- `dealloc_bytes`

Measurements used the same fixtures as Round 1, with `HELIX_LOG_LEVEL=error` to keep integration INFO log formatting out of the measured render phases:

- `cargo run --quiet -p helix-term --bin hx-bench --features "integration bench" --release -- --fixture giant-lines-render --lines 100 --bytes-per-line 18500 --renders 8 --width 120 --height 50`
- `cargo run --quiet -p helix-term --bin hx-bench --features "integration bench" --release -- --fixture document-change-fanout --lines 2000 --renders 6 --width 120 --height 50`

Primary metric below is median `alloc_calls` across 3 runs. Cached phases use the per-run average of cached renders, then the median of those 3 averages. Wall time is the same median protocol and is used only as the regression guard. The measured `app.render_timed()` phase still includes cursor/flush plumbing, so allocation counts were much steadier with INFO logs disabled but not perfectly identical across runs.

### Baseline Allocation Counts

| Fixture | Phase | alloc_calls | alloc_bytes | realloc_calls | wall median |
| --- | ---: | ---: | ---: | ---: | ---: |
| giant-lines-render | first render | 987 | 1,289,299 | 326 | 20,050 us |
| giant-lines-render | cached avg, renders 2-8 | 249.6 | 42,330 | 6.6 | 1,083 us |
| document-change-fanout | pre-mutation first render | 2,885 | 1,663,674 | 678 | 15,279 us |
| document-change-fanout | pre-mutation cached avg, renders 2-6 | 271.2 | 55,477 | 22.8 | 1,084 us |
| document-change-fanout | mutation | 68 | 78,328 | 5 | 1,299 us |
| document-change-fanout | post-mutation first render | 3,691 | 1,602,085 | 674 | 14,907 us |
| document-change-fanout | post-mutation cached avg, renders 2-6 | 261.4 | 41,509 | 21.2 | 766 us |

### Candidate Results

| Candidate | Fixture/phase | alloc_calls before | alloc_calls after | Alloc delta | wall before | wall after | Decision |
| --- | --- | ---: | ---: | ---: | ---: | ---: | --- |
| `VisualLineInfo::horizontal_checkpoints` `Vec` -> `SmallVec<[_; 4]>` | giant first render | 987 | 735 | -25.5% | 20,050 us | 40,954 us | Reverted: wall regression |
| `VisualLineInfo::horizontal_checkpoints` `Vec` -> `SmallVec<[_; 4]>` | giant cached avg | 249.6 | 231.0 | -7.5% | 1,083 us | 1,315 us | Reverted: wall regression |
| `VisualLineInfo::horizontal_checkpoints` `Vec` -> `SmallVec<[_; 4]>` | fanout pre first render | 2,885 | 2,870 | -0.5% | 15,279 us | 18,907 us | Reverted: wall regression |
| `VisualLineInfo::horizontal_checkpoints` `Vec` -> `SmallVec<[_; 4]>` | fanout post first render | 3,691 | 3,491 | -5.4% | 14,907 us | 16,488 us | Reverted: wall regression |
| Reuse `CacheStore::compose_batch` pending/output buffers | giant first render | 987 | 1,018 | +3.1% | 20,050 us | 11,274 us | Reverted: allocation regression |
| Reuse `CacheStore::compose_batch` pending/output buffers | giant cached avg | 249.6 | 254.6 | +2.0% | 1,083 us | 916 us | Reverted: allocation regression |
| Reuse `CacheStore::compose_batch` pending/output buffers | fanout pre cached avg | 271.2 | 246.4 | -9.1% | 1,084 us | 704 us | Reverted: giant regression |
| Reuse `CacheStore::compose_batch` pending/output buffers | fanout post first render | 3,691 | 3,484 | -5.6% | 14,907 us | 12,055 us | Reverted: giant regression |
| `TextRenderer` whitespace marker `String`s -> borrowed/static defaults | giant first render | 987 | 1,038 | +5.2% | 20,050 us | 12,184 us | Reverted: allocation regression |
| `TextRenderer` whitespace marker `String`s -> borrowed/static defaults | giant cached avg | 249.6 | 248.9 | -0.3% | 1,083 us | 610 us | Reverted: mixed |
| `TextRenderer` whitespace marker `String`s -> borrowed/static defaults | fanout pre first render | 2,885 | 2,849 | -1.2% | 15,279 us | 12,358 us | Reverted: post regression |
| `TextRenderer` whitespace marker `String`s -> borrowed/static defaults | fanout post first render | 3,691 | 3,856 | +4.5% | 14,907 us | 11,246 us | Reverted: allocation regression |

No render-path optimization candidate was kept in Round 2. The kept code change is the `hx-bench` allocation counter plus the unused `BenchState` import cleanup.

### Remaining Candidates

- Add a narrower allocation counter around `render_document/main_loop` or a Criterion micro-bench for `CacheStore::compose_batch`; whole-frame counts still include flush/cursor work.
- Retry `compose_batch` reuse only if a compose-specific benchmark shows allocation wins without giant-render regressions.
- Revisit whitespace marker storage only with a render-document-scoped allocation measurement; whole-frame counts showed mixed results.

## Latency Round: Main-thread bounded work

Date: 2026-07-02

### Methodology

Measurements used the existing `hx-bench` release harness with `HELIX_LOG_LEVEL=error` and `CARGO_TARGET_DIR=C:\Users\jonfo\AppData\Local\Temp\helix-fork-target-latency`.

- `cargo run --quiet -p helix-term --bin hx-bench --features "integration bench" --release -- --fixture semantic-token-overlay --lines 200000 --renders 5 --width 120 --height 50`
- `cargo run --quiet -p helix-term --bin hx-bench --features "integration bench" --release -- --fixture giant-lines-render --lines 1 --bytes-per-line 10000000 --renders 5 --width 120 --height 50`
- `cargo run --quiet -p helix-term --bin hx-bench --features "integration bench" --release -- --fixture document-change-fanout --lines 1000000 --renders 2 --width 120 --height 50`

The new `semantic-token-overlay` fixture generates content on demand and installs one synthetic semantic token per line at the current document version. This creates a deterministic full-document semantic token set without committing a large blob.

### Findings

| Site | Trigger | Before | After | Technique |
| --- | --- | ---: | ---: | --- |
| Semantic token overlay construction | 200k tokens, first render | 93,303 us frame; `render_view/overlays` 67,374 us; `overlay_advances=190,468` | 10,904 us frame; `render_view/total` 6,408 us; `overlay_advances=283` | Filter semantic tokens to viewport char range before theme lookup/sort; render work becomes O(visible tokens). |
| 10MB single line | `giant-lines-render --lines 1 --bytes-per-line 10000000` | Existing path | Verified bounded: first render 26,972 us, cached renders 3,829-6,145 us; `formatter_next_calls=114`, `skip_right_eof_fast_paths=1`, skipped 9,999,888 offscreen chars. |
| 1M+ line edit fanout | `document-change-fanout --lines 1000000` | Existing path | Verified bounded edit/render: mutation 52 us; cached renders 621-623 us; render loop maps 148 rows. |
| Path and word completion filesystem scans | Completion request on path/word providers | Existing path | Verified already off main thread: both providers are scheduled with `JoinSet::spawn_blocking`; key dispatch only builds request state. |
| Event loop input vs ingress | Runtime ingress flood risk | Existing path | Verified input priority: `tokio::select! { biased; ... input_stream.next() ... ingress.rx.recv() ... }` polls terminal input before runtime ingress deliveries. |
| Diagnostics render overlays | Large diagnostic sets | Existing path | Verified viewport bounded: render passes viewport range and `diagnostic_highlights` partitions diagnostics before building ranges. |
| Markdown streaming layout | Assistant markdown chunks | Existing path | Verified cached/incremental shape: complete prefix is cached and only incomplete tail is reparsed on append. |
| Diff view rendering | Large parsed diff document | Existing path | Verified render is visible-area bounded over precomputed hunks; hunk computation remains outside per-frame render. |

### Kept Changes

- `Document::semantic_tokens_overlay` now accepts an optional viewport char range.
- `render_view` converts the existing viewport byte range to a char range and passes it to semantic-token overlay construction.
- `hx-bench` gained the generated `semantic-token-overlay` fixture.
- Package-manager UI compile fixes were required to run the bench on the current tree: re-export `PkgConfig`, return the correct ratatui span type for package statusline rendering, derive `Hash` for package statusline cache keys, route `:pkg` through typed layer ingress, and fix a language-server borrow/name mismatch.

## Loose End: 10MB Single-Line Cached Frames

Date: 2026-07-03

### Methodology

Measured the documented loose end with the same generated fixture on Windows:

- `HELIX_LOG_LEVEL=error CARGO_TARGET_DIR=C:\Users\jonfo\AppData\Local\Temp\helix-fork-target-task cargo run --quiet -p helix-term --bin hx-bench --features "integration bench" --release -- --fixture giant-lines-render --lines 1 --bytes-per-line 10000000 --renders 5 --width 120 --height 50`

### Results

| Run | First render | Cached frames | Cached avg | Finding |
| --- | ---: | ---: | ---: | --- |
| 1 | 14,824 us | 1,952 / 1,350 / 1,280 / 1,318 us | 1,475 us | Under 2ms after build settled. |
| 2 | 420,911 us | 7,024 / 12,861 / 4,458 / 4,425 us | 7,192 us | Noisy host run; cached phases show statusline/compositor/frame composition, not `render_document`. |
| 3 | 36,127 us | 4,504 / 4,040 / 4,119 / 4,987 us | 4,413 us | Cached phases remain pure cached-cell composition. |

### Finding

No render-path code change was kept. Current cached frames do not redo 10MB-line text work: after the first render, event logs omit `render_document/main_loop` and show only cached-frame phases such as `editor_render/final_total_ratatui`, `compositor_layer/base_batch`, `render/compositor_render_only`, and `render/compositor_total`. The first render still records bounded text work (`formatter_next_calls=114`, `skip_right_eof_fast_paths=1`, and 9,999,888 offscreen chars skipped).

The remaining >2ms samples are therefore the ratatui cell-buffer/statusline/compositor/flush floor on this Windows run, not repeated grapheme scanning, style-span assembly, or full-line cursor/selection math. Hitting a hard <2ms target would require a broader retained-frame/composition change rather than another long-line text-render cache.

## FFF Cache Migration: LMDB to SQLite

Date: 2026-07-03

### Methodology

Measured the file-picker frecency cache migration with the ignored probe:

- `cargo test -p helix-term fff_cache_perf_probe_50k -- --ignored --nocapture`

The probe seeds 50,000 candidate paths with three recent accesses each, compares the old LMDB-style path-hash lookup and scoring loop with the new SQLite-backed tracker, and measures 100 write-through access bumps. The new tracker loads the workspace frecency map from `cache.sqlite3` once, then scores from memory.

### Results

| Path | Measurement |
| --- | ---: |
| Old LMDB hot read, 50k candidates | 107.4439 ms |
| SQLite load into in-memory index, one-time per workspace | 279.4447 ms |
| New in-memory hot read, 50k candidates | 81.3956 ms |
| Old LMDB write path, 100 bumps | 30.9125 ms |
| New SQLite write-through path, 100 bumps | 86.0026 ms |

Both read paths produced the same score total: `150000`.

### Decision

Do not issue a SQLite query per candidate while the user types. The retained design is an in-memory workspace frecency index loaded once from SQLite, with SQLite used as the durable cache backing store on updates. This keeps the ranking/scoring hot path faster than the old LMDB read loop in the 50k-candidate probe. The write path is slower than LMDB, but remains below 1 ms per access bump in this measurement and is not on the per-keystroke ranking path.

## File Explorer Search: FFF-backed Expansion

Date: 2026-07-04

The file explorer search path no longer recursively walks the whole tree on each query edit. Opening the explorer and changing its root prewarms an explorer-scoped fff-search `FilePicker` using `config.file_explorer` scan semantics. Every query edit dispatches immediately to a serialized latest-query worker. Pending input is coalesced, and the UI thread only consumes typed results from an already-initialized workspace; it never waits for a cold scan or an in-flight query.

Matched fff paths still expand their ancestor directories before the existing visible-row rebuild/filter pass runs, so files under collapsed directories remain discoverable while clearing search restores the saved expansion set.

## Tightening Pass: FFF and SQLite hot paths

Date: 2026-07-04

### Methodology

Measurements used the existing ignored probes plus narrow probes added for this pass, with `CARGO_TARGET_DIR=C:\Users\jonfo\AppData\Local\Temp\helix-fork-target-tightening`:

- `cargo test -p helix-term fff_cache_perf_probe_50k -- --ignored --nocapture`
- `cargo test -p helix-term file_picker_explorer_workspace_reuse_probe -- --ignored --nocapture`
- `cargo test -p helix-term file_search_timing_current_workspace -- --ignored --nocapture` with `DHX_FFF_PROBE_ROOT=E:\helix\helix-fork`
- `cargo test -p helix-store sqlite_open_perf_probe -- --ignored --nocapture`
- `cargo test -p helix-store assistant_filtered_list_perf_probe -- --ignored --nocapture`
- Direct health-style timing: `dhx.exe --health clipboard`

### Kept Changes

| Change | Probe | Before | After | Delta / rationale |
| --- | --- | ---: | ---: | --- |
| Share scan-equivalent FFF workspace when only `FilePickerConfig::hide_preview` differs | `file_picker_explorer_workspace_reuse_probe` | `same_workspace=false`, second init 28.0652 ms | `same_workspace=true`, second init 2.7936 ms | Eliminates a redundant second fff index for the same root and same scan semantics. |
| FFF persistence uses cache-only SQLite store | `sqlite_open_perf_probe` | Full `Store::open`: 75,429 us/open avg over 50 fresh opens | `CacheStore::open`: 42,143 us/open avg | -44.1%; FFF frecency/query history never touches durable assistant/pkg state. |
| Cache DB uses `PRAGMA synchronous=NORMAL` under WAL | `fff_cache_perf_probe_50k` | SQLite write-through 100 bumps: 339.3008 ms | 24.6311 ms | -92.7%; scoped only to rebuildable `cache.sqlite3`, state DB remains durable/default. |
| Assistant filtered list composite index on `(scope, rating, has_feedback, updated_at DESC)` | `assistant_filtered_list_perf_probe` | Filtered 12.5k/50k rows: 56.738 ms | 41.6208 ms | -26.6% for the measured `list_by_scope_filtered(scope, Some(rating), Some(has_feedback))` path. |

`fff_cache_perf_probe_50k` after the cache changes: old LMDB hot read 135.0076 ms, SQLite load 336.281 ms, new in-memory hot read 97.9997 ms, old LMDB 100 writes 34.3072 ms, new SQLite 100 writes 24.6311 ms, totals `(150000,150000)`.

Repo-root FFF timing after the changes (`file_search_timing_current_workspace`): workspace init 14.3969 ms, first results 94.1538 ms, empty query 1.3392 ms, `src` 24.4501 ms, `picker` 19.696 ms, `fff` 5.8725 ms.

Direct `dhx.exe --health clipboard` timing after build: 188.9 ms. The health path exits before application startup, so it does not pay the async FFF prewarm or assistant bootstrap cost.

### Rejected / Audited

- Explorer search does not issue an fff query on every sync tick. Query edits enqueue once into the serialized worker, which replaces any not-yet-started request with the newest generation. No query-result cache or debounce timer is needed.
- Search has no debounce. Immediate dispatch preserves per-keystroke feedback while serialized execution prevents overlapping FFF searches from competing for CPU; stale generations are rejected at typed result ingress.
- Assistant/pkg state lazy-open at app startup was not changed. Package state is only opened by `pkg` commands or package-resolution nudges; assistant bootstrap also restores previously open assistant layout, so deferring it would change startup-visible behavior.
- Package receipt queries already use primary-key lookup for `(kind, name)`, a bounded `ORDER BY kind, name` full list, and one transaction for legacy receipt import. No measured low-risk package receipt change was kept.
