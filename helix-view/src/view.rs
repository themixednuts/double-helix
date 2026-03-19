use crate::{
    align_view_in,
    annotations::{diagnostics::InlineDiagnostics, plugins::PluginLineAnnotations},
    bench::log_run_event,
    document::{DocumentInlayHints, Mode},
    document_lsp::DocumentColorSwatches,
    editor::{GutterConfig, GutterType},
    graphics::Rect,
    handlers::diagnostics::DiagnosticsHandler,
    history_state::ViewHistoryState,
    Align, Document, DocumentId, Editor, Theme, ViewId,
};

use helix_core::{
    char_idx_at_visual_offset,
    doc_formatter::TextFormat,
    plain_visual_col_at_char_idx,
    text_annotations::{PlainViewportSupport, TextAnnotations},
    text_folding::{FoldAnnotations, RopeSliceFoldExt},
    visual_offset_from_anchor, visual_offset_from_block, Position, RopeSlice, Selection,
    Transaction,
    VisualOffsetError::{PosAfterMaxRow, PosBeforeAnchorRow},
};

use std::{
    fmt,
    hash::{Hash, Hasher},
    sync::Arc,
};

#[derive(Clone, Debug, PartialEq, Eq, Copy, Default)]
pub struct ViewPosition {
    pub anchor: usize,
    pub horizontal_offset: usize,
    pub vertical_offset: usize,
}

#[derive(Clone, Debug)]
pub struct ComponentViewState {
    pub id: ViewId,
    pub area: Rect,
    pub doc: DocumentId,
    pub history: ViewHistoryState,
    pub object_selections: Vec<Selection>,
}

impl ComponentViewState {
    pub fn new(id: ViewId, doc: DocumentId) -> Self {
        Self {
            id,
            area: Rect::default(),
            doc,
            history: ViewHistoryState::new(doc),
            object_selections: Vec::new(),
        }
    }
}

pub enum AnyViewMut<'a> {
    Tree(&'a mut View),
    Component(&'a mut ComponentViewState),
}

impl AnyViewMut<'_> {
    pub fn doc_id(&self) -> DocumentId {
        match self {
            Self::Tree(view) => view.doc,
            Self::Component(view) => view.doc,
        }
    }

    pub fn object_selections_mut(&mut self) -> &mut Vec<Selection> {
        match self {
            Self::Tree(view) => &mut view.object_selections,
            Self::Component(view) => &mut view.object_selections,
        }
    }
}

/// Inputs that determine viewport layout for a rendered view.
///
/// When this changes, line-map and wrapping data must be rebuilt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewLayoutInputs {
    pub doc_id: crate::DocumentId,
    pub doc_version: i32,
    pub view_position: ViewPosition,
    pub area: Rect,
    pub config_gen: u64,
    pub annotation: crate::presentation_state::AnnotationSnapshot,
}

impl ViewLayoutInputs {
    pub fn can_reuse_seed_line_map(&self, current: &Self) -> bool {
        self.doc_id == current.doc_id
            && self.doc_version == current.doc_version
            && self.annotation == current.annotation
            && self.config_gen == current.config_gen
            && self.area.width == current.area.width
    }
}

impl LayoutSnapshot {
    pub fn can_seed(&self, current: &ViewLayoutInputs) -> bool {
        self.inputs.can_reuse_seed_line_map(current)
    }
}

impl PaintSnapshot {
    pub fn matches(&self, current: &ViewPaintInputs) -> bool {
        &self.inputs == current
    }
}

/// Inputs that determine painted cells for a rendered view.
///
/// When this changes, cached syntax styles or cell buffers may need to be rebuilt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewPaintInputs {
    pub layout: ViewLayoutInputs,
    pub syntax: crate::syntax_aware::SyntaxSnapshot,
    pub theme_name: Arc<str>,
    /// Primary cursor line — invalidates cache when cursor moves (affects
    /// relative line numbers and gutter selection highlights).
    pub cursor_line: usize,
    pub gutter: crate::document::GutterSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderInputs {
    pub layout: ViewLayoutInputs,
    pub paint: ViewPaintInputs,
}

#[derive(Debug, Clone)]
pub struct RenderSnapshots {
    pub layout: LayoutSnapshot,
    pub paint: PaintSnapshot,
}

impl RenderSnapshots {
    pub fn as_ref(&self) -> RenderSnapshotsRef<'_> {
        RenderSnapshotsRef {
            layout: &self.layout,
            paint: &self.paint,
        }
    }
}

#[derive(Clone, Copy)]
pub struct RenderSnapshotsRef<'a> {
    pub layout: &'a LayoutSnapshot,
    pub paint: &'a PaintSnapshot,
}

pub enum RenderPlan<'a> {
    Reuse(ReusePlan<'a>),
    Refresh(RefreshPlan<'a>),
}

pub struct ReusePlan<'a> {
    pub cached: RenderSnapshotsRef<'a>,
    pub dirty_rows: std::collections::HashSet<u16>,
    pub overlay_fingerprints: Arc<[u64]>,
}

pub struct RefreshPlan<'a> {
    pub seed_line_map: Option<&'a LineMap>,
}

impl View {
    pub fn layout_inputs(&self, doc: &Document, config_gen: u64) -> ViewLayoutInputs {
        ViewLayoutInputs {
            doc_id: self.doc,
            doc_version: doc.version(),
            view_position: doc.view_offset(self.id),
            area: self.area,
            config_gen,
            annotation: doc.annotation_snapshot(),
        }
    }

    pub fn render_inputs(
        &self,
        doc: &Document,
        config_gen: u64,
        theme_name: Arc<str>,
    ) -> RenderInputs {
        let primary_cursor = doc
            .selection(self.id)
            .primary()
            .cursor(doc.text().slice(..));
        let cursor_line = doc.text().char_to_line(primary_cursor);

        let layout = self.layout_inputs(doc, config_gen);
        let paint = ViewPaintInputs {
            layout: layout.clone(),
            syntax: doc.syntax_snapshot(),
            theme_name,
            cursor_line,
            gutter: doc.gutter_snapshot(),
        };

        RenderInputs { layout, paint }
    }
}

impl RenderInputs {
    pub fn plan<'a>(
        &self,
        cached: Option<RenderSnapshotsRef<'a>>,
        selection: &Selection,
        mode: Mode,
        is_focused: bool,
        terminal_focused: bool,
    ) -> RenderPlan<'a> {
        match cached {
            Some(cached) if cached.paint.matches(&self.paint) => {
                let overlay_fingerprints = cached.layout.line_map.overlay_fingerprints(
                    selection,
                    mode,
                    is_focused,
                    terminal_focused,
                );
                let dirty_rows =
                    LineMap::dirty_rows(&cached.paint.overlay_fingerprints, &overlay_fingerprints);
                RenderPlan::Reuse(ReusePlan {
                    cached,
                    dirty_rows,
                    overlay_fingerprints,
                })
            }
            Some(cached) if cached.layout.can_seed(&self.layout) => {
                RenderPlan::Refresh(RefreshPlan {
                    seed_line_map: Some(&cached.layout.line_map),
                })
            }
            _ => RenderPlan::Refresh(RefreshPlan {
                seed_line_map: None,
            }),
        }
    }

    pub fn into_cached_snapshots(
        self,
        line_map: LineMap,
        syntax_styles: SyntaxStyleCache,
        overlay_fingerprints: Arc<[u64]>,
    ) -> RenderSnapshots {
        RenderSnapshots {
            layout: LayoutSnapshot::new(self.layout, line_map),
            paint: PaintSnapshot::new(self.paint, syntax_styles, overlay_fingerprints),
        }
    }

    pub fn into_snapshots(
        self,
        line_map: LineMap,
        syntax_entries: Vec<SyntaxStyleEntry>,
        overlay_fingerprints: Arc<[u64]>,
    ) -> RenderSnapshots {
        RenderSnapshots {
            layout: LayoutSnapshot::new(self.layout, line_map),
            paint: PaintSnapshot::from_entries(self.paint, syntax_entries, overlay_fingerprints),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LayoutSnapshot {
    pub inputs: ViewLayoutInputs,
    pub line_map: LineMap,
}

impl LayoutSnapshot {
    pub fn new(inputs: ViewLayoutInputs, line_map: LineMap) -> Self {
        Self { inputs, line_map }
    }

    pub fn cursor_position(&self, doc: &Document, view: &View) -> Option<Position> {
        let text = doc.text().slice(..);
        let cursor = doc.selection(view.id).primary().cursor(text);
        let tab_width = doc.tab_width();

        self.line_map.lines.iter().find_map(|line| {
            if line.visible_char_start == usize::MAX || line.visible_char_last == usize::MAX {
                return None;
            }
            if cursor < line.visible_char_start || cursor > line.char_range_end {
                return None;
            }
            if line.doc_line != text.char_to_line(cursor.min(text.len_chars())) {
                return None;
            }

            let delta_chars = cursor.saturating_sub(line.visible_char_start);
            let delta_col = plain_visual_col_at_char_idx(
                text.slice(line.visible_char_start..cursor),
                delta_chars,
                tab_width,
            );

            let visual_col = line.visible_col_start + delta_col;
            if visual_col < doc.view_offset(view.id).horizontal_offset {
                return None;
            }

            Some(Position::new(
                line.visual_row as usize,
                visual_col - doc.view_offset(view.id).horizontal_offset,
            ))
        })
    }

    pub fn update_cursor_cache(&self, editor: &Editor, doc: &Document, view: &View) {
        if editor.tree.focus != view.id {
            return;
        }
        editor.cursor_cache.set(self.cursor_position(doc, view));
    }

    pub fn render_seed(&self, top_doc_line: usize, max_gap: usize) -> Option<RenderSeed> {
        self.line_map
            .best_horizontal_checkpoint_within_gap(
                top_doc_line,
                self.inputs.view_position.horizontal_offset,
                max_gap,
            )
            .map(|checkpoint| RenderSeed {
                doc_line: top_doc_line,
                char_idx: checkpoint.char_idx,
                visual_col: checkpoint.visual_col,
            })
    }
}

#[derive(Debug, Clone)]
pub struct PaintSnapshot {
    pub inputs: ViewPaintInputs,
    pub syntax_styles: SyntaxStyleCache,
    pub overlay_fingerprints: Arc<[u64]>,
}

impl PaintSnapshot {
    pub fn new(
        inputs: ViewPaintInputs,
        syntax_styles: SyntaxStyleCache,
        overlay_fingerprints: Arc<[u64]>,
    ) -> Self {
        Self {
            inputs,
            syntax_styles,
            overlay_fingerprints,
        }
    }

    pub fn from_entries(
        inputs: ViewPaintInputs,
        syntax_entries: Vec<SyntaxStyleEntry>,
        overlay_fingerprints: Arc<[u64]>,
    ) -> Self {
        Self::new(
            inputs,
            SyntaxStyleCache {
                entries: Arc::from(syntax_entries),
            },
            overlay_fingerprints,
        )
    }
}

/// A cached syntax style entry: the char position where the style takes effect,
/// and the accumulated `Style` at that position.
#[derive(Debug, Clone)]
pub struct SyntaxStyleEntry {
    pub char_idx: usize,
    pub style: crate::graphics::Style,
}

/// Cached syntax styles for a viewport. Replaces the tree-sitter `Highlighter`
/// when the content hasn't changed but overlays (cursor, selection) have.
/// Uses `Arc<[T]>` so clones are O(1) refcount bumps.
#[derive(Debug, Clone)]
pub struct SyntaxStyleCache {
    pub entries: Arc<[SyntaxStyleEntry]>,
}

impl Default for SyntaxStyleCache {
    fn default() -> Self {
        Self {
            entries: Arc::from([]),
        }
    }
}

/// Metadata for one visual line (one row on screen), built during rendering.
/// Used to map visual rows back to document positions for dirty-line detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HorizontalCheckpoint {
    pub char_idx: usize,
    pub visual_col: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderSeed {
    pub doc_line: usize,
    pub char_idx: usize,
    pub visual_col: usize,
}

#[derive(Debug, Clone)]
pub struct VisualLineInfo {
    /// The visual row index (relative to viewport top).
    pub visual_row: u16,
    /// The document line this visual row belongs to.
    pub doc_line: usize,
    /// First char index on this visual row.
    pub char_range_start: usize,
    /// Last char index on this visual row (exclusive).
    pub char_range_end: usize,
    /// First visible document char rendered on this visual row.
    pub visible_char_start: usize,
    /// Visual column for `visible_char_start`.
    pub visible_col_start: usize,
    /// Last visible document char rendered on this visual row.
    pub visible_char_last: usize,
    /// Visual column for `visible_char_last`.
    pub visible_col_last: usize,
    /// Sampled horizontal checkpoints on this row for fast re-entry at large horizontal offsets.
    pub horizontal_checkpoints: Vec<HorizontalCheckpoint>,
}

/// Per-visual-line map built during rendering. Enables dirty-line detection
/// by mapping visual rows to document positions.
/// Uses `Arc<[T]>` so clones are O(1) refcount bumps.
#[derive(Debug, Clone)]
pub struct LineMap {
    pub lines: Arc<[VisualLineInfo]>,
}

impl Default for LineMap {
    fn default() -> Self {
        Self {
            lines: Arc::from([]),
        }
    }
}

impl LineMap {
    pub fn best_horizontal_checkpoint(
        &self,
        doc_line: usize,
        horizontal_offset: usize,
    ) -> Option<HorizontalCheckpoint> {
        self.best_horizontal_checkpoint_within_gap(doc_line, horizontal_offset, usize::MAX)
    }

    pub fn best_horizontal_checkpoint_within_gap(
        &self,
        doc_line: usize,
        horizontal_offset: usize,
        max_gap: usize,
    ) -> Option<HorizontalCheckpoint> {
        let mut best = None;
        let mut best_gap = usize::MAX;

        let mut consider = |checkpoint: HorizontalCheckpoint| {
            if checkpoint.visual_col > horizontal_offset {
                return;
            }
            let gap = horizontal_offset.saturating_sub(checkpoint.visual_col);
            if gap > max_gap {
                return;
            }
            if gap < best_gap {
                best_gap = gap;
                best = Some(checkpoint);
            }
        };

        for line in self.lines.iter().filter(|line| line.doc_line == doc_line) {
            for checkpoint in &line.horizontal_checkpoints {
                consider(*checkpoint);
            }
            if line.visible_char_start != usize::MAX {
                consider(HorizontalCheckpoint {
                    char_idx: line.visible_char_start,
                    visual_col: line.visible_col_start,
                });
            }
            if line.visible_char_last != usize::MAX {
                consider(HorizontalCheckpoint {
                    char_idx: line.visible_char_last,
                    visual_col: line.visible_col_last,
                });
            }
        }

        best
    }

    /// Compute per-line overlay fingerprints. A fingerprint changes when the
    /// overlay state (selection, cursor, focus, mode) affecting that line changes.
    pub fn overlay_fingerprints(
        &self,
        selection: &Selection,
        mode: Mode,
        is_focused: bool,
        terminal_focused: bool,
    ) -> Arc<[u64]> {
        let mut fingerprints = Vec::with_capacity(self.lines.len());
        for line in self.lines.iter() {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            // Hash focus/mode state
            is_focused.hash(&mut h);
            terminal_focused.hash(&mut h);
            std::mem::discriminant(&mode).hash(&mut h);

            // Hash which selection ranges intersect this line's char range
            for (i, range) in selection.ranges().iter().enumerate() {
                let (start, end) = if range.anchor <= range.head {
                    (range.anchor, range.head)
                } else {
                    (range.head, range.anchor)
                };
                // Check intersection with this line's char range
                if start < line.char_range_end && end >= line.char_range_start {
                    i.hash(&mut h);
                    range.anchor.hash(&mut h);
                    range.head.hash(&mut h);
                }
            }
            // Hash whether primary cursor head is on this line
            let primary_head = selection.primary().head;
            if primary_head >= line.char_range_start && primary_head < line.char_range_end {
                true.hash(&mut h);
            }

            fingerprints.push(h.finish());
        }
        Arc::from(fingerprints)
    }

    /// Compare two fingerprint vecs and return the set of dirty visual rows.
    pub fn dirty_rows(old: &[u64], new: &[u64]) -> std::collections::HashSet<u16> {
        let mut dirty = std::collections::HashSet::new();
        let max_len = old.len().max(new.len());
        for i in 0..max_len {
            let old_fp = old.get(i).copied().unwrap_or(0);
            let new_fp = new.get(i).copied().unwrap_or(0);
            if old_fp != new_fp {
                dirty.insert(i as u16);
            }
        }
        dirty
    }
}

#[derive(Clone)]
pub struct View {
    pub id: ViewId,
    pub area: Rect,
    pub doc: DocumentId,
    pub history: ViewHistoryState,
    // documents accessed from this view from the oldest one to last viewed one
    pub docs_access_history: Vec<DocumentId>,
    /// the last modified files before the current one
    /// ordered from most frequent to least frequent
    // uses two docs because we want to be able to swap between the
    // two last modified docs which we need to manually keep track of
    pub last_modified_docs: [Option<DocumentId>; 2],
    /// used to store previous selections of tree-sitter objects
    pub object_selections: Vec<Selection>,
    /// all gutter-related configuration settings, used primarily for gutter rendering
    pub gutters: GutterConfig,
    // HACKS: there should really only be a global diagnostics handler (the
    // non-focused views should just not have different handling for the cursor
    // line). For that we would need accces to editor everywhere (we want to use
    // the positioning code) so this can only happen by refactoring View and
    // Document into entity component like structure. That is a huge refactor
    // left to future work. For now we treat all views as focused and give them
    // each their own handler.
    pub diagnostics_handler: DiagnosticsHandler,
}

impl fmt::Debug for View {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("View")
            .field("id", &self.id)
            .field("area", &self.area)
            .field("doc", &self.doc)
            .finish()
    }
}

impl View {
    pub fn new(doc: DocumentId, gutters: GutterConfig) -> Self {
        Self {
            id: ViewId::default(),
            doc,
            area: Rect::default(), // will get calculated upon inserting into tree
            history: ViewHistoryState::new(doc),
            docs_access_history: Vec::new(),
            last_modified_docs: [None, None],
            object_selections: Vec::new(),
            gutters,
            diagnostics_handler: DiagnosticsHandler::new(),
        }
    }

    pub fn add_to_history(&mut self, id: DocumentId) {
        if let Some(pos) = self.docs_access_history.iter().position(|&doc| doc == id) {
            self.docs_access_history.remove(pos);
        }
        self.docs_access_history.push(id);
    }

    /// The range of lines in the document that the view sees
    pub fn line_range(&self, doc: &Document) -> std::ops::Range<usize> {
        let text = doc.text();
        let text_line_count = text.len_lines();
        let first_line = text.char_to_line(doc.view_offset(self.id).anchor.min(text.len_chars()));
        let last_line = first_line
            .saturating_add(self.inner_height())
            .min(text_line_count);

        first_line..last_line
    }

    pub fn inner_area(&self, doc: &Document) -> Rect {
        self.area.clip_left(self.gutter_offset(doc)).clip_bottom(1) // -1 for statusline
    }

    pub fn inner_height(&self) -> usize {
        self.area.clip_bottom(1).height.into() // -1 for statusline
    }

    pub fn inner_width(&self, doc: &Document) -> u16 {
        self.area.clip_left(self.gutter_offset(doc)).width
    }

    pub fn gutters(&self) -> &[GutterType] {
        &self.gutters.layout
    }

    pub fn gutter_offset(&self, doc: &Document) -> u16 {
        let total_width = self
            .gutters
            .layout
            .iter()
            .map(|gutter| gutter.width(self, doc) as u16)
            .sum();
        if total_width < self.area.width {
            total_width
        } else {
            0
        }
    }

    //
    pub fn offset_coords_to_in_view(
        &self,
        doc: &Document,
        scrolloff: usize,
    ) -> Option<ViewPosition> {
        offset_coords_to_in_view_center_in::<_, false>(self, doc, scrolloff)
    }

    pub fn offset_coords_to_in_view_center<const CENTERING: bool>(
        &self,
        doc: &Document,
        scrolloff: usize,
    ) -> Option<ViewPosition> {
        offset_coords_to_in_view_center_in::<_, CENTERING>(self, doc, scrolloff)
    }

    pub fn ensure_cursor_in_view(&self, doc: &mut Document, scrolloff: usize) {
        ensure_cursor_in_view_in(self, doc, scrolloff);
    }

    pub fn ensure_cursor_in_view_center(&self, doc: &mut Document, scrolloff: usize) {
        ensure_cursor_in_view_center_in(self, doc, scrolloff);
    }

    pub fn is_cursor_in_view(&mut self, doc: &Document, scrolloff: usize) -> bool {
        self.offset_coords_to_in_view(doc, scrolloff).is_none()
    }

    /// Estimates the last visible document line on screen.
    /// This estimate is an upper bound obtained by calculating the first
    /// visible line and adding the viewport height.
    /// The actual last visible line may be smaller if softwrapping occurs
    /// or virtual text lines are visible
    #[inline]
    pub fn estimate_last_doc_line(&self, annotations: &TextAnnotations, doc: &Document) -> usize {
        let doc_text = doc.text().slice(..);
        let line = doc_text.char_to_line(doc.view_offset(self.id).anchor.min(doc_text.len_chars()));
        doc_text.nth_next_folded_line(
            &annotations.folds,
            line,
            self.inner_height().saturating_sub(1),
        )
    }

    /// Calculates the last non-empty visual line on screen
    #[inline]
    pub fn last_visual_line(&self, doc: &Document) -> usize {
        let doc_text = doc.text().slice(..);
        let viewport = self.inner_area(doc);
        let text_fmt = doc.text_format(viewport.width, None);
        let annotations = self.text_annotations(doc, None);
        let view_offset = doc.view_offset(self.id);

        // last visual line in view is trivial to compute
        let visual_height = doc.view_offset(self.id).vertical_offset + viewport.height as usize;

        // fast path when the EOF is not visible on the screen,
        if self.estimate_last_doc_line(&annotations, doc) < doc_text.len_lines() - 1 {
            return visual_height.saturating_sub(1);
        }

        // translate to document line
        let pos = visual_offset_from_anchor(
            doc_text,
            view_offset.anchor,
            usize::MAX,
            &text_fmt,
            &annotations,
            visual_height,
        );

        match pos {
            Ok((Position { row, .. }, _)) => row.saturating_sub(view_offset.vertical_offset),
            Err(PosAfterMaxRow) => visual_height.saturating_sub(1),
            Err(PosBeforeAnchorRow) => 0,
        }
    }

    /// Translates a document position to an absolute position in the terminal.
    /// Returns a (line, col) position if the position is visible on screen.
    // TODO: Could return width as well for the character width at cursor.
    pub fn screen_coords_at_pos(
        &self,
        doc: &Document,
        text: RopeSlice,
        pos: usize,
    ) -> Option<Position> {
        let view_offset = doc.view_offset(self.id);

        let viewport = self.inner_area(doc);
        let text_fmt = doc.text_format(viewport.width, None);
        let annotations = self.text_annotations(doc, None);

        let mut pos = visual_offset_from_anchor(
            text,
            view_offset.anchor,
            pos,
            &text_fmt,
            &annotations,
            viewport.height as usize,
        )
        .ok()?
        .0;
        if pos.row < view_offset.vertical_offset {
            return None;
        }
        pos.row -= view_offset.vertical_offset;
        if pos.row >= viewport.height as usize {
            return None;
        }
        pos.col = pos.col.saturating_sub(view_offset.horizontal_offset);

        Some(pos)
    }

    /// Get the text annotations to display in the current view for the given document and theme.
    pub fn text_annotations<'a>(
        &self,
        doc: &'a Document,
        theme: Option<&Theme>,
    ) -> TextAnnotations<'a> {
        let mut text_annotations = TextAnnotations::default();

        if let Some(labels) = doc.jump_labels(self.id) {
            let style = theme.and_then(|t| t.find_highlight("ui.virtual.jump-label"));
            text_annotations.add_overlay(labels, style);
        }

        if let Some(DocumentInlayHints {
            id: _,
            type_inlay_hints,
            parameter_inlay_hints,
            other_inlay_hints,
            padding_before_inlay_hints,
            padding_after_inlay_hints,
        }) = doc.inlay_hints(self.id)
        {
            let type_style = theme.and_then(|t| t.find_highlight("ui.virtual.inlay-hint.type"));
            let parameter_style =
                theme.and_then(|t| t.find_highlight("ui.virtual.inlay-hint.parameter"));
            let other_style = theme.and_then(|t| t.find_highlight("ui.virtual.inlay-hint"));

            // Overlapping annotations are ignored apart from the first so the order here is not random:
            // types -> parameters -> others should hopefully be the "correct" order for most use cases,
            // with the padding coming before and after as expected.
            text_annotations
                .add_inline_annotations(padding_before_inlay_hints, None)
                .add_inline_annotations(type_inlay_hints, type_style)
                .add_inline_annotations(parameter_inlay_hints, parameter_style)
                .add_inline_annotations(other_inlay_hints, other_style)
                .add_inline_annotations(padding_after_inlay_hints, None);
        };
        let config = doc.config.load();

        if config.lsp.display_color_swatches {
            if let Some(DocumentColorSwatches {
                color_swatches,
                colors,
                color_swatches_padding,
            }) = doc.color_swatches()
            {
                for (color_swatch, color) in color_swatches.iter().zip(colors) {
                    text_annotations
                        .add_inline_annotations(std::slice::from_ref(color_swatch), Some(*color));
                }

                text_annotations.add_inline_annotations(color_swatches_padding, None);
            }
        }

        let width = self.inner_width(doc);
        let enable_cursor_line = self
            .diagnostics_handler
            .show_cursorline_diagnostics(doc, self.id);
        let config = config.inline_diagnostics.prepare(width, enable_cursor_line);
        if !config.disabled() {
            let cursor = doc
                .selection(self.id)
                .primary()
                .cursor(doc.text().slice(..));
            if doc.text().len_bytes() >= 100_000 {
                log_run_event("view_line_annotation_attach", || {
                    format!(
                        "kind=inline_diagnostics view_id={:?} doc_id={:?} cursor={} width={}",
                        self.id,
                        doc.id(),
                        cursor,
                        width,
                    )
                });
            }
            text_annotations.add_line_annotation(InlineDiagnostics::new(
                doc,
                cursor,
                width,
                doc.view_offset(self.id).horizontal_offset,
                config,
            ));
        }

        if doc.text().len_bytes() >= 100_000 {
            log_run_event("view_line_annotation_attach", || {
                format!(
                    "kind=plugin_annotations view_id={:?} doc_id={:?} width={}",
                    self.id,
                    doc.id(),
                    width,
                )
            });
        }

        text_annotations
            .add_line_annotation(Box::new(PluginLineAnnotations::new(doc, self.id, width)));

        if let Some(fold_container) = doc.fold_container(self.id) {
            text_annotations.add_folds(fold_container);
        }

        text_annotations
    }

    pub fn fold_annotations<'a>(&self, doc: &'a Document) -> FoldAnnotations<'a> {
        FoldAnnotations::new(doc.fold_container(self.id))
    }

    pub fn text_pos_at_screen_coords(
        &self,
        doc: &Document,
        row: u16,
        column: u16,
        fmt: TextFormat,
        annotations: &TextAnnotations,
        ignore_virtual_text: bool,
    ) -> Option<usize> {
        let inner = self.inner_area(doc);
        // 1 for status
        if row < inner.top() || row >= inner.bottom() {
            return None;
        }

        if column < inner.left() || column > inner.right() {
            return None;
        }

        self.text_pos_at_visual_coords(
            doc,
            row - inner.y,
            column - inner.x,
            fmt,
            annotations,
            ignore_virtual_text,
        )
    }

    pub fn text_pos_at_visual_coords(
        &self,
        doc: &Document,
        row: u16,
        column: u16,
        text_fmt: TextFormat,
        annotations: &TextAnnotations,
        ignore_virtual_text: bool,
    ) -> Option<usize> {
        let text = doc.text().slice(..);
        let view_offset = doc.view_offset(self.id);

        let text_row = row as usize + view_offset.vertical_offset;
        let text_col = column as usize + view_offset.horizontal_offset;

        let (char_idx, virt_lines) = char_idx_at_visual_offset(
            text,
            view_offset.anchor,
            text_row as isize,
            text_col,
            &text_fmt,
            annotations,
        );

        // if the cursor is on a line with only virtual text return None
        if virt_lines != 0 && ignore_virtual_text {
            return None;
        }
        Some(char_idx)
    }

    /// Translates a screen position to position in the text document.
    /// Returns a usize typed position in bounds of the text if found in this view, None if out of view.
    pub fn pos_at_screen_coords(
        &self,
        doc: &Document,
        row: u16,
        column: u16,
        ignore_virtual_text: bool,
    ) -> Option<usize> {
        self.text_pos_at_screen_coords(
            doc,
            row,
            column,
            doc.text_format(self.inner_width(doc), None),
            &self.text_annotations(doc, None),
            ignore_virtual_text,
        )
    }

    pub fn pos_at_visual_coords(
        &self,
        doc: &Document,
        row: u16,
        column: u16,
        ignore_virtual_text: bool,
    ) -> Option<usize> {
        self.text_pos_at_visual_coords(
            doc,
            row,
            column,
            doc.text_format(self.inner_width(doc), None),
            &self.text_annotations(doc, None),
            ignore_virtual_text,
        )
    }

    /// Translates screen coordinates into coordinates on the gutter of the view.
    /// Returns a tuple of usize typed line and column numbers starting with 0.
    /// Returns None if coordinates are not on the gutter.
    pub fn gutter_coords_at_screen_coords(&self, row: u16, column: u16) -> Option<Position> {
        // 1 for status
        if row < self.area.top() || row >= self.area.bottom() {
            return None;
        }

        if column < self.area.left() || column > self.area.right() {
            return None;
        }

        Some(Position::new(
            (row - self.area.top()) as usize,
            (column - self.area.left()) as usize,
        ))
    }

    pub fn remove_document(&mut self, doc_id: &DocumentId) {
        self.history.remove_document(doc_id);
        self.docs_access_history.retain(|doc| doc != doc_id);
    }

    // pub fn traverse<F>(&self, text: RopeSlice, start: usize, end: usize, fun: F)
    // where
    //     F: Fn(usize, usize),
    // {
    //     let start = self.screen_coords_at_pos(text, start);
    //     let end = self.screen_coords_at_pos(text, end);

    //     match (start, end) {
    //         // fully on screen
    //         (Some(start), Some(end)) => {
    //             // we want to calculate ends of lines for each char..
    //         }
    //         // from start to end of screen
    //         (Some(start), None) => {}
    //         // from start of screen to end
    //         (None, Some(end)) => {}
    //         // not on screen
    //         (None, None) => return,
    //     }
    // }

    /// Applies a [`Transaction`] to the view.
    pub fn apply(&mut self, transaction: &Transaction, doc: &mut Document) {
        self.history.apply(transaction, doc);
    }

    pub fn sync_changes(&mut self, doc: &mut Document) {
        self.history.sync_changes(doc);
    }

    pub(crate) fn changes_to_sync(&mut self, doc: &mut Document) -> Option<Transaction> {
        self.history.changes_to_sync(doc)
    }
}

pub fn offset_coords_to_in_view_center_in<V, const CENTERING: bool>(
    view: &V,
    doc: &Document,
    scrolloff: usize,
) -> Option<ViewPosition>
where
    V: crate::traits::TextViewport<Document> + crate::traits::NavigableViewport<Document>,
{
    let view_offset = doc.get_view_offset(view.id())?;
    let doc_text = doc.text().slice(..);
    let viewport = view.text_area(doc);
    let vertical_viewport_end = view_offset.vertical_offset + viewport.height as usize;
    let text_fmt = doc.text_format(viewport.width, None);
    let annotations = view.text_annotations(doc);

    let (scrolloff_top, scrolloff_bottom) = if CENTERING {
        (0, 0)
    } else {
        (
            scrolloff.min(viewport.height.saturating_sub(1) as usize / 2),
            scrolloff.min(viewport.height as usize / 2),
        )
    };
    let (scrolloff_left, scrolloff_right) = if CENTERING {
        (0, 0)
    } else {
        (
            scrolloff.min(viewport.width.saturating_sub(1) as usize / 2),
            scrolloff.min(viewport.width as usize / 2),
        )
    };

    let cursor = doc.selection(view.id()).primary().cursor(doc_text);
    let mut offset = view_offset;

    if !text_fmt.soft_wrap {
        let anchor = offset.anchor.min(doc_text.len_chars());
        let anchor_line = doc_text.char_to_line(anchor);
        let top_line = anchor_line.saturating_add(offset.vertical_offset);
        let cursor_line = doc_text.char_to_line(cursor);
        let viewport_height = viewport.height as usize;

        let vertical_support = annotations.plain_viewport_support_report(top_line, cursor_line);

        let cursor_line_start = doc_text.line_to_char(cursor_line);
        let cursor_line_end = helix_core::line_ending::line_end_char_index(&doc_text, cursor_line);
        let horizontal_support =
            annotations.plain_line_seek_support(cursor_line_start, cursor_line_end, cursor);

        if matches!(vertical_support.support, PlainViewportSupport::Supported)
            && matches!(
                horizontal_support,
                helix_core::text_annotations::PlainLineSeekSupport::Supported
            )
        {
            let new_top_line = if CENTERING {
                let center_row = viewport_height.saturating_sub(1) / 2;
                cursor_line.saturating_sub(center_row)
            } else if cursor_line < top_line.saturating_add(scrolloff_top) {
                cursor_line.saturating_sub(scrolloff_top)
            } else if cursor_line.saturating_add(scrolloff_bottom) >= top_line + viewport_height {
                cursor_line
                    .saturating_add(scrolloff_bottom)
                    .saturating_add(1)
                    .saturating_sub(viewport_height)
            } else {
                top_line
            };

            if new_top_line != top_line {
                offset.anchor = doc_text.line_to_char(new_top_line);
                offset.vertical_offset = 0;
            }

            let col = plain_visual_col_at_char_idx(doc_text, cursor, text_fmt.tab_width as usize);
            let last_col = offset.horizontal_offset + viewport.width.saturating_sub(1) as usize;
            if col > last_col.saturating_sub(scrolloff_right) {
                offset.horizontal_offset += col - (last_col.saturating_sub(scrolloff_right));
            } else if col < offset.horizontal_offset + scrolloff_left {
                offset.horizontal_offset = col.saturating_sub(scrolloff_left);
            }

            if !CENTERING && offset == view_offset {
                return None;
            }

            return Some(offset);
        }

        if doc_text.len_bytes() >= 100_000 {
            log_run_event("ensure_cursor_fast_path_v2", || {
                format!(
                    "result=fallback view_id={:?} doc_id={:?} top_line={} cursor_line={} cursor={} vertical_support={} blocker={} horizontal_support={}",
                    view.id(),
                    doc.id(),
                    top_line,
                    cursor_line,
                    cursor,
                    vertical_support.support.as_str(),
                    vertical_support.blocker.unwrap_or("none"),
                    horizontal_support.as_str(),
                )
            });
        }
    }

    let off = visual_offset_from_anchor(
        doc_text,
        offset.anchor,
        cursor,
        &text_fmt,
        &annotations,
        vertical_viewport_end,
    );

    let (new_anchor, at_top) = match off {
        Ok((visual_pos, _)) if visual_pos.row < scrolloff_top + offset.vertical_offset => {
            if CENTERING {
                return None;
            }
            (true, true)
        }
        Ok((visual_pos, _)) if visual_pos.row + scrolloff_bottom >= vertical_viewport_end => {
            (true, false)
        }
        Ok((_, _)) => (false, false),
        Err(_) if CENTERING => return None,
        Err(PosBeforeAnchorRow) => (true, true),
        Err(PosAfterMaxRow) => (true, false),
    };

    if new_anchor {
        let v_off = if at_top {
            scrolloff_top as isize
        } else {
            viewport.height as isize - scrolloff_bottom as isize - 1
        };
        (offset.anchor, offset.vertical_offset) =
            char_idx_at_visual_offset(doc_text, cursor, -v_off, 0, &text_fmt, &annotations);
    }

    if text_fmt.soft_wrap {
        offset.horizontal_offset = 0;
    } else {
        let col = off
            .unwrap_or_else(|_| {
                visual_offset_from_block(doc_text, offset.anchor, cursor, &text_fmt, &annotations)
            })
            .0
            .col;

        let last_col = offset.horizontal_offset + viewport.width.saturating_sub(1) as usize;
        if col > last_col.saturating_sub(scrolloff_right) {
            offset.horizontal_offset += col - (last_col.saturating_sub(scrolloff_right))
        } else if col < offset.horizontal_offset + scrolloff_left {
            offset.horizontal_offset = col.saturating_sub(scrolloff_left)
        };
    }

    if !CENTERING && offset == view_offset {
        return None;
    }

    Some(offset)
}

pub fn ensure_cursor_in_view_in<V>(view: &V, doc: &mut Document, scrolloff: usize)
where
    V: crate::traits::TextViewport<Document> + crate::traits::NavigableViewport<Document>,
{
    if let Some(offset) = offset_coords_to_in_view_center_in::<V, false>(view, doc, scrolloff) {
        view.set_view_offset(doc, offset);
    }
}

pub fn ensure_cursor_in_view_center_in<V>(view: &V, doc: &mut Document, scrolloff: usize)
where
    V: crate::traits::TextViewport<Document> + crate::traits::NavigableViewport<Document>,
{
    if let Some(offset) = offset_coords_to_in_view_center_in::<V, true>(view, doc, scrolloff) {
        view.set_view_offset(doc, offset);
    } else {
        align_view_in(doc, view, Align::Center);
    }
}

/// Store a jump on the jumplist. Call before changing selection for motions that should be
/// reversible via the jumplist (e.g. goto file start/end, goto line).
pub fn push_jump<V, D>(view: &mut V, doc: &mut D)
where
    V: crate::traits::Jumpable<D>,
{
    view.push_jump(doc);
}

// ---------------------------------------------------------------------------
// Trait impls (helix-view::traits)
// ---------------------------------------------------------------------------

impl crate::traits::Identified for View {
    fn id(&self) -> ViewId {
        self.id
    }
}

impl crate::traits::Identified for ComponentViewState {
    fn id(&self) -> ViewId {
        self.id
    }
}

impl crate::traits::Identified for AnyViewMut<'_> {
    fn id(&self) -> ViewId {
        match self {
            Self::Tree(view) => view.id,
            Self::Component(view) => view.id,
        }
    }
}

impl crate::traits::Bounded for View {
    fn area(&self) -> Rect {
        self.area
    }

    fn set_area(&mut self, area: Rect) {
        self.area = area;
    }
}

impl crate::traits::Bounded for ComponentViewState {
    fn area(&self) -> Rect {
        self.area
    }

    fn set_area(&mut self, area: Rect) {
        self.area = area;
    }
}

impl crate::traits::Bounded for AnyViewMut<'_> {
    fn area(&self) -> Rect {
        match self {
            Self::Tree(view) => view.area(),
            Self::Component(view) => view.area(),
        }
    }

    fn set_area(&mut self, area: Rect) {
        match self {
            Self::Tree(view) => view.set_area(area),
            Self::Component(view) => view.set_area(area),
        }
    }
}

impl crate::traits::NavigableViewport<crate::Document> for View {
    fn text_area_width(&self, doc: &crate::Document) -> u16 {
        self.inner_width(doc)
    }

    fn text_annotations<'a>(
        &self,
        doc: &'a crate::Document,
    ) -> helix_core::text_annotations::TextAnnotations<'a> {
        View::text_annotations(self, doc, None)
    }
}

impl crate::traits::NavigableViewport<crate::Document> for ComponentViewState {
    fn text_area_width(&self, _doc: &crate::Document) -> u16 {
        self.area.width
    }

    fn text_annotations<'a>(
        &self,
        _doc: &'a crate::Document,
    ) -> helix_core::text_annotations::TextAnnotations<'a> {
        helix_core::text_annotations::TextAnnotations::default()
    }
}

impl crate::traits::NavigableViewport<crate::Document> for AnyViewMut<'_> {
    fn text_area_width(&self, doc: &crate::Document) -> u16 {
        match self {
            Self::Tree(view) => view.text_area_width(doc),
            Self::Component(view) => view.text_area_width(doc),
        }
    }

    fn text_annotations<'a>(
        &self,
        doc: &'a crate::Document,
    ) -> helix_core::text_annotations::TextAnnotations<'a> {
        match self {
            Self::Tree(view) => view.text_annotations(doc, None),
            Self::Component(view) => view.text_annotations(doc),
        }
    }
}

impl crate::traits::TextViewport<crate::Document> for View {
    fn text_area(&self, doc: &crate::Document) -> Rect {
        self.inner_area(doc)
    }

    fn view_offset(&self, doc: &crate::Document) -> ViewPosition {
        doc.view_offset(self.id)
    }

    fn set_view_offset(&self, doc: &mut crate::Document, pos: ViewPosition) {
        doc.set_view_offset(self.id, pos);
    }
}

impl crate::traits::TextViewport<crate::Document> for ComponentViewState {
    fn text_area(&self, _doc: &crate::Document) -> Rect {
        self.area
    }

    fn view_offset(&self, doc: &crate::Document) -> ViewPosition {
        doc.view_offset(self.id)
    }

    fn set_view_offset(&self, doc: &mut crate::Document, pos: ViewPosition) {
        doc.set_view_offset(self.id, pos);
    }
}

impl crate::traits::TextViewport<crate::Document> for AnyViewMut<'_> {
    fn text_area(&self, doc: &crate::Document) -> Rect {
        match self {
            Self::Tree(view) => view.text_area(doc),
            Self::Component(view) => view.text_area(doc),
        }
    }

    fn view_offset(&self, doc: &crate::Document) -> ViewPosition {
        match self {
            Self::Tree(view) => view.view_offset(doc),
            Self::Component(view) => view.view_offset(doc),
        }
    }

    fn set_view_offset(&self, doc: &mut crate::Document, pos: ViewPosition) {
        match self {
            Self::Tree(view) => view.set_view_offset(doc, pos),
            Self::Component(view) => view.set_view_offset(doc, pos),
        }
    }
}

impl crate::traits::HistoryViewport<crate::Document> for View {
    fn apply_history_transaction(&mut self, transaction: &Transaction, doc: &mut crate::Document) {
        self.history.apply(transaction, doc);
    }

    fn sync_changes(&mut self, doc: &mut crate::Document) {
        self.history.sync_changes(doc);
    }
}

impl crate::traits::HistoryViewport<crate::Document> for ComponentViewState {
    fn apply_history_transaction(&mut self, transaction: &Transaction, doc: &mut crate::Document) {
        self.history.apply(transaction, doc);
    }

    fn sync_changes(&mut self, doc: &mut crate::Document) {
        self.history.sync_changes(doc);
    }
}

impl crate::traits::HistoryViewport<crate::Document> for AnyViewMut<'_> {
    fn apply_history_transaction(&mut self, transaction: &Transaction, doc: &mut crate::Document) {
        match self {
            Self::Tree(view) => view.apply_history_transaction(transaction, doc),
            Self::Component(view) => view.apply_history_transaction(transaction, doc),
        }
    }

    fn sync_changes(&mut self, doc: &mut crate::Document) {
        match self {
            Self::Tree(view) => view.sync_changes(doc),
            Self::Component(view) => view.sync_changes(doc),
        }
    }
}

impl crate::traits::Jumpable<crate::Document> for View {
    fn push_jump(&mut self, doc: &mut crate::Document) {
        doc.append_changes_to_history(self);
        self.history
            .jumps
            .push((doc.id(), doc.selection(self.id).clone()));
    }
}

impl crate::traits::Jumpable<crate::Document> for ComponentViewState {
    fn push_jump(&mut self, doc: &mut crate::Document) {
        doc.append_changes_to_history(self);
        self.history
            .jumps
            .push((doc.id(), doc.selection(self.id).clone()));
    }
}

impl crate::traits::Jumpable<crate::Document> for AnyViewMut<'_> {
    fn push_jump(&mut self, doc: &mut crate::Document) {
        match self {
            Self::Tree(view) => view.push_jump(doc),
            Self::Component(view) => view.push_jump(doc),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn make_layout_inputs() -> ViewLayoutInputs {
        ViewLayoutInputs {
            doc_id: crate::DocumentId::default(),
            doc_version: 1,
            view_position: ViewPosition {
                anchor: 0,
                horizontal_offset: 0,
                vertical_offset: 0,
            },
            area: Rect::new(0, 0, 80, 24),
            config_gen: 0,
            annotation: crate::presentation_state::AnnotationSnapshot::new(
                crate::Revision::default(),
            ),
        }
    }

    fn make_paint_inputs() -> ViewPaintInputs {
        ViewPaintInputs {
            layout: make_layout_inputs(),
            syntax: crate::syntax_aware::SyntaxSnapshot::new(
                crate::Revision::default(),
                crate::syntax_aware::SyntaxStatus::Disabled,
            ),
            theme_name: "default".into(),
            cursor_line: 0,
            gutter: crate::document::GutterSnapshot::new(crate::Revision::default()),
        }
    }

    #[test]
    fn layout_inputs_equality() {
        let a = make_layout_inputs();
        let b = make_layout_inputs();
        assert_eq!(a, b);
    }

    #[test]
    fn layout_inputs_differ_on_doc_version() {
        let a = make_layout_inputs();
        let mut b = make_layout_inputs();
        b.doc_version = 2;
        assert_ne!(a, b);
    }

    #[test]
    fn paint_inputs_differ_on_syntax_snapshot() {
        let a = make_paint_inputs();
        let mut b = make_paint_inputs();
        b.syntax = crate::syntax_aware::SyntaxSnapshot::new(
            crate::Revision::from(1),
            crate::syntax_aware::SyntaxStatus::Fresh,
        );
        assert_ne!(a, b);
    }

    #[test]
    fn paint_inputs_differ_on_syntax_status() {
        let mut a = make_paint_inputs();
        let mut b = make_paint_inputs();
        a.syntax = crate::syntax_aware::SyntaxSnapshot::new(
            crate::Revision::from(7),
            crate::syntax_aware::SyntaxStatus::Fresh,
        );
        b.syntax = crate::syntax_aware::SyntaxSnapshot::new(
            crate::Revision::from(7),
            crate::syntax_aware::SyntaxStatus::StalePendingRefresh,
        );
        assert_ne!(a, b);
    }

    #[test]
    fn layout_inputs_differ_on_scroll() {
        let a = make_layout_inputs();
        let mut b = make_layout_inputs();
        b.view_position.anchor = 100;
        assert_ne!(a, b);
    }

    #[test]
    fn paint_inputs_differ_on_theme() {
        let a = make_paint_inputs();
        let mut b = make_paint_inputs();
        b.theme_name = "gruvbox".into();
        assert_ne!(a, b);
    }

    #[test]
    fn paint_inputs_differ_on_gutter_shape() {
        let mut a = make_paint_inputs();
        let mut b = make_paint_inputs();
        a.gutter = crate::document::GutterSnapshot::with_state(crate::Revision::from(5), 0, false);
        b.gutter = crate::document::GutterSnapshot::with_state(crate::Revision::from(5), 2, false);
        assert_ne!(a, b);
    }

    #[test]
    fn render_plan_reuses_matching_paint_inputs() {
        let inputs = RenderInputs {
            layout: make_layout_inputs(),
            paint: make_paint_inputs(),
        };
        let line_map = LineMap {
            lines: Arc::from([
                VisualLineInfo {
                    visual_row: 0,
                    doc_line: 0,
                    char_range_start: 0,
                    char_range_end: 10,
                    visible_char_start: 0,
                    visible_col_start: 0,
                    visible_char_last: 9,
                    visible_col_last: 9,
                    horizontal_checkpoints: Vec::new(),
                },
                VisualLineInfo {
                    visual_row: 1,
                    doc_line: 1,
                    char_range_start: 10,
                    char_range_end: 20,
                    visible_char_start: 10,
                    visible_col_start: 0,
                    visible_char_last: 19,
                    visible_col_last: 9,
                    horizontal_checkpoints: Vec::new(),
                },
            ]),
        };
        let selection = Selection::point(0);
        let overlay_fingerprints =
            line_map.overlay_fingerprints(&selection, Mode::Normal, true, true);
        let cached = RenderSnapshots {
            layout: LayoutSnapshot::new(inputs.layout.clone(), line_map),
            paint: PaintSnapshot::new(
                inputs.paint.clone(),
                SyntaxStyleCache::default(),
                overlay_fingerprints.clone(),
            ),
        };

        match inputs.plan(Some(cached.as_ref()), &selection, Mode::Normal, true, true) {
            RenderPlan::Reuse(plan) => {
                assert!(std::ptr::eq(plan.cached.layout, &cached.layout));
                assert!(std::ptr::eq(plan.cached.paint, &cached.paint));
                assert!(plan.dirty_rows.is_empty());
                assert_eq!(
                    plan.overlay_fingerprints.as_ref(),
                    overlay_fingerprints.as_ref()
                );
            }
            RenderPlan::Refresh(_) => panic!("expected render reuse"),
        }
    }

    #[test]
    fn render_plan_refreshes_with_seed_when_paint_changes() {
        let inputs = RenderInputs {
            layout: make_layout_inputs(),
            paint: make_paint_inputs(),
        };
        let mut cached_paint = inputs.paint.clone();
        cached_paint.theme_name = "gruvbox".into();
        let line_map = LineMap {
            lines: Arc::from([VisualLineInfo {
                visual_row: 0,
                doc_line: 0,
                char_range_start: 0,
                char_range_end: 10,
                visible_char_start: 0,
                visible_col_start: 0,
                visible_char_last: 9,
                visible_col_last: 9,
                horizontal_checkpoints: Vec::new(),
            }]),
        };
        let cached = RenderSnapshots {
            layout: LayoutSnapshot::new(inputs.layout.clone(), line_map.clone()),
            paint: PaintSnapshot::new(cached_paint, SyntaxStyleCache::default(), Arc::from([33])),
        };
        let selection = Selection::point(0);

        match inputs.plan(Some(cached.as_ref()), &selection, Mode::Normal, true, true) {
            RenderPlan::Refresh(plan) => {
                let seed = plan.seed_line_map.expect("expected cached seed line map");
                assert!(std::ptr::eq(seed, &cached.layout.line_map));
                assert_eq!(seed.lines.len(), line_map.lines.len());
            }
            RenderPlan::Reuse(_) => panic!("expected render refresh"),
        }
    }

    #[test]
    fn render_plan_refreshes_without_seed_when_layout_changes() {
        let inputs = RenderInputs {
            layout: make_layout_inputs(),
            paint: make_paint_inputs(),
        };
        let mut cached_layout = inputs.layout.clone();
        cached_layout.area.width += 1;
        let cached = RenderSnapshots {
            layout: LayoutSnapshot::new(cached_layout.clone(), LineMap::default()),
            paint: PaintSnapshot::new(
                ViewPaintInputs {
                    layout: cached_layout,
                    ..inputs.paint.clone()
                },
                SyntaxStyleCache::default(),
                Arc::from([]),
            ),
        };
        let selection = Selection::point(0);

        match inputs.plan(Some(cached.as_ref()), &selection, Mode::Normal, true, true) {
            RenderPlan::Refresh(plan) => assert!(plan.seed_line_map.is_none()),
            RenderPlan::Reuse(_) => panic!("expected render refresh"),
        }
    }

    #[test]
    fn layout_inputs_differ_on_config() {
        let a = make_layout_inputs();
        let mut b = make_layout_inputs();
        b.config_gen = 1;
        assert_ne!(a, b);
    }

    #[test]
    fn layout_inputs_differ_on_annotation_shape() {
        let mut a = make_layout_inputs();
        let mut b = make_layout_inputs();
        a.annotation = crate::presentation_state::AnnotationSnapshot::with_state(
            crate::Revision::from(3),
            false,
            1,
            0,
            0,
            0,
        );
        b.annotation = crate::presentation_state::AnnotationSnapshot::with_state(
            crate::Revision::from(3),
            true,
            1,
            0,
            0,
            0,
        );
        assert_ne!(a, b);
    }

    #[test]
    fn layout_inputs_differ_on_area() {
        let a = make_layout_inputs();
        let mut b = make_layout_inputs();
        b.area = Rect::new(0, 0, 120, 40);
        assert_ne!(a, b);
    }

    #[test]
    fn line_map_dirty_rows_detects_changes() {
        let old = vec![100, 200, 300, 400, 500];
        let new = vec![100, 200, 999, 400, 500];
        let dirty = LineMap::dirty_rows(&old, &new);
        assert_eq!(dirty.len(), 1);
        assert!(dirty.contains(&2));
    }

    #[test]
    fn line_map_dirty_rows_all_same() {
        let fps = vec![10, 20, 30];
        let dirty = LineMap::dirty_rows(&fps, &fps);
        assert!(dirty.is_empty());
    }

    #[test]
    fn line_map_dirty_rows_different_lengths() {
        let old = vec![1, 2, 3];
        let new = vec![1, 2, 3, 4];
        let dirty = LineMap::dirty_rows(&old, &new);
        assert_eq!(dirty.len(), 1);
        assert!(dirty.contains(&3));
    }

    #[test]
    fn overlay_fingerprints_change_on_cursor_move() {
        let line_map = LineMap {
            lines: Arc::from(vec![
                VisualLineInfo {
                    visual_row: 0,
                    doc_line: 0,
                    char_range_start: 0,
                    char_range_end: 20,
                    visible_char_start: 0,
                    visible_col_start: 0,
                    visible_char_last: 19,
                    visible_col_last: 19,
                    horizontal_checkpoints: Vec::new(),
                },
                VisualLineInfo {
                    visual_row: 1,
                    doc_line: 1,
                    char_range_start: 20,
                    char_range_end: 40,
                    visible_char_start: 20,
                    visible_col_start: 0,
                    visible_char_last: 39,
                    visible_col_last: 19,
                    horizontal_checkpoints: Vec::new(),
                },
                VisualLineInfo {
                    visual_row: 2,
                    doc_line: 2,
                    char_range_start: 40,
                    char_range_end: 60,
                    visible_char_start: 40,
                    visible_col_start: 0,
                    visible_char_last: 59,
                    visible_col_last: 19,
                    horizontal_checkpoints: Vec::new(),
                },
            ]),
        };

        let sel_a = Selection::point(5); // cursor on line 0
        let sel_b = Selection::point(25); // cursor on line 1

        let fp_a = line_map.overlay_fingerprints(&sel_a, Mode::Normal, true, true);
        let fp_b = line_map.overlay_fingerprints(&sel_b, Mode::Normal, true, true);

        // Line 0 and 1 should differ (cursor moved between them)
        assert_ne!(fp_a[0], fp_b[0]);
        assert_ne!(fp_a[1], fp_b[1]);
        // Line 2 should be the same (cursor not on either)
        assert_eq!(fp_a[2], fp_b[2]);
    }

    #[test]
    fn overlay_fingerprints_change_on_mode() {
        let line_map = LineMap {
            lines: Arc::from(vec![VisualLineInfo {
                visual_row: 0,
                doc_line: 0,
                char_range_start: 0,
                char_range_end: 20,
                visible_char_start: 0,
                visible_col_start: 0,
                visible_char_last: 19,
                visible_col_last: 19,
                horizontal_checkpoints: Vec::new(),
            }]),
        };
        let sel = Selection::point(5);
        let fp_normal = line_map.overlay_fingerprints(&sel, Mode::Normal, true, true);
        let fp_insert = line_map.overlay_fingerprints(&sel, Mode::Insert, true, true);
        assert_ne!(fp_normal[0], fp_insert[0]);
    }

    #[test]
    fn best_horizontal_checkpoint_respects_gap_limit() {
        let line_map = LineMap {
            lines: Arc::from(vec![VisualLineInfo {
                visual_row: 0,
                doc_line: 7,
                char_range_start: 0,
                char_range_end: 300_000,
                visible_char_start: 143_200,
                visible_col_start: 143_200,
                visible_char_last: 143_237,
                visible_col_last: 143_237,
                horizontal_checkpoints: vec![HorizontalCheckpoint {
                    char_idx: 139_264,
                    visual_col: 139_264,
                }],
            }]),
        };

        assert_eq!(
            line_map.best_horizontal_checkpoint_within_gap(7, 143_296, 4_096),
            Some(HorizontalCheckpoint {
                char_idx: 143_237,
                visual_col: 143_237,
            })
        );
        assert_eq!(
            line_map.best_horizontal_checkpoint_within_gap(7, 290_989, 4_096),
            None
        );
    }

    use arc_swap::ArcSwap;
    use helix_core::{syntax, Rope};

    // 1 diagnostic + 1 spacer + 3 linenr (< 1000 lines) + 1 spacer + 1 diff
    const DEFAULT_GUTTER_OFFSET: u16 = 7;

    // 1 diagnostics + 1 spacer + 1 gutter
    const DEFAULT_GUTTER_OFFSET_ONLY_DIAGNOSTICS: u16 = 3;

    use crate::document::Document;
    use crate::editor::{Config, GutterConfig, GutterLineNumbersConfig, GutterType};

    #[test]
    fn test_text_pos_at_screen_coords() {
        let mut view = View::new(DocumentId::default(), GutterConfig::default());
        view.area = Rect::new(40, 40, 40, 40);
        let rope = Rope::from_str("abc\n\tdef");
        let mut doc = Document::from(
            rope,
            None,
            Arc::new(ArcSwap::new(Arc::new(Config::default()))),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        );
        doc.ensure_view_init(view.id);

        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                40,
                2,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            None
        );

        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                40,
                41,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            None
        );

        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                0,
                2,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            None
        );

        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                0,
                49,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            None
        );

        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                0,
                41,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            None
        );

        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                40,
                81,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            None
        );

        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                78,
                41,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            None
        );

        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                40,
                40 + DEFAULT_GUTTER_OFFSET + 3,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            Some(3)
        );

        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                40,
                80,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            Some(3)
        );

        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                41,
                40 + DEFAULT_GUTTER_OFFSET + 1,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            Some(4)
        );

        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                41,
                40 + DEFAULT_GUTTER_OFFSET + 4,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            Some(5)
        );

        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                41,
                40 + DEFAULT_GUTTER_OFFSET + 7,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            Some(8)
        );

        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                41,
                80,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            Some(8)
        );
    }

    #[test]
    fn test_text_pos_at_screen_coords_without_line_numbers_gutter() {
        let mut view = View::new(
            DocumentId::default(),
            GutterConfig {
                layout: vec![GutterType::Diagnostics],
                line_numbers: GutterLineNumbersConfig::default(),
            },
        );
        view.area = Rect::new(40, 40, 40, 40);
        let rope = Rope::from_str("abc\n\tdef");
        let mut doc = Document::from(
            rope,
            None,
            Arc::new(ArcSwap::new(Arc::new(Config::default()))),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        );
        doc.ensure_view_init(view.id);
        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                41,
                40 + DEFAULT_GUTTER_OFFSET_ONLY_DIAGNOSTICS + 1,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            Some(4)
        );
    }

    #[test]
    fn test_text_pos_at_screen_coords_without_any_gutters() {
        let mut view = View::new(
            DocumentId::default(),
            GutterConfig {
                layout: vec![],
                line_numbers: GutterLineNumbersConfig::default(),
            },
        );
        view.area = Rect::new(40, 40, 40, 40);
        let rope = Rope::from_str("abc\n\tdef");
        let mut doc = Document::from(
            rope,
            None,
            Arc::new(ArcSwap::new(Arc::new(Config::default()))),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        );
        doc.ensure_view_init(view.id);
        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                41,
                40 + 1,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            Some(4)
        );
    }

    #[test]
    fn test_text_pos_at_screen_coords_cjk() {
        let mut view = View::new(DocumentId::default(), GutterConfig::default());
        view.area = Rect::new(40, 40, 40, 40);
        let rope = Rope::from_str("Hi! こんにちは皆さん");
        let mut doc = Document::from(
            rope,
            None,
            Arc::new(ArcSwap::new(Arc::new(Config::default()))),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        );
        doc.ensure_view_init(view.id);

        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                40,
                40 + DEFAULT_GUTTER_OFFSET,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            Some(0)
        );

        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                40,
                40 + DEFAULT_GUTTER_OFFSET + 4,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            Some(4)
        );
        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                40,
                40 + DEFAULT_GUTTER_OFFSET + 5,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            Some(4)
        );

        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                40,
                40 + DEFAULT_GUTTER_OFFSET + 6,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            Some(5)
        );

        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                40,
                40 + DEFAULT_GUTTER_OFFSET + 7,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            Some(5)
        );

        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                40,
                40 + DEFAULT_GUTTER_OFFSET + 8,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            Some(6)
        );
    }

    #[test]
    fn test_text_pos_at_screen_coords_graphemes() {
        let mut view = View::new(DocumentId::default(), GutterConfig::default());
        view.area = Rect::new(40, 40, 40, 40);
        let rope = Rope::from_str("Hèl̀l̀ò world!");
        let mut doc = Document::from(
            rope,
            None,
            Arc::new(ArcSwap::new(Arc::new(Config::default()))),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        );
        doc.ensure_view_init(view.id);

        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                40,
                40 + DEFAULT_GUTTER_OFFSET,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            Some(0)
        );

        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                40,
                40 + DEFAULT_GUTTER_OFFSET + 1,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            Some(1)
        );

        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                40,
                40 + DEFAULT_GUTTER_OFFSET + 2,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            Some(3)
        );

        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                40,
                40 + DEFAULT_GUTTER_OFFSET + 3,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            Some(5)
        );

        assert_eq!(
            view.text_pos_at_screen_coords(
                &doc,
                40,
                40 + DEFAULT_GUTTER_OFFSET + 4,
                TextFormat::default(),
                &TextAnnotations::default(),
                true
            ),
            Some(7)
        );
    }
}
