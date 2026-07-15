use crate::Document;
use crate::ViewId;
use helix_core::doc_formatter::{FormattedGrapheme, TextFormat};
use helix_core::text_annotations::{LineAnnotation, PlainViewportSupport};
use helix_core::{softwrapped_dimensions, Position};
use std::collections::BTreeMap;
use std::sync::Arc;

pub struct PluginLineAnnotations {
    text: helix_core::Rope,
    annotations: Arc<[crate::document::PluginAnnotation]>,
    tab_width: u16,
    width: u16,
}

impl PluginLineAnnotations {
    pub fn new(doc: &Document, view_id: ViewId, width: u16) -> Self {
        Self {
            text: doc.text().clone(),
            annotations: doc
                .visual_annotations(view_id)
                .unwrap_or_else(|| Arc::from([])),
            tab_width: doc.tab_width() as u16,
            width,
        }
    }

    fn plain_viewport_support(&self, top_line: usize, cursor_line: usize) -> PlainViewportSupport {
        if self.annotations.is_empty() {
            return PlainViewportSupport::Supported;
        }

        plugin_plain_viewport_support(&self.annotations, &self.text, top_line, cursor_line)
    }
}

fn plugin_plain_viewport_support(
    annotations: &[crate::document::PluginAnnotation],
    text: &helix_core::Rope,
    top_line: usize,
    cursor_line: usize,
) -> PlainViewportSupport {
    let start_line = top_line.min(cursor_line);
    let end_line = top_line.max(cursor_line);
    let text_len = text.len_chars();

    if annotations.iter().any(|annotation| {
        annotation.is_line && {
            let line = text.char_to_line(annotation.char_idx.min(text_len));
            // Virtual rows appended to the cursor's own line do not change the cursor's
            // vertical position; only earlier lines in the traversed span matter.
            line >= start_line && line <= end_line && line != cursor_line
        }
    }) {
        PlainViewportSupport::PluginAnnotations
    } else {
        PlainViewportSupport::Supported
    }
}

impl LineAnnotation for PluginLineAnnotations {
    fn plain_viewport_support(&self, top_line: usize, cursor_line: usize) -> PlainViewportSupport {
        self.plain_viewport_support(top_line, cursor_line)
    }

    fn reset_pos(&mut self, _char_idx: usize) -> usize {
        usize::MAX
    }

    fn skip_concealed_anchors(&mut self, _conceal_end_char_idx: usize) -> usize {
        usize::MAX
    }

    fn process_anchor(&mut self, _grapheme: &FormattedGrapheme) -> usize {
        usize::MAX
    }
    fn insert_virtual_lines(
        &mut self,
        _line_end_char_idx: usize,
        line_end_visual_pos: Position,
        doc_line: usize,
    ) -> Position {
        let mut inline_extra_rows: u16 = 0;
        let mut virt_annots_by_row: BTreeMap<u16, Vec<_>> = BTreeMap::new();
        let mut max_virt_idx: i32 = -1;
        let mut next_auto_idx: u16 = 0;

        if !self.annotations.is_empty() {
            let line_start = self.text.line_to_char(doc_line);
            let line_end = self.text.line_to_char(doc_line + 1);

            let line_annots: Vec<_> = self
                .annotations
                .iter()
                .filter(|a| a.char_idx >= line_start && a.char_idx < line_end)
                .collect();

            // Collect inline annotations
            let inline_annots: Vec<_> = line_annots.iter().filter(|a| !a.is_line).collect();

            // First pass: determine if ANY inline annotation needs to drop
            // If any needs to drop, ALL should drop together for consistent rendering
            // Calculate total width by summing all annotation character counts
            let mut should_drop_all = false;
            if let Some(first_annot) = inline_annots.first() {
                let start_col = line_end_visual_pos.col as u16 + first_annot.offset;
                let available_width = self.width.saturating_sub(start_col);
                // Drop if less than 40 columns available for the first annotation
                if available_width < 40 {
                    should_drop_all = true;
                }
            }
            // Check if all annotations fit - use conservative estimate for visual width
            if !should_drop_all {
                let total_chars: u16 = inline_annots
                    .iter()
                    .map(|a| a.text.chars().count() as u16)
                    .sum();
                let start_col = line_end_visual_pos.col as u16
                    + inline_annots.first().map(|a| a.offset).unwrap_or(0);
                // Conservative estimate: some chars may be 2 columns wide
                let estimated_end_col = start_col + total_chars + total_chars / 4;
                // If estimated total extends beyond viewport, drop all
                if estimated_end_col > self.width {
                    should_drop_all = true;
                }
            }

            // Second pass: calculate height for all inline annotations
            // Track where the last annotation ended so we can position the next one correctly
            let mut current_col: u16 = if should_drop_all {
                // For dropped: start from the first annotation's offset
                inline_annots.first().map(|a| a.offset).unwrap_or(0)
            } else {
                // For inline: start after line content + first annotation's offset
                line_end_visual_pos.col as u16
                    + inline_annots.first().map(|a| a.offset).unwrap_or(0)
            };

            for annot in &inline_annots {
                let dropped = should_drop_all;
                let start_col = current_col;
                let available_width = self.width.saturating_sub(start_col);

                if available_width > 0 {
                    let text_fmt = TextFormat {
                        soft_wrap: true,
                        tab_width: self.tab_width,
                        max_wrap: available_width.saturating_div(4).max(20),
                        max_indent_retain: 0,
                        wrap_indicator_highlight: None,
                        viewport_width: available_width,
                        soft_wrap_at_text_width: true,
                    };
                    let (height, last_line_width) =
                        softwrapped_dimensions(annot.text.as_str().into(), &text_fmt);

                    // Update current_col based on where this annotation visually ends
                    if height == 1 {
                        // Single row: next annotation starts after this one's visual width
                        current_col = start_col + last_line_width;
                    } else {
                        // Multi-row: use the text length
                        current_col = start_col + annot.text.chars().count() as u16;
                    }

                    // Track max height among all inline annotations (they share same row)
                    if dropped {
                        // Dropped: all annotations share virtual line, track max wrapped height
                        inline_extra_rows = inline_extra_rows.max(height as u16);
                    } else {
                        // Non-dropped: row 0 on code line, wrapped rows on virtual lines
                        inline_extra_rows = inline_extra_rows.max(height as u16 - 1);
                    }
                }
            }

            // 2. Group virtual line annotations
            let virt_annots: Vec<_> = line_annots.iter().filter(|a| a.is_line).collect();
            for annot in &virt_annots {
                if let Some(idx) = annot.virt_line_idx {
                    max_virt_idx = max_virt_idx.max(idx as i32);
                }
            }

            for annot in &virt_annots {
                let row_idx = if let Some(idx) = annot.virt_line_idx {
                    idx
                } else {
                    let idx = (max_virt_idx + 1) as u16 + next_auto_idx;
                    next_auto_idx += 1;
                    idx
                };
                virt_annots_by_row.entry(row_idx).or_default().push(annot);
            }

            // 3. Calculate total height
            let mut cumulative_row_offset: u16 = inline_extra_rows;
            for (_logical_row, annots_in_row) in virt_annots_by_row {
                let mut max_height_in_row: u16 = 0;
                for annot in annots_in_row {
                    let text_col = annot.offset;
                    let available_width = self.width.saturating_sub(text_col);
                    if available_width > 0 {
                        let text_fmt = TextFormat {
                            soft_wrap: true,
                            tab_width: self.tab_width,
                            max_wrap: available_width.saturating_div(4).max(20),
                            max_indent_retain: 0,
                            wrap_indicator_highlight: None,
                            viewport_width: available_width,
                            soft_wrap_at_text_width: true,
                        };
                        let height =
                            softwrapped_dimensions(annot.text.as_str().into(), &text_fmt).0;
                        max_height_in_row = max_height_in_row.max(height as u16);
                    }
                }
                cumulative_row_offset += max_height_in_row;
            }

            return Position::new(cumulative_row_offset as usize, 0);
        }

        Position::new(0, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::plugin_plain_viewport_support;
    use crate::{document::PluginAnnotation, Document};
    use arc_swap::ArcSwap;
    use helix_core::text_annotations::PlainViewportSupport;
    use helix_core::{syntax, Rope};
    use std::sync::Arc;

    fn test_doc(text: &str) -> Document {
        Document::from(
            Rope::from(text),
            None,
            Arc::new(ArcSwap::new(Arc::new(crate::editor::Config::default()))),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        )
    }

    fn line_annotation(doc: &Document, line: usize) -> PluginAnnotation {
        PluginAnnotation {
            char_idx: doc.text().line_to_char(line),
            text: "virt".into(),
            style: None,
            fg: None,
            bg: None,
            offset: 0,
            is_line: true,
            virt_line_idx: Some(0),
            dropped_text: None,
        }
    }

    #[test]
    fn plugin_annotations_on_cursor_line_keep_plain_viewport_support() {
        let doc = test_doc("zero\none\ntwo\n");
        let annotations = vec![line_annotation(&doc, 2)];

        assert_eq!(
            plugin_plain_viewport_support(&annotations, doc.text(), 0, 2),
            PlainViewportSupport::Supported
        );
    }

    #[test]
    fn plugin_annotations_before_cursor_line_block_plain_viewport_support() {
        let doc = test_doc("zero\none\ntwo\n");
        let annotations = vec![line_annotation(&doc, 1)];

        assert_eq!(
            plugin_plain_viewport_support(&annotations, doc.text(), 0, 2),
            PlainViewportSupport::PluginAnnotations
        );
    }
}
