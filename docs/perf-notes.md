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
