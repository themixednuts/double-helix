use std::collections::BTreeSet;
use std::ops::Range;

use helix_core::unicode::{segmentation::UnicodeSegmentation, width::UnicodeWidthStr};
use helix_view::graphics::{Rect, Style};
use helix_view::theme::Modifier;
use imara_diff::{Algorithm, Diff, InternedInput};
use tui::text::{Span, Spans};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffOptions {
    pub context: usize,
    pub line_numbers: bool,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            context: 3,
            line_numbers: false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct DiffStyles {
    pub text: Style,
    pub header: Style,
    pub plus: Style,
    pub minus: Style,
    pub delta: Style,
    pub plus_emphasis: Style,
    pub minus_emphasis: Style,
}

impl DiffStyles {
    #[must_use]
    pub fn from_theme(theme: &helix_view::Theme) -> Self {
        let plus = theme.get("diff.plus");
        let minus = theme.get("diff.minus");
        let delta = theme.get("diff.delta");
        Self {
            text: theme.get("ui.text"),
            header: delta.add_modifier(Modifier::DIM),
            plus,
            minus,
            delta,
            plus_emphasis: plus.add_modifier(Modifier::BOLD),
            minus_emphasis: minus.add_modifier(Modifier::BOLD),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffDocument {
    pub hunks: Vec<DiffHunk>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
    pub spans: Vec<DiffSpan>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Add,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSpan {
    pub text: String,
    pub changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffViewState {
    pub max_scroll: u16,
    pub visible_lines: u16,
    pub hunk_boundaries: Vec<Range<u16>>,
}

impl DiffViewState {
    #[must_use]
    pub fn hunk_at(&self, row: u16) -> Option<usize> {
        self.hunk_boundaries
            .iter()
            .position(|range| range.contains(&row))
    }
}

#[must_use]
pub fn diff_text(old: &str, new: &str, options: DiffOptions) -> DiffDocument {
    if old == new {
        return DiffDocument::default();
    }

    let input = InternedInput::new(old, new);
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);

    let old_lines: Vec<String> = input
        .before
        .iter()
        .map(|&token| trim_line(input.interner[token]))
        .collect();
    let new_lines: Vec<String> = input
        .after
        .iter()
        .map(|&token| trim_line(input.interner[token]))
        .collect();

    let mut hunks = Vec::new();
    for hunk in diff.hunks() {
        let old_change = hunk.before.start as usize..hunk.before.end as usize;
        let new_change = hunk.after.start as usize..hunk.after.end as usize;
        let old_start = old_change.start.saturating_sub(options.context);
        let new_start = new_change.start.saturating_sub(options.context);
        let old_end = (old_change.end + options.context).min(old_lines.len());
        let new_end = (new_change.end + options.context).min(new_lines.len());
        let mut lines = Vec::new();

        let leading = old_change
            .start
            .saturating_sub(old_start)
            .min(new_change.start.saturating_sub(new_start));
        for offset in 0..leading {
            lines.push(context_line(
                &old_lines[old_start + offset],
                old_start + offset + 1,
                new_start + offset + 1,
            ));
        }

        let removed = &old_lines[old_change.clone()];
        let added = &new_lines[new_change.clone()];
        let paired = removed.len().min(added.len());
        for index in 0..paired {
            let (old_spans, new_spans) = intraline_spans(&removed[index], &added[index]);
            lines.push(DiffLine {
                kind: DiffLineKind::Remove,
                old_line: Some(old_change.start + index + 1),
                new_line: None,
                spans: old_spans,
            });
            lines.push(DiffLine {
                kind: DiffLineKind::Add,
                old_line: None,
                new_line: Some(new_change.start + index + 1),
                spans: new_spans,
            });
        }
        for (index, line) in removed.iter().enumerate().skip(paired) {
            lines.push(DiffLine {
                kind: DiffLineKind::Remove,
                old_line: Some(old_change.start + index + 1),
                new_line: None,
                spans: vec![DiffSpan::plain(line)],
            });
        }
        for (index, line) in added.iter().enumerate().skip(paired) {
            lines.push(DiffLine {
                kind: DiffLineKind::Add,
                old_line: None,
                new_line: Some(new_change.start + index + 1),
                spans: vec![DiffSpan::plain(line)],
            });
        }

        let trailing = old_end
            .saturating_sub(old_change.end)
            .min(new_end.saturating_sub(new_change.end));
        for offset in 0..trailing {
            lines.push(context_line(
                &old_lines[old_change.end + offset],
                old_change.end + offset + 1,
                new_change.end + offset + 1,
            ));
        }

        hunks.push(DiffHunk {
            old_start: old_start + 1,
            old_len: old_end.saturating_sub(old_start),
            new_start: new_start + 1,
            new_len: new_end.saturating_sub(new_start),
            lines,
        });
    }

    DiffDocument { hunks }
}

impl DiffDocument {
    #[must_use]
    pub fn from_unified_diff(diff: &str) -> Self {
        let mut hunks = Vec::new();
        let mut current: Option<DiffHunk> = None;
        let mut old_line = 0usize;
        let mut new_line = 0usize;
        let mut pending_removed: Vec<(usize, String)> = Vec::new();

        for line in diff.lines() {
            if line.starts_with("--- ") || line.starts_with("+++ ") {
                continue;
            }
            if line.starts_with("@@") {
                flush_removed(&mut pending_removed, &mut current);
                if let Some(hunk) = current.take() {
                    hunks.push(hunk);
                }
                let (old_start, old_len, new_start, new_len) = parse_hunk_header(line);
                old_line = old_start;
                new_line = new_start;
                current = Some(DiffHunk {
                    old_start,
                    old_len,
                    new_start,
                    new_len,
                    lines: Vec::new(),
                });
                continue;
            }

            let Some(hunk) = current.as_mut() else {
                continue;
            };
            match line.as_bytes().first().copied() {
                Some(b' ') => {
                    flush_removed(&mut pending_removed, &mut current);
                    let text = line[1..].to_string();
                    if let Some(hunk) = current.as_mut() {
                        hunk.lines.push(DiffLine {
                            kind: DiffLineKind::Context,
                            old_line: Some(old_line),
                            new_line: Some(new_line),
                            spans: vec![DiffSpan::plain(&text)],
                        });
                    }
                    old_line += 1;
                    new_line += 1;
                }
                Some(b'-') => {
                    pending_removed.push((old_line, line[1..].to_string()));
                    old_line += 1;
                }
                Some(b'+') => {
                    let text = line[1..].to_string();
                    if let Some((removed_line, removed)) = pending_removed.pop() {
                        let (old_spans, new_spans) = intraline_spans(&removed, &text);
                        hunk.lines.push(DiffLine {
                            kind: DiffLineKind::Remove,
                            old_line: Some(removed_line),
                            new_line: None,
                            spans: old_spans,
                        });
                        hunk.lines.push(DiffLine {
                            kind: DiffLineKind::Add,
                            old_line: None,
                            new_line: Some(new_line),
                            spans: new_spans,
                        });
                    } else {
                        hunk.lines.push(DiffLine {
                            kind: DiffLineKind::Add,
                            old_line: None,
                            new_line: Some(new_line),
                            spans: vec![DiffSpan::plain(&text)],
                        });
                    }
                    new_line += 1;
                }
                _ => {}
            }
        }
        flush_removed(&mut pending_removed, &mut current);
        if let Some(hunk) = current {
            hunks.push(hunk);
        }
        DiffDocument { hunks }
    }

    #[must_use]
    pub fn layout_lines(
        &self,
        styles: &DiffStyles,
        options: DiffOptions,
        folded: &BTreeSet<usize>,
    ) -> Vec<Spans<'static>> {
        let old_width = self
            .hunks
            .iter()
            .flat_map(|hunk| hunk.lines.iter().filter_map(|line| line.old_line))
            .max()
            .unwrap_or(0)
            .to_string()
            .len()
            .max(1);
        let new_width = self
            .hunks
            .iter()
            .flat_map(|hunk| hunk.lines.iter().filter_map(|line| line.new_line))
            .max()
            .unwrap_or(0)
            .to_string()
            .len()
            .max(1);
        let mut lines = Vec::new();
        for (index, hunk) in self.hunks.iter().enumerate() {
            lines.push(Spans::from(Span::styled(hunk.header(), styles.header)));
            if folded.contains(&index) {
                lines.push(Spans::from(Span::styled(
                    format!("  {} lines folded", hunk.lines.len()),
                    styles.header,
                )));
                continue;
            }
            for line in &hunk.lines {
                lines.push(line.layout(styles, options.line_numbers, old_width, new_width));
            }
        }
        lines
    }
}

impl DiffHunk {
    #[must_use]
    fn header(&self) -> String {
        format!(
            "@@ -{},{} +{},{} @@",
            self.old_start, self.old_len, self.new_start, self.new_len
        )
    }
}

impl DiffLine {
    fn layout(
        &self,
        styles: &DiffStyles,
        line_numbers: bool,
        old_width: usize,
        new_width: usize,
    ) -> Spans<'static> {
        let (sign, style, changed_style) = match self.kind {
            DiffLineKind::Context => (" ", styles.text, styles.text),
            DiffLineKind::Add => ("+", styles.plus, styles.plus_emphasis),
            DiffLineKind::Remove => ("-", styles.minus, styles.minus_emphasis),
        };
        let mut spans = Vec::new();
        if line_numbers {
            spans.push(Span::styled(
                format!(
                    "{:>old_width$} {:>new_width$} ",
                    self.old_line
                        .map(|line| line.to_string())
                        .unwrap_or_default(),
                    self.new_line
                        .map(|line| line.to_string())
                        .unwrap_or_default()
                ),
                styles.header,
            ));
        }
        spans.push(Span::styled(format!("{sign} "), style));
        spans.extend(self.spans.iter().map(|span| {
            Span::styled(
                span.text.clone(),
                if span.changed { changed_style } else { style },
            )
        }));
        Spans::from(spans)
    }
}

impl DiffSpan {
    fn plain(text: &str) -> Self {
        Self {
            text: text.to_string(),
            changed: false,
        }
    }
}

pub fn diff_view(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    doc: &DiffDocument,
    scroll: u16,
    folded: &BTreeSet<usize>,
    options: DiffOptions,
    styles: &DiffStyles,
) -> DiffViewState {
    if area.width == 0 || area.height == 0 || doc.hunks.is_empty() {
        return DiffViewState::default();
    }

    let lines = doc.layout_lines(styles, options, folded);
    let total = lines.len() as u16;
    let max_scroll = total.saturating_sub(area.height);
    let scroll = scroll.min(max_scroll);
    let start = scroll as usize;
    let end = (start + area.height as usize).min(lines.len());

    for (row, line) in lines[start..end].iter().enumerate() {
        let y = area.y + row as u16;
        let style = row_style(line, styles);
        for x in area.x..area.right() {
            if let Some(cell) = surface.cell_mut((x, y)) {
                cell.set_symbol(" ");
                cell.set_style(tui::ratatui::to_ratatui_style(style));
            }
        }
        surface.set_line(area.x, y, &tui::ratatui::to_ratatui_line(line), area.width);
    }

    let mut hunk_boundaries = Vec::new();
    let mut row = 0u16;
    for (index, hunk) in doc.hunks.iter().enumerate() {
        let height = if folded.contains(&index) {
            2
        } else {
            1 + hunk.lines.len() as u16
        };
        hunk_boundaries.push(row..row + height);
        row += height;
    }

    DiffViewState {
        max_scroll,
        visible_lines: (end - start) as u16,
        hunk_boundaries,
    }
}

fn context_line(text: &str, old_line: usize, new_line: usize) -> DiffLine {
    DiffLine {
        kind: DiffLineKind::Context,
        old_line: Some(old_line),
        new_line: Some(new_line),
        spans: vec![DiffSpan::plain(text)],
    }
}

fn row_style(line: &Spans, styles: &DiffStyles) -> Style {
    let text = line
        .0
        .first()
        .map(|span| span.content.as_ref())
        .unwrap_or_default();
    if text.starts_with("@@") {
        styles.header
    } else if line.0.iter().any(|span| span.content == "+ ") {
        styles.plus
    } else if line.0.iter().any(|span| span.content == "- ") {
        styles.minus
    } else {
        styles.text
    }
}

fn trim_line(line: &str) -> String {
    line.trim_end_matches(['\r', '\n']).to_string()
}

fn parse_hunk_header(header: &str) -> (usize, usize, usize, usize) {
    let mut old = (1, 0);
    let mut new = (1, 0);
    for part in header.split_whitespace() {
        if let Some(value) = part.strip_prefix('-') {
            old = parse_range(value);
        } else if let Some(value) = part.strip_prefix('+') {
            new = parse_range(value);
        }
    }
    (old.0, old.1, new.0, new.1)
}

fn parse_range(value: &str) -> (usize, usize) {
    let mut parts = value.split(',');
    let start = parts.next().and_then(|part| part.parse().ok()).unwrap_or(1);
    let len = parts.next().and_then(|part| part.parse().ok()).unwrap_or(1);
    (start, len)
}

fn flush_removed(pending: &mut Vec<(usize, String)>, hunk: &mut Option<DiffHunk>) {
    let Some(hunk) = hunk.as_mut() else {
        pending.clear();
        return;
    };
    for (line, text) in pending.drain(..) {
        hunk.lines.push(DiffLine {
            kind: DiffLineKind::Remove,
            old_line: Some(line),
            new_line: None,
            spans: vec![DiffSpan::plain(&text)],
        });
    }
}

fn intraline_spans(old: &str, new: &str) -> (Vec<DiffSpan>, Vec<DiffSpan>) {
    const MAX_LINE: usize = 512;
    if old.width() > MAX_LINE || new.width() > MAX_LINE {
        return (vec![DiffSpan::plain(old)], vec![DiffSpan::plain(new)]);
    }

    let old_tokens = token_parts(old);
    let new_tokens = token_parts(new);
    let old_changed = changed_tokens(&old_tokens, &new_tokens);
    let new_changed = changed_tokens(&new_tokens, &old_tokens);
    (
        spans_from_tokens(&old_tokens, &old_changed),
        spans_from_tokens(&new_tokens, &new_changed),
    )
}

fn changed_tokens(left: &[String], right: &[String]) -> Vec<bool> {
    let mut lcs = vec![vec![0u16; right.len() + 1]; left.len() + 1];
    for i in (0..left.len()).rev() {
        for j in (0..right.len()).rev() {
            lcs[i][j] = if left[i] == right[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut changed = vec![true; left.len()];
    let (mut i, mut j) = (0, 0);
    while i < left.len() && j < right.len() {
        if left[i] == right[j] {
            changed[i] = false;
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    changed
}

fn token_parts(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut current_kind = TokenKind::Other;
    for grapheme in text.graphemes(true) {
        let kind = TokenKind::of(grapheme);
        if !current.is_empty() && kind != current_kind {
            parts.push(std::mem::take(&mut current));
        }
        current_kind = kind;
        current.push_str(grapheme);
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

fn spans_from_tokens(tokens: &[String], changed: &[bool]) -> Vec<DiffSpan> {
    let mut spans: Vec<DiffSpan> = Vec::new();
    for (token, changed) in tokens.iter().zip(changed.iter().copied()) {
        if let Some(last) = spans.last_mut().filter(|span| span.changed == changed) {
            last.text.push_str(token);
        } else {
            spans.push(DiffSpan {
                text: token.clone(),
                changed,
            });
        }
    }
    spans
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    Word,
    Space,
    Other,
}

impl TokenKind {
    fn of(text: &str) -> Self {
        if text.chars().all(char::is_whitespace) {
            Self::Space
        } else if text.chars().all(|ch| ch.is_alphanumeric() || ch == '_') {
            Self::Word
        } else {
            Self::Other
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tui::ratatui::{buffer::Buffer as Surface, layout::Rect as SurfaceRect};

    fn styles() -> DiffStyles {
        DiffStyles {
            text: Style::default(),
            header: Style::default().add_modifier(Modifier::DIM),
            plus: Style::default(),
            minus: Style::default(),
            delta: Style::default(),
            plus_emphasis: Style::default().add_modifier(Modifier::BOLD),
            minus_emphasis: Style::default().add_modifier(Modifier::BOLD),
        }
    }

    #[test]
    fn computes_hunks_for_small_fixture() {
        let doc = diff_text("a\nb\nc\n", "a\nx\nc\n", DiffOptions::default());
        assert_eq!(doc.hunks.len(), 1);
        let text = doc.hunks[0]
            .lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("b"));
        assert!(text.contains("x"));
    }

    #[test]
    fn marks_intraline_spans() {
        let (old, new) = intraline_spans("let old_name = 1;", "let new_name = 1;");
        assert!(old
            .iter()
            .any(|span| span.changed && span.text.contains("old")));
        assert!(new
            .iter()
            .any(|span| span.changed && span.text.contains("new")));
    }

    #[test]
    fn fold_state_math_counts_folded_hunks() {
        let doc = diff_text("a\nb\nc\n", "a\nx\nc\n", DiffOptions::default());
        let mut folded = BTreeSet::new();
        folded.insert(0);
        let mut surface = Surface::empty(SurfaceRect::new(0, 0, 40, 4));
        let state = diff_view(
            &mut surface,
            Rect::new(0, 0, 40, 4),
            &doc,
            0,
            &folded,
            DiffOptions::default(),
            &styles(),
        );
        assert_eq!(state.hunk_boundaries[0], 0..2);
    }

    #[test]
    fn empty_diff_renders_nothing() {
        let doc = diff_text("a\n", "a\n", DiffOptions::default());
        let mut surface = Surface::empty(SurfaceRect::new(0, 0, 20, 2));
        let state = diff_view(
            &mut surface,
            Rect::new(0, 0, 20, 2),
            &doc,
            0,
            &BTreeSet::new(),
            DiffOptions::default(),
            &styles(),
        );
        assert_eq!(state.visible_lines, 0);
    }
}
