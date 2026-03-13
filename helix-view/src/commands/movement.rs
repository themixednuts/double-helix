//! Movement commands that operate on editor state.
//!
//! These are frontend-agnostic: they only mutate Editor/Document/View state.
//! helix-term wraps them with Context (count, register, etc.) and keybindings.

use helix_core::{
    char_idx_at_visual_offset,
    doc_formatter::TextFormat,
    graphemes::{self, next_folded_grapheme_boundary, prev_folded_grapheme_boundary},
    line_ending::line_end_char_index,
    movement::{
        move_horizontally, move_next_paragraph, move_prev_paragraph, move_vertically,
        move_vertically_visual, Direction, Movement,
    },
    search::{self, GraphemeMatcher},
    text_annotations::TextAnnotations,
    text_folding::RopeSliceFoldExt,
    Range, RopeSlice,
};
use helix_stdx::rope::RopeSliceExt;

use crate::document::Mode;
use crate::traits::{
    FormattableText, Identified, Modal, NavigableViewport, Selectable, TextContent, TextViewport,
};
use crate::view::push_jump;
use crate::{DocumentId, Editor, ViewId};

pub type DirectionalMoveFn =
    fn(RopeSlice, Range, Direction, usize, Movement, &TextFormat, &mut TextAnnotations) -> Range;

/// Core primitive for directional movements (char, line, visual line).
///
/// Takes explicit `Movement` — the engine controls Move vs Extend.
pub fn directional_move_in<V, D>(
    target: &V,
    doc: &mut D,
    count: usize,
    move_fn: DirectionalMoveFn,
    dir: Direction,
    movement: Movement,
) where
    V: Identified + NavigableViewport<D>,
    D: FormattableText + Selectable,
{
    let text = doc.text().slice(..);
    let text_fmt = doc.text_format(target.text_area_width(doc));
    let mut annotations = target.text_annotations(doc);

    let selection = doc.selection(target.id()).clone().transform(|range| {
        move_fn(
            text,
            range,
            dir,
            count,
            movement,
            &text_fmt,
            &mut annotations,
        )
    });
    drop(annotations);
    doc.set_selection(target.id(), selection);
}

/// Core primitive for directional movements (char, line, visual line).
///
/// Takes explicit `Movement` — the engine controls Move vs Extend.
pub fn directional_move(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    move_fn: DirectionalMoveFn,
    dir: Direction,
    movement: Movement,
) {
    editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
        directional_move_in(view, doc, count, move_fn, dir, movement);
    });
}

pub type WordMoveFn = fn(RopeSlice, &TextAnnotations, Range, usize) -> Range;

/// Core primitive for word-class movements (word, WORD, sub-word).
///
/// Move replaces the range; Extend keeps the anchor and moves the head.
pub fn word_move_in<V, D>(
    target: &V,
    doc: &mut D,
    count: usize,
    word_fn: WordMoveFn,
    movement: Movement,
) where
    V: Identified + NavigableViewport<D>,
    D: TextContent + Selectable,
{
    let text = doc.text().slice(..);
    let annotations = target.text_annotations(doc);

    let selection = doc.selection(target.id()).clone().transform(|range| {
        let result = word_fn(text, &annotations, range, count);
        match movement {
            Movement::Move => result,
            Movement::Extend => {
                let pos = result.cursor(text);
                range.put_cursor(text, pos, true)
            }
        }
    });
    drop(annotations);
    doc.set_selection(target.id(), selection);
}

/// Core primitive for word-class movements (word, WORD, sub-word).
///
/// Move replaces the range; Extend keeps the anchor and moves the head.
pub fn word_move(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    word_fn: WordMoveFn,
    movement: Movement,
) {
    editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
        word_move_in(view, doc, count, word_fn, movement);
    });
}

/// Derive `Movement` from the current editor mode.
///
/// Select mode → Extend, Normal/Insert → Move.
pub fn movement_from_mode_in(modal: &impl Modal) -> Movement {
    if modal.mode() == Mode::Select {
        Movement::Extend
    } else {
        Movement::Move
    }
}

/// Derive `Movement` from the current editor mode.
///
/// Select mode → Extend, Normal/Insert → Move.
pub fn movement_from_mode(editor: &Editor) -> Movement {
    movement_from_mode_in(editor)
}

// ─── Canonical directional movements (engine-facing) ─────────────────

/// Move cursor left by `count` graphemes.
#[inline]
pub fn char_left(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    movement: Movement,
) {
    directional_move(
        editor,
        view_id,
        doc_id,
        count,
        move_horizontally,
        Direction::Backward,
        movement,
    );
}

/// Move cursor right by `count` graphemes.
#[inline]
pub fn char_right(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    movement: Movement,
) {
    directional_move(
        editor,
        view_id,
        doc_id,
        count,
        move_horizontally,
        Direction::Forward,
        movement,
    );
}

/// Move cursor up by `count` lines.
#[inline]
pub fn line_up(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    movement: Movement,
) {
    directional_move(
        editor,
        view_id,
        doc_id,
        count,
        move_vertically,
        Direction::Backward,
        movement,
    );
}

/// Move cursor down by `count` lines.
#[inline]
pub fn line_down(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    movement: Movement,
) {
    directional_move(
        editor,
        view_id,
        doc_id,
        count,
        move_vertically,
        Direction::Forward,
        movement,
    );
}

/// Move cursor up by `count` visual lines.
#[inline]
pub fn visual_line_up(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    movement: Movement,
) {
    directional_move(
        editor,
        view_id,
        doc_id,
        count,
        move_vertically_visual,
        Direction::Backward,
        movement,
    );
}

/// Move cursor down by `count` visual lines.
#[inline]
pub fn visual_line_down(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    movement: Movement,
) {
    directional_move(
        editor,
        view_id,
        doc_id,
        count,
        move_vertically_visual,
        Direction::Forward,
        movement,
    );
}

// ─── Legacy wrappers (hardcoded Move/Extend) ─────────────────────────

#[inline]
pub fn move_char_left(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    char_left(editor, view_id, doc_id, count, Movement::Move);
}
#[inline]
pub fn move_char_right(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    char_right(editor, view_id, doc_id, count, Movement::Move);
}
#[inline]
pub fn move_line_up(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    line_up(editor, view_id, doc_id, count, Movement::Move);
}
#[inline]
pub fn move_line_down(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    line_down(editor, view_id, doc_id, count, Movement::Move);
}
#[inline]
pub fn move_visual_line_up(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    visual_line_up(editor, view_id, doc_id, count, Movement::Move);
}
#[inline]
pub fn move_visual_line_down(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    visual_line_down(editor, view_id, doc_id, count, Movement::Move);
}
#[inline]
pub fn extend_char_left(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    char_left(editor, view_id, doc_id, count, Movement::Extend);
}
#[inline]
pub fn extend_char_right(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    char_right(editor, view_id, doc_id, count, Movement::Extend);
}
#[inline]
pub fn extend_line_up(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    line_up(editor, view_id, doc_id, count, Movement::Extend);
}
#[inline]
pub fn extend_line_down(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    line_down(editor, view_id, doc_id, count, Movement::Extend);
}
#[inline]
pub fn extend_visual_line_up(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    visual_line_up(editor, view_id, doc_id, count, Movement::Extend);
}
#[inline]
pub fn extend_visual_line_down(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    visual_line_down(editor, view_id, doc_id, count, Movement::Extend);
}

// --- Goto line start/end ---

pub fn goto_line_end_in(
    target: &impl Identified,
    doc: &mut (impl TextContent + Selectable),
    movement: Movement,
) {
    let text = doc.text().slice(..);

    let selection = doc.selection(target.id()).clone().transform(|range| {
        let line = range.cursor_line(text);
        let line_start = text.line_to_char(line);

        let pos = graphemes::prev_grapheme_boundary(text, line_end_char_index(&text, line))
            .max(line_start);

        range.put_cursor(text, pos, movement == Movement::Extend)
    });
    doc.set_selection(target.id(), selection);
}

/// Goto line end; movement (Move vs Extend) is derived from current mode.
pub fn goto_line_end(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    let movement = movement_from_mode(editor);
    let doc = crate::doc_mut!(editor, &doc_id);
    goto_line_end_in(&view_id, doc, movement);
}

/// Goto line end with explicit movement behaviour (e.g. for extend_to_line_end).
pub fn goto_line_end_with_movement(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    movement: Movement,
) {
    let doc = crate::doc_mut!(editor, &doc_id);
    goto_line_end_in(&view_id, doc, movement);
}

// --- Goto line end newline ---

pub fn goto_line_end_newline_in(
    target: &impl Identified,
    doc: &mut (impl TextContent + Selectable),
    movement: Movement,
) {
    let text = doc.text().slice(..);

    let selection = doc.selection(target.id()).clone().transform(|range| {
        let line = range.cursor_line(text);
        let pos = line_end_char_index(&text, line);

        range.put_cursor(text, pos, movement == Movement::Extend)
    });
    doc.set_selection(target.id(), selection);
}

pub fn goto_line_end_newline(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    let movement = movement_from_mode(editor);
    let doc = crate::doc_mut!(editor, &doc_id);
    goto_line_end_newline_in(&view_id, doc, movement);
}

pub fn goto_line_end_newline_with_movement(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    movement: Movement,
) {
    let doc = crate::doc_mut!(editor, &doc_id);
    goto_line_end_newline_in(&view_id, doc, movement);
}

// --- Goto line start ---

pub fn goto_line_start_in(
    target: &impl Identified,
    doc: &mut (impl TextContent + Selectable),
    movement: Movement,
) {
    let text = doc.text().slice(..);

    let selection = doc.selection(target.id()).clone().transform(|range| {
        let line = range.cursor_line(text);
        let pos = text.line_to_char(line);
        range.put_cursor(text, pos, movement == Movement::Extend)
    });
    doc.set_selection(target.id(), selection);
}

pub fn goto_line_start(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    let movement = movement_from_mode(editor);
    let doc = crate::doc_mut!(editor, &doc_id);
    goto_line_start_in(&view_id, doc, movement);
}

pub fn goto_line_start_with_movement(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    movement: Movement,
) {
    let doc = crate::doc_mut!(editor, &doc_id);
    goto_line_start_in(&view_id, doc, movement);
}

// --- Goto first nonwhitespace ---

pub fn goto_first_nonwhitespace_in(
    target: &impl Identified,
    doc: &mut (impl TextContent + Selectable),
    movement: Movement,
) {
    let text = doc.text().slice(..);

    let selection = doc.selection(target.id()).clone().transform(|range| {
        let line = range.cursor_line(text);

        if let Some(pos) = text.line(line).first_non_whitespace_char() {
            let pos = pos + text.line_to_char(line);
            range.put_cursor(text, pos, movement == Movement::Extend)
        } else {
            range
        }
    });
    doc.set_selection(target.id(), selection);
}

pub fn goto_first_nonwhitespace(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    let movement = movement_from_mode(editor);
    let doc = crate::doc_mut!(editor, &doc_id);
    goto_first_nonwhitespace_in(&view_id, doc, movement);
}

pub fn goto_first_nonwhitespace_with_movement(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    movement: Movement,
) {
    let doc = crate::doc_mut!(editor, &doc_id);
    goto_first_nonwhitespace_in(&view_id, doc, movement);
}

// ─── Canonical word movements (engine-facing) ───────────────────────

#[inline]
pub fn next_word_start(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    movement: Movement,
) {
    word_move(
        editor,
        view_id,
        doc_id,
        count,
        helix_core::movement::move_next_word_start,
        movement,
    );
}
#[inline]
pub fn prev_word_start(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    movement: Movement,
) {
    word_move(
        editor,
        view_id,
        doc_id,
        count,
        helix_core::movement::move_prev_word_start,
        movement,
    );
}
#[inline]
pub fn next_word_end(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    movement: Movement,
) {
    word_move(
        editor,
        view_id,
        doc_id,
        count,
        helix_core::movement::move_next_word_end,
        movement,
    );
}
#[inline]
pub fn prev_word_end(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    movement: Movement,
) {
    word_move(
        editor,
        view_id,
        doc_id,
        count,
        helix_core::movement::move_prev_word_end,
        movement,
    );
}
#[inline]
pub fn next_long_word_start(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    movement: Movement,
) {
    word_move(
        editor,
        view_id,
        doc_id,
        count,
        helix_core::movement::move_next_long_word_start,
        movement,
    );
}
#[inline]
pub fn prev_long_word_start(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    movement: Movement,
) {
    word_move(
        editor,
        view_id,
        doc_id,
        count,
        helix_core::movement::move_prev_long_word_start,
        movement,
    );
}
#[inline]
pub fn next_long_word_end(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    movement: Movement,
) {
    word_move(
        editor,
        view_id,
        doc_id,
        count,
        helix_core::movement::move_next_long_word_end,
        movement,
    );
}
#[inline]
pub fn prev_long_word_end(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    movement: Movement,
) {
    word_move(
        editor,
        view_id,
        doc_id,
        count,
        helix_core::movement::move_prev_long_word_end,
        movement,
    );
}
#[inline]
pub fn next_sub_word_start(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    movement: Movement,
) {
    word_move(
        editor,
        view_id,
        doc_id,
        count,
        helix_core::movement::move_next_sub_word_start,
        movement,
    );
}
#[inline]
pub fn prev_sub_word_start(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    movement: Movement,
) {
    word_move(
        editor,
        view_id,
        doc_id,
        count,
        helix_core::movement::move_prev_sub_word_start,
        movement,
    );
}
#[inline]
pub fn next_sub_word_end(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    movement: Movement,
) {
    word_move(
        editor,
        view_id,
        doc_id,
        count,
        helix_core::movement::move_next_sub_word_end,
        movement,
    );
}
#[inline]
pub fn prev_sub_word_end(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    movement: Movement,
) {
    word_move(
        editor,
        view_id,
        doc_id,
        count,
        helix_core::movement::move_prev_sub_word_end,
        movement,
    );
}

// ─── Legacy word wrappers (hardcoded Move/Extend) ────────────────────

#[inline]
pub fn move_next_word_start(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    next_word_start(editor, view_id, doc_id, count, Movement::Move);
}
#[inline]
pub fn move_prev_word_start(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    prev_word_start(editor, view_id, doc_id, count, Movement::Move);
}
#[inline]
pub fn move_prev_word_end(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    prev_word_end(editor, view_id, doc_id, count, Movement::Move);
}
#[inline]
pub fn move_next_word_end(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    next_word_end(editor, view_id, doc_id, count, Movement::Move);
}
#[inline]
pub fn move_next_long_word_start(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    next_long_word_start(editor, view_id, doc_id, count, Movement::Move);
}
#[inline]
pub fn move_prev_long_word_start(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    prev_long_word_start(editor, view_id, doc_id, count, Movement::Move);
}
#[inline]
pub fn move_prev_long_word_end(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    prev_long_word_end(editor, view_id, doc_id, count, Movement::Move);
}
#[inline]
pub fn move_next_long_word_end(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    next_long_word_end(editor, view_id, doc_id, count, Movement::Move);
}
#[inline]
pub fn move_next_sub_word_start(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    next_sub_word_start(editor, view_id, doc_id, count, Movement::Move);
}
#[inline]
pub fn move_prev_sub_word_start(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    prev_sub_word_start(editor, view_id, doc_id, count, Movement::Move);
}
#[inline]
pub fn move_prev_sub_word_end(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    prev_sub_word_end(editor, view_id, doc_id, count, Movement::Move);
}
#[inline]
pub fn move_next_sub_word_end(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    next_sub_word_end(editor, view_id, doc_id, count, Movement::Move);
}
#[inline]
pub fn extend_next_word_start(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    next_word_start(editor, view_id, doc_id, count, Movement::Extend);
}
#[inline]
pub fn extend_prev_word_start(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    prev_word_start(editor, view_id, doc_id, count, Movement::Extend);
}
#[inline]
pub fn extend_next_word_end(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    next_word_end(editor, view_id, doc_id, count, Movement::Extend);
}
#[inline]
pub fn extend_prev_word_end(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    prev_word_end(editor, view_id, doc_id, count, Movement::Extend);
}
#[inline]
pub fn extend_next_long_word_start(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    next_long_word_start(editor, view_id, doc_id, count, Movement::Extend);
}
#[inline]
pub fn extend_prev_long_word_start(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    prev_long_word_start(editor, view_id, doc_id, count, Movement::Extend);
}
#[inline]
pub fn extend_prev_long_word_end(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    prev_long_word_end(editor, view_id, doc_id, count, Movement::Extend);
}
#[inline]
pub fn extend_next_long_word_end(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    next_long_word_end(editor, view_id, doc_id, count, Movement::Extend);
}
#[inline]
pub fn extend_next_sub_word_start(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    next_sub_word_start(editor, view_id, doc_id, count, Movement::Extend);
}
#[inline]
pub fn extend_prev_sub_word_start(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    prev_sub_word_start(editor, view_id, doc_id, count, Movement::Extend);
}
#[inline]
pub fn extend_prev_sub_word_end(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    prev_sub_word_end(editor, view_id, doc_id, count, Movement::Extend);
}
#[inline]
pub fn extend_next_sub_word_end(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    next_sub_word_end(editor, view_id, doc_id, count, Movement::Extend);
}

// --- Paragraph movements ---

pub fn goto_paragraph_in<V, D, F>(
    target: &V,
    doc: &mut D,
    count: usize,
    move_fn: F,
    movement: Movement,
) where
    V: Identified + NavigableViewport<D>,
    D: TextContent + Selectable,
    F: Fn(RopeSlice, &TextAnnotations, Range, usize, Movement) -> Range,
{
    let text = doc.text().slice(..);
    let annotations = target.text_annotations(doc);

    let selection = doc
        .selection(target.id())
        .clone()
        .transform(|range| move_fn(text, &annotations, range, count, movement));

    drop(annotations);
    doc.set_selection(target.id(), selection);
}

fn goto_para_impl<F>(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    move_fn: F,
) where
    F: Fn(RopeSlice, &TextAnnotations, Range, usize, Movement) -> Range + Copy + Send + Sync + 'static,
{
    let movement = movement_from_mode(editor);
    editor.apply_motion(move |ed: &mut Editor| {
        ed.with_view_doc_mut(view_id, doc_id, |view, doc| {
            goto_paragraph_in(view, doc, count, move_fn, movement);
        });
    });
}

pub fn goto_prev_paragraph(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    goto_para_impl(editor, view_id, doc_id, count, move_prev_paragraph);
}

pub fn goto_next_paragraph(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    goto_para_impl(editor, view_id, doc_id, count, move_next_paragraph);
}

// --- Goto file start/end ---

pub fn goto_file_start(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: Option<std::num::NonZeroUsize>,
    movement: Movement,
) {
    if count.is_some() {
        goto_line(editor, view_id, doc_id, count, movement);
    } else {
        editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
            push_jump(view, doc);
            goto_file_start_in(&view_id, doc, movement);
        });
    }
}

pub fn goto_file_end(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, movement: Movement) {
    editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
        push_jump(view, doc);
        goto_file_end_in(&view_id, doc, movement);
    });
}

// --- Goto line (by line number) ---

pub fn goto_line(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: Option<std::num::NonZeroUsize>,
    movement: Movement,
) {
    if let Some(count) = count {
        editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
            push_jump(view, doc);
            goto_line_without_jumplist_in(&view_id, doc, count.get(), movement);
        });
    }
}

pub fn goto_line_without_jumplist(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    line_num: usize,
    movement: Movement,
) {
    let doc = crate::doc_mut!(editor, &doc_id);
    goto_line_without_jumplist_in(&view_id, doc, line_num, movement);
}

pub fn goto_line_without_jumplist_in(
    target: &impl Identified,
    doc: &mut (impl TextContent + Selectable),
    line_num: usize,
    movement: Movement,
) {
    let text = doc.text().slice(..);
    let max_line = if text.line(text.len_lines().saturating_sub(1)).len_chars() == 0 {
        text.len_lines().saturating_sub(2)
    } else {
        text.len_lines() - 1
    };
    let line_idx = std::cmp::min(line_num.saturating_sub(1), max_line);
    let pos = text.line_to_char(line_idx);
    let selection = doc
        .selection(target.id())
        .clone()
        .transform(|range| range.put_cursor(text, pos, movement == Movement::Extend));

    doc.set_selection(target.id(), selection);
}

// --- Goto last line ---

pub fn goto_last_line(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    movement: Movement,
) {
    editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
        push_jump(view, doc);
        goto_last_line_in(view, doc, movement);
    });
}

// --- Goto column ---

pub fn goto_column(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    movement: Movement,
) {
    editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
        push_jump(view, doc);
        goto_column_in(&view_id, doc, count, movement);
    });
}

// --- Extend line / select line / extend to line bounds ---

#[derive(Clone, Copy)]
pub enum Extend {
    Above,
    Below,
}

pub fn extend_line(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    let doc = crate::doc!(editor, &doc_id);
    let extend = match doc.selection(view_id).primary().direction() {
        Direction::Forward => Extend::Below,
        Direction::Backward => Extend::Above,
    };
    extend_line_impl(editor, view_id, doc_id, count, extend);
}

pub fn extend_line_below(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    extend_line_impl(editor, view_id, doc_id, count, Extend::Below);
}

pub fn extend_line_above(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    extend_line_impl(editor, view_id, doc_id, count, Extend::Above);
}

fn extend_line_impl(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    extend: Extend,
) {
    editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
        extend_line_in(view, doc, count, extend);
    });
}

pub fn select_line_below(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    select_line_impl(editor, view_id, doc_id, count, Extend::Below);
}

pub fn select_line_above(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    select_line_impl(editor, view_id, doc_id, count, Extend::Above);
}

fn select_line_impl(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    extend: Extend,
) {
    let doc = crate::doc_mut!(editor, &doc_id);
    select_line_in(&view_id, doc, count, extend);
}

pub fn extend_line_in<V, D>(target: &V, doc: &mut D, count: usize, extend: Extend)
where
    V: Identified + NavigableViewport<D>,
    D: TextContent + Selectable,
{
    let annotations = target.text_annotations(doc);
    let text = doc.text();
    let selection = doc.selection(target.id()).clone().transform(|range| {
        let (start_line, end_line) = range.line_range(text.slice(..));

        let start = text.line_to_char(start_line);
        let end = text.line_to_char((end_line + 1).min(text.len_lines()));

        let (anchor, head) = if range.from() == start && range.to() == end {
            match extend {
                Extend::Above => (
                    end,
                    text.line_to_char(text.slice(..).nth_prev_folded_line(
                        &annotations.folds,
                        start_line,
                        count,
                    )),
                ),
                Extend::Below => (
                    start,
                    text.line_to_char({
                        let mut idx = text.slice(..).nth_next_folded_line(
                            &annotations.folds,
                            end_line,
                            count,
                        );
                        if idx < text.len_lines() {
                            idx += 1;
                        }
                        idx
                    }),
                ),
            }
        } else {
            match extend {
                Extend::Above => (
                    end,
                    text.line_to_char(text.slice(..).nth_prev_folded_line(
                        &annotations.folds,
                        start_line,
                        count - 1,
                    )),
                ),
                Extend::Below => (
                    start,
                    text.line_to_char({
                        let mut idx = text.slice(..).nth_next_folded_line(
                            &annotations.folds,
                            end_line,
                            count - 1,
                        );
                        if idx < text.len_lines() {
                            idx += 1;
                        }
                        idx
                    }),
                ),
            }
        };

        Range::new(anchor, head)
    });

    drop(annotations);
    doc.set_selection(target.id(), selection);
}

pub fn select_line_in(
    target: &impl Identified,
    doc: &mut (impl TextContent + Selectable),
    count: usize,
    extend: Extend,
) {
    let text = doc.text();
    let saturating_add = |a: usize, b: usize| (a + b).min(text.len_lines());
    let mut count = count;
    let selection = doc.selection(target.id()).clone().transform(|range| {
        let (start_line, end_line) = range.line_range(text.slice(..));
        let start = text.line_to_char(start_line);
        let end = text.line_to_char(saturating_add(end_line, 1));
        let direction = range.direction();

        if range.from() != start || range.to() != end {
            count = count.saturating_sub(1);
        }
        let (anchor_line, head_line) = match (&extend, direction) {
            (Extend::Above, Direction::Forward) => (start_line, end_line.saturating_sub(count)),
            (Extend::Above, Direction::Backward) => (end_line, start_line.saturating_sub(count)),
            (Extend::Below, Direction::Forward) => (start_line, saturating_add(end_line, count)),
            (Extend::Below, Direction::Backward) => (end_line, saturating_add(start_line, count)),
        };
        let (anchor, head) = match anchor_line.cmp(&head_line) {
            std::cmp::Ordering::Less => (
                text.line_to_char(anchor_line),
                text.line_to_char(saturating_add(head_line, 1)),
            ),
            std::cmp::Ordering::Equal => match extend {
                Extend::Above => (
                    text.line_to_char(saturating_add(anchor_line, 1)),
                    text.line_to_char(head_line),
                ),
                Extend::Below => (
                    text.line_to_char(head_line),
                    text.line_to_char(saturating_add(anchor_line, 1)),
                ),
            },
            std::cmp::Ordering::Greater => (
                text.line_to_char(saturating_add(anchor_line, 1)),
                text.line_to_char(head_line),
            ),
        };
        Range::new(anchor, head)
    });

    doc.set_selection(target.id(), selection);
}

pub fn extend_to_line_bounds(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    let doc = crate::doc_mut!(editor, &doc_id);
    extend_to_line_bounds_in(&view_id, doc);
}

pub fn shrink_to_line_bounds(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    let doc = crate::doc_mut!(editor, &doc_id);
    shrink_to_line_bounds_in(&view_id, doc);
}

pub fn goto_file_end_in(
    target: &impl Identified,
    doc: &mut (impl TextContent + Selectable),
    movement: Movement,
) {
    let text = doc.text().slice(..);
    let pos = doc.text().len_chars();
    let selection = doc
        .selection(target.id())
        .clone()
        .transform(|range| range.put_cursor(text, pos, movement == Movement::Extend));
    doc.set_selection(target.id(), selection);
}

pub fn goto_file_start_in(
    target: &impl Identified,
    doc: &mut (impl TextContent + Selectable),
    movement: Movement,
) {
    let text = doc.text().slice(..);
    let selection = doc
        .selection(target.id())
        .clone()
        .transform(|range| range.put_cursor(text, 0, movement == Movement::Extend));
    doc.set_selection(target.id(), selection);
}

pub fn goto_last_line_in<V, D>(target: &V, doc: &mut D, movement: Movement)
where
    V: Identified + NavigableViewport<D>,
    D: TextContent + Selectable,
{
    let text = doc.text().slice(..);
    let annotations = target.text_annotations(doc);

    let last_visible_line = if let Some(fold) = annotations
        .folds
        .superest_fold_containing(text.len_lines(), |fold| fold.start.line..=fold.end.line)
    {
        fold.start.line - 1
    } else {
        text.len_lines().saturating_sub(1)
    };

    let line_idx = if text.line(last_visible_line).len_chars() == 0 {
        text.prev_folded_line(&annotations.folds, last_visible_line)
    } else {
        last_visible_line
    };

    let pos = text.line_to_char(line_idx);
    let selection = doc
        .selection(target.id())
        .clone()
        .transform(|range| range.put_cursor(text, pos, movement == Movement::Extend));

    drop(annotations);
    doc.set_selection(target.id(), selection);
}

pub fn goto_column_in(
    target: &impl Identified,
    doc: &mut (impl TextContent + Selectable),
    count: usize,
    movement: Movement,
) {
    let text = doc.text().slice(..);
    let selection = doc.selection(target.id()).clone().transform(|range| {
        let line = range.cursor_line(text);
        let line_start = text.line_to_char(line);
        let line_end = line_end_char_index(&text, line);
        let pos = graphemes::nth_next_grapheme_boundary(text, line_start, count.saturating_sub(1))
            .min(line_end);
        range.put_cursor(text, pos, movement == Movement::Extend)
    });
    doc.set_selection(target.id(), selection);
}

pub fn extend_to_line_bounds_in(
    target: &impl Identified,
    doc: &mut (impl TextContent + Selectable),
) {
    let text = doc.text();
    let selection = doc.selection(target.id()).clone().transform(|range| {
        let (start_line, end_line) = range.line_range(text.slice(..));
        let start = text.line_to_char(start_line);
        let end = text.line_to_char((end_line + 1).min(text.len_lines()));

        Range::new(start, end).with_direction(range.direction())
    });
    doc.set_selection(target.id(), selection);
}

pub fn shrink_to_line_bounds_in(
    target: &impl Identified,
    doc: &mut (impl TextContent + Selectable),
) {
    let text = doc.text();
    let selection = doc.selection(target.id()).clone().transform(|range| {
        let (start_line, end_line) = range.line_range(text.slice(..));

        // Preserve existing single-line behavior to avoid command-specific branching.
        if start_line == end_line {
            return range;
        }

        let mut start = text.line_to_char(start_line);
        let mut end = text.line_to_char((end_line + 1).min(text.len_lines()));

        if start != range.from() {
            start = text.line_to_char((start_line + 1).min(text.len_lines()));
        }

        if end != range.to() {
            end = text.line_to_char(end_line);
        }

        Range::new(start, end).with_direction(range.direction())
    });
    doc.set_selection(target.id(), selection);
}

// ─── Scroll ──────────────────────────────────────────────────────────

/// Scroll the viewport by `offset` lines in `direction`.
///
/// If `sync_cursor` is true, the cursor moves with the viewport.
/// Otherwise the cursor is clamped to remain within the visible scrolloff region.
pub fn scroll(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    offset: usize,
    direction: Direction,
    sync_cursor: bool,
) {
    let scrolloff = editor.config().scrolloff;
    let mode = editor.mode();
    editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
        scroll_in(view, doc, mode, scrolloff, offset, direction, sync_cursor);
    });
}

/// Scroll a text viewport by visual lines.
pub fn scroll_in<V, D>(
    target: &V,
    doc: &mut D,
    mode: Mode,
    scrolloff: usize,
    offset: usize,
    direction: Direction,
    sync_cursor: bool,
) where
    V: TextViewport<D>,
    D: FormattableText + Selectable,
{
    use Direction::*;

    let mut view_offset = target.view_offset(doc);

    let range = doc.selection(target.id()).primary();
    let text = doc.text().slice(..);

    let cursor = range.cursor(text);
    let viewport = target.text_area(doc);
    let height = viewport.height;

    let scrolloff = scrolloff.min(height.saturating_sub(1) as usize / 2);
    let offset_signed = match direction {
        Forward => offset as isize,
        Backward => -(offset as isize),
    };

    let doc_text = doc.text().slice(..);
    let text_fmt = doc.text_format(viewport.width);
    (view_offset.anchor, view_offset.vertical_offset) = char_idx_at_visual_offset(
        doc_text,
        view_offset.anchor,
        view_offset.vertical_offset as isize + offset_signed,
        0,
        &text_fmt,
        &target.text_annotations(doc),
    );
    target.set_view_offset(doc, view_offset);

    let doc_text = doc.text().slice(..);
    let mut annotations = target.text_annotations(doc);

    if sync_cursor {
        let movement = movement_from_mode_in(&mode);
        let selection = doc.selection(target.id()).clone().transform(|range| {
            move_vertically_visual(
                doc_text,
                range,
                direction,
                offset_signed.unsigned_abs(),
                movement,
                &text_fmt,
                &mut annotations,
            )
        });
        drop(annotations);
        doc.set_selection(target.id(), selection);
        return;
    }

    let view_offset = target.view_offset(doc);

    let mut head;
    match direction {
        Forward => {
            let off;
            (head, off) = char_idx_at_visual_offset(
                doc_text,
                view_offset.anchor,
                (view_offset.vertical_offset + scrolloff) as isize,
                0,
                &text_fmt,
                &annotations,
            );
            head += (off != 0) as usize;
            if head <= cursor {
                return;
            }
        }
        Backward => {
            head = char_idx_at_visual_offset(
                doc_text,
                view_offset.anchor,
                (view_offset.vertical_offset + height as usize - scrolloff - 1) as isize,
                0,
                &text_fmt,
                &annotations,
            )
            .0;
            if head >= cursor {
                return;
            }
        }
    }

    let anchor = if mode == Mode::Select {
        range.anchor
    } else {
        head
    };

    let prim_sel = Range::new(anchor, head);
    let mut sel = doc.selection(target.id()).clone();
    let idx = sel.primary_index();
    sel = sel.replace(idx, prim_sel);
    drop(annotations);
    doc.set_selection(target.id(), sel);
}

// ─── Tree-sitter object navigation ──────────────────────────────────

/// Navigate to the next/previous tree-sitter object (function, class, parameter, comment).
pub fn goto_ts_object(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    object: &str,
    direction: Direction,
    count: usize,
) {
    let mode = editor.mode;
    let loader = editor.syn_loader.load();
    let mut syntax_missing = false;
    editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
        if let Some(syntax) = doc.syntax() {
            let text = doc.text().slice(..);
            let root = syntax.tree().root_node();
            let annotations = view.text_annotations(doc);

            let selection = doc.selection(view_id).clone().transform(|range| {
                let new_range = helix_core::movement::goto_treesitter_object(
                    text,
                    &annotations,
                    range,
                    object,
                    direction,
                    &root,
                    syntax,
                    &loader,
                    count,
                );

                if mode == Mode::Select {
                    let head = if new_range.head < range.anchor {
                        new_range.anchor
                    } else {
                        new_range.head
                    };
                    Range::new(range.anchor, head)
                } else {
                    new_range.with_direction(direction)
                }
            });
            drop(annotations);

            push_jump(view, doc);
            doc.set_selection(view_id, selection);
        } else {
            syntax_missing = true;
        }
    });
    if syntax_missing {
        editor.set_status("Syntax-tree is not available in current buffer");
    }
}

// ─── Find char (f/t/F/T) ────────────────────────────────────────────

/// Find character motion — used by f/t/F/T commands.
///
/// `direction`: Forward (f/t) or Backward (F/T).
/// `inclusive`: true for f/F, false for t/T.
/// `movement`: Move or Extend (engine decides based on mode).
#[allow(clippy::too_many_arguments)]
pub fn find_char(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    ch: char,
    direction: Direction,
    inclusive: bool,
    count: usize,
    movement: Movement,
) {
    let extend = matches!(movement, Movement::Extend);
    let search_fn = match direction {
        Direction::Forward => find_next_char_impl,
        Direction::Backward => find_prev_char_impl,
    };
    editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
        find_char_in(view, doc, &search_fn, inclusive, extend, ch, count);
    });
}

#[allow(clippy::too_many_arguments)]
pub fn find_char_in<V, D, F, M>(
    target: &V,
    doc: &mut D,
    search_fn: &F,
    inclusive: bool,
    extend: bool,
    matcher: M,
    count: usize,
) where
    V: Identified + NavigableViewport<D>,
    D: TextContent + Selectable,
    F: Fn(RopeSlice, &TextAnnotations, M, usize, usize, bool) -> Option<usize> + 'static,
    M: GraphemeMatcher + Copy,
{
    let text = doc.text().slice(..);
    let annotations = target.text_annotations(doc);

    let selection = doc.selection(target.id()).clone().transform(|range| {
        search_fn(
            text,
            &annotations,
            matcher,
            range.cursor(text),
            count,
            inclusive,
        )
        .map_or(range, |pos| {
            if extend {
                range.put_cursor(text, pos, true)
            } else {
                Range::point(range.cursor(text)).put_cursor(text, pos, true)
            }
        })
    });
    drop(annotations);
    doc.set_selection(target.id(), selection);
}

fn find_next_char_impl(
    text: RopeSlice,
    annotations: &TextAnnotations,
    matcher: impl GraphemeMatcher,
    pos: usize,
    n: usize,
    inclusive: bool,
) -> Option<usize> {
    if inclusive {
        search::find_folded_nth_next(text, &annotations.folds, matcher, pos, n)
    } else {
        let n = match text
            .folded_graphemes_at(&annotations.folds, text.char_to_byte(pos))
            .nth(1)
        {
            Some(g) if matcher.grapheme_match(g) => n + 1,
            _ => n,
        };
        search::find_folded_nth_next(text, &annotations.folds, matcher, pos, n)
            .map(|idx| prev_folded_grapheme_boundary(text, &annotations.folds, idx))
    }
}

fn find_prev_char_impl(
    text: RopeSlice,
    annotations: &TextAnnotations,
    matcher: impl GraphemeMatcher,
    pos: usize,
    n: usize,
    inclusive: bool,
) -> Option<usize> {
    if inclusive {
        search::find_folded_nth_prev(text, &annotations.folds, matcher, pos, n)
    } else {
        let n = match text
            .folded_graphemes_at(&annotations.folds, text.char_to_byte(pos))
            .prev()
        {
            Some(g) if matcher.grapheme_match(g) => n + 1,
            _ => n,
        };
        search::find_folded_nth_prev(text, &annotations.folds, matcher, pos, n)
            .map(|idx| next_folded_grapheme_boundary(text, &annotations.folds, idx))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;

    use super::*;
    use crate::editor::Config;
    use crate::traits::{Identified, NavigableViewport};
    use crate::Document;
    use helix_core::syntax;

    struct TestModal(Mode);

    impl Modal for TestModal {
        fn mode(&self) -> Mode {
            self.0
        }

        fn set_mode(&mut self, mode: Mode) {
            self.0 = mode;
        }
    }

    struct TestViewport {
        id: ViewId,
    }

    impl Identified for TestViewport {
        fn id(&self) -> ViewId {
            self.id
        }
    }

    impl NavigableViewport<Document> for TestViewport {
        fn text_area_width(&self, _doc: &Document) -> u16 {
            80
        }

        fn text_annotations<'a>(&self, _doc: &'a Document) -> TextAnnotations<'a> {
            TextAnnotations::default()
        }
    }

    fn test_doc(text: &str) -> Document {
        Document::from(
            helix_core::Rope::from(text),
            None,
            Arc::new(ArcSwap::new(Arc::new(Config::default()))),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        )
    }

    #[test]
    fn movement_from_mode_in_maps_select_to_extend() {
        assert!(movement_from_mode_in(&TestModal(Mode::Select)) == Movement::Extend);
        assert!(movement_from_mode_in(&TestModal(Mode::Normal)) == Movement::Move);
    }

    #[test]
    fn goto_line_end_in_moves_to_end_of_current_line() {
        let view_id = ViewId::default();
        let mut doc = test_doc("ab\ncd");
        doc.set_selection(view_id, helix_core::Selection::point(0));

        goto_line_end_in(&view_id, &mut doc, Movement::Move);

        assert_eq!(
            doc.selection(view_id)
                .primary()
                .cursor(doc.text().slice(..)),
            1
        );
    }

    #[test]
    fn goto_line_without_jumplist_in_clamps_to_last_line() {
        let view_id = ViewId::default();
        let mut doc = test_doc("ab\ncd");
        doc.set_selection(view_id, helix_core::Selection::point(0));

        goto_line_without_jumplist_in(&view_id, &mut doc, 99, Movement::Move);

        assert_eq!(
            doc.selection(view_id)
                .primary()
                .cursor(doc.text().slice(..)),
            3
        );
    }

    #[test]
    fn extend_to_line_bounds_in_expands_selection_to_full_line() {
        let view_id = ViewId::default();
        let mut doc = test_doc("abc\ndef");
        doc.set_selection(view_id, helix_core::Selection::single(1, 2));

        extend_to_line_bounds_in(&view_id, &mut doc);

        let range = doc.selection(view_id).primary();
        assert_eq!(range.from(), 0);
        assert_eq!(range.to(), 4);
    }

    #[test]
    fn shrink_to_line_bounds_in_trims_multiline_selection_edges() {
        let view_id = ViewId::default();
        let mut doc = test_doc("first\nsecond\nthird\n");
        doc.set_selection(view_id, helix_core::Selection::single(1, 14));

        shrink_to_line_bounds_in(&view_id, &mut doc);

        let range = doc.selection(view_id).primary();
        assert_eq!(range.from(), "first\n".chars().count());
        assert_eq!(range.to(), "first\nsecond\n".chars().count());
    }

    #[test]
    fn shrink_to_line_bounds_in_preserves_single_line_selection() {
        let view_id = ViewId::default();
        let mut doc = test_doc("first\nsecond\n");
        doc.set_selection(view_id, helix_core::Selection::single(1, 3));
        let original = doc.selection(view_id).clone();

        shrink_to_line_bounds_in(&view_id, &mut doc);

        assert_eq!(doc.selection(view_id), &original);
    }

    #[test]
    fn goto_file_start_in_moves_cursor_to_start_of_buffer() {
        let view_id = ViewId::default();
        let mut doc = test_doc("abc\ndef");
        doc.set_selection(view_id, helix_core::Selection::point(5));

        goto_file_start_in(&view_id, &mut doc, Movement::Move);

        assert_eq!(
            doc.selection(view_id)
                .primary()
                .cursor(doc.text().slice(..)),
            0
        );
    }

    #[test]
    fn goto_paragraph_in_moves_to_previous_blank_line_boundary() {
        let view_id = ViewId::default();
        let viewport = TestViewport { id: view_id };
        let mut doc = test_doc("one\n\ntwo\nthree");
        doc.set_selection(view_id, helix_core::Selection::point(9));

        goto_paragraph_in(&viewport, &mut doc, 1, move_prev_paragraph, Movement::Move);

        assert_eq!(
            doc.selection(view_id)
                .primary()
                .cursor(doc.text().slice(..)),
            5
        );
    }

    #[test]
    fn find_char_in_moves_to_next_matching_character() {
        let view_id = ViewId::default();
        let viewport = TestViewport { id: view_id };
        let mut doc = test_doc("abc def");
        doc.set_selection(view_id, helix_core::Selection::point(0));

        find_char_in(
            &viewport,
            &mut doc,
            &find_next_char_impl,
            true,
            false,
            'd',
            1,
        );

        assert_eq!(
            doc.selection(view_id)
                .primary()
                .cursor(doc.text().slice(..)),
            4
        );
    }

    #[test]
    fn select_line_in_expands_selection_by_whole_lines() {
        let view_id = ViewId::default();
        let mut doc = test_doc("one\ntwo\nthree");
        doc.set_selection(view_id, helix_core::Selection::point(1));

        select_line_in(&view_id, &mut doc, 2, Extend::Below);

        let range = doc.selection(view_id).primary();
        assert_eq!(range.from(), 0);
        assert_eq!(range.to(), 8);
    }

    #[test]
    fn extend_line_in_extends_selection_downward_by_line() {
        let view_id = ViewId::default();
        let viewport = TestViewport { id: view_id };
        let mut doc = test_doc("one\ntwo\nthree");
        doc.set_selection(view_id, helix_core::Selection::single(0, 4));

        extend_line_in(&viewport, &mut doc, 1, Extend::Below);

        let range = doc.selection(view_id).primary();
        assert_eq!(range.from(), 0);
        assert_eq!(range.to(), 8);
    }
}
