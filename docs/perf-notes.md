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
