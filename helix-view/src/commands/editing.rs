//! Text editing and selection commands that operate on editor state.
//!
//! Frontend-agnostic: only mutate Editor/Document/View state.
//! helix-term wraps them with Context (count, register, callbacks).

use std::borrow::Cow;
use std::char::{ToLowercase, ToUppercase};
use std::time::Instant;

use helix_core::{
    auto_pairs, comment,
    graphemes::{self, prev_grapheme_boundary},
    increment as hx_increment,
    line_ending::line_end_char_index,
    movement::{self, Direction, Movement},
    object,
    surround::FindType,
    syntax::{config::BlockCommentToken, Syntax},
    text_annotations::TextAnnotations,
    textobject, Position, Range, Rope, RopeSlice, Selection, SmallVec, Tendril, Transaction,
};

use helix_core::unicode::width::UnicodeWidthChar;
use helix_stdx::rope::RopeSliceExt;

use crate::document::Mode;
use crate::traits::{
    Identified, Indentation, Jumpable, Modal, MutableText, NavigableViewport, Selectable,
    SyntaxAware, SyntaxContext, TextContent, TextMetrics, TextViewport, Undoable,
};
use crate::{bench::log_command_phase, Document, DocumentId, Editor, ViewId};

// ─── Helpers ────────────────────────────────────────────────────────

/// Returns true if every range in the selection spans whole lines.
pub fn selection_is_linewise(selection: &Selection, text: &Rope) -> bool {
    selection.ranges().iter().all(|range| {
        let text = text.slice(..);
        if range.slice(text).len_lines() < 2 {
            return false;
        }
        let (start_line, end_line) = range.line_range(text);
        let start = text.line_to_char(start_line);
        let end = text.line_to_char((end_line + 1).min(text.len_lines()));
        start == range.from() && end == range.to()
    })
}

/// Collects the sorted, deduplicated line numbers covered by the current selection.
pub fn get_lines(doc: &Document, view_id: ViewId) -> Vec<usize> {
    get_lines_in(&view_id, doc)
}

/// Collects the sorted, deduplicated line numbers covered by the current selection.
pub fn get_lines_in(target: &impl Identified, doc: &(impl TextContent + Selectable)) -> Vec<usize> {
    let mut lines = Vec::new();
    for range in doc.selection(target.id()) {
        let (start, end) = range.line_range(doc.text().slice(..));
        for line in start..=end {
            lines.push(line);
        }
    }
    lines.sort_unstable();
    lines.dedup();
    lines
}

/// Exit select mode if currently in select mode.
#[inline]
pub fn exit_select_mode_in(modal: &mut impl Modal) {
    if modal.mode() == Mode::Select {
        modal.set_mode(Mode::Normal);
    }
}

/// Exit select mode if currently in select mode.
#[inline]
pub fn exit_select_mode(editor: &mut Editor, _view_id: ViewId, _doc_id: DocumentId) {
    exit_select_mode_in(editor);
}

/// Rotate the primary selection index forward.
pub fn rotate_selections_forward_in(
    target: &impl Identified,
    doc: &mut impl Selectable,
    count: usize,
) {
    let mut selection = doc.selection(target.id()).clone();
    let index = selection.primary_index();
    let len = selection.len();
    selection.set_primary_index((index + count) % len);
    doc.set_selection(target.id(), selection);
}

/// Rotate the primary selection index backward.
pub fn rotate_selections_backward_in(
    target: &impl Identified,
    doc: &mut impl Selectable,
    count: usize,
) {
    let mut selection = doc.selection(target.id()).clone();
    let index = selection.primary_index();
    let len = selection.len();
    selection.set_primary_index((index + (len.saturating_sub(count) % len)) % len);
    doc.set_selection(target.id(), selection);
}

/// Make the first selection primary.
pub fn rotate_selections_first_in(target: &impl Identified, doc: &mut impl Selectable) {
    let mut selection = doc.selection(target.id()).clone();
    selection.set_primary_index(0);
    doc.set_selection(target.id(), selection);
}

/// Make the last selection primary.
pub fn rotate_selections_last_in(target: &impl Identified, doc: &mut impl Selectable) {
    let mut selection = doc.selection(target.id()).clone();
    let len = selection.len();
    selection.set_primary_index(len - 1);
    doc.set_selection(target.id(), selection);
}

fn reorder_selection_contents_in(
    target: &impl Identified,
    doc: &mut (impl Selectable + MutableText),
    strategy: ReorderStrategy,
    count: usize,
) {
    let text = doc.text().slice(..);
    let selection = doc.selection(target.id());

    let mut ranges: Vec<Tendril> = selection
        .slices(text)
        .map(|fragment| fragment.chunks().collect())
        .collect();

    let rotate_by = count.min(ranges.len());

    let primary_index = match strategy {
        ReorderStrategy::RotateForward => {
            ranges.rotate_right(rotate_by);
            (selection.primary_index() + ranges.len() + rotate_by) % ranges.len()
        }
        ReorderStrategy::RotateBackward => {
            ranges.rotate_left(rotate_by);
            (selection.primary_index() + ranges.len() - rotate_by) % ranges.len()
        }
        ReorderStrategy::Reverse => {
            if rotate_by.is_multiple_of(2) {
                return;
            }
            ranges.reverse();
            (ranges.len() - 1) - selection.primary_index()
        }
    };

    let transaction = Transaction::change(
        doc.text(),
        selection
            .ranges()
            .iter()
            .zip(ranges)
            .map(|(range, fragment)| (range.from(), range.to(), Some(fragment))),
    );

    doc.set_selection(
        target.id(),
        Selection::new(selection.ranges().into(), primary_index),
    );
    doc.apply(&transaction, target.id());
}

pub fn increment_in(
    target: &impl Identified,
    doc: &mut (impl MutableText + Selectable),
    mut amount: i64,
    increase_by: i64,
) -> bool {
    let selection = doc.selection(target.id()).clone();
    let text = doc.text().slice(..);

    let mut new_selection_ranges = SmallVec::new();
    let mut cumulative_length_diff: i128 = 0;
    let mut changes = vec![];

    for range in selection.iter() {
        let selected_text: Cow<str> = range.fragment(text);
        let new_from = ((range.from() as i128) + cumulative_length_diff) as usize;
        let incremented = [hx_increment::integer, hx_increment::date_time]
            .iter()
            .find_map(|incrementor| incrementor(selected_text.as_ref(), amount));

        amount += increase_by;

        match incremented {
            None => {
                let new_range = Range::new(
                    new_from,
                    (range.to() as i128 + cumulative_length_diff) as usize,
                );
                new_selection_ranges.push(new_range);
            }
            Some(new_text) => {
                let new_range = Range::new(new_from, new_from + new_text.len());
                cumulative_length_diff += new_text.len() as i128 - selected_text.len() as i128;
                new_selection_ranges.push(new_range);
                changes.push((range.from(), range.to(), Some(new_text.into())));
            }
        }
    }

    if changes.is_empty() {
        return false;
    }

    let new_selection = Selection::new(new_selection_ranges, selection.primary_index());
    let transaction =
        Transaction::change(doc.text(), changes.into_iter()).with_selection(new_selection);
    doc.apply(&transaction, target.id());
    true
}

/// Enter select mode.
pub fn select_mode_in(
    target: &impl Identified,
    doc: &mut (impl TextContent + Selectable),
    modal: &mut impl Modal,
) {
    let text = doc.text().slice(..);

    // Make sure end-of-document selections are also 1-width.
    let selection = doc.selection(target.id()).clone().transform(|range| {
        if range.is_empty() && range.head == text.len_chars() {
            Range::new(prev_grapheme_boundary(text, range.anchor), range.head)
        } else {
            range
        }
    });
    doc.set_selection(target.id(), selection);
    modal.set_mode(Mode::Select);
}

/// Trim whitespace from both ends of each selection.
pub fn trim_selections_in(target: &impl Identified, doc: &mut (impl TextContent + Selectable)) {
    let text = doc.text().slice(..);

    let ranges: SmallVec<[Range; 1]> = doc
        .selection(target.id())
        .iter()
        .filter_map(|range| {
            if range.is_empty() || range.slice(text).chars().all(|ch| ch.is_whitespace()) {
                return None;
            }
            let mut start = range.from();
            let mut end = range.to();
            start = movement::skip_while(text, start, |x| x.is_whitespace()).unwrap_or(start);
            end = movement::backwards_skip_while(text, end, |x| x.is_whitespace()).unwrap_or(end);
            Some(Range::new(start, end).with_direction(range.direction()))
        })
        .collect();

    if !ranges.is_empty() {
        let primary = doc.selection(target.id()).primary();
        let idx = ranges
            .iter()
            .position(|range| range.overlaps(&primary))
            .unwrap_or(ranges.len() - 1);
        doc.set_selection(target.id(), Selection::new(ranges, idx));
    } else {
        collapse_selection_in(target, doc);
        keep_primary_selection_in(target, doc);
    }
}

// ─── Selection manipulation ────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionCommandError {
    NoSelectionsRemaining,
}

/// Select the entire document.
pub fn select_all_in(target: &impl Identified, doc: &mut (impl TextContent + Selectable)) {
    let end = doc.text().len_chars();
    doc.set_selection(target.id(), Selection::single(0, end));
}

/// Select the entire document.
pub fn select_all(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    let doc = crate::doc_mut!(editor, &doc_id);
    select_all_in(&view_id, doc);
}

/// Collapse each selection to a single cursor at its head position.
pub fn collapse_selection_in(target: &impl Identified, doc: &mut (impl TextContent + Selectable)) {
    let text = doc.text().slice(..);
    let selection = doc.selection(target.id()).clone().transform(|range| {
        let pos = range.cursor(text);
        Range::new(pos, pos)
    });
    doc.set_selection(target.id(), selection);
}

/// Collapse each selection to a single cursor at its head position.
pub fn collapse_selection(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    let doc = crate::doc_mut!(editor, &doc_id);
    collapse_selection_in(&view_id, doc);
}

/// Flip anchor and head of each selection.
pub fn flip_selections_in(target: &impl Identified, doc: &mut impl Selectable) {
    let selection = doc
        .selection(target.id())
        .clone()
        .transform(|range| range.flip());
    doc.set_selection(target.id(), selection);
}

/// Flip anchor and head of each selection.
pub fn flip_selections(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    let doc = crate::doc_mut!(editor, &doc_id);
    flip_selections_in(&view_id, doc);
}

/// Ensure all selections face forward (anchor <= head).
pub fn ensure_selections_forward_in(target: &impl Identified, doc: &mut impl Selectable) {
    let selection = doc
        .selection(target.id())
        .clone()
        .transform(|r| r.with_direction(Direction::Forward));
    doc.set_selection(target.id(), selection);
}

/// Ensure all selections face forward (anchor <= head).
pub fn ensure_selections_forward(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    let doc = crate::doc_mut!(editor, &doc_id);
    ensure_selections_forward_in(&view_id, doc);
}

/// Keep only the primary selection, discarding all others.
pub fn keep_primary_selection_in(target: &impl Identified, doc: &mut impl Selectable) {
    let range = doc.selection(target.id()).primary();
    doc.set_selection(target.id(), Selection::single(range.anchor, range.head));
}

/// Keep only the primary selection, discarding all others.
pub fn keep_primary_selection(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    let doc = crate::doc_mut!(editor, &doc_id);
    keep_primary_selection_in(&view_id, doc);
}

/// Remove the primary selection.
pub fn remove_primary_selection_in(
    target: &impl Identified,
    doc: &mut impl Selectable,
) -> Result<(), SelectionCommandError> {
    let selection = doc.selection(target.id());
    if selection.len() == 1 {
        return Err(SelectionCommandError::NoSelectionsRemaining);
    }

    let index = selection.primary_index();
    let selection = selection.clone().remove(index);
    doc.set_selection(target.id(), selection);
    Ok(())
}

/// Remove the primary selection. Fails if only one selection remains.
pub fn remove_primary_selection(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    let remove_result = {
        let doc = crate::doc_mut!(editor, &doc_id);
        remove_primary_selection_in(&view_id, doc)
    };
    if remove_result.is_err() {
        editor.set_error("no selections remaining");
    }
}

/// Rotate the primary selection index forward.
pub fn rotate_selections_forward(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    let doc = crate::doc_mut!(editor, &doc_id);
    rotate_selections_forward_in(&view_id, doc, count);
}

/// Rotate the primary selection index backward.
pub fn rotate_selections_backward(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    let doc = crate::doc_mut!(editor, &doc_id);
    rotate_selections_backward_in(&view_id, doc, count);
}

/// Make the first selection primary.
pub fn rotate_selections_first(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    let doc = crate::doc_mut!(editor, &doc_id);
    rotate_selections_first_in(&view_id, doc);
}

/// Make the last selection primary.
pub fn rotate_selections_last(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    let doc = crate::doc_mut!(editor, &doc_id);
    rotate_selections_last_in(&view_id, doc);
}

/// Rotate the text contents of selections forward.
pub fn rotate_selection_contents_forward(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    reorder_selection_contents(
        editor,
        view_id,
        doc_id,
        ReorderStrategy::RotateForward,
        count,
    );
}

/// Rotate the text contents of selections backward.
pub fn rotate_selection_contents_backward(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    reorder_selection_contents(
        editor,
        view_id,
        doc_id,
        ReorderStrategy::RotateBackward,
        count,
    );
}

/// Reverse the text contents of selections.
pub fn reverse_selection_contents(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    reorder_selection_contents(editor, view_id, doc_id, ReorderStrategy::Reverse, count);
}

/// Increment objects within selections by `amount`.
pub fn increment(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    amount: i64,
    increase_by: i64,
) {
    let changed = {
        let doc = crate::doc_mut!(editor, &doc_id);
        increment_in(&view_id, doc, amount, increase_by)
    };
    if changed {
        exit_select_mode_in(editor);
    }
}

/// Decrement objects within selections by `amount`.
pub fn decrement(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    amount: i64,
    increase_by: i64,
) {
    increment(editor, view_id, doc_id, -amount, -increase_by);
}

enum ReorderStrategy {
    RotateForward,
    RotateBackward,
    Reverse,
}

fn reorder_selection_contents(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    strategy: ReorderStrategy,
    count: usize,
) {
    let doc = crate::doc_mut!(editor, &doc_id);
    reorder_selection_contents_in(&view_id, doc, strategy, count);
}

/// Enter select mode.
pub fn select_mode(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    let mut mode = editor.mode();
    {
        let doc = crate::doc_mut!(editor, &doc_id);
        select_mode_in(&view_id, doc, &mut mode);
    }
    editor.mode = mode;
}

/// Match brackets and jump to the matching bracket.
pub fn match_brackets_in(
    target: &impl Identified,
    doc: &mut (impl TextContent + Selectable + SyntaxAware),
    movement: Movement,
) {
    let (text_slice, syntax) = doc.text_syntax();

    let selection = doc.selection(target.id()).clone().transform(|range| {
        let pos = range.cursor(text_slice);
        if let Some(matched_pos) = syntax.map_or_else(
            || helix_core::match_brackets::find_matching_bracket_plaintext(text_slice, pos),
            |syntax| {
                helix_core::match_brackets::find_matching_bracket_fuzzy(syntax, text_slice, pos)
            },
        ) {
            range.put_cursor(text_slice, matched_pos, movement == Movement::Extend)
        } else {
            range
        }
    });

    doc.set_selection(target.id(), selection);
}

/// Match brackets and jump to the matching bracket.
pub fn match_brackets(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    let doc = crate::doc_mut!(editor, &doc_id);
    let movement = if editor.mode == Mode::Select {
        Movement::Extend
    } else {
        Movement::Move
    };
    match_brackets_in(&view_id, doc, movement);
}

/// Save the current selection to the jumplist.
pub fn save_selection(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
        save_selection_in(view, doc);
    });
    editor.set_status("Selection saved to jumplist");
}

/// Save the current selection into a jump-capable history.
pub fn save_selection_in<V, D>(target: &mut V, doc: &mut D)
where
    V: Jumpable<D>,
{
    target.push_jump(doc);
}

/// Trim whitespace from both ends of each selection.
pub fn trim_selections(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    let doc = crate::doc_mut!(editor, &doc_id);
    trim_selections_in(&view_id, doc);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignSelectionsError {
    MultilineSelection,
}

/// Align selections by inserting spaces to match the rightmost column.
#[allow(deprecated)] // visual_coords_at_pos is correct here — alignment ignores softwrap/decorations
pub fn align_selections(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    let result = {
        let doc = crate::doc_mut!(editor, &doc_id);
        align_selections_in(&view_id, doc)
    };
    match result {
        Ok(()) => exit_select_mode(editor, view_id, doc_id),
        Err(AlignSelectionsError::MultilineSelection) => {
            editor.set_error("align cannot work with multi line selections");
        }
    }
}

/// Align selections by inserting spaces to match the rightmost column.
#[allow(deprecated)] // visual_coords_at_pos is correct here — alignment ignores softwrap/decorations
pub fn align_selections_in(
    target: &impl Identified,
    doc: &mut (impl Selectable + MutableText + TextMetrics),
) -> Result<(), AlignSelectionsError> {
    use helix_core::visual_coords_at_pos;
    let text = doc.text().slice(..);
    let selection = doc.selection(target.id());
    let tab_width = doc.tab_width();

    let mut column_widths: Vec<Vec<(usize, usize)>> = Vec::new();
    let mut last_line = text.len_lines() + 1;
    let mut col = 0;

    for range in selection {
        let coords = visual_coords_at_pos(text, range.head, tab_width);
        let anchor_coords = visual_coords_at_pos(text, range.anchor, tab_width);

        if coords.row != anchor_coords.row {
            return Err(AlignSelectionsError::MultilineSelection);
        }

        col = if coords.row == last_line { col + 1 } else { 0 };

        if col >= column_widths.len() {
            column_widths.push(Vec::new());
        }
        column_widths[col].push((range.from(), coords.col));

        last_line = coords.row;
    }

    let mut changes = Vec::with_capacity(selection.len());
    let len = column_widths.first().map(|cols| cols.len()).unwrap_or(0);
    let mut offs = vec![0; len];

    for col in column_widths {
        let max_col = col
            .iter()
            .enumerate()
            .map(|(row, (_, cursor))| *cursor + offs[row])
            .max()
            .unwrap_or(0);

        for (row, (insert_pos, last_col)) in col.into_iter().enumerate() {
            let ins_count = max_col - (last_col + offs[row]);
            if ins_count == 0 {
                continue;
            }
            offs[row] += ins_count;
            changes.push((
                insert_pos,
                insert_pos,
                Some(Tendril::from(" ".repeat(ins_count))),
            ));
        }
    }

    changes.sort_unstable_by_key(|(from, _, _)| *from);

    let transaction = Transaction::change(doc.text(), changes.into_iter());
    doc.apply(&transaction, target.id());
    Ok(())
}

/// Copy selection to the next line(s).
pub fn copy_selection_on_next_line(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    copy_selection_on_line(editor, view_id, doc_id, count, Direction::Forward);
}

/// Copy selection to the previous line(s).
pub fn copy_selection_on_prev_line(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    copy_selection_on_line(editor, view_id, doc_id, count, Direction::Backward);
}

#[allow(deprecated)] // pos_at_visual_coords/visual_coords_at_pos are correct here — column copying ignores softwrap
fn copy_selection_on_line(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    direction: Direction,
) {
    let doc = crate::doc_mut!(editor, &doc_id);
    copy_selection_on_line_in(&view_id, doc, count, direction);
}

#[allow(deprecated)] // pos_at_visual_coords/visual_coords_at_pos are correct here — column copying ignores softwrap
fn copy_selection_on_line_in(
    target: &impl Identified,
    doc: &mut (impl TextContent + Selectable + TextMetrics),
    count: usize,
    direction: Direction,
) {
    use helix_core::{pos_at_visual_coords, visual_coords_at_pos};
    let text = doc.text().slice(..);
    let selection = doc.selection(target.id());
    let mut ranges = SmallVec::with_capacity(selection.ranges().len() * (count + 1));
    ranges.extend_from_slice(selection.ranges());
    let mut primary_index = 0;

    for range in selection.iter() {
        let is_primary = *range == selection.primary();

        let (head, anchor) = if range.anchor < range.head {
            (range.head - 1, range.anchor)
        } else {
            (range.head, range.anchor.saturating_sub(1))
        };

        let tab_width = doc.tab_width();
        let head_pos = visual_coords_at_pos(text, head, tab_width);
        let anchor_pos = visual_coords_at_pos(text, anchor, tab_width);

        let height = std::cmp::max(head_pos.row, anchor_pos.row)
            - std::cmp::min(head_pos.row, anchor_pos.row)
            + 1;

        if is_primary {
            primary_index = ranges.len();
        }
        ranges.push(*range);

        let mut sels = 0;
        let mut i = 0;
        while sels < count {
            let offset = (i + 1) * height;

            let anchor_row = match direction {
                Direction::Forward => anchor_pos.row + offset,
                Direction::Backward => anchor_pos.row.saturating_sub(offset),
            };

            let head_row = match direction {
                Direction::Forward => head_pos.row + offset,
                Direction::Backward => head_pos.row.saturating_sub(offset),
            };

            if anchor_row >= text.len_lines() || head_row >= text.len_lines() {
                break;
            }

            let anchor =
                pos_at_visual_coords(text, Position::new(anchor_row, anchor_pos.col), tab_width);
            let head = pos_at_visual_coords(text, Position::new(head_row, head_pos.col), tab_width);

            if visual_coords_at_pos(text, anchor, tab_width).col == anchor_pos.col
                && visual_coords_at_pos(text, head, tab_width).col == head_pos.col
            {
                if is_primary {
                    primary_index = ranges.len();
                }
                ranges.push(Range::point(anchor).put_cursor(text, head, true));
                sels += 1;
            }

            if anchor_row == 0 && head_row == 0 {
                break;
            }

            i += 1;
        }
    }

    let selection = Selection::new(ranges, primary_index);
    doc.set_selection(target.id(), selection);
}

// ─── Text manipulation ─────────────────────────────────────────────

/// Yank (copy) the current selection to a register.
pub fn yank(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, register: char) {
    let (values, selections) = {
        let doc = crate::doc_mut!(editor, &doc_id);
        let values = yank_selections_in(&view_id, doc);
        let selections = values.len();
        (values, selections)
    };

    match editor.registers.write(register, values) {
        Ok(_) => editor.set_status(format!(
            "yanked {selections} selection{} to register {register}",
            if selections == 1 { "" } else { "s" }
        )),
        Err(err) => editor.set_error(err.to_string()),
    }
}

/// Yank selections joined by separator to a register.
pub fn yank_joined(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    register: char,
    separator: &str,
) {
    let (joined, selections) = {
        let doc = crate::doc_mut!(editor, &doc_id);
        let selections = doc.selection(view_id).len();
        let joined = yank_joined_in(&view_id, doc, separator);
        (joined, selections)
    };

    match editor.registers.write(register, vec![joined]) {
        Ok(_) => editor.set_status(format!(
            "joined and yanked {selections} selection{} to register {register}",
            if selections == 1 { "" } else { "s" }
        )),
        Err(err) => editor.set_error(err.to_string()),
    }
}

/// Delete the current selection, optionally yanking it first.
///
/// After deletion, exits select mode. For `change` behavior (enter insert mode),
/// the caller should set the mode after calling this.
pub fn delete_selection(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    register: char,
    yank: bool,
) {
    if register != '_' && yank {
        let values = {
            let doc = crate::doc_mut!(editor, &doc_id);
            yank_selections_in(&view_id, doc)
        };
        if let Err(err) = editor.registers.write(register, values) {
            editor.set_error(err.to_string());
            return;
        }
    }

    let doc = crate::doc_mut!(editor, &doc_id);
    delete_selection_in(&view_id, doc);
}

/// Change the current selection: delete it (optionally yanking) and enter insert mode.
///
/// If the selection spans whole lines, opens a new line above instead of
/// entering insert mode inline.
pub fn change_selection(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    register: char,
    yank: bool,
) -> bool {
    let only_whole_lines = {
        let doc = crate::doc_mut!(editor, &doc_id);
        selection_is_linewise(doc.selection(view_id), doc.text())
    };
    delete_selection(editor, view_id, doc_id, register, yank);
    // Return whether caller should use open_above (true) or just enter insert mode (false).
    only_whole_lines
}

pub fn yank_selections_in(
    target: &impl Identified,
    doc: &(impl TextContent + Selectable),
) -> Vec<String> {
    let text = doc.text().slice(..);
    doc.selection(target.id())
        .fragments(text)
        .map(Cow::into_owned)
        .collect()
}

pub fn yank_joined_in(
    target: &impl Identified,
    doc: &(impl TextContent + Selectable),
    separator: &str,
) -> String {
    let text = doc.text().slice(..);
    doc.selection(target.id())
        .fragments(text)
        .fold(String::new(), |mut acc, fragment| {
            if !acc.is_empty() {
                acc.push_str(separator);
            }
            acc.push_str(&fragment);
            acc
        })
}

pub fn delete_selection_in(target: &impl Identified, doc: &mut (impl Selectable + MutableText)) {
    let before_lines = doc.text().len_lines();
    let before_bytes = doc.text().len_bytes();
    let selection = doc.selection(target.id());
    let selection_len = selection.len();
    let build_start = Instant::now();
    let transaction =
        Transaction::delete_by_selection(doc.text(), selection, |range| (range.from(), range.to()));
    let build_dur = build_start.elapsed();
    log_command_phase("delete_selection", "build_transaction", build_dur, || {
        format!(
            "selections={} lines={} bytes={}",
            selection_len, before_lines, before_bytes
        )
    });
    let apply_start = Instant::now();
    doc.apply(&transaction, target.id());
    let apply_dur = apply_start.elapsed();
    log_command_phase("delete_selection", "apply", apply_dur, || {
        format!(
            "selections={} before_lines={} after_lines={} before_bytes={} after_bytes={}",
            selection_len,
            before_lines,
            doc.text().len_lines(),
            before_bytes,
            doc.text().len_bytes()
        )
    });
}

/// Paste register contents after selection.
pub fn paste_after(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    register: char,
    count: usize,
) {
    paste(editor, view_id, doc_id, register, Paste::After, count);
}

/// Paste register contents before selection.
pub fn paste_before(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    register: char,
    count: usize,
) {
    paste(editor, view_id, doc_id, register, Paste::Before, count);
}

/// Paste register contents at cursor.
pub fn paste_cursor(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    register: char,
    count: usize,
) {
    paste(editor, view_id, doc_id, register, Paste::Cursor, count);
}

/// Replace selection with register contents.
pub fn replace_with_yanked(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    register: char,
    count: usize,
) {
    use helix_core::regex::Regex;
    use once_cell::sync::Lazy;
    static LINE_ENDING_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"\r\n|\r|\n").unwrap());

    let register_start = Instant::now();
    let Some(values) = editor
        .registers
        .read(register, editor)
        .filter(|values| values.len() > 0)
    else {
        return;
    };
    let register_dur = register_start.elapsed();
    let register_len = values.len();
    log_command_phase("replace_with_yanked", "read_register", register_dur, || {
        format!(
            "register={} values={} count={}",
            register, register_len, count
        )
    });
    let scrolloff = editor.config().scrolloff;
    let doc = crate::doc!(editor, &doc_id);
    let before_lines = doc.text().len_lines();
    let before_bytes = doc.text().len_bytes();

    let map_value = |value: &Cow<str>| {
        let value = LINE_ENDING_REGEX.replace_all(value, doc.line_ending().as_str());
        let mut out = Tendril::from(value.as_ref());
        for _ in 1..count {
            out.push_str(&value);
        }
        out
    };
    let mut values_rev = values.rev().peekable();
    let last = values_rev.peek().unwrap();
    let repeat = std::iter::repeat(map_value(last));
    let mut values = values_rev
        .rev()
        .map(|value| map_value(&value))
        .chain(repeat);
    let selection = doc.selection(view_id);
    let selection_len = selection.len();
    let build_start = Instant::now();
    let transaction = Transaction::change_by_selection(doc.text(), selection, |range| {
        if !range.is_empty() {
            (range.from(), range.to(), Some(values.next().unwrap()))
        } else {
            (range.from(), range.to(), None)
        }
    });
    let build_dur = build_start.elapsed();
    log_command_phase(
        "replace_with_yanked",
        "build_transaction",
        build_dur,
        || {
            format!(
                "register={} values={} selections={} count={} lines={} bytes={}",
                register, register_len, selection_len, count, before_lines, before_bytes
            )
        },
    );
    drop(values);

    editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
        let apply_start = Instant::now();
        doc.apply(&transaction, view_id);
        doc.append_changes_to_history(view);
        crate::view::ensure_cursor_in_view_in(view, doc, scrolloff);
        let apply_dur = apply_start.elapsed();
        log_command_phase("replace_with_yanked", "apply_and_history", apply_dur, || {
            format!(
                "register={} values={} selections={} before_lines={} after_lines={} before_bytes={} after_bytes={}",
                register,
                register_len,
                selection_len,
                before_lines,
                doc.text().len_lines(),
                before_bytes,
                doc.text().len_bytes()
            )
        });
    });
}

#[derive(Copy, Clone)]
enum Paste {
    Before,
    After,
    Cursor,
}

fn paste(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    register: char,
    action: Paste,
    count: usize,
) {
    use helix_core::line_ending::get_line_ending_of_str;
    use helix_core::regex::Regex;
    use helix_core::text_folding::RopeSliceFoldExt;
    use once_cell::sync::Lazy;
    static LINE_ENDING_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"\r\n|\r|\n").unwrap());

    let register_start = Instant::now();
    let Some(values) = editor.registers.read(register, editor) else {
        return;
    };
    let values: Vec<_> = values.map(|value| value.to_string()).collect();
    let register_dur = register_start.elapsed();
    let value_count = values.len();
    let action_name = match action {
        Paste::Before => "paste_before",
        Paste::After => "paste_after",
        Paste::Cursor => "paste_cursor",
    };
    log_command_phase(action_name, "read_register", register_dur, || {
        format!(
            "register={} values={} count={}",
            register, value_count, count
        )
    });

    if values.is_empty() {
        return;
    }

    let mode = editor.mode;

    if mode == Mode::Insert {
        editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
            doc.append_changes_to_history(view);
        });
    }

    editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
        // If any value ends with a line ending, it's linewise paste
        let before_lines = doc.text().len_lines();
        let before_bytes = doc.text().len_bytes();
        let linewise = values
            .iter()
            .any(|value| get_line_ending_of_str(value).is_some());

        let map_value = |value: &str| {
            let value = LINE_ENDING_REGEX.replace_all(value, doc.line_ending().as_str());
            let mut out = Tendril::from(value.as_ref());
            for _ in 1..count {
                out.push_str(&value);
            }
            out
        };

        let repeat = std::iter::repeat(map_value(values.last().unwrap()));
        let mut values = values.iter().map(|value| map_value(value)).chain(repeat);

        let text = doc.text();
        let selection = doc.selection(view_id);
        let selection_len = selection.len();
        let annotations = view.text_annotations(doc);
        let folds = &annotations.folds;

        let mut offset = 0;
        let mut ranges = SmallVec::with_capacity(selection.len());

        let build_start = Instant::now();
        let mut transaction = Transaction::change_by_selection(text, selection, |range| {
            let pos = match (action, linewise) {
                (Paste::Before, true) => text.line_to_char(text.char_to_line(range.from())),
                (Paste::After, true) => {
                    let line = range.line_range(text.slice(..)).1;
                    text.line_to_char(text.slice(..).next_folded_line(folds, line))
                }
                (Paste::Before, false) => range.from(),
                (Paste::After, false) => text
                    .slice(..)
                    .next_folded_char(folds, prev_grapheme_boundary(text.slice(..), range.to())),
                (Paste::Cursor, _) => range.cursor(text.slice(..)),
            };

            let value = values.next();
            let value_len = value
                .as_ref()
                .map(|content| content.chars().count())
                .unwrap_or_default();
            let anchor = offset + pos;

            let new_range =
                Range::new(anchor, anchor + value_len).with_direction(range.direction());
            ranges.push(new_range);
            offset += value_len;

            (pos, pos, value)
        });
        let build_dur = build_start.elapsed();
        log_command_phase(action_name, "build_transaction", build_dur, || {
            format!(
                "register={} values={} selections={} count={} linewise={} lines={} bytes={}",
                register,
                value_count,
                selection_len,
                count,
                linewise,
                before_lines,
                before_bytes
            )
        });

        if mode == Mode::Normal {
            transaction =
                transaction.with_selection(Selection::new(ranges, selection.primary_index()));
        }

        drop(annotations);
        let apply_start = Instant::now();
        doc.apply(&transaction, view_id);
        doc.append_changes_to_history(view);
        let apply_dur = apply_start.elapsed();
        log_command_phase(action_name, "apply_and_history", apply_dur, || {
            format!(
                "register={} values={} selections={} before_lines={} after_lines={} before_bytes={} after_bytes={} linewise={}",
                register,
                value_count,
                selection_len,
                before_lines,
                doc.text().len_lines(),
                before_bytes,
                doc.text().len_bytes(),
                linewise
            )
        });
    });
}

// ─── Indent / Unindent ─────────────────────────────────────────────

/// Indent selected lines by count levels.
pub fn indent(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    let doc = crate::doc_mut!(editor, &doc_id);
    indent_in(&view_id, doc, count);
    exit_select_mode(editor, view_id, doc_id);
}

/// Unindent selected lines by count levels.
pub fn unindent(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    let doc = crate::doc_mut!(editor, &doc_id);
    unindent_in(&view_id, doc, count);
    exit_select_mode(editor, view_id, doc_id);
}

pub fn indent_in(
    target: &impl Identified,
    doc: &mut (impl Selectable + MutableText + Indentation),
    count: usize,
) {
    let lines = get_lines_in(target, doc);
    let indent = Tendril::from(doc.indent_style().as_str().repeat(count));

    let transaction = Transaction::change(
        doc.text(),
        lines.into_iter().filter_map(|line| {
            let is_blank = doc.text().line(line).chunks().all(|s| s.trim().is_empty());
            if is_blank {
                return None;
            }
            let pos = doc.text().line_to_char(line);
            Some((pos, pos, Some(indent.clone())))
        }),
    );
    doc.apply(&transaction, target.id());
}

pub fn unindent_in(
    target: &impl Identified,
    doc: &mut (impl Selectable + MutableText + Indentation + TextMetrics),
    count: usize,
) {
    let lines = get_lines_in(target, doc);
    let mut changes = Vec::with_capacity(lines.len());
    let tab_width = doc.tab_width();
    let indent_width = count * doc.indent_width();

    for line_idx in lines {
        let line = doc.text().line(line_idx);
        let mut width = 0;
        let mut pos = 0;

        for ch in line.chars() {
            match ch {
                ' ' => width += 1,
                '\t' => width = (width / tab_width + 1) * tab_width,
                _ => break,
            }
            pos += 1;
            if width >= indent_width {
                break;
            }
        }

        if pos > 0 {
            let start = doc.text().line_to_char(line_idx);
            changes.push((start, start + pos, None));
        }
    }

    let transaction = Transaction::change(doc.text(), changes.into_iter());
    doc.apply(&transaction, target.id());
}

// ─── Case switching ────────────────────────────────────────────────

/// Switch case of each character in selections (upper↔lower).
pub fn switch_case(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    switch_case_impl(editor, view_id, doc_id, |string| {
        string
            .chars()
            .flat_map(|ch| {
                if ch.is_lowercase() {
                    CaseSwitcher::Upper(ch.to_uppercase())
                } else if ch.is_uppercase() {
                    CaseSwitcher::Lower(ch.to_lowercase())
                } else {
                    CaseSwitcher::Keep(Some(ch))
                }
            })
            .collect()
    });
}

/// Convert selections to uppercase.
pub fn switch_to_uppercase(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    switch_case_impl(editor, view_id, doc_id, |string| {
        string.chunks().map(|chunk| chunk.to_uppercase()).collect()
    });
}

/// Convert selections to lowercase.
pub fn switch_to_lowercase(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    switch_case_impl(editor, view_id, doc_id, |string| {
        string.chunks().map(|chunk| chunk.to_lowercase()).collect()
    });
}

fn switch_case_impl<F>(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, change_fn: F)
where
    F: Fn(RopeSlice) -> Tendril,
{
    let doc = crate::doc_mut!(editor, &doc_id);
    switch_case_impl_in(&view_id, doc, change_fn);
    exit_select_mode(editor, view_id, doc_id);
}

fn switch_case_impl_in<F>(
    target: &impl Identified,
    doc: &mut (impl Selectable + MutableText),
    change_fn: F,
) where
    F: Fn(RopeSlice) -> Tendril,
{
    let selection = doc.selection(target.id());
    let selection_len = selection.len();
    let before_lines = doc.text().len_lines();
    let before_bytes = doc.text().len_bytes();
    let build_start = Instant::now();
    let transaction = Transaction::change_by_selection(doc.text(), selection, |range| {
        let text: Tendril = change_fn(range.slice(doc.text().slice(..)));
        (range.from(), range.to(), Some(text))
    });
    let build_dur = build_start.elapsed();
    log_command_phase("switch_case", "build_transaction", build_dur, || {
        format!(
            "selections={} lines={} bytes={}",
            selection_len, before_lines, before_bytes
        )
    });
    let apply_start = Instant::now();
    doc.apply(&transaction, target.id());
    let apply_dur = apply_start.elapsed();
    log_command_phase("switch_case", "apply", apply_dur, || {
        format!(
            "selections={} before_lines={} after_lines={} before_bytes={} after_bytes={}",
            selection_len,
            before_lines,
            doc.text().len_lines(),
            before_bytes,
            doc.text().len_bytes()
        )
    });
}

enum CaseSwitcher {
    Upper(ToUppercase),
    Lower(ToLowercase),
    Keep(Option<char>),
}

impl Iterator for CaseSwitcher {
    type Item = char;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            CaseSwitcher::Upper(upper) => upper.next(),
            CaseSwitcher::Lower(lower) => lower.next(),
            CaseSwitcher::Keep(ch) => ch.take(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            CaseSwitcher::Upper(upper) => upper.size_hint(),
            CaseSwitcher::Lower(lower) => lower.size_hint(),
            CaseSwitcher::Keep(ch) => {
                let n = if ch.is_some() { 1 } else { 0 };
                (n, Some(n))
            }
        }
    }
}

impl ExactSizeIterator for CaseSwitcher {}

// ─── Undo / Redo ───────────────────────────────────────────────────

/// Undo the last change, up to `count` times.
pub fn undo(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    let applied = editor.with_view_doc_mut(view_id, doc_id, |view, doc| undo_in(view, doc, count));
    if applied < count {
        editor.set_status("Already at oldest change");
    }
}

/// Redo the last undone change, up to `count` times.
pub fn redo(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    let applied = editor.with_view_doc_mut(view_id, doc_id, |view, doc| redo_in(view, doc, count));
    if applied < count {
        editor.set_status("Already at newest change");
    }
}

/// Go to an earlier state in the undo history.
pub fn earlier(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    use helix_core::history::UndoKind;
    let applied = editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
        earlier_in(view, doc, UndoKind::Steps(1), count)
    });
    if applied < count {
        editor.set_status("Already at oldest change");
    }
}

/// Go to a later state in the undo history.
pub fn later(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    use helix_core::history::UndoKind;
    let applied = editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
        later_in(view, doc, UndoKind::Steps(1), count)
    });
    if applied < count {
        editor.set_status("Already at newest change");
    }
}

/// Commit current changes to the undo history as a checkpoint.
pub fn commit_undo_checkpoint(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
        commit_undo_checkpoint_in(view, doc);
    });
}

pub fn undo_in<V>(target: &mut V, doc: &mut impl Undoable<V>, count: usize) -> usize {
    let mut applied = 0;
    for _ in 0..count {
        if !doc.undo(target) {
            break;
        }
        applied += 1;
    }
    applied
}

pub fn redo_in<V>(target: &mut V, doc: &mut impl Undoable<V>, count: usize) -> usize {
    let mut applied = 0;
    for _ in 0..count {
        if !doc.redo(target) {
            break;
        }
        applied += 1;
    }
    applied
}

pub fn earlier_in<V>(
    target: &mut V,
    doc: &mut impl Undoable<V>,
    kind: helix_core::history::UndoKind,
    count: usize,
) -> usize {
    let mut applied = 0;
    for _ in 0..count {
        if !doc.earlier(target, kind) {
            break;
        }
        applied += 1;
    }
    applied
}

pub fn later_in<V>(
    target: &mut V,
    doc: &mut impl Undoable<V>,
    kind: helix_core::history::UndoKind,
    count: usize,
) -> usize {
    let mut applied = 0;
    for _ in 0..count {
        if !doc.later(target, kind) {
            break;
        }
        applied += 1;
    }
    applied
}

pub fn commit_undo_checkpoint_in<V>(target: &mut V, doc: &mut impl Undoable<V>) {
    doc.commit_undo_checkpoint(target);
}

// ─── Comment toggling ──────────────────────────────────────────────

type CommentTransactionFn =
    fn(Option<&str>, Option<&[BlockCommentToken]>, &Rope, &Selection) -> Transaction;

fn toggle_comments_impl(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    comment_transaction: CommentTransactionFn,
) {
    let doc = crate::doc_mut!(editor, &doc_id);
    let line_token: Option<&str> = doc
        .language_config()
        .and_then(|lc| lc.comment_tokens.as_ref())
        .and_then(|tc| tc.first())
        .map(|tc| tc.as_str());
    let block_tokens: Option<&[BlockCommentToken]> = doc
        .language_config()
        .and_then(|lc| lc.block_comment_tokens.as_ref())
        .map(|tc| &tc[..]);

    let transaction =
        comment_transaction(line_token, block_tokens, doc.text(), doc.selection(view_id));

    doc.apply(&transaction, view_id);
    exit_select_mode(editor, view_id, doc_id);
}

/// Toggle comments using the best available comment style.
pub fn toggle_comments(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    toggle_comments_impl(
        editor,
        view_id,
        doc_id,
        |line_token, block_tokens, doc, selection| {
            let text = doc.slice(..);

            if line_token.is_some() && block_tokens.is_none() {
                return comment::toggle_line_comments(doc, selection, line_token);
            }

            let split_lines = comment::split_lines_of_selection(text, selection);
            let default_block_tokens = &[BlockCommentToken::default()];
            let block_comment_tokens = block_tokens.unwrap_or(default_block_tokens);

            let (line_commented, line_comment_changes) =
                comment::find_block_comments(block_comment_tokens, text, &split_lines);

            if line_commented {
                return comment::create_block_comment_transaction(
                    doc,
                    &split_lines,
                    line_commented,
                    line_comment_changes,
                )
                .0;
            }

            let (block_commented, comment_changes) =
                comment::find_block_comments(block_comment_tokens, text, selection);

            if block_commented {
                return comment::create_block_comment_transaction(
                    doc,
                    selection,
                    block_commented,
                    comment_changes,
                )
                .0;
            }

            if line_token.is_none() && block_tokens.is_some() {
                return comment::create_block_comment_transaction(
                    doc,
                    &split_lines,
                    line_commented,
                    line_comment_changes,
                )
                .0;
            }

            comment::toggle_line_comments(doc, selection, line_token)
        },
    );
}

/// Toggle line comments.
pub fn toggle_line_comments(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    toggle_comments_impl(
        editor,
        view_id,
        doc_id,
        |line_token, block_tokens, doc, selection| {
            if line_token.is_none() && block_tokens.is_some() {
                let default_block_tokens = &[BlockCommentToken::default()];
                let block_comment_tokens = block_tokens.unwrap_or(default_block_tokens);
                comment::toggle_block_comments(
                    doc,
                    &comment::split_lines_of_selection(doc.slice(..), selection),
                    block_comment_tokens,
                )
            } else {
                comment::toggle_line_comments(doc, selection, line_token)
            }
        },
    );
}

/// Toggle block comments.
pub fn toggle_block_comments(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    toggle_comments_impl(
        editor,
        view_id,
        doc_id,
        |line_token, block_tokens, doc, selection| {
            if line_token.is_some() && block_tokens.is_none() {
                comment::toggle_line_comments(doc, selection, line_token)
            } else {
                let default_block_tokens = &[BlockCommentToken::default()];
                let block_comment_tokens = block_tokens.unwrap_or(default_block_tokens);
                comment::toggle_block_comments(doc, selection, block_comment_tokens)
            }
        },
    );
}

// ─── Join selections ────────────────────────────────────────────────

/// Join selected lines, replacing line breaks with spaces.
pub fn join_selections(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    join_selections_impl(editor, view_id, doc_id, false);
}

/// Join selected lines, selecting the inserted spaces.
pub fn join_selections_space(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    join_selections_impl(editor, view_id, doc_id, true);
}

fn join_selections_impl(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    select_space: bool,
) {
    use helix_core::line_ending::line_end_char_index;
    use helix_stdx::rope::RopeSliceExt;
    use movement::skip_while;

    let doc = crate::doc_mut!(editor, &doc_id);
    let text = doc.text();
    let slice = text.slice(..);
    let before_lines = text.len_lines();
    let before_bytes = text.len_bytes();
    let selection_len = doc.selection(view_id).len();

    let comment_tokens = doc
        .language_config()
        .and_then(|config| config.comment_tokens.as_deref())
        .unwrap_or(&[]);
    let token_start = Instant::now();
    let mut comment_tokens: Vec<&str> = comment_tokens.iter().map(|x| x.as_str()).collect();
    comment_tokens.sort_unstable_by_key(|x| std::cmp::Reverse(x.len()));
    let token_dur = token_start.elapsed();
    log_command_phase(
        "join_selections",
        "prepare_comment_tokens",
        token_dur,
        || {
            format!(
                "tokens={} selections={} select_space={} lines={} bytes={}",
                comment_tokens.len(),
                selection_len,
                select_space,
                before_lines,
                before_bytes
            )
        },
    );

    let mut changes = Vec::new();
    let collect_start = Instant::now();
    for selection in doc.selection(view_id) {
        let (start, mut end) = selection.line_range(slice);
        if start == end {
            end = (end + 1).min(text.len_lines() - 1);
        }
        let lines = start..end;

        changes.reserve(lines.len());

        let first_line_idx = slice.line_to_char(start);
        let first_line_idx = skip_while(slice, first_line_idx, |ch| matches!(ch, ' ' | '\t'))
            .unwrap_or(first_line_idx);
        let first_line = slice.slice(first_line_idx..);
        let mut current_comment_token = comment_tokens
            .iter()
            .find(|token| first_line.starts_with(token));

        for line in lines {
            let start = line_end_char_index(&slice, line);
            let mut end = text.line_to_char(line + 1);
            end = skip_while(slice, end, |ch| matches!(ch, ' ' | '\t')).unwrap_or(end);
            let slice_from_end = slice.slice(end..);
            if let Some(token) = comment_tokens
                .iter()
                .find(|token| slice_from_end.starts_with(token))
            {
                if Some(token) == current_comment_token {
                    end += token.chars().count();
                    end = skip_while(slice, end, |ch| matches!(ch, ' ' | '\t')).unwrap_or(end);
                } else {
                    current_comment_token = Some(token);
                }
            }

            let separator = if end == line_end_char_index(&slice, line + 1) {
                None
            } else {
                Some(Tendril::from(" "))
            };
            changes.push((start, end, separator));
        }
    }
    let collect_dur = collect_start.elapsed();
    log_command_phase("join_selections", "collect_changes", collect_dur, || {
        format!(
            "changes={} selections={} select_space={} lines={} bytes={}",
            changes.len(),
            selection_len,
            select_space,
            before_lines,
            before_bytes
        )
    });

    if changes.is_empty() {
        return;
    }

    changes.sort_unstable_by_key(|(from, _to, _text)| *from);
    changes.dedup();

    let build_start = Instant::now();
    let transaction = if select_space {
        let mut offset: usize = 0;
        let ranges: SmallVec<_> = changes
            .iter()
            .filter_map(|change| {
                if change.2.is_some() {
                    let range = Range::point(change.0 - offset);
                    offset += change.1 - change.0 - 1;
                    Some(range)
                } else {
                    offset += change.1 - change.0;
                    None
                }
            })
            .collect();
        let t = Transaction::change(text, changes.into_iter());
        if ranges.is_empty() {
            t
        } else {
            let selection = Selection::new(ranges, 0);
            t.with_selection(selection)
        }
    } else {
        Transaction::change(text, changes.into_iter())
    };
    let build_dur = build_start.elapsed();
    log_command_phase("join_selections", "build_transaction", build_dur, || {
        format!(
            "changes={} selections={} select_space={} lines={} bytes={}",
            transaction.changes().len(),
            selection_len,
            select_space,
            before_lines,
            before_bytes
        )
    });

    let apply_start = Instant::now();
    doc.apply(&transaction, view_id);
    let apply_dur = apply_start.elapsed();
    log_command_phase("join_selections", "apply", apply_dur, || {
        format!(
            "selections={} select_space={} before_lines={} after_lines={} before_bytes={} after_bytes={}",
            selection_len,
            select_space,
            before_lines,
            doc.text().len_lines(),
            before_bytes,
            doc.text().len_bytes()
        )
    });
}

// ─── View / Navigation ─────────────────────────────────────────────

/// Jump forward in the jumplist.
pub fn jump_forward(editor: &mut Editor, view_id: ViewId, _doc_id: DocumentId, count: usize) {
    editor.jump_forward(view_id, count);
}

/// Jump backward in the jumplist.
pub fn jump_backward(editor: &mut Editor, view_id: ViewId, _doc_id: DocumentId, count: usize) {
    editor.jump_backward(view_id, count);
}

/// Align the view so the cursor is at the center.
pub fn align_view_center(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    use crate::Align;
    editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
        crate::align_view_in(doc, view, Align::Center);
    });
}

/// Align the view so the cursor is at the top.
pub fn align_view_top(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    use crate::Align;
    editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
        crate::align_view_in(doc, view, Align::Top);
    });
}

/// Align the view so the cursor is at the bottom.
pub fn align_view_bottom(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    use crate::Align;
    editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
        crate::align_view_in(doc, view, Align::Bottom);
    });
}

/// Align the view so the cursor is at the horizontal middle.
pub fn align_view_middle(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    use helix_core::visual_offset_from_block;
    editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
        let text_fmt = doc.text_format(view.text_area_width(doc), None);
        if text_fmt.soft_wrap {
            return;
        }
        let doc_text = doc.text().slice(..);
        let pos = doc.selection(view_id).primary().cursor(doc_text);
        let annotations = view.text_annotations(doc);
        let pos = visual_offset_from_block(
            doc_text,
            view.view_offset(doc).anchor,
            pos,
            &text_fmt,
            &annotations,
        )
        .0;

        let mut offset = view.view_offset(doc);
        offset.horizontal_offset = pos
            .col
            .saturating_sub((view.text_area(doc).width as usize) / 2);
        drop(annotations);
        view.set_view_offset(doc, offset);
    });
}

// ─── Add newline ─────────────────────────────────────────────────────

/// Add `count` newlines above the current selection without entering insert mode.
pub fn add_newline_above(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    add_newline_impl(editor, view_id, doc_id, count, true);
}

/// Add `count` newlines below the current selection without entering insert mode.
pub fn add_newline_below(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    add_newline_impl(editor, view_id, doc_id, count, false);
}

fn add_newline_impl(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
    above: bool,
) {
    let doc = crate::doc_mut!(editor, &doc_id);
    let selection = doc.selection(view_id);
    let text = doc.text();
    let slice = text.slice(..);

    let changes = selection.into_iter().map(|range| {
        let (start, end) = range.line_range(slice);
        let line = if above { start } else { end + 1 };
        let pos = text.line_to_char(line);
        (
            pos,
            pos,
            Some(doc.line_ending().as_str().repeat(count).into()),
        )
    });

    let transaction = Transaction::change(text, changes);
    doc.apply(&transaction, view_id);
}

// ─── Move selection (move lines up/down) ─────────────────────────────

#[derive(Clone, Copy)]
pub enum MoveDirection {
    Above,
    Below,
}

#[derive(Clone)]
struct ExtendedChange {
    line_start: usize,
    line_end: usize,
    line_text: Option<Tendril>,
    line_selection: Option<(usize, usize)>,
}

/// Move lines covered by the current selection up or down.
pub fn move_lines(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    direction: MoveDirection,
) {
    let doc = crate::doc_mut!(editor, &doc_id);
    let selection = doc.selection(view_id);
    let text = doc.text();
    let slice = text.slice(..);
    let mut last_step_changes: Vec<ExtendedChange> = vec![];
    let mut at_doc_edge = false;
    let all_changes = selection.into_iter().map(|range| {
        let (start, end) = range.line_range(slice);
        let line_start = text.line_to_char(start);
        let line_end = line_end_char_index(&slice, end);
        let line_text = text.slice(line_start..line_end).to_string();
        let next_line = match direction {
            MoveDirection::Above => start.saturating_sub(1),
            MoveDirection::Below => end + 1,
        };
        let rel_pos_anchor = range.anchor - line_start;
        let rel_pos_head = range.head - line_start;
        let cursor_rel_pos = (rel_pos_anchor, rel_pos_head);
        if next_line == start || next_line >= text.len_lines() || at_doc_edge {
            at_doc_edge = true;
            let changes = vec![ExtendedChange {
                line_start,
                line_end,
                line_text: Some(line_text.into()),
                line_selection: Some(cursor_rel_pos),
            }];
            last_step_changes = changes.clone();
            changes
        } else {
            let next_line_start = text.line_to_char(next_line);
            let next_line_end = line_end_char_index(&slice, next_line);
            let next_line_text = text.slice(next_line_start..next_line_end).to_string();
            let changes = match direction {
                MoveDirection::Above => vec![
                    ExtendedChange {
                        line_start: next_line_start,
                        line_end: next_line_end,
                        line_text: Some(line_text.into()),
                        line_selection: Some(cursor_rel_pos),
                    },
                    ExtendedChange {
                        line_start,
                        line_end,
                        line_text: Some(next_line_text.into()),
                        line_selection: None,
                    },
                ],
                MoveDirection::Below => vec![
                    ExtendedChange {
                        line_start,
                        line_end,
                        line_text: Some(next_line_text.into()),
                        line_selection: None,
                    },
                    ExtendedChange {
                        line_start: next_line_start,
                        line_end: next_line_end,
                        line_text: Some(line_text.into()),
                        line_selection: Some(cursor_rel_pos),
                    },
                ],
            };
            let changes = if last_step_changes.len() > 1 {
                evaluate_move_changes(last_step_changes.clone(), changes, &direction)
            } else {
                changes
            };
            last_step_changes = changes.clone();
            changes
        }
    });

    let mut flattened: Vec<Vec<ExtendedChange>> = all_changes.into_iter().collect();
    let last_changes = flattened.pop().unwrap_or_default();
    let acc_cursors = get_adjusted_selection(doc, &last_changes, direction, at_doc_edge);
    let changes = last_changes
        .into_iter()
        .map(|change| (change.line_start, change.line_end, change.line_text));
    let new_sel = Selection::new(acc_cursors.into(), 0);
    let transaction = Transaction::change(doc.text(), changes);
    doc.apply(&transaction, view_id);
    doc.set_selection(view_id, new_sel);
}

/// Merge changes from subsequent cursors during line moves.
fn evaluate_move_changes(
    mut last_changes: Vec<ExtendedChange>,
    current_changes: Vec<ExtendedChange>,
    direction: &MoveDirection,
) -> Vec<ExtendedChange> {
    let mut current_it = current_changes.into_iter();
    if let (Some(mut last), Some(mut current_first), Some(current_last)) =
        (last_changes.pop(), current_it.next(), current_it.next())
    {
        if last.line_start == current_first.line_start {
            match direction {
                MoveDirection::Above => {
                    last.line_start = current_last.line_start;
                    last.line_end = current_last.line_end;
                    if let Some(first) = last_changes.pop() {
                        last_changes.push(first)
                    }
                    last_changes.extend(vec![current_first, last]);
                    last_changes
                }
                MoveDirection::Below => {
                    current_first.line_start = last_changes[0].line_start;
                    current_first.line_end = last_changes[0].line_end;
                    last_changes[0] = current_first;
                    last_changes.extend(vec![last, current_last]);
                    last_changes
                }
            }
        } else {
            if let Some(first) = last_changes.pop() {
                last_changes.push(first)
            }
            last_changes.extend(vec![last, current_first, current_last]);
            last_changes
        }
    } else {
        last_changes
    }
}

fn get_adjusted_selection(
    doc: &Document,
    last_changes: &[ExtendedChange],
    direction: MoveDirection,
    at_doc_edge: bool,
) -> Vec<Range> {
    let mut first_change_len = 0;
    let mut next_start = 0;
    let mut acc_cursors: Vec<Range> = vec![];

    for change in last_changes.iter() {
        let change_len = change.line_text.as_ref().map_or(0, |x| x.chars().count());
        if let Some((rel_anchor, rel_head)) = change.line_selection {
            let (anchor, head) = if at_doc_edge {
                let anchor = change.line_start + rel_anchor;
                let head = change.line_start + rel_head;
                (anchor, head)
            } else {
                match direction {
                    MoveDirection::Above => {
                        if next_start == 0 {
                            next_start = change.line_start;
                        }
                        let anchor = next_start + rel_anchor;
                        let head = next_start + rel_head;
                        next_start += change_len + doc.line_ending().len_chars();
                        (anchor, head)
                    }
                    MoveDirection::Below => {
                        let anchor = change.line_start + first_change_len + rel_anchor - change_len;
                        let head = change.line_start + first_change_len + rel_head - change_len;
                        (anchor, head)
                    }
                }
            };
            let cursor = Range::new(anchor, head);
            if let Some(last) = acc_cursors.pop() {
                if cursor.overlaps(&last) {
                    acc_cursors.push(last);
                } else {
                    acc_cursors.push(last);
                    acc_cursors.push(cursor);
                };
            } else {
                acc_cursors.push(cursor);
            };
        } else {
            first_change_len = change.line_text.as_ref().map_or(0, |x| x.chars().count());
            next_start = 0;
        };
    }
    acc_cursors
}

// ─── Insert tab ──────────────────────────────────────────────────────

/// Insert `count` copies of the document's indent style at each cursor.
pub fn insert_tab(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    let doc = crate::doc_mut!(editor, &doc_id);
    let indent = Tendril::from(doc.indent_style().as_str().repeat(count));
    let transaction = Transaction::insert(
        doc.text(),
        &doc.selection(view_id).clone().cursors(doc.text().slice(..)),
        indent,
    );
    doc.apply(&transaction, view_id);
}

// ─── Insert mode: character insertion ────────────────────────────────

/// Compute the transaction for inserting a character, handling auto-pairs.
///
/// Returns the transaction to apply (or `None` if no insertion needed).
/// The caller is responsible for dispatching PostInsertChar after applying.
pub fn insert_char_transaction(
    editor: &Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    c: char,
) -> Option<Transaction> {
    let doc = crate::doc!(editor, &doc_id);
    let text = doc.text();
    let selection = doc.selection(view_id);

    let loader: &helix_core::syntax::Loader = &editor.syn_loader.load();
    let auto_pairs = doc.auto_pairs(editor, loader, &view_id);

    auto_pairs
        .as_ref()
        .and_then(|ap| auto_pairs::hook(text, selection, c, ap))
        .or_else(|| insert_single_char(text, selection, c))
}

/// Plain character insertion (no auto-pairs).
#[allow(clippy::unnecessary_wraps)]
fn insert_single_char(doc: &Rope, selection: &Selection, ch: char) -> Option<Transaction> {
    let cursors = selection.clone().cursors(doc.slice(..));
    let mut t = Tendril::new();
    t.push(ch);
    Some(Transaction::insert(doc, &cursors, t))
}

// ─── Insert mode: delete backward ───────────────────────────────────

/// Delete character(s) backward in insert mode, respecting indent units and auto-pairs.
pub fn delete_char_backward(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    let doc = crate::doc!(editor, &doc_id);
    let text = doc.text().slice(..);
    let tab_width = doc.tab_width();
    let indent_width = doc.indent_width();

    let loader: &helix_core::syntax::Loader = &editor.syn_loader.load();
    let auto_pairs = doc.auto_pairs(editor, loader, &view_id);

    let transaction =
        Transaction::delete_by_selection(doc.text(), doc.selection(view_id), |range| {
            let pos = range.cursor(text);
            if pos == 0 {
                return (pos, pos);
            }
            let line_start_pos = text.line_to_char(range.cursor_line(text));
            let fragment = Cow::from(text.slice(line_start_pos..pos));
            if !fragment.is_empty() && fragment.chars().all(|ch| ch == ' ' || ch == '\t') {
                if text.get_char(pos.saturating_sub(1)) == Some('\t') {
                    (graphemes::nth_prev_grapheme_boundary(text, pos, 1), pos)
                } else {
                    let width: usize = fragment
                        .chars()
                        .map(|ch| {
                            if ch == '\t' {
                                tab_width
                            } else {
                                ch.width().unwrap_or(1)
                            }
                        })
                        .sum();
                    let mut drop = width % indent_width;
                    if drop == 0 {
                        drop = indent_width
                    };
                    let mut chars = fragment.chars().rev();
                    let mut start = pos;
                    for _ in 0..drop {
                        match chars.next() {
                            Some(' ') => start -= 1,
                            _ => break,
                        }
                    }
                    (start, pos)
                }
            } else {
                match (
                    text.get_char(pos.saturating_sub(1)),
                    text.get_char(pos),
                    auto_pairs,
                ) {
                    (Some(_x), Some(_y), Some(ap))
                        if range.is_single_grapheme(text)
                            && ap.get(_x).is_some()
                            && ap.get(_x).unwrap().open == _x
                            && ap.get(_x).unwrap().close == _y =>
                    {
                        (
                            graphemes::nth_prev_grapheme_boundary(text, pos, count),
                            graphemes::nth_next_grapheme_boundary(text, pos, count),
                        )
                    }
                    _ => (graphemes::nth_prev_grapheme_boundary(text, pos, count), pos),
                }
            }
        });
    let doc = crate::doc_mut!(editor, &doc_id);
    doc.apply(&transaction, view_id);
}

// ─── Insert mode: delete forward ─────────────────────────────────────

type Deletion = (usize, usize);

/// Delete character(s) forward in insert mode.
pub fn delete_char_forward(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    delete_by_selection_insert_mode(
        editor,
        view_id,
        doc_id,
        |text, range| {
            let pos = range.cursor(text);
            (pos, graphemes::nth_next_grapheme_boundary(text, pos, count))
        },
        Direction::Forward,
    )
}

/// Delete word backward in insert mode.
pub fn delete_word_backward(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    count: usize,
) {
    delete_by_selection_insert_mode(
        editor,
        view_id,
        doc_id,
        |text, range| {
            let anchor =
                movement::move_prev_word_start(text, &TextAnnotations::default(), *range, count)
                    .from();
            let next = Range::new(anchor, range.cursor(text));
            let range = exclude_cursor(text, next, *range);
            (range.from(), range.to())
        },
        Direction::Backward,
    );
}

/// Delete word forward in insert mode.
pub fn delete_word_forward(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, count: usize) {
    delete_by_selection_insert_mode(
        editor,
        view_id,
        doc_id,
        |text, range| {
            let head =
                movement::move_next_word_end(text, &TextAnnotations::default(), *range, count).to();
            (range.cursor(text), head)
        },
        Direction::Forward,
    );
}

/// Exclude the cursor from a range (used by word-delete operations).
fn exclude_cursor(text: RopeSlice, range: Range, cursor: Range) -> Range {
    if range.to() == cursor.to() && text.len_chars() != cursor.to() {
        Range::new(range.from(), prev_grapheme_boundary(text, cursor.to()))
    } else {
        range
    }
}

/// Core insert-mode deletion helper. Applies `f` to compute deletion ranges,
/// handles forward-delete cursor adjustment and EOF newline insertion.
#[inline]
fn delete_by_selection_insert_mode(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    mut f: impl FnMut(RopeSlice, &Range) -> Deletion,
    direction: Direction,
) {
    let doc = crate::doc_mut!(editor, &doc_id);
    let text = doc.text().slice(..);
    let mut selection = SmallVec::new();
    let mut insert_newline = false;
    let text_len = text.len_chars();
    let mut transaction =
        Transaction::delete_by_selection(doc.text(), doc.selection(view_id), |range| {
            let (start, end) = f(text, range);
            if direction == Direction::Forward {
                let mut range = *range;
                if range.head > range.anchor {
                    insert_newline |= end == text_len;
                    range.head += 1;
                }
                selection.push(range);
            }
            (start, end)
        });

    if insert_newline {
        transaction = transaction.insert_at_eof(doc.line_ending().as_str().into());
    }

    if direction == Direction::Forward {
        doc.set_selection(
            view_id,
            Selection::new(selection, doc.selection(view_id).primary_index()),
        );
    }
    doc.apply(&transaction, view_id);
}

// ─── Insert newline ──────────────────────────────────────────────────

/// Insert a newline at each cursor position in insert mode.
///
/// Handles indent calculation, comment continuation, auto-pair expansion,
/// and trailing whitespace trimming.
pub fn insert_newline(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    let config = editor.config();
    let doc = crate::doc!(editor, &doc_id);
    let loader = editor.syn_loader.load();
    let text = doc.text().slice(..);
    let line_ending = doc.line_ending().as_str();

    let contents = doc.text();
    let selection = doc.selection(view_id);
    let mut ranges = SmallVec::with_capacity(selection.len());

    let mut global_offs = 0;
    let mut new_text = String::new();

    let continue_comment_tokens = if config.continue_comments {
        doc.language_config()
            .and_then(|config| config.comment_tokens.as_ref())
    } else {
        None
    };

    let mut last_pos = 0;
    let mut transaction = Transaction::change_by_selection(contents, selection, |range| {
        let mut chars_deleted = 0;
        let pos = range.cursor(text);

        let prev = if pos == 0 {
            ' '
        } else {
            contents.char(pos - 1)
        };
        let curr = contents.get_char(pos).unwrap_or(' ');

        let current_line = text.char_to_line(pos);
        let line_start = text.line_to_char(current_line);

        let continue_comment_token = continue_comment_tokens
            .and_then(|tokens| comment::get_comment_token(text, tokens, current_line));

        let (from, to, local_offs) = if let Some(idx) =
            text.slice(line_start..pos).last_non_whitespace_char()
        {
            let first_trailing_whitespace_char = (line_start + idx + 1).clamp(last_pos, pos);
            last_pos = pos;
            let line = text.line(current_line);

            let indent_val = match line.first_non_whitespace_char() {
                Some(p) if continue_comment_token.is_some() => line.slice(..p).to_string(),
                _ => doc.indent_for_newline(
                    &loader,
                    &config.indent_heuristic,
                    text,
                    current_line,
                    pos,
                    current_line,
                ),
            };

            let loader2: &helix_core::syntax::Loader = &editor.syn_loader.load();
            let on_auto_pair = doc
                .auto_pairs(editor, loader2, &view_id)
                .and_then(|pairs| pairs.get(prev))
                .is_some_and(|pair| pair.open == prev && pair.close == curr);

            let local_offs = if let Some(token) = continue_comment_token {
                new_text.reserve_exact(line_ending.len() + indent_val.len() + token.len() + 1);
                new_text.push_str(line_ending);
                new_text.push_str(&indent_val);
                new_text.push_str(token);
                new_text.push(' ');
                new_text.chars().count()
            } else if on_auto_pair {
                let inner_indent = indent_val.clone() + doc.indent_style().as_str();
                new_text
                    .reserve_exact(line_ending.len() * 2 + indent_val.len() + inner_indent.len());
                new_text.push_str(line_ending);
                new_text.push_str(&inner_indent);

                let local_offs = new_text.chars().count();
                new_text.push_str(line_ending);
                new_text.push_str(&indent_val);

                local_offs
            } else {
                new_text.reserve_exact(line_ending.len() + indent_val.len());
                new_text.push_str(line_ending);
                new_text.push_str(&indent_val);

                new_text.chars().count()
            };

            chars_deleted = pos - first_trailing_whitespace_char;

            (
                first_trailing_whitespace_char,
                pos,
                local_offs as isize - chars_deleted as isize,
            )
        } else {
            new_text.push_str(line_ending);
            (line_start, line_start, new_text.chars().count() as isize)
        };

        let new_range = if range.cursor(text) > range.anchor {
            Range::new(
                (range.anchor as isize + global_offs) as usize,
                (range.head as isize + local_offs + global_offs) as usize,
            )
        } else {
            Range::new(
                (range.anchor as isize + local_offs + global_offs) as usize,
                (range.head as isize + local_offs + global_offs) as usize,
            )
        };

        ranges.push(new_range);
        global_offs += new_text.chars().count() as isize - chars_deleted as isize;
        let tendril = Tendril::from(&new_text);
        new_text.clear();

        (from, to, Some(tendril))
    });

    transaction = transaction.with_selection(Selection::new(ranges, selection.primary_index()));

    let doc = crate::doc_mut!(editor, &doc_id);
    doc.apply(&transaction, view_id);
}

// ─── Kill to line start / end ────────────────────────────────────────

/// Delete from cursor to line start (or first non-blank) in insert mode.
pub fn kill_to_line_start(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    delete_by_selection_insert_mode(
        editor,
        view_id,
        doc_id,
        move |text, range| {
            let line = range.cursor_line(text);
            let first_char = text.line_to_char(line);
            let anchor = range.cursor(text);
            let head = if anchor == first_char && line != 0 {
                line_end_char_index(&text, line - 1)
            } else if let Some(pos) = text.line(line).first_non_whitespace_char() {
                if first_char + pos < anchor {
                    first_char + pos
                } else {
                    first_char
                }
            } else {
                first_char
            };
            (head, anchor)
        },
        Direction::Backward,
    );
}

/// Delete from cursor to line end in insert mode.
pub fn kill_to_line_end(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    delete_by_selection_insert_mode(
        editor,
        view_id,
        doc_id,
        |text, range| {
            let line = range.cursor_line(text);
            let line_end_pos = line_end_char_index(&text, line);
            let pos = range.cursor(text);

            if pos == line_end_pos {
                (pos, text.line_to_char(line + 1))
            } else {
                (pos, line_end_pos)
            }
        },
        Direction::Forward,
    );
}

// ─── Text objects ────────────────────────────────────────────────────

/// Select word text object.
pub fn textobject_word_in(
    target: &impl Identified,
    doc: &mut (impl TextContent + Selectable),
    obj: textobject::TextObject,
    count: usize,
    longword: bool,
) {
    let text = doc.text().slice(..);
    let selection = doc
        .selection(target.id())
        .clone()
        .transform(|range| textobject::textobject_word(text, range, obj, count, longword));
    doc.set_selection(target.id(), selection);
}

/// Select word text object.
pub fn textobject_word(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    obj: textobject::TextObject,
    count: usize,
    longword: bool,
) {
    let doc = crate::doc_mut!(editor, &doc_id);
    textobject_word_in(&view_id, doc, obj, count, longword);
}

/// Select tree-sitter text object (function, class, parameter, comment, etc.).
pub fn textobject_treesitter_in(
    target: &impl Identified,
    doc: &mut (impl TextContent + Selectable + SyntaxAware),
    loader: &helix_core::syntax::Loader,
    obj_type: textobject::TextObject,
    object_name: &str,
    count: usize,
) -> bool {
    let Some((syntax, text)) = doc.syntax_text() else {
        return false;
    };
    let selection = doc.selection(target.id()).clone().transform(|range| {
        textobject::textobject_treesitter(text, range, obj_type, object_name, syntax, loader, count)
    });
    doc.set_selection(target.id(), selection);
    true
}

/// Select tree-sitter text object (function, class, parameter, comment, etc.).
pub fn textobject_treesitter(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    obj_type: textobject::TextObject,
    object_name: &str,
    count: usize,
) {
    let loader = editor.syn_loader.load();
    let doc = crate::doc_mut!(editor, &doc_id);
    if !textobject_treesitter_in(&view_id, doc, &loader, obj_type, object_name, count) {
        editor.set_status("Syntax information is not available in current buffer");
    }
}

/// Select paragraph text object.
pub fn textobject_paragraph_in(
    target: &impl Identified,
    doc: &mut (impl TextContent + Selectable),
    obj: textobject::TextObject,
    count: usize,
) {
    let text = doc.text().slice(..);
    let selection = doc
        .selection(target.id())
        .clone()
        .transform(|range| textobject::textobject_paragraph(text, range, obj, count));
    doc.set_selection(target.id(), selection);
}

/// Select paragraph text object.
pub fn textobject_paragraph(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    obj: textobject::TextObject,
    count: usize,
) {
    let doc = crate::doc_mut!(editor, &doc_id);
    textobject_paragraph_in(&view_id, doc, obj, count);
}

/// Select closest surrounding pair text object.
pub fn textobject_closest_surrounding_pair_in(
    target: &impl Identified,
    doc: &mut (impl TextContent + Selectable + SyntaxAware),
    obj: textobject::TextObject,
    count: usize,
) {
    let (text, syntax) = doc.text_syntax();
    let selection = doc.selection(target.id()).clone().transform(|range| {
        textobject::textobject_pair_surround_closest(syntax, text, range, obj, count)
    });
    doc.set_selection(target.id(), selection);
}

/// Select closest surrounding pair text object.
pub fn textobject_closest_surrounding_pair(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    obj: textobject::TextObject,
    count: usize,
) {
    let doc = crate::doc_mut!(editor, &doc_id);
    textobject_closest_surrounding_pair_in(&view_id, doc, obj, count);
}

/// Select surrounding pair text object with optional direction.
pub fn textobject_surrounding_pair_in(
    target: &impl Identified,
    doc: &mut (impl TextContent + Selectable + SyntaxAware),
    obj: textobject::TextObject,
    pair_char: char,
    direction: Option<Direction>,
    count: usize,
) {
    let (text, syntax) = doc.text_syntax();
    let selection = doc.selection(target.id()).clone().transform(|range| {
        let find_type = match direction {
            None => FindType::Surround,
            Some(Direction::Forward) => FindType::Next,
            Some(Direction::Backward) => FindType::Prev,
        };
        let mut range = textobject::textobject_pair_surround(
            syntax, text, range, obj, pair_char, find_type, count,
        );
        if let Some(direction) = direction {
            range = range.with_direction(direction);
        }
        range
    });
    doc.set_selection(target.id(), selection);
}

/// Select surrounding pair text object with optional direction.
pub fn textobject_surrounding_pair(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    obj: textobject::TextObject,
    pair_char: char,
    direction: Option<Direction>,
    count: usize,
) {
    if pair_char.is_ascii_alphanumeric() {
        return;
    }
    let doc = crate::doc_mut!(editor, &doc_id);
    textobject_surrounding_pair_in(&view_id, doc, obj, pair_char, direction, count);
}

// ─── Expand / Shrink selection ───────────────────────────────────────

/// Expand selection to parent syntax node.
pub fn expand_selection(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
        if let Some((syntax, text)) = doc.syntax_text() {
            let current_selection = doc.selection(view_id);
            let selection = object::expand_selection(syntax, text, current_selection.clone());

            if *current_selection != selection {
                view.object_selections_mut().push(current_selection.clone());
                doc.set_selection(view_id, selection);
            }
        }
    });
}

/// Shrink selection to previous or first child syntax node.
pub fn shrink_selection(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    // Check if we have a saved object selection to restore
    editor.with_view_doc_mut(view_id, doc_id, |view, doc| {
        let prev_selection = view.object_selections_mut().pop();
        let current_selection = doc.selection(view_id);
        if let Some(prev_selection) = prev_selection {
            if current_selection.contains(&prev_selection) {
                doc.set_selection(view_id, prev_selection);
                return;
            }
            view.object_selections_mut().clear();
        }

        if let Some((syntax, text)) = doc.syntax_text() {
            let current_selection = doc.selection(view_id).clone();
            let selection = object::shrink_selection(syntax, text, current_selection);
            doc.set_selection(view_id, selection);
        }
    });
}

// ─── Sibling selection ───────────────────────────────────────────────

/// Select next sibling syntax node.
pub fn select_next_sibling(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    syntax_select_impl(editor, view_id, doc_id, object::select_next_sibling);
}

/// Select previous sibling syntax node.
pub fn select_prev_sibling(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    syntax_select_impl(editor, view_id, doc_id, object::select_prev_sibling);
}

/// Select all sibling syntax nodes.
pub fn select_all_siblings(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    syntax_select_impl(editor, view_id, doc_id, object::select_all_siblings);
}

/// Select all children syntax nodes.
pub fn select_all_children(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    syntax_select_impl(editor, view_id, doc_id, object::select_all_children);
}

/// Helper: apply a syntax-based selection function.
pub fn syntax_select_in<F>(
    target: &impl Identified,
    doc: &mut (impl TextContent + Selectable + SyntaxAware),
    select_fn: F,
) -> bool
where
    F: Fn(&Syntax, RopeSlice, Selection) -> Selection,
{
    let Some((syntax, text)) = doc.syntax_text() else {
        return false;
    };
    let current_selection = doc.selection(target.id());
    let selection = select_fn(syntax, text, current_selection.clone());
    doc.set_selection(target.id(), selection);
    true
}

/// Helper: apply a syntax-based selection function.
pub fn syntax_select_impl<F>(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId, select_fn: F)
where
    F: Fn(&Syntax, RopeSlice, Selection) -> Selection,
{
    let doc = crate::doc_mut!(editor, &doc_id);
    let _ = syntax_select_in(&view_id, doc, select_fn);
}

// ─── Diff/change text object ─────────────────────────────────────────

/// Select the diff hunk (change) at cursor.
pub fn textobject_change(editor: &mut Editor, view_id: ViewId, doc_id: DocumentId) {
    let has_diff = {
        let doc = crate::doc!(editor, &doc_id);
        doc.diff_handle().is_some()
    };
    if !has_diff {
        editor.set_status("Diff is not available in current buffer");
        return;
    }
    let doc = crate::doc_mut!(editor, &doc_id);
    let diff_handle = doc.diff_handle().unwrap();
    let diff = diff_handle.load();
    let text = doc.text().slice(..);
    let selection = doc.selection(view_id).clone().transform(|range| {
        let line = range.cursor_line(text);
        let Some(hunk_idx) = diff.hunk_at(line as u32, false) else {
            return range;
        };
        let hunk = diff.nth_hunk(hunk_idx).after;
        let start = text.line_to_char(hunk.start as usize);
        let end = text.line_to_char(hunk.end as usize);
        Range::new(start, end).with_direction(range.direction())
    });
    drop(diff);
    doc.set_selection(view_id, selection);
}

// ─── Node boundary movement ─────────────────────────────────────────

/// Move cursor to parent node boundary.
pub fn move_node_bound_in(
    target: &impl Identified,
    doc: &mut (impl TextContent + Selectable + SyntaxAware),
    dir: Direction,
    movement: Movement,
) -> bool {
    let Some((syntax, text)) = doc.syntax_text() else {
        return false;
    };
    let current_selection = doc.selection(target.id());
    let selection = helix_core::movement::move_parent_node_end(
        syntax,
        text,
        current_selection.clone(),
        dir,
        movement,
    );
    doc.set_selection(target.id(), selection);
    true
}

/// Move cursor to parent node boundary.
pub fn move_node_bound(
    editor: &mut Editor,
    view_id: ViewId,
    doc_id: DocumentId,
    dir: Direction,
    movement: Movement,
) {
    let doc = crate::doc_mut!(editor, &doc_id);
    let _ = move_node_bound_in(&view_id, doc, dir, movement);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use helix_core::syntax;
    use helix_loader::runtime_dirs;

    use super::*;
    use crate::{
        editor::{Action, Config, Editor},
        graphics::Rect,
        handlers::Handlers,
        theme,
    };

    struct TestModal(Mode);

    impl Modal for TestModal {
        fn mode(&self) -> Mode {
            self.0
        }

        fn set_mode(&mut self, mode: Mode) {
            self.0 = mode;
        }
    }

    fn test_doc(text: &str) -> Document {
        Document::from(
            Rope::from(text),
            None,
            Arc::new(ArcSwap::new(Arc::new(Config::default()))),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        )
    }

    fn giant_line_fixture(line_count: usize, line_len: usize) -> String {
        let line = "x".repeat(line_len);
        let mut text = String::with_capacity(line_count * (line_len + 1));
        for idx in 0..line_count {
            text.push_str(&line);
            if idx + 1 != line_count {
                text.push('\n');
            }
        }
        text
    }

    fn test_editor_with_text(text: &str) -> (Editor, ViewId, DocumentId) {
        let theme_loader = theme::Loader::new(runtime_dirs());
        let syn_loader = helix_core::config::default_lang_loader();
        let config = Arc::new(ArcSwap::from_pointee(Config::default()));
        let mut editor = Editor::new(
            Rect::new(0, 0, 80, 24),
            Arc::new(theme_loader),
            Arc::new(ArcSwap::from_pointee(syn_loader)),
            Arc::new(arc_swap::access::Map::new(config, |cfg: &Config| cfg)),
            helix_runtime::test::runtime(),
            Handlers::dummy(),
        );
        let doc = Document::from(
            Rope::from(text),
            None,
            editor.config.clone(),
            editor.syn_loader.clone(),
        );
        let doc_id = editor.new_file_from_document(Action::VerticalSplit, doc);
        let view_id = editor.tree.focus;
        let end = editor
            .document(doc_id)
            .expect("document")
            .text()
            .len_chars();
        editor
            .document_mut(doc_id)
            .expect("document")
            .set_selection(view_id, Selection::single(0, end));
        (editor, view_id, doc_id)
    }

    #[test]
    fn select_all_in_uses_identified_target() {
        let view_id = ViewId::default();
        let mut doc = test_doc("hello\nworld");
        doc.set_selection(view_id, Selection::single(0, 0));

        select_all_in(&view_id, &mut doc);

        assert_eq!(
            doc.selection(view_id),
            &Selection::single(0, doc.text().len_chars())
        );
    }

    #[test]
    fn remove_primary_selection_in_errors_on_last_selection() {
        let view_id = ViewId::default();
        let mut doc = test_doc("hello");
        doc.set_selection(view_id, Selection::single(0, 0));
        let original = doc.selection(view_id).clone();

        let result = remove_primary_selection_in(&view_id, &mut doc);

        assert_eq!(result, Err(SelectionCommandError::NoSelectionsRemaining));
        assert_eq!(doc.selection(view_id), &original);
    }

    #[test]
    fn remove_primary_selection_in_drops_primary_range() {
        let view_id = ViewId::default();
        let mut doc = test_doc("hello");
        doc.set_selection(
            view_id,
            Selection::new(
                SmallVec::from_vec(vec![Range::new(0, 0), Range::new(2, 2)]),
                0,
            ),
        );

        let result = remove_primary_selection_in(&view_id, &mut doc);

        assert_eq!(result, Ok(()));
        let selection = doc.selection(view_id);
        assert_eq!(selection.len(), 1);
        assert_eq!(selection.primary().cursor(doc.text().slice(..)), 2);
    }

    #[test]
    fn reorder_selection_contents_in_rotates_text_and_primary_index() {
        let view_id = ViewId::default();
        let mut doc = test_doc("abcd");
        doc.set_selection(
            view_id,
            Selection::new(
                SmallVec::from_vec(vec![Range::new(0, 1), Range::new(2, 3)]),
                0,
            ),
        );

        reorder_selection_contents_in(&view_id, &mut doc, ReorderStrategy::RotateForward, 1);

        assert_eq!(doc.text().to_string(), "cbad");
        assert_eq!(doc.selection(view_id).primary_index(), 1);
    }

    #[test]
    fn rotate_selections_first_in_sets_primary_to_first_range() {
        let view_id = ViewId::default();
        let mut doc = test_doc("abcd");
        doc.set_selection(
            view_id,
            Selection::new(
                SmallVec::from_vec(vec![Range::new(0, 0), Range::new(2, 2), Range::new(3, 3)]),
                2,
            ),
        );

        rotate_selections_first_in(&view_id, &mut doc);

        assert_eq!(doc.selection(view_id).primary_index(), 0);
    }

    #[test]
    fn rotate_selections_last_in_sets_primary_to_last_range() {
        let view_id = ViewId::default();
        let mut doc = test_doc("abcd");
        doc.set_selection(
            view_id,
            Selection::new(
                SmallVec::from_vec(vec![Range::new(0, 0), Range::new(2, 2), Range::new(3, 3)]),
                0,
            ),
        );

        rotate_selections_last_in(&view_id, &mut doc);

        assert_eq!(doc.selection(view_id).primary_index(), 2);
    }

    #[test]
    fn increment_in_updates_numbers_and_preserves_primary_index() {
        let view_id = ViewId::default();
        let mut doc = test_doc("1 9");
        doc.set_selection(
            view_id,
            Selection::new(
                SmallVec::from_vec(vec![Range::new(0, 0), Range::new(2, 2)]),
                1,
            ),
        );

        let changed = increment_in(&view_id, &mut doc, 2, 1);

        assert!(changed);
        assert_eq!(doc.text().to_string(), "3 12");
        assert_eq!(doc.selection(view_id).primary_index(), 1);
    }

    #[test]
    fn increment_in_returns_false_when_no_selection_matches() {
        let view_id = ViewId::default();
        let mut doc = test_doc("aa bb");
        doc.set_selection(view_id, Selection::single(0, 1));
        let original = doc.text().to_string();

        let changed = increment_in(&view_id, &mut doc, 1, 0);

        assert!(!changed);
        assert_eq!(doc.text().to_string(), original);
    }

    #[test]
    fn select_mode_in_updates_modal_and_expands_eof_cursor() {
        let view_id = ViewId::default();
        let mut doc = test_doc("ab");
        let eof = doc.text().len_chars();
        doc.set_selection(view_id, Selection::point(eof));
        let mut modal = TestModal(Mode::Normal);

        select_mode_in(&view_id, &mut doc, &mut modal);

        assert_eq!(modal.mode(), Mode::Select);
        assert!(!doc.selection(view_id).primary().is_empty());
    }

    #[test]
    fn match_brackets_in_uses_plaintext_when_syntax_is_unavailable() {
        let view_id = ViewId::default();
        let mut doc = test_doc("(ab)");
        doc.set_selection(view_id, Selection::point(0));

        match_brackets_in(&view_id, &mut doc, Movement::Move);

        assert_eq!(
            doc.selection(view_id)
                .primary()
                .cursor(doc.text().slice(..)),
            3
        );
    }

    #[test]
    fn textobject_word_in_selects_current_word() {
        let view_id = ViewId::default();
        let mut doc = test_doc("alpha beta");
        doc.set_selection(view_id, Selection::point(1));

        textobject_word_in(&view_id, &mut doc, textobject::TextObject::Inside, 1, false);

        let range = doc.selection(view_id).primary();
        assert_eq!(range.from(), 0);
        assert_eq!(range.to(), 5);
    }

    #[test]
    fn copy_selection_on_line_in_duplicates_selection_below() {
        let view_id = ViewId::default();
        let mut doc = test_doc("abc\ndef");
        doc.set_selection(view_id, Selection::single(0, 1));

        copy_selection_on_line_in(&view_id, &mut doc, 1, Direction::Forward);

        let selection = doc.selection(view_id);
        assert_eq!(selection.len(), 2);
        assert_eq!(selection.ranges()[1], Range::new(4, 5));
    }

    #[test]
    fn join_selections_collapses_giant_multiline_fixture() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = runtime.enter();

        let text = giant_line_fixture(256, 256);
        let (mut editor, view_id, doc_id) = test_editor_with_text(&text);
        let before_lines = editor
            .document(doc_id)
            .expect("document")
            .text()
            .len_lines();

        join_selections_impl(&mut editor, view_id, doc_id, false);

        let doc = editor.document(doc_id).expect("document");
        let after = doc.text().to_string();
        let newline_count = after.chars().filter(|&ch| ch == '\n').count();

        assert!(before_lines > 200);
        assert!(doc.text().len_lines() <= 2);
        assert!(newline_count <= 1);
        assert!(doc.text().len_bytes() > 32 * 1024);
    }

    #[test]
    #[ignore = "targeted local repro for post-xJ giant-line state"]
    fn join_selections_giant_line_repro() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = runtime.enter();

        let text = giant_line_fixture(6_000, 300);
        let (mut editor, view_id, doc_id) = test_editor_with_text(&text);
        let start = std::time::Instant::now();

        join_selections_impl(&mut editor, view_id, doc_id, false);

        let elapsed = start.elapsed();
        let doc = editor.document(doc_id).expect("document");
        eprintln!(
            "join_selections_giant_line_repro: elapsed_us={} lines={} bytes={}",
            elapsed.as_micros(),
            doc.text().len_lines(),
            doc.text().len_bytes()
        );
        assert!(doc.text().len_lines() <= 2);
    }

    #[test]
    fn indent_in_inserts_indent_on_selected_lines() {
        let view_id = ViewId::default();
        let mut doc = test_doc("one\ntwo");
        doc.set_selection(view_id, Selection::single(0, doc.text().len_chars()));

        indent_in(&view_id, &mut doc, 1);

        assert_eq!(doc.text().to_string(), "\tone\n\ttwo");
    }

    #[test]
    fn switch_case_impl_in_rewrites_selected_text() {
        let view_id = ViewId::default();
        let mut doc = test_doc("Ab");
        doc.set_selection(view_id, Selection::single(0, 2));

        switch_case_impl_in(&view_id, &mut doc, |string| {
            string.chunks().map(|chunk| chunk.to_lowercase()).collect()
        });

        assert_eq!(doc.text().to_string(), "ab");
    }

    #[test]
    fn align_selections_in_pads_to_rightmost_column() {
        let view_id = ViewId::default();
        let mut doc = test_doc("a = 1\nbb= 2");
        doc.set_selection(
            view_id,
            Selection::new(
                SmallVec::from_vec(vec![Range::new(1, 2), Range::new(8, 9)]),
                0,
            ),
        );

        let result = align_selections_in(&view_id, &mut doc);

        assert_eq!(result, Ok(()));
        assert_eq!(doc.text().to_string(), "a  = 1\nbb= 2");
    }
}
