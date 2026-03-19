use std::{
    borrow::Cow,
    cmp::Ordering,
    ops::{Add, AddAssign, Sub, SubAssign},
};

use helix_stdx::rope::RopeSliceExt;
use memchr::memchr;

use crate::{
    chars::char_is_line_ending,
    doc_formatter::{DocumentFormatter, TextFormat},
    graphemes::{ensure_grapheme_boundary_prev, grapheme_width},
    line_ending::line_end_char_index,
    text_annotations::{PlainLineSeekSupport, TextAnnotations},
    RopeSlice,
};

/// Represents a single point in a text buffer. Zero indexed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position {
    pub row: usize,
    pub col: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisualBlockOffsetSeekKind {
    PlainFastPath,
    FormatterFallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisualBlockOffsetResult {
    pub char_idx: usize,
    pub visual_pos: Position,
    pub virtual_lines: usize,
    pub kind: VisualBlockOffsetSeekKind,
    pub plain_seek_support: PlainLineSeekSupport,
}

impl AddAssign for Position {
    fn add_assign(&mut self, rhs: Self) {
        self.row += rhs.row;
        self.col += rhs.col;
    }
}

impl SubAssign for Position {
    fn sub_assign(&mut self, rhs: Self) {
        self.row -= rhs.row;
        self.col -= rhs.col;
    }
}

impl Sub for Position {
    type Output = Position;

    fn sub(mut self, rhs: Self) -> Self::Output {
        self -= rhs;
        self
    }
}

impl Add for Position {
    type Output = Position;

    fn add(mut self, rhs: Self) -> Self::Output {
        self += rhs;
        self
    }
}

impl Position {
    pub const fn new(row: usize, col: usize) -> Self {
        Self { row, col }
    }

    pub const fn is_zero(self) -> bool {
        self.row == 0 && self.col == 0
    }

    // TODO: generalize
    pub fn traverse(self, text: &crate::Tendril) -> Self {
        let Self { mut row, mut col } = self;
        // TODO: there should be a better way here
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if char_is_line_ending(ch) && !(ch == '\r' && chars.peek() == Some(&'\n')) {
                row += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        Self { row, col }
    }
}

impl From<(usize, usize)> for Position {
    fn from(tuple: (usize, usize)) -> Self {
        Self {
            row: tuple.0,
            col: tuple.1,
        }
    }
}

/// Convert a character index to (line, column) coordinates.
///
/// column in `char` count which can be used for row:column display in
/// status line. See [`visual_coords_at_pos`] for a visual one.
pub fn coords_at_pos(text: RopeSlice, pos: usize) -> Position {
    let line = text.char_to_line(pos);

    let line_start = text.line_to_char(line);
    let pos = ensure_grapheme_boundary_prev(text, pos);
    let line_slice = text.slice(line_start..pos);
    let col = if line_slice.chunks().all(|chunk| chunk.is_ascii()) {
        pos - line_start
    } else {
        line_slice.graphemes().count()
    };

    Position::new(line, col)
}

/// Convert a character index to (line, column) coordinates visually.
///
/// Takes \t, double-width characters (CJK) into account as well as text
/// not in the document in the future.
/// See [`coords_at_pos`] for an "objective" one.
///
/// This function should be used very rarely. Usually `visual_offset_from_anchor`
/// or `visual_offset_from_block` is preferable. However when you want to compute the
/// actual visual row/column in the text (not what is actually shown on screen)
/// then you should use this function. For example aligning text should ignore virtual
/// text and softwrap.
#[deprecated = "Doesn't account for softwrap or decorations, use visual_offset_from_anchor instead"]
pub fn visual_coords_at_pos(text: RopeSlice, pos: usize, tab_width: usize) -> Position {
    let line = text.char_to_line(pos);

    let line_start = text.line_to_char(line);
    let pos = ensure_grapheme_boundary_prev(text, pos);

    let mut col = 0;

    for grapheme in text.slice(line_start..pos).graphemes() {
        if grapheme == "\t" {
            col += tab_width - (col % tab_width);
        } else {
            let grapheme = Cow::from(grapheme);
            col += grapheme_width(&grapheme);
        }
    }

    Position::new(line, col)
}

/// Returns the visual offset from the start of the first visual line
/// in the block that contains anchor.
/// Text is always wrapped at blocks, they usually correspond to
/// actual line breaks but for very long lines
/// softwrapping positions are estimated with an O(1) algorithm
/// to ensure consistent performance for large lines (currently unimplemented)
///
/// Usually you want to use `visual_offset_from_anchor` instead but this function
/// can be useful (and faster) if
/// * You already know the visual position of the block
/// * You only care about the horizontal offset (column) and not the vertical offset (row)
pub fn visual_offset_from_block(
    text: RopeSlice,
    anchor: usize,
    pos: usize,
    text_fmt: &TextFormat,
    annotations: &TextAnnotations,
) -> (Position, usize) {
    visual_offset_from_block_with_metrics(text, anchor, pos, text_fmt, annotations).result
}

#[derive(Debug, Clone, Copy, Default)]
pub struct VisualOffsetFromBlockMetrics {
    pub next_calls: usize,
    pub formatter_next_calls: usize,
    pub formatter_advance_grapheme_calls: usize,
    pub formatter_word_refills: usize,
    pub formatter_fold_skip_count: usize,
    pub formatter_inline_annotation_hits: usize,
    pub formatter_overlay_hits: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct VisualOffsetFromBlockResult {
    pub result: (Position, usize),
    pub metrics: VisualOffsetFromBlockMetrics,
}

pub fn visual_offset_from_block_with_metrics(
    text: RopeSlice,
    anchor: usize,
    pos: usize,
    text_fmt: &TextFormat,
    annotations: &TextAnnotations,
) -> VisualOffsetFromBlockResult {
    let mut last_pos = Position::default();
    let mut formatter =
        DocumentFormatter::new_at_prev_checkpoint(text, text_fmt, annotations, anchor);
    let block_start = formatter.next_char_pos();
    let mut next_calls = 0usize;

    while let Some(grapheme) = formatter.next() {
        next_calls += 1;
        last_pos = grapheme.visual_pos;
        if formatter.next_char_pos() > pos {
            let stats = formatter.stats();
            return VisualOffsetFromBlockResult {
                result: (grapheme.visual_pos, block_start),
                metrics: VisualOffsetFromBlockMetrics {
                    next_calls,
                    formatter_next_calls: stats.next_calls,
                    formatter_advance_grapheme_calls: stats.advance_grapheme_calls,
                    formatter_word_refills: stats.word_refills,
                    formatter_fold_skip_count: stats.fold_skip_count,
                    formatter_inline_annotation_hits: stats.inline_annotation_hits,
                    formatter_overlay_hits: stats.overlay_hits,
                },
            };
        }
    }

    let stats = formatter.stats();
    VisualOffsetFromBlockResult {
        result: (last_pos, block_start),
        metrics: VisualOffsetFromBlockMetrics {
            next_calls,
            formatter_next_calls: stats.next_calls,
            formatter_advance_grapheme_calls: stats.advance_grapheme_calls,
            formatter_word_refills: stats.word_refills,
            formatter_fold_skip_count: stats.fold_skip_count,
            formatter_inline_annotation_hits: stats.inline_annotation_hits,
            formatter_overlay_hits: stats.overlay_hits,
        },
    }
}

/// Returns the height of the given text when softwrapping
pub fn softwrapped_dimensions(text: RopeSlice, text_fmt: &TextFormat) -> (usize, u16) {
    let last_pos =
        visual_offset_from_block(text, 0, usize::MAX, text_fmt, &TextAnnotations::default()).0;
    if last_pos.row == 0 {
        (1, last_pos.col as u16)
    } else {
        (last_pos.row + 1, text_fmt.viewport_width)
    }
}

fn plain_char_idx_and_visual_col_at_visual_column(
    text: RopeSlice,
    anchor: usize,
    column: usize,
    tab_width: usize,
) -> (usize, usize) {
    let line = text.char_to_line(anchor.min(text.len_chars()));
    let line_start = text.line_to_char(line);
    let line_end = line_end_char_index(&text, line);
    let line_slice = text.slice(line_start..line_end);

    let mut char_idx = line_start;
    let mut visual_col = 0usize;
    let mut slice_byte_offset = 0usize;

    for chunk in line_slice.chunks() {
        let bytes = chunk.as_bytes();
        if bytes.is_ascii() {
            let mut chunk_offset = 0usize;

            while chunk_offset < bytes.len() {
                let remaining = column.saturating_sub(visual_col);
                if remaining == 0 {
                    return (char_idx, visual_col);
                }

                let haystack = &bytes[chunk_offset..];
                let next_tab = memchr(b'\t', haystack).unwrap_or(haystack.len());
                let ascii_run = next_tab.min(remaining);

                if ascii_run != 0 {
                    char_idx += ascii_run;
                    visual_col += ascii_run;
                    chunk_offset += ascii_run;
                    slice_byte_offset += ascii_run;
                }

                if visual_col == column {
                    return (char_idx, visual_col);
                }

                if chunk_offset == bytes.len() {
                    break;
                }

                debug_assert_eq!(bytes[chunk_offset], b'\t');
                let width = tab_width - (visual_col % tab_width);
                if visual_col + width > column {
                    return (char_idx, visual_col);
                }
                char_idx += 1;
                visual_col += width;
                chunk_offset += 1;
                slice_byte_offset += 1;
            }

            continue;
        }

        let mut i = 0usize;

        while i < bytes.len() {
            let ascii_start = i;
            while i < bytes.len() && bytes[i].is_ascii() && bytes[i] != b'\t' {
                i += 1;
            }

            // Check if the byte after the ASCII run is a combining character
            // (leading byte 0xCC or 0xCD for U+0300..U+036F). If so, the last
            // ASCII byte is the base of a multi-char grapheme — exclude it from
            // the fast-path count so the grapheme iterator handles the full cluster.
            let next_is_combining =
                i < bytes.len() && i > ascii_start && (bytes[i] == 0xCC || bytes[i] == 0xCD);
            let ascii_len = if next_is_combining {
                i - ascii_start - 1
            } else {
                i - ascii_start
            };

            if ascii_len != 0 {
                if visual_col + ascii_len > column {
                    let delta = column - visual_col;
                    return (char_idx + delta, visual_col + delta);
                }
                char_idx += ascii_len;
                visual_col += ascii_len;
                slice_byte_offset += ascii_len;
                if visual_col == column {
                    return (char_idx, visual_col);
                }
            }

            if !next_is_combining && i == bytes.len() {
                break;
            }

            if !next_is_combining && bytes[i] == b'\t' {
                let width = tab_width - (visual_col % tab_width);
                if visual_col + width > column {
                    return (char_idx, visual_col);
                }
                char_idx += 1;
                visual_col += width;
                i += 1;
                slice_byte_offset += 1;
                if visual_col == column {
                    return (char_idx, visual_col);
                }
                continue;
            }

            for (byte_idx, grapheme) in line_slice.grapheme_indices_at(slice_byte_offset) {
                let grapheme_char_idx = line_start + line_slice.byte_to_char(byte_idx);
                let width = grapheme
                    .as_str()
                    .map_or_else(|| grapheme_width(&grapheme.to_string()), grapheme_width);
                if visual_col + width > column {
                    return (grapheme_char_idx, visual_col);
                }
                visual_col += width;
                char_idx = grapheme_char_idx + grapheme.chars().count();
                if visual_col == column {
                    return (char_idx, visual_col);
                }
            }

            return (line_end, visual_col);
        }
    }

    (line_end, visual_col)
}

pub fn plain_visual_col_at_char_idx(text: RopeSlice, pos: usize, tab_width: usize) -> usize {
    let line = text.char_to_line(pos.min(text.len_chars()));
    let line_start = text.line_to_char(line);
    let target = pos.min(line_end_char_index(&text, line));
    let line_slice = text.slice(line_start..target);

    let mut visual_col = 0usize;
    let mut slice_byte_offset = 0usize;

    for chunk in line_slice.chunks() {
        let bytes = chunk.as_bytes();
        if bytes.is_ascii() {
            let mut chunk_offset = 0usize;
            while chunk_offset < bytes.len() {
                let haystack = &bytes[chunk_offset..];
                let next_tab = memchr(b'\t', haystack).unwrap_or(haystack.len());

                if next_tab != 0 {
                    visual_col += next_tab;
                    chunk_offset += next_tab;
                    slice_byte_offset += next_tab;
                }

                if chunk_offset == bytes.len() {
                    break;
                }

                debug_assert_eq!(bytes[chunk_offset], b'\t');
                let width = tab_width - (visual_col % tab_width);
                visual_col += width;
                chunk_offset += 1;
                slice_byte_offset += 1;
            }

            continue;
        }

        let mut i = 0usize;
        while i < bytes.len() {
            let ascii_start = i;
            while i < bytes.len() && bytes[i].is_ascii() && bytes[i] != b'\t' {
                i += 1;
            }

            // Exclude last ASCII byte if followed by a combining character
            // (0xCC/0xCD lead byte for U+0300..U+036F) so the grapheme
            // iterator can see the full cluster.
            let next_is_combining =
                i < bytes.len() && i > ascii_start && (bytes[i] == 0xCC || bytes[i] == 0xCD);
            let ascii_len = if next_is_combining {
                i - ascii_start - 1
            } else {
                i - ascii_start
            };
            if next_is_combining {
                i -= 1;
            }

            if ascii_len != 0 {
                visual_col += ascii_len;
                slice_byte_offset += ascii_len;
            }

            if i == bytes.len() {
                break;
            }

            if bytes[i] == b'\t' {
                let width = tab_width - (visual_col % tab_width);
                visual_col += width;
                i += 1;
                slice_byte_offset += 1;
                continue;
            }

            let chunk_end = slice_byte_offset + bytes.len() - i;
            for (byte_idx, grapheme) in line_slice.grapheme_indices_at(slice_byte_offset) {
                if byte_idx >= chunk_end {
                    break;
                }
                let width = grapheme
                    .as_str()
                    .map_or_else(|| grapheme_width(&grapheme.to_string()), grapheme_width);
                visual_col += width;
                let next_byte = byte_idx + grapheme.len_bytes();
                i += next_byte - slice_byte_offset;
                slice_byte_offset = next_byte;
            }
        }
    }

    visual_col
}

pub fn char_idx_and_visual_offset_at_visual_block_offset_with_kind(
    text: RopeSlice,
    anchor: usize,
    row: usize,
    column: usize,
    text_fmt: &TextFormat,
    annotations: &TextAnnotations,
) -> VisualBlockOffsetResult {
    let line = text.char_to_line(anchor.min(text.len_chars()));
    let line_start = text.line_to_char(line);
    let line_end = line_end_char_index(&text, line);
    let mut plain_seek_support = PlainLineSeekSupport::Supported;

    if !text_fmt.soft_wrap && row == 0 {
        let (char_idx, visual_col) = plain_char_idx_and_visual_col_at_visual_column(
            text,
            anchor,
            column,
            text_fmt.tab_width as usize,
        );
        plain_seek_support = annotations.plain_line_seek_support(line_start, line_end, char_idx);
        if matches!(plain_seek_support, PlainLineSeekSupport::Supported) {
            return VisualBlockOffsetResult {
                char_idx,
                visual_pos: Position::new(0, visual_col),
                virtual_lines: 0,
                kind: VisualBlockOffsetSeekKind::PlainFastPath,
                plain_seek_support,
            };
        }
    }

    let mut formatter =
        DocumentFormatter::new_at_prev_checkpoint(text, text_fmt, annotations, anchor);
    let mut last_char_idx = formatter.next_char_pos();
    let mut last_pos = Position::default();
    let mut found_non_virtual_on_row = false;
    let mut last_row = 0;
    for grapheme in &mut formatter {
        match grapheme.visual_pos.row.cmp(&row) {
            Ordering::Equal => {
                if grapheme.visual_pos.col + grapheme.width() > column {
                    if !grapheme.is_virtual() {
                        return VisualBlockOffsetResult {
                            char_idx: grapheme.char_idx,
                            visual_pos: grapheme.visual_pos,
                            virtual_lines: 0,
                            kind: VisualBlockOffsetSeekKind::FormatterFallback,
                            plain_seek_support,
                        };
                    } else if found_non_virtual_on_row {
                        return VisualBlockOffsetResult {
                            char_idx: last_char_idx,
                            visual_pos: last_pos,
                            virtual_lines: 0,
                            kind: VisualBlockOffsetSeekKind::FormatterFallback,
                            plain_seek_support,
                        };
                    }
                } else if !grapheme.is_virtual() {
                    found_non_virtual_on_row = true;
                    last_char_idx = grapheme.char_idx;
                    last_pos = grapheme.visual_pos;
                }
            }
            Ordering::Greater if found_non_virtual_on_row => {
                return VisualBlockOffsetResult {
                    char_idx: last_char_idx,
                    visual_pos: last_pos,
                    virtual_lines: 0,
                    kind: VisualBlockOffsetSeekKind::FormatterFallback,
                    plain_seek_support,
                };
            }
            Ordering::Greater => {
                return VisualBlockOffsetResult {
                    char_idx: last_char_idx,
                    visual_pos: last_pos,
                    virtual_lines: row - last_row,
                    kind: VisualBlockOffsetSeekKind::FormatterFallback,
                    plain_seek_support,
                };
            }
            Ordering::Less => {
                if !grapheme.is_virtual() {
                    last_row = grapheme.visual_pos.row;
                    last_char_idx = grapheme.char_idx;
                    last_pos = grapheme.visual_pos;
                }
            }
        }
    }

    VisualBlockOffsetResult {
        char_idx: formatter.next_char_pos(),
        visual_pos: formatter.next_visual_pos(),
        virtual_lines: 0,
        kind: VisualBlockOffsetSeekKind::FormatterFallback,
        plain_seek_support,
    }
}

pub fn char_idx_and_visual_offset_at_visual_block_offset(
    text: RopeSlice,
    anchor: usize,
    row: usize,
    column: usize,
    text_fmt: &TextFormat,
    annotations: &TextAnnotations,
) -> (usize, Position, usize) {
    let result = char_idx_and_visual_offset_at_visual_block_offset_with_kind(
        text,
        anchor,
        row,
        column,
        text_fmt,
        annotations,
    );
    (result.char_idx, result.visual_pos, result.virtual_lines)
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum VisualOffsetError {
    PosBeforeAnchorRow,
    PosAfterMaxRow,
}

/// Returns the visual offset from the start of the visual line
/// that contains anchor.
pub fn visual_offset_from_anchor(
    text: RopeSlice,
    anchor: usize,
    pos: usize,
    text_fmt: &TextFormat,
    annotations: &TextAnnotations,
    max_rows: usize,
) -> Result<(Position, usize), VisualOffsetError> {
    let mut formatter =
        DocumentFormatter::new_at_prev_checkpoint(text, text_fmt, annotations, anchor);
    let mut anchor_line = None;
    let mut found_pos = None;
    let mut last_pos = Position::default();

    let block_start = formatter.next_char_pos();
    if pos < block_start {
        return Err(VisualOffsetError::PosBeforeAnchorRow);
    }

    while let Some(grapheme) = formatter.next() {
        last_pos = grapheme.visual_pos;

        if formatter.next_char_pos() > pos {
            if let Some(anchor_line) = anchor_line {
                last_pos.row -= anchor_line;
                return Ok((last_pos, block_start));
            } else {
                found_pos = Some(last_pos);
            }
        }
        if formatter.next_char_pos() > anchor && anchor_line.is_none() {
            if let Some(mut found_pos) = found_pos {
                return if found_pos.row == last_pos.row {
                    found_pos.row = 0;
                    Ok((found_pos, block_start))
                } else {
                    Err(VisualOffsetError::PosBeforeAnchorRow)
                };
            } else {
                anchor_line = Some(last_pos.row);
            }
        }

        if let Some(anchor_line) = anchor_line {
            if grapheme.visual_pos.row >= anchor_line + max_rows {
                return Err(VisualOffsetError::PosAfterMaxRow);
            }
        }
    }

    let anchor_line = anchor_line.unwrap_or(last_pos.row);
    last_pos.row -= anchor_line;

    Ok((last_pos, block_start))
}

/// Convert (line, column) coordinates to a character index.
///
/// If the `line` coordinate is beyond the end of the file, the EOF
/// position will be returned.
///
/// If the `column` coordinate is past the end of the given line, the
/// line-end position will be returned.  What constitutes the "line-end
/// position" depends on the parameter `limit_before_line_ending`.  If it's
/// `true`, the line-end position will be just *before* the line ending
/// character.  If `false` it will be just *after* the line ending
/// character--on the border between the current line and the next.
///
/// Usually you only want `limit_before_line_ending` to be `true` if you're working
/// with left-side block-cursor positions, as this prevents the the block cursor
/// from jumping to the next line.  Otherwise you typically want it to be `false`,
/// such as when dealing with raw anchor/head positions.
pub fn pos_at_coords(text: RopeSlice, coords: Position, limit_before_line_ending: bool) -> usize {
    let Position { mut row, col } = coords;
    if limit_before_line_ending {
        let lines = text.len_lines() - 1;

        row = row.min(if crate::line_ending::get_line_ending(&text).is_some() {
            // if the last line is empty, don't jump to it
            lines - 1
        } else {
            lines
        });
    };
    let line_start = text.line_to_char(row);
    let line_end = if limit_before_line_ending {
        line_end_char_index(&text, row)
    } else {
        text.line_to_char((row + 1).min(text.len_lines()))
    };

    let mut col_char_offset = 0;
    for (i, g) in text.slice(line_start..line_end).graphemes().enumerate() {
        if i == col {
            break;
        }
        col_char_offset += g.chars().count();
    }

    line_start + col_char_offset
}

/// Convert visual (line, column) coordinates to a character index.
///
/// If the `line` coordinate is beyond the end of the file, the EOF
/// position will be returned.
///
/// If the `column` coordinate is past the end of the given line, the
/// line-end position (in this case, just before the line ending
/// character) will be returned.
/// This function should be used very rarely. Usually `char_idx_at_visual_offset` is preferable.
/// However when you want to compute a char position from the visual row/column in the text
/// (not what is actually shown on screen) then you should use this function.
/// For example aligning text should ignore virtual text and softwrap.
#[deprecated = "Doesn't account for softwrap or decorations, use char_idx_at_visual_offset instead"]
pub fn pos_at_visual_coords(text: RopeSlice, coords: Position, tab_width: usize) -> usize {
    let Position { mut row, col } = coords;
    row = row.min(text.len_lines() - 1);
    let line_start = text.line_to_char(row);
    let line_end = line_end_char_index(&text, row);

    let mut col_char_offset = 0;
    let mut cols_remaining = col;
    for grapheme in text.slice(line_start..line_end).graphemes() {
        let grapheme_width = if grapheme == "\t" {
            tab_width - ((col - cols_remaining) % tab_width)
        } else {
            let grapheme = Cow::from(grapheme);
            grapheme_width(&grapheme)
        };

        // If pos is in the middle of a wider grapheme (tab for example)
        // return the starting offset.
        if grapheme_width > cols_remaining {
            break;
        }

        cols_remaining -= grapheme_width;
        col_char_offset += grapheme.chars().count();
    }

    line_start + col_char_offset
}

/// Returns the char index on the visual line `row_offset` below the visual line of
/// the provided char index `anchor` that is closest to the supplied visual `column`.
///
/// If the targeted visual line is entirely covered by virtual text the last
/// char position before the virtual text and a virtual offset is returned instead.
///
/// If no (text) grapheme starts at exactly at the specified column the
/// start of the grapheme to the left is returned. If there is no grapheme
/// to the left (for example if the line starts with virtual text) then the positioning
/// of the next grapheme to the right is returned.
///
/// If the `line` coordinate is beyond the end of the file, the EOF
/// position will be returned.
///
/// If the `column` coordinate is past the end of the given line, the
/// line-end position (in this case, just before the line ending
/// character) will be returned.
///
/// # Returns
///
/// `(real_char_idx, virtual_lines)`
///
/// The nearest character idx "closest" (see above) to the specified visual offset
/// on the visual line is returned if the visual line contains any text:
/// If the visual line at the specified offset is a virtual line generated by a `LineAnnotation`
/// the previous char_index is returned, together with the remaining vertical offset (`virtual_lines`)
pub fn char_idx_at_visual_offset(
    text: RopeSlice,
    mut anchor: usize,
    mut row_offset: isize,
    column: usize,
    text_fmt: &TextFormat,
    annotations: &TextAnnotations,
) -> (usize, usize) {
    let mut pos = anchor;
    // convert row relative to visual line containing anchor to row relative to a block containing anchor (anchor may change)
    loop {
        let (visual_pos_in_block, block_char_offset) =
            visual_offset_from_block(text, anchor, pos, text_fmt, annotations);
        row_offset += visual_pos_in_block.row as isize;
        anchor = block_char_offset;
        if row_offset >= 0 {
            break;
        }

        if block_char_offset == 0 {
            row_offset = 0;
            break;
        }
        // the row_offset is negative so we need to look at the previous block
        // set the anchor to the last char before the current block so that we can compute
        // the distance of this block from the start of the previous block
        pos = anchor;
        anchor -= 1;
    }

    char_idx_at_visual_block_offset(
        text,
        anchor,
        row_offset as usize,
        column,
        text_fmt,
        annotations,
    )
}

/// This function behaves the same as `char_idx_at_visual_offset`, except that
/// the vertical offset `row` is always computed relative to the block that contains `anchor`
/// instead of the visual line that contains `anchor`.
/// Usually `char_idx_at_visual_offset` is more useful but this function can be
/// used in some situations as an optimization when `visual_offset_from_block` was used
///
/// # Returns
///
/// `(real_char_idx, virtual_lines)`
///
/// See `char_idx_at_visual_offset` for details
pub fn char_idx_at_visual_block_offset(
    text: RopeSlice,
    anchor: usize,
    row: usize,
    column: usize,
    text_fmt: &TextFormat,
    annotations: &TextAnnotations,
) -> (usize, usize) {
    let (char_idx, _, virtual_lines) = char_idx_and_visual_offset_at_visual_block_offset(
        text,
        anchor,
        row,
        column,
        text_fmt,
        annotations,
    );
    (char_idx, virtual_lines)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::text_annotations::InlineAnnotation;
    use crate::Rope;

    #[test]
    fn test_ordering() {
        // (0, 5) is less than (1, 0)
        assert!(Position::new(0, 5) < Position::new(1, 0));
    }

    #[test]
    fn test_coords_at_pos() {
        let text = Rope::from("ḧëḷḷö\nẅöṛḷḋ");
        let slice = text.slice(..);
        assert_eq!(coords_at_pos(slice, 0), (0, 0).into());
        assert_eq!(coords_at_pos(slice, 5), (0, 5).into()); // position on \n
        assert_eq!(coords_at_pos(slice, 6), (1, 0).into()); // position on w
        assert_eq!(coords_at_pos(slice, 7), (1, 1).into()); // position on o
        assert_eq!(coords_at_pos(slice, 10), (1, 4).into()); // position on d

        // Test with wide characters.
        let text = Rope::from("今日はいい\n");
        let slice = text.slice(..);
        assert_eq!(coords_at_pos(slice, 0), (0, 0).into());
        assert_eq!(coords_at_pos(slice, 1), (0, 1).into());
        assert_eq!(coords_at_pos(slice, 2), (0, 2).into());
        assert_eq!(coords_at_pos(slice, 3), (0, 3).into());
        assert_eq!(coords_at_pos(slice, 4), (0, 4).into());
        assert_eq!(coords_at_pos(slice, 5), (0, 5).into());
        assert_eq!(coords_at_pos(slice, 6), (1, 0).into());

        // Test with grapheme clusters.
        let text = Rope::from("a̐éö̲\r\n");
        let slice = text.slice(..);
        assert_eq!(coords_at_pos(slice, 0), (0, 0).into());
        assert_eq!(coords_at_pos(slice, 2), (0, 1).into());
        assert_eq!(coords_at_pos(slice, 4), (0, 2).into());
        assert_eq!(coords_at_pos(slice, 7), (0, 3).into());
        assert_eq!(coords_at_pos(slice, 9), (1, 0).into());

        // Test with wide-character grapheme clusters.
        let text = Rope::from("किमपि\n");
        let slice = text.slice(..);
        assert_eq!(coords_at_pos(slice, 0), (0, 0).into());
        assert_eq!(coords_at_pos(slice, 2), (0, 1).into());
        assert_eq!(coords_at_pos(slice, 3), (0, 2).into());
        assert_eq!(coords_at_pos(slice, 5), (0, 3).into());
        assert_eq!(coords_at_pos(slice, 6), (1, 0).into());

        // Test with tabs.
        let text = Rope::from("\tHello\n");
        let slice = text.slice(..);
        assert_eq!(coords_at_pos(slice, 0), (0, 0).into());
        assert_eq!(coords_at_pos(slice, 1), (0, 1).into());
        assert_eq!(coords_at_pos(slice, 2), (0, 2).into());

        // Test ASCII fast path on long single lines.
        let text = Rope::from("a".repeat(16_384));
        let slice = text.slice(..);
        assert_eq!(coords_at_pos(slice, 16_384), (0, 16_384).into());
    }

    #[test]
    #[allow(deprecated)]
    fn test_visual_coords_at_pos() {
        let text = Rope::from("ḧëḷḷö\nẅöṛḷḋ");
        let slice = text.slice(..);
        assert_eq!(visual_coords_at_pos(slice, 0, 8), (0, 0).into());
        assert_eq!(visual_coords_at_pos(slice, 5, 8), (0, 5).into()); // position on \n
        assert_eq!(visual_coords_at_pos(slice, 6, 8), (1, 0).into()); // position on w
        assert_eq!(visual_coords_at_pos(slice, 7, 8), (1, 1).into()); // position on o
        assert_eq!(visual_coords_at_pos(slice, 10, 8), (1, 4).into()); // position on d

        // Test with wide characters.
        let text = Rope::from("今日はいい\n");
        let slice = text.slice(..);
        assert_eq!(visual_coords_at_pos(slice, 0, 8), (0, 0).into());
        assert_eq!(visual_coords_at_pos(slice, 1, 8), (0, 2).into());
        assert_eq!(visual_coords_at_pos(slice, 2, 8), (0, 4).into());
        assert_eq!(visual_coords_at_pos(slice, 3, 8), (0, 6).into());
        assert_eq!(visual_coords_at_pos(slice, 4, 8), (0, 8).into());
        assert_eq!(visual_coords_at_pos(slice, 5, 8), (0, 10).into());
        assert_eq!(visual_coords_at_pos(slice, 6, 8), (1, 0).into());

        // Test with grapheme clusters.
        let text = Rope::from("a̐éö̲\r\n");
        let slice = text.slice(..);
        assert_eq!(visual_coords_at_pos(slice, 0, 8), (0, 0).into());
        assert_eq!(visual_coords_at_pos(slice, 2, 8), (0, 1).into());
        assert_eq!(visual_coords_at_pos(slice, 4, 8), (0, 2).into());
        assert_eq!(visual_coords_at_pos(slice, 7, 8), (0, 3).into());
        assert_eq!(visual_coords_at_pos(slice, 9, 8), (1, 0).into());

        // Test with wide-character grapheme clusters.
        // TODO: account for cluster.
        let text = Rope::from("किमपि\n");
        let slice = text.slice(..);
        assert_eq!(visual_coords_at_pos(slice, 0, 8), (0, 0).into());
        assert_eq!(visual_coords_at_pos(slice, 2, 8), (0, 2).into());
        assert_eq!(visual_coords_at_pos(slice, 3, 8), (0, 3).into());
        assert_eq!(visual_coords_at_pos(slice, 5, 8), (0, 5).into());
        assert_eq!(visual_coords_at_pos(slice, 6, 8), (1, 0).into());

        // Test with tabs.
        let text = Rope::from("\tHello\n");
        let slice = text.slice(..);
        assert_eq!(visual_coords_at_pos(slice, 0, 8), (0, 0).into());
        assert_eq!(visual_coords_at_pos(slice, 1, 8), (0, 8).into());
        assert_eq!(visual_coords_at_pos(slice, 2, 8), (0, 9).into());
    }

    #[test]
    fn test_visual_off_from_block() {
        let text = Rope::from("ḧëḷḷö\nẅöṛḷḋ");
        let slice = text.slice(..);
        let annot = TextAnnotations::default();
        let text_fmt = TextFormat::default();
        assert_eq!(
            visual_offset_from_block(slice, 0, 0, &text_fmt, &annot).0,
            (0, 0).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 5, &text_fmt, &annot).0,
            (0, 5).into()
        ); // position on \n
        assert_eq!(
            visual_offset_from_block(slice, 0, 6, &text_fmt, &annot).0,
            (1, 0).into()
        ); // position on w
        assert_eq!(
            visual_offset_from_block(slice, 0, 7, &text_fmt, &annot).0,
            (1, 1).into()
        ); // position on o
        assert_eq!(
            visual_offset_from_block(slice, 0, 10, &text_fmt, &annot).0,
            (1, 4).into()
        ); // position on d

        // Test with wide characters.
        let text = Rope::from("今日はいい\n");
        let slice = text.slice(..);
        assert_eq!(
            visual_offset_from_block(slice, 0, 0, &text_fmt, &annot).0,
            (0, 0).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 1, &text_fmt, &annot).0,
            (0, 2).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 2, &text_fmt, &annot).0,
            (0, 4).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 3, &text_fmt, &annot).0,
            (0, 6).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 4, &text_fmt, &annot).0,
            (0, 8).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 5, &text_fmt, &annot).0,
            (0, 10).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 6, &text_fmt, &annot).0,
            (1, 0).into()
        );

        // Test with grapheme clusters.
        let text = Rope::from("a̐éö̲\r\n");
        let slice = text.slice(..);
        assert_eq!(
            visual_offset_from_block(slice, 0, 0, &text_fmt, &annot).0,
            (0, 0).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 2, &text_fmt, &annot).0,
            (0, 1).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 4, &text_fmt, &annot).0,
            (0, 2).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 7, &text_fmt, &annot).0,
            (0, 3).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 9, &text_fmt, &annot).0,
            (1, 0).into()
        );

        // Test with wide-character grapheme clusters.
        // TODO: account for cluster.
        let text = Rope::from("किमपि\n");
        let slice = text.slice(..);
        assert_eq!(
            visual_offset_from_block(slice, 0, 0, &text_fmt, &annot).0,
            (0, 0).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 2, &text_fmt, &annot).0,
            (0, 2).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 3, &text_fmt, &annot).0,
            (0, 3).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 5, &text_fmt, &annot).0,
            (0, 5).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 6, &text_fmt, &annot).0,
            (1, 0).into()
        );

        // Test with tabs.
        let text = Rope::from("\tHello\n");
        let slice = text.slice(..);
        assert_eq!(
            visual_offset_from_block(slice, 0, 0, &text_fmt, &annot).0,
            (0, 0).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 1, &text_fmt, &annot).0,
            (0, 4).into()
        );
        assert_eq!(
            visual_offset_from_block(slice, 0, 2, &text_fmt, &annot).0,
            (0, 5).into()
        );
    }

    #[test]
    fn char_idx_and_visual_offset_fast_path_plain_ascii_line() {
        let text = Rope::from("abcdef\n");
        let slice = text.slice(..);
        let annotations = TextAnnotations::default();
        let fmt = TextFormat::default();

        let (char_idx, visual_pos, virtual_lines) =
            char_idx_and_visual_offset_at_visual_block_offset(slice, 0, 0, 4, &fmt, &annotations);

        assert_eq!(char_idx, 4);
        assert_eq!(visual_pos, Position::new(0, 4));
        assert_eq!(virtual_lines, 0);
    }

    #[test]
    fn char_idx_and_visual_offset_fast_path_handles_tabs() {
        let text = Rope::from("\tabc\n");
        let slice = text.slice(..);
        let annotations = TextAnnotations::default();
        let fmt = TextFormat::default();

        let (char_idx, visual_pos, virtual_lines) =
            char_idx_and_visual_offset_at_visual_block_offset(slice, 0, 0, 2, &fmt, &annotations);

        assert_eq!(char_idx, 0);
        assert_eq!(visual_pos, Position::new(0, 0));
        assert_eq!(virtual_lines, 0);

        let (char_idx, visual_pos, virtual_lines) =
            char_idx_and_visual_offset_at_visual_block_offset(slice, 0, 0, 4, &fmt, &annotations);

        assert_eq!(char_idx, 1);
        assert_eq!(visual_pos, Position::new(0, 4));
        assert_eq!(virtual_lines, 0);
    }

    #[test]
    fn char_idx_and_visual_offset_fast_path_skips_large_ascii_prefix() {
        let text = Rope::from(format!("{}{}\n", "a".repeat(200_000), "\txyz"));
        let slice = text.slice(..);
        let annotations = TextAnnotations::default();
        let fmt = TextFormat::default();

        let (char_idx, visual_pos, virtual_lines) =
            char_idx_and_visual_offset_at_visual_block_offset(
                slice,
                0,
                0,
                150_000,
                &fmt,
                &annotations,
            );

        assert_eq!(char_idx, 150_000);
        assert_eq!(visual_pos, Position::new(0, 150_000));
        assert_eq!(virtual_lines, 0);
    }

    #[test]
    fn plain_visual_col_at_char_idx_fast_path_plain_ascii_line() {
        let text = Rope::from("abcdef\n");
        let slice = text.slice(..);

        assert_eq!(plain_visual_col_at_char_idx(slice, 0, 4), 0);
        assert_eq!(plain_visual_col_at_char_idx(slice, 4, 4), 4);
        assert_eq!(plain_visual_col_at_char_idx(slice, 6, 4), 6);
    }

    #[test]
    fn plain_visual_col_at_char_idx_fast_path_handles_tabs() {
        let text = Rope::from("\tabc\tz\n");
        let slice = text.slice(..);

        assert_eq!(plain_visual_col_at_char_idx(slice, 0, 4), 0);
        assert_eq!(plain_visual_col_at_char_idx(slice, 1, 4), 4);
        assert_eq!(plain_visual_col_at_char_idx(slice, 4, 4), 7);
        assert_eq!(plain_visual_col_at_char_idx(slice, 5, 4), 8);
    }

    #[test]
    fn char_idx_and_visual_offset_fast_path_ignores_irrelevant_line_annotations() {
        struct EmptyLineAnnotation;

        impl crate::text_annotations::LineAnnotation for EmptyLineAnnotation {
            fn insert_virtual_lines(
                &mut self,
                _line_end_char_idx: usize,
                _line_end_visual_pos: Position,
                _doc_line: usize,
            ) -> Position {
                Position::default()
            }
        }

        let text = Rope::from("abcdef\n");
        let slice = text.slice(..);
        let mut annotations = TextAnnotations::default();
        annotations.add_line_annotation(Box::new(EmptyLineAnnotation));
        let fmt = TextFormat::default();

        let result = char_idx_and_visual_offset_at_visual_block_offset_with_kind(
            slice,
            0,
            0,
            4,
            &fmt,
            &annotations,
        );

        assert_eq!(result.kind, VisualBlockOffsetSeekKind::PlainFastPath);
        assert_eq!(result.char_idx, 4);
        assert_eq!(result.visual_pos, Position::new(0, 4));
        assert_eq!(result.virtual_lines, 0);
    }

    #[test]
    fn char_idx_and_visual_offset_fast_path_ignores_inline_annotations_after_target() {
        let text = Rope::from("abcdefghijklmnopqrstuvwxyz\n");
        let slice = text.slice(..);
        let inline = [crate::text_annotations::InlineAnnotation::new(20, "hint")];
        let mut annotations = TextAnnotations::default();
        annotations.add_inline_annotations(&inline, None);
        let fmt = TextFormat::default();

        let result = char_idx_and_visual_offset_at_visual_block_offset_with_kind(
            slice,
            0,
            0,
            8,
            &fmt,
            &annotations,
        );

        assert_eq!(result.kind, VisualBlockOffsetSeekKind::PlainFastPath);
        assert_eq!(result.char_idx, 8);
        assert_eq!(result.visual_pos, Position::new(0, 8));
    }

    #[test]
    fn char_idx_and_visual_offset_fast_path_ignores_overlays_after_target() {
        let text = Rope::from("abcdefghijklmnopqrstuvwxyz\n");
        let slice = text.slice(..);
        let overlays = [crate::text_annotations::Overlay::new(18, "X")];
        let mut annotations = TextAnnotations::default();
        annotations.add_overlay(&overlays, None);
        let fmt = TextFormat::default();

        let result = char_idx_and_visual_offset_at_visual_block_offset_with_kind(
            slice,
            0,
            0,
            6,
            &fmt,
            &annotations,
        );

        assert_eq!(result.kind, VisualBlockOffsetSeekKind::PlainFastPath);
        assert_eq!(result.char_idx, 6);
        assert_eq!(result.visual_pos, Position::new(0, 6));
    }
    #[test]
    fn test_pos_at_coords() {
        let text = Rope::from("ḧëḷḷö\nẅöṛḷḋ");
        let slice = text.slice(..);
        assert_eq!(pos_at_coords(slice, (0, 0).into(), false), 0);
        assert_eq!(pos_at_coords(slice, (0, 5).into(), false), 5); // position on \n
        assert_eq!(pos_at_coords(slice, (0, 6).into(), false), 6); // position after \n
        assert_eq!(pos_at_coords(slice, (0, 6).into(), true), 5); // position after \n
        assert_eq!(pos_at_coords(slice, (1, 0).into(), false), 6); // position on w
        assert_eq!(pos_at_coords(slice, (1, 1).into(), false), 7); // position on o
        assert_eq!(pos_at_coords(slice, (1, 4).into(), false), 10); // position on d

        // Test with wide characters.
        // TODO: account for character width.
        let text = Rope::from("今日はいい\n");
        let slice = text.slice(..);
        assert_eq!(pos_at_coords(slice, (0, 0).into(), false), 0);
        assert_eq!(pos_at_coords(slice, (0, 1).into(), false), 1);
        assert_eq!(pos_at_coords(slice, (0, 2).into(), false), 2);
        assert_eq!(pos_at_coords(slice, (0, 3).into(), false), 3);
        assert_eq!(pos_at_coords(slice, (0, 4).into(), false), 4);
        assert_eq!(pos_at_coords(slice, (0, 5).into(), false), 5);
        assert_eq!(pos_at_coords(slice, (0, 6).into(), false), 6);
        assert_eq!(pos_at_coords(slice, (0, 6).into(), true), 5);
        assert_eq!(pos_at_coords(slice, (1, 0).into(), false), 6);

        // Test with grapheme clusters.
        let text = Rope::from("a̐éö̲\r\n");
        let slice = text.slice(..);
        assert_eq!(pos_at_coords(slice, (0, 0).into(), false), 0);
        assert_eq!(pos_at_coords(slice, (0, 1).into(), false), 2);
        assert_eq!(pos_at_coords(slice, (0, 2).into(), false), 4);
        assert_eq!(pos_at_coords(slice, (0, 3).into(), false), 7); // \r\n is one char here
        assert_eq!(pos_at_coords(slice, (0, 4).into(), false), 9);
        assert_eq!(pos_at_coords(slice, (0, 4).into(), true), 7);
        assert_eq!(pos_at_coords(slice, (1, 0).into(), false), 9);

        // Test with wide-character grapheme clusters.
        // TODO: account for character width.
        let text = Rope::from("किमपि");
        // 2 - 1 - 2 codepoints
        // TODO: delete handling as per https://news.ycombinator.com/item?id=20058454
        let slice = text.slice(..);
        assert_eq!(pos_at_coords(slice, (0, 0).into(), false), 0);
        assert_eq!(pos_at_coords(slice, (0, 1).into(), false), 2);
        assert_eq!(pos_at_coords(slice, (0, 2).into(), false), 3);
        assert_eq!(pos_at_coords(slice, (0, 3).into(), false), 5);
        assert_eq!(pos_at_coords(slice, (0, 3).into(), true), 5);

        // Test with tabs.
        // Todo: account for tab stops.
        let text = Rope::from("\tHello\n");
        let slice = text.slice(..);
        assert_eq!(pos_at_coords(slice, (0, 0).into(), false), 0);
        assert_eq!(pos_at_coords(slice, (0, 1).into(), false), 1);
        assert_eq!(pos_at_coords(slice, (0, 2).into(), false), 2);

        // Test out of bounds.
        let text = Rope::new();
        let slice = text.slice(..);
        assert_eq!(pos_at_coords(slice, (10, 0).into(), true), 0);
        assert_eq!(pos_at_coords(slice, (0, 10).into(), true), 0);
        assert_eq!(pos_at_coords(slice, (10, 10).into(), true), 0);
    }

    #[test]
    #[allow(deprecated)]
    fn test_pos_at_visual_coords() {
        let text = Rope::from("ḧëḷḷö\nẅöṛḷḋ");
        let slice = text.slice(..);
        assert_eq!(pos_at_visual_coords(slice, (0, 0).into(), 4), 0);
        assert_eq!(pos_at_visual_coords(slice, (0, 5).into(), 4), 5); // position on \n
        assert_eq!(pos_at_visual_coords(slice, (0, 6).into(), 4), 5); // position after \n
        assert_eq!(pos_at_visual_coords(slice, (1, 0).into(), 4), 6); // position on w
        assert_eq!(pos_at_visual_coords(slice, (1, 1).into(), 4), 7); // position on o
        assert_eq!(pos_at_visual_coords(slice, (1, 4).into(), 4), 10); // position on d

        // Test with wide characters.
        let text = Rope::from("今日はいい\n");
        let slice = text.slice(..);
        assert_eq!(pos_at_visual_coords(slice, (0, 0).into(), 4), 0);
        assert_eq!(pos_at_visual_coords(slice, (0, 1).into(), 4), 0);
        assert_eq!(pos_at_visual_coords(slice, (0, 2).into(), 4), 1);
        assert_eq!(pos_at_visual_coords(slice, (0, 3).into(), 4), 1);
        assert_eq!(pos_at_visual_coords(slice, (0, 4).into(), 4), 2);
        assert_eq!(pos_at_visual_coords(slice, (0, 5).into(), 4), 2);
        assert_eq!(pos_at_visual_coords(slice, (0, 6).into(), 4), 3);
        assert_eq!(pos_at_visual_coords(slice, (0, 7).into(), 4), 3);
        assert_eq!(pos_at_visual_coords(slice, (0, 8).into(), 4), 4);
        assert_eq!(pos_at_visual_coords(slice, (0, 9).into(), 4), 4);
        // assert_eq!(pos_at_visual_coords(slice, (0, 10).into(), 4, false), 5);
        // assert_eq!(pos_at_visual_coords(slice, (0, 10).into(), 4, true), 5);
        assert_eq!(pos_at_visual_coords(slice, (1, 0).into(), 4), 6);

        // Test with grapheme clusters.
        let text = Rope::from("a̐éö̲\r\n");
        let slice = text.slice(..);
        assert_eq!(pos_at_visual_coords(slice, (0, 0).into(), 4), 0);
        assert_eq!(pos_at_visual_coords(slice, (0, 1).into(), 4), 2);
        assert_eq!(pos_at_visual_coords(slice, (0, 2).into(), 4), 4);
        assert_eq!(pos_at_visual_coords(slice, (0, 3).into(), 4), 7); // \r\n is one char here
        assert_eq!(pos_at_visual_coords(slice, (0, 4).into(), 4), 7);
        assert_eq!(pos_at_visual_coords(slice, (1, 0).into(), 4), 9);

        // Test with wide-character grapheme clusters.
        let text = Rope::from("किमपि");
        // 2 - 1 - 2 codepoints
        // TODO: delete handling as per https://news.ycombinator.com/item?id=20058454
        let slice = text.slice(..);
        assert_eq!(pos_at_visual_coords(slice, (0, 0).into(), 4), 0);
        assert_eq!(pos_at_visual_coords(slice, (0, 1).into(), 4), 0);
        assert_eq!(pos_at_visual_coords(slice, (0, 2).into(), 4), 2);
        assert_eq!(pos_at_visual_coords(slice, (0, 3).into(), 4), 3);

        // Test with tabs.
        let text = Rope::from("\tHello\n");
        let slice = text.slice(..);
        assert_eq!(pos_at_visual_coords(slice, (0, 0).into(), 4), 0);
        assert_eq!(pos_at_visual_coords(slice, (0, 1).into(), 4), 0);
        assert_eq!(pos_at_visual_coords(slice, (0, 2).into(), 4), 0);
        assert_eq!(pos_at_visual_coords(slice, (0, 3).into(), 4), 0);
        assert_eq!(pos_at_visual_coords(slice, (0, 4).into(), 4), 1);
        assert_eq!(pos_at_visual_coords(slice, (0, 5).into(), 4), 2);

        // Test out of bounds.
        let text = Rope::new();
        let slice = text.slice(..);
        assert_eq!(pos_at_visual_coords(slice, (10, 0).into(), 4), 0);
        assert_eq!(pos_at_visual_coords(slice, (0, 10).into(), 4), 0);
        assert_eq!(pos_at_visual_coords(slice, (10, 10).into(), 4), 0);
    }

    #[test]
    fn test_char_idx_at_visual_row_offset_inline_annotation() {
        let text = Rope::from("foo\nbar");
        let slice = text.slice(..);
        let mut text_fmt = TextFormat::default();
        let annotations = [InlineAnnotation {
            text: "x".repeat(100).into(),
            char_idx: 3,
        }];
        text_fmt.soft_wrap = true;

        assert_eq!(
            char_idx_at_visual_offset(
                slice,
                0,
                1,
                0,
                &text_fmt,
                TextAnnotations::default().add_inline_annotations(&annotations, None)
            ),
            (2, 1)
        );
    }

    #[test]
    fn test_char_idx_at_visual_row_offset() {
        let text = Rope::from("ḧëḷḷö\nẅöṛḷḋ\nfoo");
        let slice = text.slice(..);
        let mut text_fmt = TextFormat::default();
        for i in 0isize..3isize {
            for j in -2isize..=2isize {
                if !(0..3).contains(&(i + j)) {
                    continue;
                }
                println!("{i} {j}");
                assert_eq!(
                    char_idx_at_visual_offset(
                        slice,
                        slice.line_to_char(i as usize),
                        j,
                        3,
                        &text_fmt,
                        &TextAnnotations::default(),
                    )
                    .0,
                    slice.line_to_char((i + j) as usize) + 3
                );
            }
        }

        text_fmt.soft_wrap = true;
        let mut softwrapped_text = "foo ".repeat(10);
        softwrapped_text.push('\n');
        let last_char = softwrapped_text.len() - 1;

        let text = Rope::from(softwrapped_text.repeat(3));
        let slice = text.slice(..);
        assert_eq!(
            char_idx_at_visual_offset(
                slice,
                last_char,
                0,
                0,
                &text_fmt,
                &TextAnnotations::default(),
            )
            .0,
            32
        );
        assert_eq!(
            char_idx_at_visual_offset(
                slice,
                last_char,
                -1,
                0,
                &text_fmt,
                &TextAnnotations::default(),
            )
            .0,
            16
        );
        assert_eq!(
            char_idx_at_visual_offset(
                slice,
                last_char,
                -2,
                0,
                &text_fmt,
                &TextAnnotations::default(),
            )
            .0,
            0
        );
        assert_eq!(
            char_idx_at_visual_offset(
                slice,
                softwrapped_text.len() + last_char,
                -2,
                0,
                &text_fmt,
                &TextAnnotations::default(),
            )
            .0,
            softwrapped_text.len()
        );

        assert_eq!(
            char_idx_at_visual_offset(
                slice,
                softwrapped_text.len() + last_char,
                -5,
                0,
                &text_fmt,
                &TextAnnotations::default(),
            )
            .0,
            0
        );
    }
}
