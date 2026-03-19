use std::cmp::min;
use std::sync::Arc;
use std::time::Instant;

use helix_core::doc_formatter::{
    DocumentFormatter, DocumentFormatterStats, FormattedGrapheme, GraphemeSource,
    HorizontalLineSeekResult, TextFormat,
};
use helix_core::graphemes::Grapheme;
use helix_core::str_utils::char_to_byte_idx;
use helix_core::syntax::{self, HighlightEvent, Highlighter, OverlayHighlights};
use helix_core::text_annotations::TextAnnotations;
use helix_core::{
    char_idx_and_visual_offset_at_visual_block_offset_with_kind,
    visual_offset_from_block_with_metrics, Position, RopeSlice, VisualBlockOffsetSeekKind,
};
use helix_stdx::rope::RopeSliceExt;
use helix_view::editor::{WhitespaceConfig, WhitespaceRenderValue};
use helix_view::graphics::Rect;
use helix_view::theme::Style;
use helix_view::view::{RenderSeed, ViewPosition};
use helix_view::{Document, Theme};
use tui::buffer::Buffer as Surface;

use crate::ui::text_decorations::DecorationManager;

/// Input to the document renderer: either a live tree-sitter highlighter
/// or cached syntax styles from a previous frame.
pub enum HighlighterInput<'a> {
    /// Live highlighter — runs tree-sitter queries. Output is recorded for caching.
    Live(Option<Highlighter<'a>>),
    /// Cached styles — replays pre-computed styles. No tree-sitter queries.
    Cached(&'a [helix_view::view::SyntaxStyleEntry]),
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub struct LinePos {
    /// Indicates whether the given visual line
    /// is the first visual line of the given document line
    pub first_visual_line: bool,
    /// The line index of the document line that contains the given visual line
    pub doc_line: usize,
    /// Vertical offset from the top of the inner view area
    pub visual_line: u16,
    /// The given visual line is the last visual line of the document line
    pub is_last_visual_line: bool,
}

/// Return type for render_document / render_text: cached syntax styles + line map.
pub struct RenderOutput {
    pub syntax_styles: Vec<helix_view::view::SyntaxStyleEntry>,
    pub line_map: helix_view::view::LineMap,
}

#[allow(clippy::too_many_arguments)]
pub fn render_document(
    surface: &mut Surface,
    viewport: Rect,
    doc: &Document,
    offset: ViewPosition,
    doc_annotations: &TextAnnotations,
    highlighter_input: HighlighterInput<'_>,
    overlay_highlights: Vec<syntax::OverlayHighlights>,
    theme: &Theme,
    decorations: DecorationManager,
    dirty_rows: Option<&std::collections::HashSet<u16>>,
    seed: Option<RenderSeed>,
    seed_line_map: Option<&helix_view::view::LineMap>,
) -> RenderOutput {
    let mut renderer = TextRenderer::new(
        surface,
        doc,
        theme,
        Position::new(offset.vertical_offset, offset.horizontal_offset),
        viewport,
    );
    render_text(
        &mut renderer,
        doc.text().slice(..),
        offset.anchor,
        &doc.text_format(viewport.width, Some(theme)),
        doc_annotations,
        highlighter_input,
        overlay_highlights,
        theme,
        decorations,
        dirty_rows,
        seed,
        seed_line_map,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn render_text(
    renderer: &mut TextRenderer,
    text: RopeSlice<'_>,
    anchor: usize,
    text_fmt: &TextFormat,
    text_annotations: &TextAnnotations,
    highlighter_input: HighlighterInput<'_>,
    overlay_highlights: Vec<syntax::OverlayHighlights>,
    theme: &Theme,
    mut decorations: DecorationManager,
    dirty_rows: Option<&std::collections::HashSet<u16>>,
    seed: Option<RenderSeed>,
    seed_line_map: Option<&helix_view::view::LineMap>,
) -> RenderOutput {
    use helix_view::view::{HorizontalCheckpoint, LineMap, VisualLineInfo};

    #[derive(Clone, Copy)]
    enum SeedSource {
        Explicit,
        LineMap,
        CurrentSeek,
    }

    const HORIZONTAL_CHECKPOINT_STRIDE: usize = 4096;

    fn record_horizontal_checkpoint(line: &mut VisualLineInfo, char_idx: usize, visual_col: usize) {
        if line
            .horizontal_checkpoints
            .last()
            .is_some_and(|checkpoint| checkpoint.visual_col == visual_col)
        {
            return;
        }
        line.horizontal_checkpoints.push(HorizontalCheckpoint {
            char_idx,
            visual_col,
        });
    }

    fn can_resume_from_seed(
        text: RopeSlice<'_>,
        line_idx: usize,
        current_char_idx: usize,
        seed_char_idx: usize,
    ) -> bool {
        let text_len = text.len_chars();
        if seed_char_idx > text_len || seed_char_idx <= current_char_idx {
            return false;
        }
        text.char_to_line(seed_char_idx.min(text_len)) == line_idx
    }

    let row_offset_start = Instant::now();
    let anchor_line = text.char_to_line(anchor.min(text.len_chars()));
    let (row_off, row_offset_details) = if !text_fmt.soft_wrap
        && matches!(
            text_annotations.plain_viewport_support(anchor_line, anchor_line),
            helix_core::text_annotations::PlainViewportSupport::Supported
        ) {
        (
            0,
            format!(
                concat!(
                    "anchor={} row_off=0 block_start={} soft_wrap=false viewport_width={} ",
                    "text_chars={} text_bytes={} fast_path=plain_viewport"
                ),
                anchor,
                text.line_to_char(anchor_line),
                text_fmt.viewport_width,
                text.len_chars(),
                text.len_bytes(),
            ),
        )
    } else {
        let row_offset =
            visual_offset_from_block_with_metrics(text, anchor, anchor, text_fmt, text_annotations);
        (
            row_offset.result.0.row,
            format!(
                concat!(
                    "anchor={} row_off={} block_start={} soft_wrap={} viewport_width={} ",
                    "text_chars={} text_bytes={} next_calls={} formatter_next_calls={} ",
                    "advance_grapheme_calls={} word_refills={} fold_skips={} ",
                    "inline_annotations={} overlays={} fast_path=formatter"
                ),
                anchor,
                row_offset.result.0.row,
                row_offset.result.1,
                text_fmt.soft_wrap,
                text_fmt.viewport_width,
                text.len_chars(),
                text.len_bytes(),
                row_offset.metrics.next_calls,
                row_offset.metrics.formatter_next_calls,
                row_offset.metrics.formatter_advance_grapheme_calls,
                row_offset.metrics.formatter_word_refills,
                row_offset.metrics.formatter_fold_skip_count,
                row_offset.metrics.formatter_inline_annotation_hits,
                row_offset.metrics.formatter_overlay_hits,
            ),
        )
    };
    helix_view::bench::log_run_phase(
        "render_document",
        "row_offset",
        row_offset_start.elapsed(),
        || row_offset_details.clone(),
    );
    let mut reached_view_top = false;
    let mut formatter = if let Some(seed) = seed.filter(|seed| {
        !text_fmt.soft_wrap
            && row_off == 0
            && seed.doc_line == anchor_line
            && seed.char_idx <= text.len_chars()
    }) {
        reached_view_top = true;
        decorations.fast_forward_to_char(seed.char_idx, seed.doc_line);
        DocumentFormatter::new_at_checkpoint(
            text,
            text_fmt,
            text_annotations,
            seed.char_idx,
            Position::new(0, seed.visual_col),
        )
    } else {
        DocumentFormatter::new_at_prev_checkpoint(text, text_fmt, text_annotations, anchor)
    };
    let mut syntax_highlighter = match highlighter_input {
        HighlighterInput::Live(highlighter) => {
            SyntaxHighlighter::new(highlighter, text, theme, renderer.text_style)
        }
        HighlighterInput::Cached(entries) => {
            SyntaxHighlighter::from_cache(entries, text, theme, renderer.text_style)
        }
    };
    let mut overlay_highlighter = OverlayHighlighter::new(overlay_highlights, theme);

    if let Some(seed) = seed.filter(|seed| {
        !text_fmt.soft_wrap
            && row_off == 0
            && seed.doc_line == anchor_line
            && seed.char_idx <= text.len_chars()
    }) {
        syntax_highlighter.advance_to(seed.char_idx);
        overlay_highlighter.advance_to(seed.char_idx);
    }

    let mut last_line_pos = LinePos {
        first_visual_line: false,
        doc_line: usize::MAX,
        visual_line: u16::MAX,
        is_last_visual_line: true,
    };
    let mut last_line_end = 0;
    let mut is_in_indent_area = true;
    let mut last_line_indent_level = 0;
    let loop_start = Instant::now();
    let mut skipped_before_top = 0usize;
    let mut skipped_left = 0usize;
    let mut skipped_right = 0usize;
    let mut drawn = 0usize;
    let mut dirty_offscreen = 0usize;
    let mut syntax_advances = 0usize;
    let mut overlay_advances = 0usize;
    let mut virtual_drawn = 0usize;
    let mut zero_width_drawn = 0usize;
    let mut max_drawn_col = 0usize;
    let mut first_visible_char = None;
    let mut last_visible_char = None;
    let mut first_drawn_char = None;
    let mut last_drawn_char = None;
    let formatter_stats_start: DocumentFormatterStats = formatter.stats();
    let mut formatter_next_us = 0u64;
    let mut advance_loop_us = 0u64;
    let mut decoration_us = 0u64;
    let mut draw_us = 0u64;
    let mut bookkeeping_us = 0u64;
    let mut skip_right_us = 0u64;
    let mut skip_left_syntax_us = 0u64;
    let mut skip_left_overlay_us = 0u64;
    let mut skip_left_decor_us = 0u64;
    let mut skip_right_syntax_us = 0u64;
    let mut skip_right_overlay_us = 0u64;
    let mut skip_right_decor_us = 0u64;
    let mut line_finalize_us = 0u64;
    let mut skipped_offscreen_left_lines = 0usize;
    let viewport_right = renderer.offset.col + renderer.viewport.width as usize;

    // Line map: track visual row → document position mapping
    let mut line_map_lines: Vec<VisualLineInfo> = Vec::new();

    let mut pending_grapheme: Option<FormattedGrapheme<'_>> = None;
    'render: loop {
        let next_start = Instant::now();
        let Some(mut grapheme) = pending_grapheme.take().or_else(|| formatter.next()) else {
            break;
        };
        formatter_next_us += next_start.elapsed().as_micros() as u64;

        // skip any graphemes on visual lines before the block start
        if grapheme.visual_pos.row < row_off {
            skipped_before_top += 1;
            continue;
        }
        grapheme.visual_pos.row -= row_off;
        if !reached_view_top {
            decorations.prepare_for_rendering(grapheme.char_idx);
            reached_view_top = true;
        }

        // if the end of the viewport is reached stop rendering
        if grapheme.visual_pos.row as u16 >= renderer.viewport.height + renderer.offset.row as u16 {
            break;
        }

        let visual_row = grapheme.visual_pos.row as u16;

        // Is this row dirty? (None = all rows dirty = full render)
        let row_is_dirty = dirty_rows.is_none_or(|dr| dr.contains(&visual_row));

        // apply decorations before rendering a new line
        if visual_row != last_line_pos.visual_line {
            if last_line_pos.doc_line == grapheme.line_idx {
                last_line_pos.is_last_visual_line = false;
            }

            // we initiate doc_line with usize::MAX because no file
            // can reach that size (memory allocations are limited to isize::MAX)
            // initially there is no "previous" line (so doc_line is set to usize::MAX)
            // in that case we don't need to draw indent guides/virtual text
            if last_line_pos.doc_line != usize::MAX {
                let prev_row_dirty =
                    dirty_rows.is_none_or(|dr| dr.contains(&last_line_pos.visual_line));
                if prev_row_dirty {
                    let line_finalize_start = Instant::now();
                    renderer.draw_indent_guides(last_line_indent_level, last_line_pos.visual_line);
                    decorations.render_virtual_lines(renderer, last_line_pos, last_line_end);
                    line_finalize_us += line_finalize_start.elapsed().as_micros() as u64;
                }
                is_in_indent_area = true;

                // Finalize previous line's entry in the line map
                if let Some(last_entry) = line_map_lines.last_mut() {
                    last_entry.char_range_end = grapheme.char_idx;
                }
            }

            // Start a new line map entry
            line_map_lines.push(VisualLineInfo {
                visual_row,
                doc_line: grapheme.line_idx,
                char_range_start: grapheme.char_idx,
                char_range_end: grapheme.char_idx, // updated when line ends
                visible_char_start: usize::MAX,
                visible_col_start: 0,
                visible_char_last: usize::MAX,
                visible_col_last: 0,
                horizontal_checkpoints: Vec::new(),
            });
            last_line_pos = LinePos {
                first_visual_line: grapheme.line_idx != last_line_pos.doc_line,
                doc_line: grapheme.line_idx,
                visual_line: visual_row,
                is_last_visual_line: true,
            };
            if row_is_dirty {
                decorations.decorate_line(renderer, last_line_pos);
            }

            if !text_fmt.soft_wrap {
                if grapheme.line_idx != anchor_line
                    && renderer.offset.col > 0
                    && text_annotations.is_empty()
                {
                    let current_doc_line = grapheme.line_idx;
                    match formatter
                        .seek_to_visual_col_in_current_line(grapheme, renderer.offset.col)
                    {
                        HorizontalLineSeekResult::Visible(found) => {
                            grapheme = found;
                        }
                        HorizontalLineSeekResult::LineEnded {
                            next_grapheme,
                            next_char_idx,
                            line_end_col,
                        } => {
                            let skip_left_syntax_start = Instant::now();
                            syntax_highlighter.advance_to(next_char_idx);
                            skip_left_syntax_us +=
                                skip_left_syntax_start.elapsed().as_micros() as u64;
                            let skip_left_overlay_start = Instant::now();
                            overlay_highlighter.advance_to(next_char_idx);
                            skip_left_overlay_us +=
                                skip_left_overlay_start.elapsed().as_micros() as u64;
                            let skip_left_decor_start = Instant::now();
                            decorations.fast_forward_to_char(next_char_idx, current_doc_line);
                            skip_left_decor_us +=
                                skip_left_decor_start.elapsed().as_micros() as u64;
                            last_line_end = line_end_col;
                            if let Some(last_entry) = line_map_lines.last_mut() {
                                last_entry.char_range_end = next_char_idx;
                            }
                            if row_is_dirty {
                                dirty_offscreen += 1;
                            }
                            skipped_offscreen_left_lines += 1;
                            pending_grapheme = next_grapheme;
                            continue 'render;
                        }
                    }
                }

                if let Some((seed, seed_source)) = seed
                    .filter(|_| grapheme.line_idx == anchor_line)
                    .map(|seed| (seed, SeedSource::Explicit))
                    .or_else(|| {
                        seed_line_map.and_then(|line_map| {
                            line_map.best_horizontal_checkpoint_within_gap(
                                grapheme.line_idx,
                                renderer.offset.col,
                                HORIZONTAL_CHECKPOINT_STRIDE,
                            )
                        })
                        .map(|checkpoint| RenderSeed {
                            doc_line: grapheme.line_idx,
                            char_idx: checkpoint.char_idx,
                            visual_col: checkpoint.visual_col,
                        })
                        .map(|seed| (seed, SeedSource::LineMap))
                    })
                    .or_else(|| {
                        (renderer.offset.col > 0).then(|| {
                            let seek_start = Instant::now();
                            let result =
                                char_idx_and_visual_offset_at_visual_block_offset_with_kind(
                                    text,
                                    grapheme.char_idx,
                                    0,
                                    renderer.offset.col,
                                    text_fmt,
                                    text_annotations,
                                );
                            helix_view::bench::log_run_phase(
                                "render_document",
                                "seed_seek",
                                seek_start.elapsed(),
                                || {
                                    format!(
                                        concat!(
                                            "line={} anchor_char={} h_offset={} kind={} ",
                                            "plain_seek_support={} result_char={} result_col={} virtual_lines={}"
                                        ),
                                        grapheme.line_idx,
                                        grapheme.char_idx,
                                        renderer.offset.col,
                                        match result.kind {
                                            VisualBlockOffsetSeekKind::PlainFastPath => {
                                                "plain_fast_path"
                                            }
                                            VisualBlockOffsetSeekKind::FormatterFallback => {
                                                "formatter_fallback"
                                            }
                                        },
                                        result.plain_seek_support.as_str(),
                                        result.char_idx,
                                        result.visual_pos.col,
                                        result.virtual_lines,
                                    )
                                },
                            );
                            (
                                RenderSeed {
                                    doc_line: grapheme.line_idx,
                                    char_idx: result.char_idx,
                                    visual_col: result.visual_pos.col,
                                },
                                SeedSource::CurrentSeek,
                            )
                        })
                    })
                    .filter(|(seed, _)| {
                        can_resume_from_seed(
                            text,
                            grapheme.line_idx,
                            grapheme.char_idx,
                            seed.char_idx,
                        )
                    })
                {
                    if matches!(seed_source, SeedSource::CurrentSeek)
                        && seed.visual_col < renderer.offset.col
                        && seed.char_idx > grapheme.char_idx
                    {
                        let current_line = grapheme.line_idx;
                        let next_line_char =
                            formatter.skip_to_next_line().unwrap_or(text.len_chars());
                        syntax_highlighter.advance_to(next_line_char);
                        overlay_highlighter.advance_to(next_line_char);
                        decorations.fast_forward_to_char(next_line_char, current_line);
                        last_line_end = seed.visual_col;
                        if let Some(last_entry) = line_map_lines.last_mut() {
                            last_entry.char_range_end = next_line_char;
                        }
                        if row_is_dirty {
                            dirty_offscreen += 1;
                        }
                        skipped_offscreen_left_lines += 1;
                        continue;
                    }

                    if let Some(last_entry) = line_map_lines.last_mut() {
                        record_horizontal_checkpoint(
                            last_entry,
                            seed.char_idx,
                            seed.visual_col,
                        );
                    }
                    helix_view::bench::log_run_event("resume_seed", || {
                        format!(
                            "line={} source={} current_char={} seed_char={} seed_col={} h_offset={}",
                            grapheme.line_idx,
                            match seed_source {
                                SeedSource::Explicit => "explicit",
                                SeedSource::LineMap => "line_map",
                                SeedSource::CurrentSeek => "current_seek",
                            },
                            grapheme.char_idx,
                            seed.char_idx,
                            seed.visual_col,
                            renderer.offset.col,
                        )
                    });
                    let absolute_row = grapheme.visual_pos.row + row_off;
                    decorations.fast_forward_to_char(seed.char_idx, grapheme.line_idx);
                    syntax_highlighter.advance_to(seed.char_idx);
                    overlay_highlighter.advance_to(seed.char_idx);
                    formatter.reset_to_checkpoint(
                        seed.char_idx,
                        Position::new(absolute_row, seed.visual_col),
                    );
                    continue;
                }
            }
        }

        // acquire the correct grapheme style — always advance highlighters
        // to maintain state, even for clean rows
        let advance_start = Instant::now();
        while grapheme.char_idx >= syntax_highlighter.pos {
            syntax_highlighter.advance();
            syntax_advances += 1;
        }
        while grapheme.char_idx >= overlay_highlighter.pos {
            overlay_highlighter.advance();
            overlay_advances += 1;
        }
        advance_loop_us += advance_start.elapsed().as_micros() as u64;

        let grapheme_width = grapheme.width();
        let visible_left_edge = grapheme.visual_pos.col + grapheme_width > renderer.offset.col;
        let visible_right_edge = grapheme.visual_pos.col < viewport_right;
        if !visible_left_edge {
            skipped_left += 1;
            if !text_fmt.soft_wrap && !grapheme.is_virtual() && grapheme.doc_chars() != 0 {
                if let Some(last_entry) = line_map_lines.last_mut() {
                    let next_target =
                        last_entry
                            .horizontal_checkpoints
                            .last()
                            .map_or(0, |checkpoint| {
                                checkpoint
                                    .visual_col
                                    .saturating_add(HORIZONTAL_CHECKPOINT_STRIDE)
                            });
                    if grapheme.visual_pos.col >= next_target {
                        record_horizontal_checkpoint(
                            last_entry,
                            grapheme.char_idx,
                            grapheme.visual_pos.col,
                        );
                    }
                }
            }
        } else if !visible_right_edge {
            skipped_right += 1;
        }

        if !text_fmt.soft_wrap && !visible_right_edge {
            let skip_right_start = Instant::now();
            let current_line = grapheme.line_idx;
            let next_line_char = formatter.skip_to_next_line().unwrap_or(text.len_chars());
            let skipped_tail = next_line_char.saturating_sub(grapheme.char_idx);
            skipped_right += skipped_tail;
            let skip_right_syntax_start = Instant::now();
            syntax_highlighter.advance_to(next_line_char);
            skip_right_syntax_us += skip_right_syntax_start.elapsed().as_micros() as u64;
            let skip_right_overlay_start = Instant::now();
            overlay_highlighter.advance_to(next_line_char);
            skip_right_overlay_us += skip_right_overlay_start.elapsed().as_micros() as u64;
            let skip_right_decor_start = Instant::now();
            decorations.fast_forward_to_char(next_line_char, current_line);
            skip_right_decor_us += skip_right_decor_start.elapsed().as_micros() as u64;
            last_line_end = viewport_right;
            if let Some(last_entry) = line_map_lines.last_mut() {
                last_entry.char_range_end = next_line_char;
            }
            skip_right_us += skip_right_start.elapsed().as_micros() as u64;
            continue;
        }

        let bookkeeping_start = Instant::now();
        if visible_left_edge && visible_right_edge {
            if let Some(last_entry) = line_map_lines.last_mut() {
                if !grapheme.is_virtual()
                    && grapheme.doc_chars() != 0
                    && last_entry.visible_char_start == usize::MAX
                {
                    last_entry.visible_char_start = grapheme.char_idx;
                    last_entry.visible_col_start = grapheme.visual_pos.col;
                    first_visible_char.get_or_insert(grapheme.char_idx);
                    record_horizontal_checkpoint(
                        last_entry,
                        grapheme.char_idx,
                        grapheme.visual_pos.col,
                    );
                }
                if !grapheme.is_virtual() && grapheme.doc_chars() != 0 {
                    last_entry.visible_char_last = grapheme.char_idx;
                    last_entry.visible_col_last = grapheme.visual_pos.col;
                    last_visible_char = Some(grapheme.char_idx);
                    record_horizontal_checkpoint(
                        last_entry,
                        grapheme.char_idx,
                        grapheme.visual_pos.col,
                    );
                }
            }
        }
        bookkeeping_us += bookkeeping_start.elapsed().as_micros() as u64;

        // Skip cell writes for clean rows
        if !row_is_dirty {
            // Still track end position for line map and visual line end
            last_line_end = grapheme.visual_pos.col + 1;
            // Track last char_idx for line map finalization
            if let Some(last_entry) = line_map_lines.last_mut() {
                last_entry.char_range_end = grapheme.char_idx + 1;
            }
            continue;
        }

        if !visible_left_edge || !visible_right_edge {
            dirty_offscreen += 1;
        }

        let grapheme_style = if let GraphemeSource::VirtualText { highlight } = grapheme.source {
            let mut style = renderer.text_style;
            if let Some(highlight) = highlight {
                style = style.patch(theme.highlight(highlight));
            }
            GraphemeStyle {
                syntax_style: style,
                overlay_style: Style::default(),
            }
        } else {
            GraphemeStyle {
                syntax_style: syntax_highlighter.style,
                overlay_style: overlay_highlighter.style,
            }
        };
        let decoration_start = Instant::now();
        decorations.decorate_grapheme(renderer, &grapheme);
        decoration_us += decoration_start.elapsed().as_micros() as u64;

        let virt = grapheme.is_virtual();
        if virt {
            virtual_drawn += 1;
        }
        let draw_start = Instant::now();
        let grapheme_width = renderer.draw_grapheme(
            &grapheme,
            grapheme_style,
            virt,
            &mut last_line_indent_level,
            &mut is_in_indent_area,
            grapheme.visual_pos,
        );
        draw_us += draw_start.elapsed().as_micros() as u64;
        if grapheme_width == 0 {
            zero_width_drawn += 1;
        }
        drawn += 1;
        first_drawn_char.get_or_insert(grapheme.char_idx);
        last_drawn_char = Some(grapheme.char_idx);
        max_drawn_col = max_drawn_col.max(grapheme.visual_pos.col);
        last_line_end = grapheme.visual_pos.col + grapheme_width;
        // Track char range end for line map
        let bookkeeping_end_start = Instant::now();
        if let Some(last_entry) = line_map_lines.last_mut() {
            last_entry.char_range_end = grapheme.char_idx + 1;
        }
        bookkeeping_us += bookkeeping_end_start.elapsed().as_micros() as u64;
    }

    // char_range_end is continuously updated during iteration
    // (set to grapheme.char_idx when entering a new line, or grapheme.char_idx+1 for each grapheme)

    let last_row_dirty = dirty_rows.is_none_or(|dr| dr.contains(&last_line_pos.visual_line));
    if last_row_dirty {
        let line_finalize_start = Instant::now();
        renderer.draw_indent_guides(last_line_indent_level, last_line_pos.visual_line);
        decorations.render_virtual_lines(renderer, last_line_pos, last_line_end);
        line_finalize_us += line_finalize_start.elapsed().as_micros() as u64;
    }
    let formatter_stats_end = formatter.stats();
    let formatter_stats = DocumentFormatterStats {
        next_calls: formatter_stats_end
            .next_calls
            .saturating_sub(formatter_stats_start.next_calls),
        advance_grapheme_calls: formatter_stats_end
            .advance_grapheme_calls
            .saturating_sub(formatter_stats_start.advance_grapheme_calls),
        inline_annotation_hits: formatter_stats_end
            .inline_annotation_hits
            .saturating_sub(formatter_stats_start.inline_annotation_hits),
        overlay_hits: formatter_stats_end
            .overlay_hits
            .saturating_sub(formatter_stats_start.overlay_hits),
        fold_skip_count: formatter_stats_end
            .fold_skip_count
            .saturating_sub(formatter_stats_start.fold_skip_count),
        folded_chars_skipped: formatter_stats_end
            .folded_chars_skipped
            .saturating_sub(formatter_stats_start.folded_chars_skipped),
        word_refills: formatter_stats_end
            .word_refills
            .saturating_sub(formatter_stats_start.word_refills),
        skip_to_next_line_calls: formatter_stats_end
            .skip_to_next_line_calls
            .saturating_sub(formatter_stats_start.skip_to_next_line_calls),
        yielded_document: formatter_stats_end
            .yielded_document
            .saturating_sub(formatter_stats_start.yielded_document),
        yielded_virtual: formatter_stats_end
            .yielded_virtual
            .saturating_sub(formatter_stats_start.yielded_virtual),
        yielded_newlines: formatter_stats_end
            .yielded_newlines
            .saturating_sub(formatter_stats_start.yielded_newlines),
        yielded_eof: formatter_stats_end
            .yielded_eof
            .saturating_sub(formatter_stats_start.yielded_eof),
    };

    helix_view::bench::log_run_phase("render_document", "main_loop", loop_start.elapsed(), || {
        format!(
            concat!(
                "anchor={} row_off={} soft_wrap={} viewport={}x{} h_offset={} v_offset={}",
                " skipped_before_top={} skipped_left={} skipped_right={}",
                " dirty_offscreen={} drawn={} line_map_rows={}",
                " syntax_advances={} overlay_advances={} virtual_drawn={} zero_width_drawn={}",
                " first_visible_char={:?} last_visible_char={:?}",
                " first_drawn_char={:?} last_drawn_char={:?} max_drawn_col={}",
                " skipped_offscreen_left_lines={}",
                " formatter_next_us={} advance_loop_us={} decoration_us={} draw_us={} bookkeeping_us={} skip_right_us={}",
                " skip_left_syntax_us={} skip_left_overlay_us={} skip_left_decor_us={}",
                " skip_right_syntax_us={} skip_right_overlay_us={} skip_right_decor_us={}",
                " line_finalize_us={}",
                " formatter_next_calls={} formatter_advance_grapheme_calls={} formatter_inline_annotation_hits={}",
                " formatter_overlay_hits={} formatter_fold_skip_count={} formatter_folded_chars_skipped={}",
                " formatter_word_refills={} formatter_skip_to_next_line_calls={}",
                " formatter_yielded_document={} formatter_yielded_virtual={} formatter_yielded_newlines={} formatter_yielded_eof={}"
            ),
            anchor,
            row_off,
            text_fmt.soft_wrap,
            renderer.viewport.width,
            renderer.viewport.height,
            renderer.offset.col,
            renderer.offset.row,
            skipped_before_top,
            skipped_left,
            skipped_right,
            dirty_offscreen,
            drawn,
            line_map_lines.len(),
            syntax_advances,
            overlay_advances,
            virtual_drawn,
            zero_width_drawn,
            first_visible_char,
            last_visible_char,
            first_drawn_char,
            last_drawn_char,
            max_drawn_col,
            skipped_offscreen_left_lines,
            formatter_next_us,
            advance_loop_us,
            decoration_us,
            draw_us,
            bookkeeping_us,
            skip_right_us,
            skip_left_syntax_us,
            skip_left_overlay_us,
            skip_left_decor_us,
            skip_right_syntax_us,
            skip_right_overlay_us,
            skip_right_decor_us,
            line_finalize_us,
            formatter_stats.next_calls,
            formatter_stats.advance_grapheme_calls,
            formatter_stats.inline_annotation_hits,
            formatter_stats.overlay_hits,
            formatter_stats.fold_skip_count,
            formatter_stats.folded_chars_skipped,
            formatter_stats.word_refills,
            formatter_stats.skip_to_next_line_calls,
            formatter_stats.yielded_document,
            formatter_stats.yielded_virtual,
            formatter_stats.yielded_newlines,
            formatter_stats.yielded_eof,
        )
    });

    RenderOutput {
        syntax_styles: syntax_highlighter.take_recorded_styles(),
        line_map: LineMap {
            lines: Arc::from(line_map_lines),
        },
    }
}

#[derive(Debug)]
pub struct TextRenderer<'a> {
    surface: &'a mut Surface,
    pub text_style: Style,
    pub whitespace_style: Style,
    pub indent_guide_char: String,
    pub indent_guide_style: Style,
    pub newline: String,
    pub nbsp: String,
    pub nnbsp: String,
    pub space: String,
    pub tab: String,
    pub virtual_tab: String,
    pub indent_width: u16,
    pub starting_indent: usize,
    pub draw_indent_guides: bool,
    pub viewport: Rect,
    pub offset: Position,
}

pub struct GraphemeStyle {
    syntax_style: Style,
    overlay_style: Style,
}

impl<'a> TextRenderer<'a> {
    pub fn new(
        surface: &'a mut Surface,
        doc: &Document,
        theme: &Theme,
        offset: Position,
        viewport: Rect,
    ) -> TextRenderer<'a> {
        let editor_config = doc.config.load();
        let WhitespaceConfig {
            render: ws_render,
            characters: ws_chars,
        } = &editor_config.whitespace;

        let tab_width = doc.tab_width();
        let tab = if ws_render.tab() == WhitespaceRenderValue::All {
            std::iter::once(ws_chars.tab)
                .chain(std::iter::repeat_n(ws_chars.tabpad, tab_width - 1))
                .collect()
        } else {
            " ".repeat(tab_width)
        };
        let virtual_tab = " ".repeat(tab_width);
        let newline = if ws_render.newline() == WhitespaceRenderValue::All {
            ws_chars.newline.into()
        } else {
            " ".to_owned()
        };

        let space = if ws_render.space() == WhitespaceRenderValue::All {
            ws_chars.space.into()
        } else {
            " ".to_owned()
        };
        let nbsp = if ws_render.nbsp() == WhitespaceRenderValue::All {
            ws_chars.nbsp.into()
        } else {
            " ".to_owned()
        };
        let nnbsp = if ws_render.nnbsp() == WhitespaceRenderValue::All {
            ws_chars.nnbsp.into()
        } else {
            " ".to_owned()
        };

        let text_style = theme.get("ui.text");

        let indent_width = doc.indent_style().indent_width(tab_width) as u16;

        TextRenderer {
            surface,
            indent_guide_char: editor_config.indent_guides.character.into(),
            newline,
            nbsp,
            nnbsp,
            space,
            tab,
            virtual_tab,
            whitespace_style: theme.get("ui.virtual.whitespace"),
            indent_width,
            starting_indent: offset.col / indent_width as usize
                + !offset.col.is_multiple_of(indent_width as usize) as usize
                + editor_config.indent_guides.skip_levels as usize,
            indent_guide_style: text_style.patch(
                theme
                    .try_get("ui.virtual.indent-guide")
                    .unwrap_or_else(|| theme.get("ui.virtual.whitespace")),
            ),
            text_style,
            draw_indent_guides: editor_config.indent_guides.render,
            viewport,
            offset,
        }
    }
    /// Draws a single `grapheme` at the current render position with a specified `style`.
    pub fn draw_decoration_grapheme(
        &mut self,
        grapheme: Grapheme,
        style: Style,
        mut row: u16,
        col: u16,
    ) -> bool {
        if (row as usize) < self.offset.row
            || row >= self.viewport.height
            || col >= self.viewport.width
        {
            return false;
        }
        row -= self.offset.row as u16;

        let grapheme = match grapheme {
            Grapheme::Tab { width } => {
                let grapheme_tab_width = char_to_byte_idx(&self.virtual_tab, width);
                &self.virtual_tab[..grapheme_tab_width]
            }
            Grapheme::Other { ref g } if g == "\u{00A0}" => " ",
            Grapheme::Other { ref g } => g,
            Grapheme::Newline => " ",
        };

        self.surface.set_string(
            self.viewport.x + col,
            self.viewport.y + row,
            grapheme,
            style,
        );
        true
    }

    /// Draws a single `grapheme` at the current render position with a specified `style`.
    pub fn draw_grapheme(
        &mut self,
        grapheme: &FormattedGrapheme,
        grapheme_style: GraphemeStyle,
        is_virtual: bool,
        last_indent_level: &mut usize,
        is_in_indent_area: &mut bool,
        mut position: Position,
    ) -> usize {
        if position.row < self.offset.row {
            return 0;
        }
        position.row -= self.offset.row;
        let cut_off_start = self.offset.col.saturating_sub(position.col);
        let is_whitespace = grapheme.is_whitespace();

        // TODO is it correct to apply the whitespace style to all unicode white spaces?
        let mut style = grapheme_style.syntax_style;
        if is_whitespace {
            style = style.patch(self.whitespace_style);
        }
        style = style.patch(grapheme_style.overlay_style);

        let width = grapheme.width();
        let space = if is_virtual { " " } else { &self.space };
        let nbsp = if is_virtual { " " } else { &self.nbsp };
        let nnbsp = if is_virtual { " " } else { &self.nnbsp };
        let tab = if is_virtual {
            &self.virtual_tab
        } else {
            &self.tab
        };
        let grapheme = match grapheme.raw {
            Grapheme::Tab { width } => {
                let grapheme_tab_width = char_to_byte_idx(tab, width);
                &tab[..grapheme_tab_width]
            }
            // TODO special rendering for other whitespaces?
            Grapheme::Other { ref g } if g == " " && !grapheme.source.is_eof() => space,
            Grapheme::Other { ref g } if g == "\u{00A0}" => nbsp,
            Grapheme::Other { ref g } if g == "\u{202F}" => nnbsp,
            Grapheme::Other { ref g } => g,
            Grapheme::Newline => &self.newline,
        };

        let in_bounds = self.column_in_bounds(position.col, width);

        if in_bounds {
            self.surface.set_string(
                self.viewport.x + (position.col - self.offset.col) as u16,
                self.viewport.y + position.row as u16,
                grapheme,
                style,
            );
        } else if cut_off_start != 0 && cut_off_start < width {
            // partially on screen
            let rect = Rect::new(
                self.viewport.x,
                self.viewport.y + position.row as u16,
                (width - cut_off_start) as u16,
                1,
            );
            self.surface.set_style(rect, style);
        }
        if *is_in_indent_area && !is_whitespace {
            *last_indent_level = position.col;
            *is_in_indent_area = false;
        }

        width
    }

    pub fn column_in_bounds(&self, colum: usize, width: usize) -> bool {
        self.offset.col <= colum && colum + width <= self.offset.col + self.viewport.width as usize
    }

    /// Overlay indentation guides ontop of a rendered line
    /// The indentation level is computed in `draw_lines`.
    /// Therefore this function must always be called afterwards.
    pub fn draw_indent_guides(&mut self, indent_level: usize, mut row: u16) {
        if !self.draw_indent_guides || self.offset.row > row as usize {
            return;
        }
        row -= self.offset.row as u16;

        // Don't draw indent guides outside of view
        let end_indent = min(
            indent_level,
            // Add indent_width - 1 to round up, since the first visible
            // indent might be a bit after offset.col
            self.offset.col + self.viewport.width as usize + (self.indent_width as usize - 1),
        ) / self.indent_width as usize;

        for i in self.starting_indent..end_indent {
            let x = (self.viewport.x as usize + (i * self.indent_width as usize) - self.offset.col)
                as u16;
            let y = self.viewport.y + row;
            debug_assert!(self.surface.in_bounds(x, y));
            self.surface
                .set_string(x, y, &self.indent_guide_char, self.indent_guide_style);
        }
    }

    pub fn set_string(&mut self, x: u16, y: u16, string: impl AsRef<str>, style: Style) {
        if (y as usize) < self.offset.row {
            return;
        }
        self.surface
            .set_string(x, y + self.viewport.y, string, style)
    }

    pub fn set_stringn(
        &mut self,
        x: u16,
        y: u16,
        string: impl AsRef<str>,
        width: usize,
        style: Style,
    ) {
        if (y as usize) < self.offset.row {
            return;
        }
        self.surface
            .set_stringn(x, y + self.viewport.y, string, width, style);
    }

    /// Sets the style of an area **within the text viewport* this accounts
    /// both for the renderers vertical offset and its viewport
    pub fn set_style(&mut self, mut area: Rect, style: Style) {
        area = area.clip_top(self.offset.row as u16);
        area.y += self.viewport.y;
        self.surface.set_style(area, style);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_string_truncated(
        &mut self,
        x: u16,
        y: u16,
        string: &str,
        width: usize,
        style: impl Fn(usize) -> Style, // Map a grapheme's string offset to a style
        ellipsis: bool,
        truncate_start: bool,
    ) -> (u16, u16) {
        if (y as usize) < self.offset.row {
            return (x, y);
        }
        self.surface.set_string_truncated(
            x,
            y + self.viewport.y,
            string,
            width,
            style,
            ellipsis,
            truncate_start,
        )
    }
}

/// Source of syntax highlight data — either a live tree-sitter Highlighter
/// or cached style entries from a previous frame.
enum SyntaxSource<'h> {
    /// Live tree-sitter highlighter. Used on cache miss (content changed).
    Live(Highlighter<'h>),
    /// Cached styles from a previous render. Used on cache hit (only overlays changed).
    Cached {
        entries: &'h [helix_view::view::SyntaxStyleEntry],
        cursor: usize,
    },
}

struct SyntaxHighlighter<'h, 'r, 't> {
    source: Option<SyntaxSource<'h>>,
    text: RopeSlice<'r>,
    /// The character index of the next highlight event, or `usize::MAX` if the highlighter is
    /// finished.
    pos: usize,
    theme: &'t Theme,
    text_style: Style,
    style: Style,
    /// Collects style entries during live highlighting for caching.
    /// Only populated when source is `Live`.
    recorded_styles: Vec<helix_view::view::SyntaxStyleEntry>,
}

impl<'h, 'r, 't> SyntaxHighlighter<'h, 'r, 't> {
    fn new(
        inner: Option<Highlighter<'h>>,
        text: RopeSlice<'r>,
        theme: &'t Theme,
        text_style: Style,
    ) -> Self {
        let source = inner.map(SyntaxSource::Live);
        let mut highlighter = Self {
            source,
            text,
            pos: 0,
            theme,
            style: text_style,
            text_style,
            recorded_styles: Vec::new(),
        };
        highlighter.update_pos();
        highlighter
    }

    fn from_cache(
        entries: &'h [helix_view::view::SyntaxStyleEntry],
        text: RopeSlice<'r>,
        theme: &'t Theme,
        text_style: Style,
    ) -> Self {
        let pos = entries.first().map_or(usize::MAX, |e| e.char_idx);
        Self {
            source: Some(SyntaxSource::Cached { entries, cursor: 0 }),
            text,
            pos,
            theme,
            style: text_style,
            text_style,
            recorded_styles: Vec::new(),
        }
    }

    fn update_pos(&mut self) {
        self.pos = match &self.source {
            Some(SyntaxSource::Live(highlighter)) => {
                let next_byte_idx = highlighter.next_event_offset();
                if next_byte_idx != u32::MAX {
                    self.text
                        .byte_to_char(self.text.ceil_char_boundary(next_byte_idx as usize))
                } else {
                    usize::MAX
                }
            }
            Some(SyntaxSource::Cached { entries, cursor }) => {
                entries.get(*cursor).map_or(usize::MAX, |e| e.char_idx)
            }
            None => usize::MAX,
        };
    }

    fn advance(&mut self) {
        // Save the position of the event being processed (where the style change takes effect).
        let event_pos = self.pos;
        match &mut self.source {
            Some(SyntaxSource::Live(highlighter)) => {
                let (event, highlights) = highlighter.advance();
                let base = match event {
                    HighlightEvent::Refresh => self.text_style,
                    HighlightEvent::Push => self.style,
                };
                self.style = highlights.fold(base, |acc, highlight| {
                    acc.patch(self.theme.highlight(highlight))
                });
            }
            Some(SyntaxSource::Cached { entries, cursor }) => {
                if let Some(entry) = entries.get(*cursor) {
                    self.style = entry.style;
                    *cursor += 1;
                }
            }
            None => {}
        }
        self.update_pos();
        // Record the style at the event position for caching (only meaningful for live source).
        if matches!(&self.source, Some(SyntaxSource::Live(_))) {
            self.recorded_styles
                .push(helix_view::view::SyntaxStyleEntry {
                    char_idx: event_pos,
                    style: self.style,
                });
        }
    }

    /// Take the recorded styles for caching. Only meaningful after a live render pass.
    fn take_recorded_styles(&mut self) -> Vec<helix_view::view::SyntaxStyleEntry> {
        std::mem::take(&mut self.recorded_styles)
    }

    fn advance_to(&mut self, char_idx: usize) {
        while self.pos < char_idx {
            self.advance();
        }
    }
}

struct OverlayHighlighter<'t> {
    inner: syntax::OverlayHighlighter,
    pos: usize,
    theme: &'t Theme,
    style: Style,
}

impl<'t> OverlayHighlighter<'t> {
    fn new(overlays: Vec<OverlayHighlights>, theme: &'t Theme) -> Self {
        let inner = syntax::OverlayHighlighter::new(overlays);
        let mut highlighter = Self {
            inner,
            pos: 0,
            theme,
            style: Style::default(),
        };
        highlighter.update_pos();
        highlighter
    }

    fn update_pos(&mut self) {
        self.pos = self.inner.next_event_offset();
    }

    fn advance(&mut self) {
        let (event, highlights) = self.inner.advance();
        let base = match event {
            HighlightEvent::Refresh => Style::default(),
            HighlightEvent::Push => self.style,
        };

        self.style = highlights.fold(base, |acc, highlight| {
            acc.patch(self.theme.highlight(highlight))
        });
        self.update_pos();
    }

    fn advance_to(&mut self, char_idx: usize) {
        while self.pos < char_idx {
            self.advance();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use helix_core::syntax;
    use helix_view::{editor::Config, graphics::Rect, theme::Theme, view::ViewPosition};
    use tui::buffer::Buffer as Surface;

    use super::*;

    fn test_doc(text: &str) -> Document {
        Document::from(
            helix_core::Rope::from(text),
            None,
            Arc::new(ArcSwap::new(Arc::new(Config::default()))),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        )
    }

    fn giant_two_line_fixture(bytes_per_line: usize) -> String {
        format!(
            "{}\n{}",
            "a".repeat(bytes_per_line),
            "b".repeat(bytes_per_line)
        )
    }

    #[test]
    fn render_document_handles_post_join_giant_lines() {
        let doc = test_doc(&giant_two_line_fixture(32 * 1024));
        let viewport = Rect::new(0, 0, 160, 61);
        let mut surface = Surface::empty(viewport);

        let output = render_document(
            &mut surface,
            viewport,
            &doc,
            ViewPosition::default(),
            &TextAnnotations::default(),
            HighlighterInput::Live(None),
            Vec::new(),
            &Theme::default(),
            DecorationManager::default(),
            None,
            None,
            None,
        );

        assert!(!output.line_map.lines.is_empty());
        assert!(output.line_map.lines.len() <= viewport.height as usize);
    }

    #[test]
    #[ignore = "targeted local repro for post-xJ giant-line render"]
    fn render_document_post_join_giant_line_repro() {
        let event_log_path = std::env::temp_dir().join(format!(
            "helix-render-document-repro-{}.log",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&event_log_path);
        let _run_guard = helix_view::bench::enter_bench_run(helix_view::bench::BenchRunContext {
            seed: 0,
            event_log_path: event_log_path.clone(),
        });
        let doc = test_doc(&giant_two_line_fixture(900_000));
        let viewport = Rect::new(0, 0, 160, 61);
        let mut surface = Surface::empty(viewport);
        let start = std::time::Instant::now();

        let output = render_document(
            &mut surface,
            viewport,
            &doc,
            ViewPosition::default(),
            &TextAnnotations::default(),
            HighlighterInput::Live(None),
            Vec::new(),
            &Theme::default(),
            DecorationManager::default(),
            None,
            None,
            None,
        );

        eprintln!(
            "render_document_post_join_giant_line_repro: elapsed_us={} rows={} mapped_lines={}",
            start.elapsed().as_micros(),
            viewport.height,
            output.line_map.lines.len()
        );
        if let Ok(trace) = std::fs::read_to_string(&event_log_path) {
            eprintln!("{trace}");
        }
        assert!(!output.line_map.lines.is_empty());
    }

    #[test]
    #[ignore = "targeted local repro for giant-line horizontal render states"]
    fn render_document_giant_line_horizontal_offsets_repro() {
        let event_log_path = std::env::temp_dir().join(format!(
            "helix-render-document-horizontal-repro-{}.log",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&event_log_path);
        let _run_guard = helix_view::bench::enter_bench_run(helix_view::bench::BenchRunContext {
            seed: 0,
            event_log_path: event_log_path.clone(),
        });
        let doc = test_doc(&giant_two_line_fixture(900_000));
        let viewport = Rect::new(0, 0, 160, 61);
        let offsets = [
            ViewPosition::default(),
            ViewPosition {
                horizontal_offset: 17,
                ..ViewPosition::default()
            },
            ViewPosition {
                horizontal_offset: 11_101,
                ..ViewPosition::default()
            },
            ViewPosition {
                horizontal_offset: 28_049,
                ..ViewPosition::default()
            },
        ];

        for offset in offsets {
            let mut surface = Surface::empty(viewport);
            let start = std::time::Instant::now();
            let output = render_document(
                &mut surface,
                viewport,
                &doc,
                offset,
                &TextAnnotations::default(),
                HighlighterInput::Live(None),
                Vec::new(),
                &Theme::default(),
                DecorationManager::default(),
                None,
                None,
                None,
            );
            eprintln!(
                "render_document_giant_line_horizontal_offsets_repro: h_offset={} elapsed_us={} mapped_lines={}",
                offset.horizontal_offset,
                start.elapsed().as_micros(),
                output.line_map.lines.len()
            );
        }

        if let Ok(trace) = std::fs::read_to_string(&event_log_path) {
            eprintln!("{trace}");
        }
    }
}
