# Phase 4 — Fix Redraw/Re-highlight Performance

## Problem

Every frame, every view re-renders from scratch — fresh tree-sitter highlight
queries, rebuilt decorations, recomputed overlays — even when nothing changed.
With 4 splits, a keypress in one view causes 4× the CPU work needed.

**Root cause chain:**
1. `Editor.needs_redraw` is a single global bool — no per-view granularity
2. `EditorView::render()` (editor.rs:2162) loops ALL views unconditionally
3. `render_view()` creates a new `Highlighter` via `doc_syntax_highlighter()`
   every frame — tree-sitter query walk for the visible range
4. Rainbow brackets, diagnostics, selections, decorations all rebuilt per frame
5. `Component::should_update()` exists but is NEVER checked by compositor

**What's NOT broken:** Terminal layer does cell-level diffing (`Buffer::diff()`
in helix-tui), so only changed cells go to the terminal. But all the CPU work
to produce the cells happens upstream.

**The Highlighter can't be cached** — its lifetimes (`'a`, `'tree`) borrow
`&Syntax` and `RopeSlice` that only live within a single `render_view` call.
But the **rendered cells** can be cached.

## Strategy: Per-View Surface Cache

Cache the final rendered `Buffer` (cells) per view. On each frame, compute a
`ViewRenderState` fingerprint. If it matches the cached state, blit the cached
cells directly to the surface — skipping the entire render pipeline for that
view.

**Why surface-level, not highlight-level:**
- The view's output depends on highlights AND decorations AND diagnostics AND
  selections AND gutter AND statusline. Caching only highlights still requires
  re-running everything else.
- Surface caching gives maximum skip — zero work for unchanged views.
- Simple to reason about: if the fingerprint matches, the output is identical.

## Implementation Steps

### 4.1 — Add `diagnostics_gen` counter to Document

**File:** `helix-view/src/document.rs`

Add a `diagnostics_gen: u64` field to `Document`. Initialize to `0`. Increment
in `replace_diagnostics()` and `clear_diagnostics_for_language_server()`.

This gives cheap change detection for diagnostics without comparing the full
`Vec<Diagnostic>`.

```rust
// On Document struct:
pub(crate) diagnostics_gen: u64,

// In replace_diagnostics():
self.diagnostics_gen = self.diagnostics_gen.wrapping_add(1);

// In clear_diagnostics_for_language_server():
self.diagnostics_gen = self.diagnostics_gen.wrapping_add(1);
```

**Zero behavior change.** Pure data addition.

---

### 4.2 — Add `config_gen` counter to Editor

**File:** `helix-view/src/editor.rs`

Add `config_gen: u64` to `Editor`. Initialize to `0`. Increment when config
events are processed.

**File:** `helix-term/src/application.rs`

Increment `self.editor.config_gen` after applying config changes in the config
event handler.

**Zero behavior change.**

---

### 4.3 — Define `ViewRenderState` in helix-view

**File:** `helix-view/src/view.rs`

A composite fingerprint of everything that determines a view's rendered output.
Derived by tracing every input to `render_view()` (editor.rs:363-554).

```rust
/// Fingerprint of the state that determines a view's rendered output.
/// If two states are equal, the view's rendered cells are identical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewRenderState {
    doc_version: i32,
    view_position: ViewPosition,
    area: Rect,
    is_focused: bool,
    mode: Mode,
    selection_hash: u64,
    theme_name: String,
    diagnostics_gen: u64,
    config_gen: u64,
    terminal_focused: bool,
    has_dap_frame: bool,
}
```

**Cache key justification** (traced from render_view):
- `doc_version` — text content (line 411: `doc_syntax_highlighter` uses text)
- `view_position` — scroll position (line 378, fed to render_document)
- `area` — viewport rect (line 373, determines gutter + inner area)
- `is_focused` — cursors, selections, cursorline (lines 383, 387, 446, 491)
- `mode` — cursor shape (line 451: doc_selection_highlights)
- `selection_hash` — selection overlay (lines 450-457)
- `theme_name` — all theme lookups
- `diagnostics_gen` — diagnostic overlays (line 444)
- `config_gen` — gutter, cursorline, cursorcolumn, etc. (lines 376, 387, 393)
- `terminal_focused` — inactive background (line 383)
- `has_dap_frame` — DAP highlight overlays (line 398)

**Selection hashing** — hash primary cursor + range count + first/last range.
Cheaper than cloning and comparing the full Selection:

```rust
fn hash_selection(selection: &Selection) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let p = selection.primary();
    p.anchor.hash(&mut h);
    p.head.hash(&mut h);
    selection.len().hash(&mut h);
    if selection.len() > 1 {
        let r = selection.ranges();
        r[0].anchor.hash(&mut h);
        r[r.len() - 1].head.hash(&mut h);
    }
    h.finish()
}
```

Add a `View::render_state(...)` method that builds the fingerprint from current
editor/document state. No behavior change.

---

### 4.4 — Add `ViewRenderCache` to EditorView

**File:** `helix-term/src/ui/editor.rs`

```rust
use std::collections::HashMap;

struct ViewRenderCacheEntry {
    state: ViewRenderState,
    cells: tui::buffer::Buffer,
}

struct ViewRenderCache {
    entries: HashMap<ViewId, ViewRenderCacheEntry>,
}
```

Add `render_cache: ViewRenderCache` field to `EditorView`. Initialize empty.

**Why on EditorView, not View:** View lives in helix-view, which can't depend on
helix-tui for the `Buffer` type. EditorView has access to both.

**No behavior change yet.** Cache exists but isn't consulted.

---

### 4.5 — Integrate cache into the render loop

**File:** `helix-term/src/ui/editor.rs`

Replace the view iteration loop in `EditorView::render()` (line 2162-2165):

```rust
// Evict entries for closed views
let active_ids: HashSet<ViewId> = cx.editor.tree.views()
    .map(|(v, _)| v.id).collect();
self.render_cache.entries.retain(|id, _| active_ids.contains(id));

for (view, is_focused) in cx.editor.tree.views() {
    let doc = cx.editor.document(view.doc).unwrap();

    let current = view.render_state(
        doc,
        cx.editor.mode,
        cx.editor.theme.name(),
        doc.diagnostics_gen,
        cx.editor.config_gen,
        is_focused,
        self.terminal_focused,
    );

    // Cache hit → blit stored cells, skip render pipeline
    if let Some(cached) = self.render_cache.entries.get(&view.id) {
        if cached.state == current {
            blit(&cached.cells, surface);
            continue;
        }
    }

    // Cache miss → full render
    self.render_view(cx.editor, doc, view, area, surface, is_focused);

    // Store rendered cells
    let cells = copy_region(surface, view.area);
    self.render_cache.entries.insert(view.id, ViewRenderCacheEntry {
        state: current,
        cells,
    });
}
```

**Borrow checker:** `render_view` takes `&self`, `render()` has `&mut self`.
Rust allows reborrowing `&mut self` as `&self` for the call, then `&mut self`
is available again after. Cache reads/writes only touch `self.render_cache`
(on EditorView), not `cx.editor`, so no conflict with the `tree.views()`
iteration over `cx.editor`.

**Helper functions:**

```rust
fn blit(source: &tui::buffer::Buffer, target: &mut Surface) {
    let a = source.area;
    for y in a.top()..a.bottom() {
        for x in a.left()..a.right() {
            target[(x, y)] = source[(x, y)].clone();
        }
    }
}

fn copy_region(source: &Surface, area: Rect) -> tui::buffer::Buffer {
    let mut buf = tui::buffer::Buffer::empty(area);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)] = source[(x, y)].clone();
        }
    }
    buf
}
```

**Edge cases handled naturally:**
- Terminal resize → `view.area` changes → fingerprint differs → cache miss
- Document switch → `doc_version` changes → miss
- Theme change → `theme_name` changes → miss
- View closed → eviction at top of loop

---

### 4.6 — Invalidate cache on full_redraw

**File:** `helix-term/src/ui/editor.rs`

In `EditorView::handle_event`, clear the cache on resize events:

```rust
Event::Resize(..) => {
    self.render_cache.entries.clear();
    // ... existing handling
}
```

Also clear on terminal focus/blur events (which change `terminal_focused` —
already in the fingerprint, but clearing the cache is a safety belt).

---

### 4.7 — Add render statistics (debug builds only)

**File:** `helix-term/src/ui/editor.rs`

```rust
#[cfg(debug_assertions)]
struct RenderStats {
    hits: u64,
    misses: u64,
    frames: u64,
}
```

Add to `ViewRenderCache`. Increment on hit/miss. Log every 300 frames (10s at
30fps) at `log::debug!` level. Gives measurable verification.

---

### 4.8 — Tests

**File:** `helix-view/tests/` or inline in `view.rs`

1. `ViewRenderState` equality: same inputs → same state
2. `ViewRenderState` inequality: changing any single field → different state
3. Selection hash: different cursors → different hashes; same cursor → same hash

**File:** Integration test (helix-term test)

4. With 2 views on same doc, editing in one → only that view's cache misses
5. Scrolling one view → only that view misses
6. Theme change → all caches miss

---

## What NOT To Do

1. **No full damage tracking.** Per-view caching is simpler and sufficient.
   (#11928 comment: "damage tracking not realistic")

2. **No Highlighter caching.** Its lifetimes are tied to `&Syntax` +
   `RopeSlice` scoped to a single render call. Would need unsafe or
   self-referential structs.

3. **No separate highlight span cache.** View output depends on highlights +
   decorations + diagnostics + selections + gutter + statusline. Caching only
   highlights still requires re-running the rest.

4. **No `should_update()` integration.** Would require compositor-level changes.
   Per-view caching inside EditorView achieves the same result.

5. **No `PartialEq` on Theme.** Theme name comparison is sufficient — themes
   are immutable once loaded, changing theme changes the name.

6. **No cross-document-switch caching.** When a view switches docs, the cache
   naturally misses via doc_version. Correct behavior.

---

## Expected Impact

| Scenario | Before | After |
|---|---|---|
| 4 splits, keypress in one | 4 full render_view calls | 1 render + 3 blits |
| Scrolling in one view | All views re-render | 1 render + (N-1) blits |
| Typing in insert mode | All views re-render | 1 render + (N-1) blits |
| Theme change | All views render | All views render (correct) |
| LSP diagnostics arrive | All views render | Affected views render, rest blit |

**Cell blit cost:** For 200×50 terminal, blitting 10,000 cells (Cell::clone)
is negligible vs tree-sitter queries + DocumentFormatter + decoration building.

---

## Risk Assessment

**False hits (stale output):** The main risk. Mitigated by:
- Conservative fingerprint covering all render_view inputs
- Easy to add more fields to fingerprint as edge cases surface

**Known gaps to address incrementally:**
- Inlay hints from LSP (add `inlay_hints_gen` when observed)
- Plugin annotations (add gen counter when plugin system uses them)
- Git diff gutter (add vcs gen counter)
- Inline blame (add blame gen counter)

For initial implementation, these edge cases cause stale rendering only until
the next cache-busting event (keypress, scroll). Acceptable tradeoff for the
major perf win on the common case.

---

## File Summary

| Step | Files Modified | Nature |
|---|---|---|
| 4.1 | `helix-view/src/document.rs` | Add field + increment |
| 4.2 | `helix-view/src/editor.rs`, `helix-term/src/application.rs` | Add field + increment |
| 4.3 | `helix-view/src/view.rs` | New type + method |
| 4.4 | `helix-term/src/ui/editor.rs` | New field on EditorView |
| 4.5 | `helix-term/src/ui/editor.rs` | Render loop change (THE behavioral change) |
| 4.6 | `helix-term/src/ui/editor.rs` | Cache invalidation on resize |
| 4.7 | `helix-term/src/ui/editor.rs` | Debug stats |
| 4.8 | `helix-view/src/view.rs`, test files | Tests |
