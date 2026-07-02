use std::{borrow::Cow, fmt::Write, path::Path};

use helix_core::syntax::config::LanguageServerFeature;
use helix_core::text_folding::Fold;

use crate::{
    editor::{Breakpoint, GutterType, LineNumber},
    graphics::{Style, UnderlineStyle},
    icons::ICONS,
    Document, Theme, View,
};

fn count_digits(n: usize) -> usize {
    (usize::checked_ilog10(n).unwrap_or(0) + 1) as usize
}

pub type GutterFn<'doc> = Box<dyn FnMut(usize, bool, bool, &mut String) -> Option<Style> + 'doc>;
pub type Gutter = for<'doc> fn(
    &'doc GutterContext<'doc>,
    &'doc Document,
    &View,
    &Theme,
    bool,
    usize,
) -> GutterFn<'doc>;

#[derive(Clone, Copy)]
pub struct DebugExecutionPosition<'a> {
    pub line: usize,
    pub path: Option<&'a Path>,
}

pub struct GutterContext<'a> {
    pub mode: crate::document::Mode,
    pub line_number: LineNumber,
    pub wrap_indicator: Cow<'a, str>,
    pub breakpoints: Option<&'a [Breakpoint]>,
    pub debug_execution: Option<DebugExecutionPosition<'a>>,
}

impl GutterType {
    pub fn style<'doc>(
        self,
        context: &'doc GutterContext<'doc>,
        doc: &'doc Document,
        view: &View,
        theme: &Theme,
        is_focused: bool,
    ) -> GutterFn<'doc> {
        match self {
            GutterType::Diagnostics => {
                diagnostics_or_breakpoints(context, doc, view, theme, is_focused)
            }
            GutterType::LineNumbers => line_numbers(context, doc, view, theme, is_focused),
            GutterType::Spacer => padding(context, doc, view, theme, is_focused),
            GutterType::Diff => diff(context, doc, view, theme, is_focused),
        }
    }

    pub fn width(self, view: &View, doc: &Document) -> usize {
        match self {
            GutterType::Diagnostics => 1,
            GutterType::LineNumbers => line_numbers_width(view, doc),
            GutterType::Spacer => 1,
            GutterType::Diff => 1,
        }
    }
}

pub fn diagnostic<'doc>(
    doc: &'doc Document,
    _view: &View,
    theme: &Theme,
    _is_focused: bool,
) -> GutterFn<'doc> {
    let warning = theme.get("warning");
    let error = theme.get("error");
    let info = theme.get("info");
    let hint = theme.get("hint");
    let diagnostics = doc.diagnostics();

    Box::new(
        move |line: usize, _selected: bool, first_visual_line: bool, out: &mut String| {
            if !first_visual_line {
                return None;
            }
            use helix_core::diagnostic::Severity;
            let first_diag_idx_maybe_on_line = diagnostics.partition_point(|d| d.line < line);
            let diagnostics_on_line = diagnostics[first_diag_idx_maybe_on_line..]
                .iter()
                .take_while(|d| {
                    d.line == line
                        && d.provider.language_server_id().is_none_or(|id| {
                            doc.language_servers_with_feature(LanguageServerFeature::Diagnostics)
                                .any(|ls| ls.id() == id)
                        })
                });

            diagnostics_on_line
                .max_by_key(|d| d.severity)
                .map(move |d| {
                    let icons = ICONS.load();
                    let (style, symbol) = match d.severity {
                        Some(Severity::Error) => (error, icons.diagnostic().error()),
                        Some(Severity::Warning) | None => (warning, icons.diagnostic().warning()),
                        Some(Severity::Info) => (info, icons.diagnostic().info()),
                        Some(Severity::Hint) => (hint, icons.diagnostic().hint()),
                    };
                    out.push_str(symbol);
                    style
                })
        },
    )
}

pub fn diff<'doc>(
    _context: &'doc GutterContext<'doc>,
    doc: &'doc Document,
    _view: &View,
    theme: &Theme,
    _is_focused: bool,
) -> GutterFn<'doc> {
    // Fall back to the non-gutter diff scopes so themes that define
    // `diff.plus` / `diff.minus` / `diff.delta` (for inline diff
    // markers) get visible gutter markers automatically without
    // needing to define a `.gutter` variant explicitly. Without
    // this fallback, themes that style only the inline diffs leave
    // gutter markers colorless and indistinguishable from the
    // rest of the gutter.
    let added = theme
        .try_get("diff.plus.gutter")
        .or_else(|| theme.try_get("diff.plus"))
        .unwrap_or_default();
    let deleted = theme
        .try_get("diff.minus.gutter")
        .or_else(|| theme.try_get("diff.minus"))
        .unwrap_or_default();
    let modified = theme
        .try_get("diff.delta.gutter")
        .or_else(|| theme.try_get("diff.delta"))
        .unwrap_or_default();
    if let Some(diff_handle) = doc.diff_handle() {
        let hunks = diff_handle.load();
        let mut hunk_i = 0;
        let mut hunk = hunks.nth_hunk(hunk_i);
        Box::new(
            move |line: usize, _selected: bool, first_visual_line: bool, out: &mut String| {
                // truncating the line is fine here because we don't compute diffs
                // for files with more lines than i32::MAX anyways
                // we need to special case removals here
                // these technically do not have a range of lines to highlight (`hunk.after.start == hunk.after.end`).
                // However we still want to display these hunks correctly we must not yet skip to the next hunk here
                while hunk.after.end < line as u32
                    || !hunk.is_pure_removal() && line as u32 == hunk.after.end
                {
                    hunk_i += 1;
                    hunk = hunks.nth_hunk(hunk_i);
                }

                if hunk.after.start > line as u32 {
                    return None;
                }

                let icons = ICONS.load();

                let (icon, style) = if hunk.is_pure_insertion() {
                    (icons.gutter().added(), added)
                } else if hunk.is_pure_removal() {
                    if !first_visual_line {
                        return None;
                    }
                    (icons.gutter().deleted(), deleted)
                } else {
                    (icons.gutter().modified(), modified)
                };

                write!(out, "{}", icon).unwrap();
                Some(style)
            },
        )
    } else {
        Box::new(move |_, _, _, _| None)
    }
}

pub fn line_numbers<'doc>(
    context: &'doc GutterContext<'doc>,
    doc: &'doc Document,
    view: &View,
    theme: &Theme,
    is_focused: bool,
) -> GutterFn<'doc> {
    let text = doc.text().slice(..);
    let width = line_numbers_width(view, doc);
    let annotations = view.text_annotations(doc, None);

    let last_line_in_view = view.estimate_last_doc_line(&annotations, doc);

    // Whether to draw the line number for the last line of the
    // document or not.  We only draw it if it's not an empty line.
    let draw_last = text.line_to_byte(last_line_in_view) < text.len_bytes();

    let linenr = theme.get("ui.linenr");
    let linenr_select = theme.get("ui.linenr.selected");

    let current_line = doc
        .text()
        .char_to_line(doc.selection(view.id).primary().cursor(text));

    let line_number = context.line_number;
    let wrap_indicator = context
        .wrap_indicator
        .chars()
        .take(width)
        .collect::<String>();
    let mode = context.mode;

    // folded lines between `line` and `current_line`
    let mut folded_lines = None;
    let folded_lines_of = |fold: Fold| fold.end.line - fold.start.line + 1;

    Box::new(
        move |line: usize, selected: bool, first_visual_line: bool, out: &mut String| {
            let fold_annotations = &annotations.folds;

            if line == last_line_in_view && !draw_last {
                write!(out, "{:>1$}", '~', width).unwrap();
                Some(linenr)
            } else {
                use crate::{document::Mode, editor::LineNumber};
                use std::cmp::Ordering::*;
                use std::cmp::{max, min};

                // calculate folded lines
                match current_line.cmp(&line) {
                    Greater if folded_lines.is_none() => {
                        fold_annotations.reset_pos(line, |fold| fold.start.line);
                        let line_range = min(current_line, line)..=max(current_line, line);
                        folded_lines = Some(fold_annotations.folded_lines_between(&line_range));
                    }
                    Greater => match folded_lines {
                        Some(n) if n > 0 => {
                            if let Some(fold) =
                                fold_annotations.consume_next(line, |fold| fold.end.line + 1)
                            {
                                folded_lines = Some(n - folded_lines_of(fold))
                            }
                        }
                        _ => (),
                    },
                    Equal => {
                        if folded_lines.is_none() {
                            fold_annotations.reset_pos(line, |fold| fold.start.line);
                        }
                        fold_annotations.consume_next(line, |fold| fold.end.line + 1);
                        folded_lines = Some(0);
                    }
                    Less => {
                        if let Some(fold) =
                            fold_annotations.consume_next(line, |fold| fold.end.line + 1)
                        {
                            folded_lines = Some(folded_lines.unwrap() + folded_lines_of(fold));
                        }
                    }
                }

                let relative = line_number == LineNumber::Relative
                    && mode != Mode::Insert
                    && is_focused
                    && current_line != line;

                let display_num = if relative {
                    match current_line
                        .abs_diff(line)
                        .checked_sub(folded_lines.unwrap_or(0))
                    {
                        Some(r) => r,
                        None => {
                            let msg = format!(
                                "`folded_lines` can not exceed the abs between `current_line` and `line`\n\
                                \tcurrent_line = {current_line}; \
                                \tline = {line}; \
                                \tfolded_lines = {folded_lines:?}"
                            );
                            if cfg!(debug_assertions) {
                                panic!("{msg}");
                            } else {
                                log::error!("{msg}");
                                current_line.abs_diff(line)
                            }
                        }
                    }
                } else {
                    line + 1
                };

                let style = if selected && is_focused {
                    linenr_select
                } else {
                    linenr
                };

                if first_visual_line {
                    write!(out, "{:>1$}", display_num, width).unwrap();
                } else {
                    write!(out, "{:>1$}", wrap_indicator, width).unwrap();
                }

                Some(style)
            }
        },
    )
}

/// The width of a "line-numbers" gutter
///
/// The width of the gutter depends on the number of lines in the document,
/// whether there is content on the last line (the `~` line), and the
/// `editor.gutters.line-numbers.min-width` settings.
fn line_numbers_width(view: &View, doc: &Document) -> usize {
    let text = doc.text();
    let last_line = text.len_lines().saturating_sub(1);
    let draw_last = text.line_to_byte(last_line) < text.len_bytes();
    let last_drawn = if draw_last { last_line + 1 } else { last_line };
    let digits = count_digits(last_drawn);
    let n_min = view.gutters.line_numbers.min_width;
    digits.max(n_min)
}

pub fn padding<'doc>(
    _context: &'doc GutterContext<'doc>,
    _doc: &'doc Document,
    _view: &View,
    _theme: &Theme,
    _is_focused: bool,
) -> GutterFn<'doc> {
    Box::new(|_line: usize, _selected: bool, _first_visual_line: bool, _out: &mut String| None)
}

pub fn breakpoints<'doc>(
    context: &'doc GutterContext<'doc>,
    doc: &'doc Document,
    _view: &View,
    theme: &Theme,
    _is_focused: bool,
) -> GutterFn<'doc> {
    let error = theme.get("error");
    let info = theme.get("info");
    let breakpoint_style = theme.get("ui.debug.breakpoint");

    let breakpoints = context.breakpoints.filter(|_| doc.path().is_some());

    let breakpoints = match breakpoints {
        Some(breakpoints) => breakpoints,
        None => return Box::new(move |_, _, _, _| None),
    };

    Box::new(
        move |line: usize, _selected: bool, first_visual_line: bool, out: &mut String| {
            if !first_visual_line {
                return None;
            }
            let breakpoint = breakpoints
                .iter()
                .find(|breakpoint| breakpoint.line == line)?;

            let style = if breakpoint.condition.is_some() && breakpoint.log_message.is_some() {
                error.underline_style(UnderlineStyle::Line)
            } else if breakpoint.condition.is_some() {
                error
            } else if breakpoint.log_message.is_some() {
                info
            } else {
                breakpoint_style
            };

            let icons = ICONS.load();

            let sym = if breakpoint.verified {
                icons.dap().verified()
            } else {
                icons.dap().unverified()
            };
            write!(out, "{}", sym).unwrap();
            Some(style)
        },
    )
}

fn execution_pause_indicator<'doc>(
    context: &'doc GutterContext<'doc>,
    doc: &'doc Document,
    theme: &Theme,
    is_focused: bool,
) -> GutterFn<'doc> {
    let style = theme.get("ui.debug.active");
    let frame_line = context.debug_execution.map(|position| position.line);
    let frame_source_path = context.debug_execution.and_then(|position| position.path);
    let should_display_for_current_doc =
        doc.path().is_some() && frame_source_path == doc.path().map(|path| path.as_path());

    Box::new(
        move |line: usize, _selected: bool, first_visual_line: bool, out: &mut String| {
            if !first_visual_line
                || !is_focused
                || line != frame_line?
                || !should_display_for_current_doc
            {
                return None;
            }

            let sym = "▶";
            write!(out, "{}", sym).unwrap();
            Some(style)
        },
    )
}

pub fn diagnostics_or_breakpoints<'doc>(
    context: &'doc GutterContext<'doc>,
    doc: &'doc Document,
    view: &View,
    theme: &Theme,
    is_focused: bool,
) -> GutterFn<'doc> {
    let mut diagnostics = diagnostic(doc, view, theme, is_focused);
    let mut breakpoints = breakpoints(context, doc, view, theme, is_focused);
    let mut execution_pause_indicator = execution_pause_indicator(context, doc, theme, is_focused);

    Box::new(move |line, selected, first_visual_line: bool, out| {
        execution_pause_indicator(line, selected, first_visual_line, out)
            .or_else(|| breakpoints(line, selected, first_visual_line, out))
            .or_else(|| diagnostics(line, selected, first_visual_line, out))
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::document::Document;
    use crate::editor::{Config, GutterConfig, GutterLineNumbersConfig};
    use crate::graphics::Rect;
    use crate::DocumentId;
    use arc_swap::ArcSwap;
    use helix_core::{syntax, Rope};

    #[test]
    fn test_default_gutter_widths() {
        let mut view = View::new(DocumentId::default(), GutterConfig::default());
        view.area = Rect::new(40, 40, 40, 40);

        let rope = Rope::from_str("abc\n\tdef");
        let doc = Document::from(
            rope,
            None,
            Arc::new(ArcSwap::new(Arc::new(Config::default()))),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        );

        assert_eq!(view.gutters.layout.len(), 5);
        assert_eq!(view.gutters.layout[0].width(&view, &doc), 1);
        assert_eq!(view.gutters.layout[1].width(&view, &doc), 1);
        assert_eq!(view.gutters.layout[2].width(&view, &doc), 3);
        assert_eq!(view.gutters.layout[3].width(&view, &doc), 1);
        assert_eq!(view.gutters.layout[4].width(&view, &doc), 1);
    }

    #[test]
    fn test_configured_gutter_widths() {
        let gutters = GutterConfig {
            layout: vec![GutterType::Diagnostics],
            ..Default::default()
        };

        let mut view = View::new(DocumentId::default(), gutters);
        view.area = Rect::new(40, 40, 40, 40);

        let rope = Rope::from_str("abc\n\tdef");
        let doc = Document::from(
            rope,
            None,
            Arc::new(ArcSwap::new(Arc::new(Config::default()))),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        );

        assert_eq!(view.gutters.layout.len(), 1);
        assert_eq!(view.gutters.layout[0].width(&view, &doc), 1);

        let gutters = GutterConfig {
            layout: vec![GutterType::Diagnostics, GutterType::LineNumbers],
            line_numbers: GutterLineNumbersConfig { min_width: 10 },
        };

        let mut view = View::new(DocumentId::default(), gutters);
        view.area = Rect::new(40, 40, 40, 40);

        let rope = Rope::from_str("abc\n\tdef");
        let doc = Document::from(
            rope,
            None,
            Arc::new(ArcSwap::new(Arc::new(Config::default()))),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        );

        assert_eq!(view.gutters.layout.len(), 2);
        assert_eq!(view.gutters.layout[0].width(&view, &doc), 1);
        assert_eq!(view.gutters.layout[1].width(&view, &doc), 10);
    }

    #[test]
    fn test_line_numbers_gutter_width_resizes() {
        let gutters = GutterConfig {
            layout: vec![GutterType::Diagnostics, GutterType::LineNumbers],
            line_numbers: GutterLineNumbersConfig { min_width: 1 },
        };

        let mut view = View::new(DocumentId::default(), gutters);
        view.area = Rect::new(40, 40, 40, 40);

        let rope = Rope::from_str("a\nb");
        let doc_short = Document::from(
            rope,
            None,
            Arc::new(ArcSwap::new(Arc::new(Config::default()))),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        );

        let rope = Rope::from_str("a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np");
        let doc_long = Document::from(
            rope,
            None,
            Arc::new(ArcSwap::new(Arc::new(Config::default()))),
            Arc::new(ArcSwap::from_pointee(syntax::Loader::default())),
        );

        assert_eq!(view.gutters.layout.len(), 2);
        assert_eq!(view.gutters.layout[1].width(&view, &doc_short), 1);
        assert_eq!(view.gutters.layout[1].width(&view, &doc_long), 2);
    }
}
