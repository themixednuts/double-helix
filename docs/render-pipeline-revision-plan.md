# Render Pipeline Revision Plan

## Problem

Helix currently invalidates render caches with hand-maintained fingerprints such as `ViewContentState`.
That approach keeps breaking as new syntax, annotation, and viewport features are added because each
feature has to remember to extend every downstream cache key.

The result is a steady stream of stale-render bugs:

- tree-sitter refreshes that do not invalidate cached paint
- diagnostics and inlay hint changes that only partially invalidate layout
- viewport-dependent caches that silently reuse data across incompatible states
- multi-document workspace edits that publish partially-applied intermediate states

## Goals

- Replace ad hoc cache invalidation with explicit revisioned dependencies.
- Make syntax freshness a first-class state transition instead of an implicit boolean.
- Separate text, syntax, annotation, layout, and paint derivation boundaries.
- Apply workspace edits transactionally so the editor publishes one coherent state update.
- Make future render features declare their dependencies once instead of patching cache keys by hand.

## Non-Goals

- Rewrite the entire rendering pipeline in one change.
- Move all render logic out of `helix-term` immediately.
- Replace every generation counter in the first milestone.

## Target Model

### Shared Primitive

- Introduce a small `Revision` type for monotonic snapshot identity.

### Derived Snapshots

- `TextSnapshot`: text rope, document version, text revision.
- `SyntaxSnapshot`: syntax tree, syntax revision, status (`Fresh`, `StalePendingRefresh`, `Disabled`).
- `AnnotationSnapshot`: diagnostics, inlay hints, overlays, annotation revision.
- `LayoutSnapshot`: viewport-dependent line map and wrapping data.
- `PaintSnapshot`: rendered cells for one viewport/theme/config tuple.

### Dependency Rules

- syntax depends on text
- annotations depend on text plus LSP/diagnostic producers
- layout depends on text, annotations, viewport, config, theme-adjacent layout settings
- paint depends on syntax, annotations, layout, theme, and focus/selection overlays

Each cache should key off the revisions of its direct inputs instead of reconstructing a large global
"content state" fingerprint.

## Phases

### Phase 1: Syntax Snapshot Foundation ✅

- Add `Revision` as a reusable value type.
- Replace `syntax_stale` plus `syntax_gen` with an explicit syntax snapshot state.
- Expose `syntax_revision()` and `syntax_status()` from `Document`.
- Key render cache invalidation off syntax revision instead of a raw generation counter.

Implemented: `helix-view/src/revision.rs` (Revision type), `helix-view/src/syntax_aware.rs` (SyntaxStatus, SyntaxSnapshot — 639 lines).

### Phase 2: Split Layout and Paint Inputs ✅

- Replace `ViewContentState` with smaller typed cache inputs.
- Introduce `LayoutInputs` and `PaintInputs`.
- Keep line-map reuse and cell reuse separate so overlay-only changes do not contaminate syntax/layout reuse.

Implemented: `ViewLayoutInputs`, `ViewPaintInputs`, `LayoutSnapshot`, `PaintSnapshot`, `RenderSnapshots`, `RenderSnapshotsRef` in `helix-view/src/view.rs`.

### Phase 3: Annotation Snapshot ✅

- Consolidate diagnostics, inlay hints, jump labels, and related overlays under a shared annotation revision.
- Route line-map and paint invalidation through annotation snapshots rather than multiple unrelated counters.

Implemented: `AnnotationSnapshot` with `Revision` tracking in `helix-view/src/presentation_state.rs` (229 lines). `annotation_gen: u64` on Document for render cache invalidation.

### Phase 4: Transactional Workspace Edits ✅

- Parse and validate all workspace edits before mutating documents.
- Build per-document transactions against checked document versions.
- Abort the whole apply if any edit is invalid or outdated.
- Publish one editor state transition after successful application.

Implemented: `helix-view/src/handlers/workspace_edit.rs` (679 lines) — planning-before-apply pattern with version checking. Tests: `workspace_edit_text_edits_are_planned_before_apply`, `mixed_workspace_edit_operations_are_planned_before_apply`, `directory_rename_workspace_edit_maps_descendant_text_edits`.

### Phase 5: Render Pipeline Cleanup ✅

- Convert render cache entries to typed snapshot dependencies.
- Remove legacy catch-all fingerprint fields once covered by snapshot revisions.
- Add transition-focused tests for text edits, idle syntax refresh, diagnostics updates, resize, theme changes, and workspace edits.

Implemented: `ViewRenderCache` in `helix-term/src/ui/editor.rs` — per-view HashMap-based cache with `ViewRenderCacheEntry { snapshots, cells }`, snapshot-based hit/miss detection, debug-mode statistics logging.

## Success Criteria

- Idle syntax refreshes cannot leave stale syntax paint on screen.
- Adding a new render-affecting subsystem requires wiring a single snapshot dependency, not patching multiple cache keys.
- Workspace-edit failures leave documents unchanged.
- Render-cache tests validate revision transitions directly.

## Initial Implementation Slice

The first implementation step for this plan is:

1. add `Revision`
2. replace syntax freshness booleans with `SyntaxStatus`
3. move syntax state toward a dedicated snapshot structure
4. thread syntax revision through render cache inputs

This is intentionally incremental: it stabilizes the syntax/render boundary first, which is where the
current rename-highlighting failures originate.
