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
