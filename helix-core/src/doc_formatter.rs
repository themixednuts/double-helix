//! The `DocumentFormatter` forms the bridge between the raw document text
//! and onscreen positioning. It yields the text graphemes as an iterator
//! and traverses (part) of the document text. During that traversal it
//! handles grapheme detection, softwrapping and annotations.
//! It yields `FormattedGrapheme`s and their corresponding visual coordinates.
//!
//! As both virtual text and softwrapping can insert additional lines into the document
//! it is generally not possible to find the start of the previous visual line.
//! Instead the `DocumentFormatter` starts at the last "checkpoint" (usually a linebreak)
//! called a "block" and the caller must advance it as needed.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::fmt::Debug;
use std::mem::replace;

#[cfg(test)]
mod test;

use unicode_segmentation::{Graphemes, UnicodeSegmentation};

use helix_stdx::rope::{RopeGraphemes, RopeSliceExt};

use crate::graphemes::{next_grapheme_boundary, Grapheme, GraphemeStr};
use crate::syntax::Highlight;
use crate::text_annotations::TextAnnotations;
use crate::{Position, RopeSlice};

#[derive(Debug, Clone, Copy)]
pub enum GraphemeSource {
    Document {
        codepoints: u32,
    },
    /// Inline virtual text can not be highlighted with a `Highlight` iterator
    /// because it's not part of the document. Instead the `Highlight`
    /// is emitted right by the document formatter
    VirtualText {
        highlight: Option<Highlight>,
    },
}

impl GraphemeSource {
    /// Returns whether this grapheme is virtual inline text
    pub fn is_virtual(self) -> bool {
        matches!(self, GraphemeSource::VirtualText { .. })
    }

    pub fn is_eof(self) -> bool {
        // all doc chars except the EOF char have non-zero codepoints
        matches!(self, GraphemeSource::Document { codepoints: 0 })
    }

    pub fn doc_chars(self) -> usize {
        match self {
            GraphemeSource::Document { codepoints } => codepoints as usize,
            GraphemeSource::VirtualText { .. } => 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FormattedGrapheme<'a> {
    pub raw: Grapheme<'a>,
    pub source: GraphemeSource,
    pub visual_pos: Position,
    /// Document line at the start of the grapheme
    pub line_idx: usize,
    /// Document char position at the start of the grapheme
    pub char_idx: usize,
}

impl FormattedGrapheme<'_> {
    pub fn is_virtual(&self) -> bool {
        self.source.is_virtual()
    }

    pub fn doc_chars(&self) -> usize {
        self.source.doc_chars()
    }

    pub fn is_whitespace(&self) -> bool {
        self.raw.is_whitespace()
    }

    pub fn width(&self) -> usize {
        self.raw.width()
    }

    pub fn is_word_boundary(&self) -> bool {
        self.raw.is_word_boundary()
    }
}

#[derive(Debug, Clone)]
struct GraphemeWithSource<'a> {
    grapheme: Grapheme<'a>,
    source: GraphemeSource,
}

impl<'a> GraphemeWithSource<'a> {
    fn new(
        g: GraphemeStr<'a>,
        visual_x: usize,
        tab_width: u16,
        source: GraphemeSource,
    ) -> GraphemeWithSource<'a> {
        GraphemeWithSource {
            grapheme: Grapheme::new(g, visual_x, tab_width),
            source,
        }
    }
    fn placeholder() -> Self {
        GraphemeWithSource {
            grapheme: Grapheme::Other { g: " ".into() },
            source: GraphemeSource::Document { codepoints: 0 },
        }
    }

    fn doc_chars(&self) -> usize {
        self.source.doc_chars()
    }

    fn is_whitespace(&self) -> bool {
        self.grapheme.is_whitespace()
    }

    fn is_newline(&self) -> bool {
        matches!(self.grapheme, Grapheme::Newline)
    }

    fn is_eof(&self) -> bool {
        self.source.is_eof()
    }

    fn width(&self) -> usize {
        self.grapheme.width()
    }

    fn is_word_boundary(&self) -> bool {
        self.grapheme.is_word_boundary()
    }
}

#[derive(Debug, Clone)]
pub struct TextFormat {
    pub soft_wrap: bool,
    pub tab_width: u16,
    pub max_wrap: u16,
    pub max_indent_retain: u16,
    pub wrap_indicator_highlight: Option<Highlight>,
    pub viewport_width: u16,
    pub soft_wrap_at_text_width: bool,
}

// test implementation is basically only used for testing or when softwrap is always disabled
impl Default for TextFormat {
    fn default() -> Self {
        TextFormat {
            soft_wrap: false,
            tab_width: 4,
            max_wrap: 3,
            max_indent_retain: 4,
            viewport_width: 17,
            wrap_indicator_highlight: None,
            soft_wrap_at_text_width: false,
        }
    }
}

#[derive(Debug)]
pub struct DocumentFormatter<'t> {
    text: RopeSlice<'t>,

    text_fmt: &'t TextFormat,
    annotations: &'t TextAnnotations<'t>,

    /// The visual position at the end of the last yielded word boundary
    visual_pos: Position,
    graphemes: RopeGraphemes<'t>,
    /// The character pos of the `graphemes` iter used for inserting annotations
    char_pos: usize,
    /// The line pos of the `graphemes` iter used for inserting annotations
    line_pos: usize,
    /// The char index at the start of the current document line.
    line_start_char: usize,
    exhausted: bool,

    inline_annotation_graphemes: Option<(Graphemes<'t>, Option<Highlight>)>,

    // softwrap specific
    /// The indentation of the current line
    /// Is set to `None` if the indentation level is not yet known
    /// because no non-whitespace graphemes have been encountered yet
    indent_level: Option<usize>,
    /// In case a long word needs to be split a single grapheme might need to be wrapped
    /// while the rest of the word stays on the same line
    peeked_grapheme: Option<GraphemeWithSource<'t>>,
    /// A first-in first-out (fifo) buffer for the Graphemes of any given word
    word_buf: Vec<GraphemeWithSource<'t>>,
    /// The index of the next grapheme that will be yielded from the `word_buf`
    word_i: usize,
    stats: DocumentFormatterStats,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DocumentFormatterStats {
    pub next_calls: usize,
    pub advance_grapheme_calls: usize,
    pub inline_annotation_hits: usize,
    pub overlay_hits: usize,
    pub fold_skip_count: usize,
    pub folded_chars_skipped: usize,
    pub word_refills: usize,
    pub skip_to_next_line_calls: usize,
    pub yielded_document: usize,
    pub yielded_virtual: usize,
    pub yielded_newlines: usize,
    pub yielded_eof: usize,
}

pub enum HorizontalLineSeekResult<'t> {
    Visible(FormattedGrapheme<'t>),
    LineEnded {
        next_grapheme: Option<FormattedGrapheme<'t>>,
        next_char_idx: usize,
        line_end_col: usize,
    },
}

impl<'t> DocumentFormatter<'t> {
    pub fn reset_to_checkpoint(&mut self, char_idx: usize, visual_pos: Position) {
        let char_idx = char_idx.min(self.text.len_chars());
        let line_idx = self.text.char_to_line(char_idx);
        self.annotations.reset_pos(char_idx);
        self.visual_pos = visual_pos;
        self.graphemes = self.text.slice(char_idx..).graphemes();
        self.char_pos = char_idx;
        self.line_pos = line_idx;
        self.line_start_char = self.text.line_to_char(line_idx);
        self.exhausted = false;
        self.inline_annotation_graphemes = None;
        self.indent_level = None;
        self.peeked_grapheme = None;
        self.word_buf.clear();
        self.word_i = 0;
    }

    pub fn new_at_checkpoint(
        text: RopeSlice<'t>,
        text_fmt: &'t TextFormat,
        annotations: &'t TextAnnotations,
        char_idx: usize,
        visual_pos: Position,
    ) -> Self {
        let mut formatter = DocumentFormatter {
            text,
            text_fmt,
            annotations,
            visual_pos: Position::default(),
            graphemes: text.slice(char_idx..).graphemes(),
            char_pos: 0,
            exhausted: false,
            indent_level: None,
            peeked_grapheme: None,
            word_buf: Vec::with_capacity(64),
            word_i: 0,
            stats: DocumentFormatterStats::default(),
            line_pos: 0,
            line_start_char: 0,
            inline_annotation_graphemes: None,
        };
        formatter.reset_to_checkpoint(char_idx, visual_pos);
        formatter
    }

    /// Creates a new formatter at the last block before `char_idx`.
    /// A block is a chunk which always ends with a linebreak.
    /// This is usually just a normal line break.
    /// However very long lines are always wrapped at constant intervals that can be cheaply calculated
    /// to avoid pathological behaviour.
    pub fn new_at_prev_checkpoint(
        text: RopeSlice<'t>,
        text_fmt: &'t TextFormat,
        annotations: &'t TextAnnotations,
        mut char_idx: usize,
    ) -> Self {
        // if `char_idx` is folded restore its value to the starting char of the block
        if let Some(fold) = annotations
            .folds
            .superest_fold_containing(char_idx, |fold| fold.start.char..=fold.end.char)
        {
            char_idx = fold.start.char
        }

        // TODO divide long lines into blocks to avoid bad performance for long lines
        let block_line_idx = text.char_to_line(char_idx.min(text.len_chars()));
        let block_char_idx = text.line_to_char(block_line_idx);
        annotations.reset_pos(block_char_idx);

        Self::new_at_checkpoint(
            text,
            text_fmt,
            annotations,
            block_char_idx,
            Position { row: 0, col: 0 },
        )
    }

    fn next_inline_annotation_grapheme(
        &mut self,
        char_pos: usize,
    ) -> Option<(&'t str, Option<Highlight>)> {
        loop {
            if let Some(&mut (ref mut annotation, highlight)) =
                self.inline_annotation_graphemes.as_mut()
            {
                if let Some(grapheme) = annotation.next() {
                    return Some((grapheme, highlight));
                }
            }

            if let Some((annotation, highlight)) =
                self.annotations.next_inline_annotation_at(char_pos)
            {
                self.inline_annotation_graphemes = Some((
                    UnicodeSegmentation::graphemes(&*annotation.text, true),
                    highlight,
                ))
            } else {
                return None;
            }
        }
    }

    fn advance_grapheme(
        &mut self,
        col: usize,
        mut char_pos: usize,
    ) -> Option<GraphemeWithSource<'t>> {
        self.stats.advance_grapheme_calls += 1;
        if let Some(folded_chars) = self.skip_folded_chars(char_pos) {
            char_pos += folded_chars;
        }

        let (grapheme, source) =
            if let Some((grapheme, highlight)) = self.next_inline_annotation_grapheme(char_pos) {
                self.stats.inline_annotation_hits += 1;
                (grapheme.into(), GraphemeSource::VirtualText { highlight })
            } else if let Some(grapheme) = self.graphemes.next() {
                let codepoints = grapheme.len_chars() as u32;

                let overlay = self.annotations.overlay_at(char_pos);
                let grapheme = match overlay {
                    Some((overlay, _)) => {
                        self.stats.overlay_hits += 1;
                        overlay.grapheme.as_str().into()
                    }
                    None => Cow::from(grapheme).into(),
                };

                (grapheme, GraphemeSource::Document { codepoints })
            } else {
                if self.exhausted {
                    return None;
                }
                self.exhausted = true;
                // EOF grapheme is required for rendering
                // and correct position computations
                return Some(GraphemeWithSource {
                    grapheme: Grapheme::Other { g: " ".into() },
                    source: GraphemeSource::Document { codepoints: 0 },
                });
            };

        let grapheme = GraphemeWithSource::new(grapheme, col, self.text_fmt.tab_width, source);

        Some(grapheme)
    }

    fn skip_folded_chars(&mut self, char_pos: usize) -> Option<usize> {
        let (folded_chars, folded_lines) = self
            .annotations
            .folds
            .consume_next(char_pos, |fold| fold.start.char)
            .map(|fold| {
                (
                    next_grapheme_boundary(self.text, fold.end.char) - fold.start.char,
                    fold.end.line - fold.start.line + 1,
                )
            })?;
        self.stats.fold_skip_count += 1;
        self.stats.folded_chars_skipped += folded_chars;

        if char_pos + folded_chars < self.text.len_chars() {
            self.graphemes = self.text.slice(char_pos + folded_chars..).graphemes();
        } else {
            self.graphemes = RopeSlice::from("").graphemes();
        }
        self.annotations.reset_pos(char_pos + folded_chars);

        self.char_pos += folded_chars;
        self.line_pos += folded_lines;
        self.line_start_char = self.text.line_to_char(self.line_pos);

        Some(folded_chars)
    }

    /// Move a word to the next visual line
    fn wrap_word(&mut self) -> usize {
        // softwrap this word to the next line
        let indent_carry_over = if let Some(indent) = self.indent_level {
            if indent as u16 <= self.text_fmt.max_indent_retain {
                indent as u16
            } else {
                0
            }
        } else {
            // ensure the indent stays 0
            self.indent_level = Some(0);
            0
        };

        let virtual_lines =
            self.annotations
                .virtual_lines_at(self.char_pos, self.visual_pos, self.line_pos);
        self.visual_pos.col = indent_carry_over as usize;
        self.visual_pos.row += 1 + virtual_lines;
        let mut word_width = 0;
        for grapheme in &mut self.word_buf {
            let visual_x = self.visual_pos.col + word_width;
            grapheme
                .grapheme
                .change_position(visual_x, self.text_fmt.tab_width);
            word_width += grapheme.width();
        }
        if let Some(grapheme) = &mut self.peeked_grapheme {
            let visual_x = self.visual_pos.col + word_width;
            grapheme
                .grapheme
                .change_position(visual_x, self.text_fmt.tab_width);
        }
        word_width
    }

    fn peek_grapheme(&mut self, col: usize, char_pos: usize) -> Option<&GraphemeWithSource<'t>> {
        if self.peeked_grapheme.is_none() {
            self.peeked_grapheme = self.advance_grapheme(col, char_pos);
        }
        self.peeked_grapheme.as_ref()
    }

    fn next_grapheme(&mut self, col: usize, char_pos: usize) -> Option<GraphemeWithSource<'t>> {
        self.peek_grapheme(col, char_pos);
        self.peeked_grapheme.take()
    }

    fn advance_to_next_word(&mut self) {
        self.stats.word_refills += 1;
        self.word_buf.clear();
        let mut word_width = 0;
        let mut word_chars = 0;

        if self.exhausted {
            return;
        }

        loop {
            let mut col = self.visual_pos.col + word_width;
            let char_pos = self.char_pos + word_chars;
            match col.cmp(&(self.text_fmt.viewport_width as usize)) {
                // The EOF char and newline chars are always selectable in helix. That means
                // that wrapping happens "too-early" if a word fits a line perfectly. This
                // is intentional so that all selectable graphemes are always visible (and
                // therefore the cursor never disappears). However if the user manually set a
                // lower softwrap width then this is undesirable. Just increasing the viewport-
                // width by one doesn't work because if a line is wrapped multiple times then
                // some words may extend past the specified width.
                //
                // So we special case a word that ends exactly at line bounds and is followed
                // by a newline/eof character here.
                Ordering::Equal
                    if self.text_fmt.soft_wrap_at_text_width
                        && self
                            .peek_grapheme(col, char_pos)
                            .is_some_and(|grapheme| grapheme.is_newline() || grapheme.is_eof()) => {
                }
                Ordering::Equal if word_width > self.text_fmt.max_wrap as usize => return,
                Ordering::Greater if word_width > self.text_fmt.max_wrap as usize => {
                    self.peeked_grapheme = self.word_buf.pop();
                    return;
                }
                Ordering::Equal | Ordering::Greater => {
                    word_width = self.wrap_word();
                    col = self.visual_pos.col + word_width;
                }
                Ordering::Less => (),
            }

            let Some(grapheme) = self.next_grapheme(col, char_pos) else {
                return;
            };
            word_chars += grapheme.doc_chars();

            // Track indentation
            if !grapheme.is_whitespace() && self.indent_level.is_none() {
                self.indent_level = Some(self.visual_pos.col);
            } else if grapheme.grapheme == Grapheme::Newline {
                self.indent_level = None;
            }

            let is_word_boundary = grapheme.is_word_boundary();
            word_width += grapheme.width();
            self.word_buf.push(grapheme);

            if is_word_boundary {
                return;
            }
        }
    }

    /// Skip to the start of the next document line without yielding graphemes.
    /// Only valid in no-softwrap mode. Returns the char index of the next line,
    /// or `None` if at the end of the document.
    ///
    /// This is an optimization for long lines: instead of iterating every grapheme
    /// past the viewport right edge, we jump directly to the next line boundary.
    pub fn skip_to_next_line(&mut self) -> Option<usize> {
        self.stats.skip_to_next_line_calls += 1;
        debug_assert!(
            !self.text_fmt.soft_wrap,
            "skip_to_next_line is only valid without soft-wrap"
        );
        let next_line = self.line_pos + 1;
        if next_line >= self.text.len_lines() {
            self.exhausted = true;
            self.char_pos = self.text.len_chars();
            self.graphemes = RopeSlice::from("").graphemes();
            self.inline_annotation_graphemes = None;
            return None;
        }
        let next_line_char = self.text.line_to_char(next_line);
        let virtual_lines = if self.annotations.is_empty() {
            0
        } else {
            // Account for virtual lines after the newline (e.g. diagnostics, inlay hints).
            self.annotations.virtual_lines_at(
                next_line_char,
                Position {
                    row: self.visual_pos.row,
                    col: 1,
                },
                self.line_pos,
            )
        };
        self.char_pos = next_line_char;
        self.line_pos = next_line;
        self.line_start_char = next_line_char;
        self.visual_pos.row += 1 + virtual_lines;
        self.visual_pos.col = 0;
        self.graphemes = self.text.slice(next_line_char..).graphemes();
        self.inline_annotation_graphemes = None;
        if !self.annotations.is_empty() {
            self.annotations.reset_pos(next_line_char);
        }
        Some(next_line_char)
    }

    /// returns the char index at the end of the last yielded grapheme
    pub fn next_char_pos(&self) -> usize {
        self.char_pos
    }
    /// returns the visual position at the end of the last yielded grapheme
    pub fn next_visual_pos(&self) -> Position {
        self.visual_pos
    }

    pub fn stats(&self) -> DocumentFormatterStats {
        self.stats
    }

    pub fn seek_to_visual_col_in_current_line(
        &mut self,
        mut grapheme: FormattedGrapheme<'t>,
        target_col: usize,
    ) -> HorizontalLineSeekResult<'t> {
        debug_assert!(
            !self.text_fmt.soft_wrap,
            "seek_to_visual_col_in_current_line is only valid without soft-wrap"
        );
        let visual_row = grapheme.visual_pos.row;
        let mut line_end_col = grapheme.visual_pos.col + grapheme.width();

        loop {
            if grapheme.visual_pos.col + grapheme.width() > target_col {
                return HorizontalLineSeekResult::Visible(grapheme);
            }

            let Some(next_grapheme) = self.next() else {
                return HorizontalLineSeekResult::LineEnded {
                    next_grapheme: None,
                    next_char_idx: self.next_char_pos(),
                    line_end_col,
                };
            };

            if next_grapheme.visual_pos.row != visual_row {
                return HorizontalLineSeekResult::LineEnded {
                    next_grapheme: Some(next_grapheme),
                    next_char_idx: self.next_char_pos(),
                    line_end_col,
                };
            }

            line_end_col = next_grapheme.visual_pos.col + next_grapheme.width();
            grapheme = next_grapheme;
        }
    }
}

impl<'t> Iterator for DocumentFormatter<'t> {
    type Item = FormattedGrapheme<'t>;

    fn next(&mut self) -> Option<Self::Item> {
        self.stats.next_calls += 1;
        let grapheme = if self.text_fmt.soft_wrap {
            if self.word_i >= self.word_buf.len() {
                self.advance_to_next_word();
                self.word_i = 0;
            }
            let grapheme = replace(
                self.word_buf.get_mut(self.word_i)?,
                GraphemeWithSource::placeholder(),
            );
            self.word_i += 1;
            grapheme
        } else {
            self.advance_grapheme(self.visual_pos.col, self.char_pos)?
        };

        let grapheme = FormattedGrapheme {
            raw: grapheme.grapheme,
            source: grapheme.source,
            visual_pos: self.visual_pos,
            line_idx: self.line_pos,
            char_idx: self.char_pos,
        };

        if grapheme.is_virtual() {
            self.stats.yielded_virtual += 1;
        } else {
            self.stats.yielded_document += 1;
            if grapheme.source.is_eof() {
                self.stats.yielded_eof += 1;
            }
        }
        if grapheme.raw == Grapheme::Newline {
            self.stats.yielded_newlines += 1;
        }

        self.char_pos += grapheme.doc_chars();
        if !grapheme.is_virtual() {
            self.annotations.process_virtual_text_anchors(&grapheme);
        }
        if grapheme.raw == Grapheme::Newline {
            // move to end of newline char
            self.visual_pos.col += 1;
            let virtual_lines =
                self.annotations
                    .virtual_lines_at(self.char_pos, self.visual_pos, self.line_pos);
            self.visual_pos.row += 1 + virtual_lines;
            self.visual_pos.col = 0;
            if !grapheme.is_virtual() {
                self.line_pos += 1;
                self.line_start_char = self.char_pos;
            }
        } else {
            self.visual_pos.col += grapheme.width();
        }
        Some(grapheme)
    }
}
