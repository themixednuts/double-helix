use crate::compositor::{Component, RenderContext};
use arc_swap::ArcSwap;
use tui::text::{Span, Spans, Text};

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use pulldown_cmark::{
    Alignment, CodeBlockKind, Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd,
};

use helix_core::{
    syntax::{self, HighlightEvent, OverlayHighlights},
    RopeSlice, Syntax,
};
use helix_view::{
    graphics::{Margin, Rect, Style},
    theme::Modifier,
    Theme,
};

use super::text_layout::{self, Align, TruncateAt};

fn styled_multiline_text<'a>(text: &str, style: Style) -> Text<'a> {
    let spans: Vec<_> = text
        .lines()
        .map(|line| Span::styled(line.to_string(), style))
        .map(Spans::from)
        .collect();
    Text::from(spans)
}

pub fn highlighted_code_block<'a>(
    text: &str,
    language: &str,
    theme: Option<&Theme>,
    loader: &syntax::Loader,
    // Optional overlay highlights to mix in with the syntax highlights.
    //
    // Note that `OverlayHighlights` is typically used with char indexing but the only caller
    // which passes this parameter currently passes **byte indices** instead.
    additional_highlight_spans: Option<OverlayHighlights>,
) -> Text<'a> {
    let mut spans = Vec::new();
    let mut lines = Vec::new();

    let get_theme = |key: &str| -> Style { theme.map(|t| t.get(key)).unwrap_or_default() };
    let text_style = get_theme(Markdown::TEXT_STYLE);
    let code_style = get_theme(Markdown::BLOCK_STYLE);

    let theme = match theme {
        Some(t) => t,
        None => return styled_multiline_text(text, code_style),
    };

    let ropeslice = RopeSlice::from(text);
    let Some(syntax) = loader
        .language_for_match(RopeSlice::from(language))
        .and_then(|lang| Syntax::new(ropeslice, lang, loader).ok())
    else {
        return styled_multiline_text(text, code_style);
    };

    let mut syntax_highlighter = syntax.highlighter(ropeslice, loader, ..);
    let mut syntax_highlight_stack = Vec::new();
    let mut overlay_highlight_stack = Vec::new();
    let mut overlay_highlighter = syntax::OverlayHighlighter::new(additional_highlight_spans);
    let mut pos = 0;

    while pos < ropeslice.len_bytes() as u32 {
        if pos == syntax_highlighter.next_event_offset() {
            let (event, new_highlights) = syntax_highlighter.advance();
            if event == HighlightEvent::Refresh {
                syntax_highlight_stack.clear();
            }
            syntax_highlight_stack.extend(new_highlights);
        } else if pos == overlay_highlighter.next_event_offset() as u32 {
            let (event, new_highlights) = overlay_highlighter.advance();
            if event == HighlightEvent::Refresh {
                overlay_highlight_stack.clear();
            }
            overlay_highlight_stack.extend(new_highlights)
        }

        let start = pos;
        pos = syntax_highlighter
            .next_event_offset()
            .min(overlay_highlighter.next_event_offset() as u32);
        if pos == u32::MAX {
            pos = ropeslice.len_bytes() as u32;
        }
        if pos == start {
            continue;
        }
        debug_assert!(pos > start);
        if pos < start {
            log::error!("Failed to highlight '{language}': {text:?}");
            return styled_multiline_text(text, code_style);
        }

        let style = syntax_highlight_stack
            .iter()
            .chain(overlay_highlight_stack.iter())
            .fold(text_style, |acc, highlight| {
                acc.patch(theme.highlight(*highlight))
            });

        let mut slice = &text[start as usize..pos as usize];
        while let Some(end) = slice.find('\n') {
            let text = &slice[..end];
            let text = text.replace('\t', "    ");
            let span = Span::styled(text, style);
            spans.push(span);

            slice = &slice[end + 1..];

            let spans = std::mem::take(&mut spans);
            lines.push(Spans::from(spans));
        }

        if !slice.is_empty() {
            let span = Span::styled(slice.replace('\t', "    "), style);
            spans.push(span);
        }
    }

    if !spans.is_empty() {
        let spans = std::mem::take(&mut spans);
        lines.push(Spans::from(spans));
    }

    Text::from(lines)
}

#[derive(Clone)]
pub struct MarkdownLineStyles {
    pub heading: Style,
    pub code: Style,
    pub bold: Style,
    pub italic: Style,
    pub strike: Style,
    pub link: Style,
    pub quote: Style,
    pub list: Style,
    pub separator: Style,
}

impl MarkdownLineStyles {
    #[must_use]
    pub fn from_theme(theme: Option<&Theme>, base: Style) -> Self {
        let get = |scope: &str| theme.map(|theme| theme.get(scope)).unwrap_or(base);
        Self {
            heading: get("markup.heading.1"),
            code: get("markup.raw.inline"),
            bold: base.add_modifier(Modifier::BOLD),
            italic: base.add_modifier(Modifier::ITALIC),
            strike: base.add_modifier(Modifier::CROSSED_OUT),
            link: get("markup.link.url"),
            quote: get("markup.quote"),
            list: get("markup.list.unnumbered"),
            separator: get("ui.statusline.separator"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownCacheKey {
    len: usize,
    complete_len: usize,
    width: usize,
}

impl MarkdownCacheKey {
    #[must_use]
    pub fn new(text: &str, width: usize) -> Self {
        Self {
            len: text.len(),
            complete_len: complete_markdown_prefix_len(text),
            width,
        }
    }
}

#[derive(Default, Clone)]
pub struct MarkdownCache {
    text: String,
    key: Option<MarkdownCacheKey>,
    complete_hash: u64,
    complete_lines: Vec<Spans<'static>>,
    lines: Vec<Spans<'static>>,
    block_lines: HashMap<(u64, usize), Vec<Spans<'static>>>,
    hits: usize,
    misses: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MarkdownCacheStats {
    pub hits: usize,
    pub misses: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MarkdownState {
    pub max_scroll: u16,
    pub visible_lines: u16,
}

pub fn render_to_surface(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    lines: &[Spans<'_>],
    scroll: u16,
) -> MarkdownState {
    if area.width == 0 || area.height == 0 {
        return MarkdownState::default();
    }

    let total = lines.len() as u16;
    let max_scroll = total.saturating_sub(area.height);
    let scroll = scroll.min(max_scroll);
    let start = scroll as usize;
    let end = (start + area.height as usize).min(lines.len());
    for (row, line) in lines[start..end].iter().enumerate() {
        surface.set_line(
            area.x,
            area.y + row as u16,
            &tui::ratatui::to_ratatui_line(line),
            area.width,
        );
    }

    MarkdownState {
        max_scroll,
        visible_lines: (end - start) as u16,
    }
}

impl MarkdownCache {
    #[must_use]
    pub fn stats(&self) -> MarkdownCacheStats {
        MarkdownCacheStats {
            hits: self.hits,
            misses: self.misses,
        }
    }

    pub fn layout(
        &mut self,
        doc: &Doc,
        width: usize,
        base_style: Style,
        styles: &MarkdownLineStyles,
        theme: Option<&Theme>,
        loader: &syntax::Loader,
    ) -> Vec<Spans<'static>> {
        let key = MarkdownCacheKey::new(&doc.text, width);
        if self.key.as_ref() == Some(&key) && self.text == doc.text {
            self.hits += 1;
            return self.lines.clone();
        }

        if doc.text.starts_with(&self.text) {
            if let Some(previous) = &self.key {
                if previous.width == width && previous.complete_len > 0 {
                    let prefix_len = previous.complete_len.min(doc.text.len());
                    let prefix_hash = content_hash(&doc.text[..prefix_len]);
                    if prefix_hash == self.complete_hash {
                        self.hits += 1;
                        let tail = &doc.text[prefix_len..];
                        let mut lines = self.complete_lines.clone();
                        lines.extend(render_markdown(
                            tail, width, base_style, styles, theme, loader,
                        ));
                        self.store(
                            doc.text.clone(),
                            key,
                            lines.clone(),
                            width,
                            base_style,
                            styles,
                            theme,
                            loader,
                        );
                        return lines;
                    }
                }
            }
        }

        self.misses += 1;
        let cache_key = (content_hash(&doc.text), width);
        if let Some(lines) = self.block_lines.get(&cache_key).cloned() {
            self.hits += 1;
            self.store_from_lines(doc.text.clone(), key, lines.clone());
            return lines;
        }

        let lines = render_markdown(&doc.text, width, base_style, styles, theme, loader);
        self.block_lines.insert(cache_key, lines.clone());
        self.store(
            doc.text.clone(),
            key,
            lines.clone(),
            width,
            base_style,
            styles,
            theme,
            loader,
        );
        lines
    }

    fn store(
        &mut self,
        text: String,
        key: MarkdownCacheKey,
        lines: Vec<Spans<'static>>,
        width: usize,
        base_style: Style,
        styles: &MarkdownLineStyles,
        theme: Option<&Theme>,
        loader: &syntax::Loader,
    ) {
        let complete_len = key.complete_len;
        let complete_lines = if complete_len == text.len() {
            lines.clone()
        } else if complete_len > 0 {
            let prefix = &text[..complete_len];
            let cache_key = (content_hash(prefix), width);
            if let Some(lines) = self.block_lines.get(&cache_key).cloned() {
                lines
            } else {
                let lines = render_markdown(prefix, width, base_style, styles, theme, loader);
                self.block_lines.insert(cache_key, lines.clone());
                lines
            }
        } else {
            Vec::new()
        };
        self.complete_hash = content_hash(&text[..complete_len]);
        self.complete_lines = complete_lines;
        self.lines = lines;
        self.text = text;
        self.key = Some(key);
    }

    fn store_from_lines(
        &mut self,
        text: String,
        key: MarkdownCacheKey,
        lines: Vec<Spans<'static>>,
    ) {
        self.complete_hash = content_hash(&text[..key.complete_len]);
        self.complete_lines = if key.complete_len == text.len() {
            lines.clone()
        } else {
            Vec::new()
        };
        self.lines = lines;
        self.text = text;
        self.key = Some(key);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Doc {
    text: String,
}

impl Doc {
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }

    #[must_use]
    pub fn layout(
        &self,
        width: usize,
        base_style: Style,
        styles: &MarkdownLineStyles,
        theme: Option<&Theme>,
        loader: &syntax::Loader,
    ) -> Vec<Spans<'static>> {
        render_markdown(&self.text, width, base_style, styles, theme, loader)
    }
}

pub struct Markdown {
    pub contents: String,

    config_loader: Arc<ArcSwap<syntax::Loader>>,
    cache: MarkdownCache,
}

impl Markdown {
    const TEXT_STYLE: &'static str = "ui.text";
    const BLOCK_STYLE: &'static str = "markup.raw.inline";
    const RULE_STYLE: &'static str = "punctuation.special";
    const UNNUMBERED_LIST_STYLE: &'static str = "markup.list.unnumbered";
    const HEADING_STYLES: [&'static str; 6] = [
        "markup.heading.1",
        "markup.heading.2",
        "markup.heading.3",
        "markup.heading.4",
        "markup.heading.5",
        "markup.heading.6",
    ];

    #[must_use]
    pub fn new(contents: String, config_loader: Arc<ArcSwap<syntax::Loader>>) -> Self {
        Self {
            contents,
            config_loader,
            cache: MarkdownCache::default(),
        }
    }

    #[must_use]
    pub fn doc(text: impl Into<String>) -> Doc {
        Doc::new(text)
    }

    #[must_use]
    pub fn parse(&self, theme: Option<&Theme>) -> tui::text::Text<'static> {
        let base = theme
            .map(|theme| theme.get(Self::TEXT_STYLE))
            .unwrap_or_default();
        let styles = MarkdownLineStyles::from_theme(theme, base);
        Text::from(Doc::new(self.contents.clone()).layout(
            usize::MAX / 4,
            base,
            &styles,
            theme,
            &self.config_loader.load(),
        ))
    }

    pub fn layout(&mut self, width: usize, theme: Option<&Theme>) -> tui::text::Text<'static> {
        let base = theme
            .map(|theme| theme.get(Self::TEXT_STYLE))
            .unwrap_or_default();
        let styles = Self::styles(theme);
        let doc = Doc::new(self.contents.clone());
        Text::from(self.cache.layout(
            &doc,
            width,
            base,
            &styles,
            theme,
            &self.config_loader.load(),
        ))
    }

    fn styles(theme: Option<&Theme>) -> MarkdownLineStyles {
        let get = |key: &str| -> Style { theme.map(|t| t.get(key)).unwrap_or_default() };
        let text_style = get(Self::TEXT_STYLE);
        let mut styles = MarkdownLineStyles::from_theme(theme, text_style);
        styles.code = get(Self::BLOCK_STYLE);
        styles.separator = get(Self::RULE_STYLE);
        styles.list = get(Self::UNNUMBERED_LIST_STYLE);
        styles.heading = get(Self::HEADING_STYLES[0]);
        styles
    }
}

#[must_use]
pub fn fit_bubble_width(text: &str, min_w: usize, max_w: usize) -> usize {
    let max_w = max_w.max(4);
    let min_w = min_w.min(max_w);
    let inner_max = max_w.saturating_sub(4).max(1);
    let wrapped = wrap_text(text, inner_max);
    let longest = wrapped
        .iter()
        .map(|l| text_layout::display_width(l))
        .max()
        .unwrap_or(0);
    (longest + 4).clamp(min_w, max_w)
}

#[must_use]
pub fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    text_layout::wrap_to_width(text, max_width)
}

pub fn render_markdown_lines<'a>(
    text: &str,
    lines: &mut Vec<Spans<'a>>,
    base_style: Style,
    styles: &MarkdownLineStyles,
    theme: Option<&Theme>,
    loader: &syntax::Loader,
) {
    lines.extend(
        render_markdown(text, usize::MAX / 4, base_style, styles, theme, loader)
            .into_iter()
            .map(|line| line),
    );
}

#[must_use]
pub fn complete_markdown_prefix_len(text: &str) -> usize {
    let mut in_fence = false;
    let mut complete = 0;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        }
        offset += line.len();
        if !in_fence && (line == "\n" || line.trim().is_empty() && line.ends_with('\n')) {
            complete = offset;
        }
    }
    if !in_fence && text.ends_with('\n') {
        complete = text.len();
    }
    complete.min(text.len())
}

#[derive(Default)]
struct Table {
    alignments: Vec<Alignment>,
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: String,
}

fn render_markdown(
    text: &str,
    width: usize,
    base_style: Style,
    styles: &MarkdownLineStyles,
    theme: Option<&Theme>,
    loader: &syntax::Loader,
) -> Vec<Spans<'static>> {
    fn push_line(spans: &mut Vec<Span<'static>>, lines: &mut Vec<Spans<'static>>, width: usize) {
        let spans = std::mem::take(spans);
        if spans.is_empty() {
            lines.push(Spans::default());
        } else {
            lines.extend(wrap_spans(Spans::from(spans), width));
        }
    }

    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);
    let parser = Parser::new_ext(text, options);

    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut tags = Vec::new();
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut quote_depth = 0usize;
    let mut link_destinations = Vec::new();
    let mut table: Option<Table> = None;
    let mut footnote_refs: HashMap<String, usize> = HashMap::new();
    let mut footnote_order = Vec::new();
    let mut footnote: Option<(String, String)> = None;

    let indent = |level: usize| "  ".repeat(level.saturating_sub(1));

    for event in parser {
        let mut finish_table = false;
        if let Some(table_state) = &mut table {
            match event {
                Event::Start(Tag::TableCell) => table_state.cell.clear(),
                Event::End(TagEnd::TableCell) => {
                    table_state.row.push(table_state.cell.trim().to_string())
                }
                Event::End(TagEnd::TableRow) | Event::End(TagEnd::TableHead) => {
                    if !table_state.row.is_empty() {
                        table_state.rows.push(std::mem::take(&mut table_state.row));
                    }
                }
                Event::End(TagEnd::Table) => {
                    finish_table = true;
                }
                Event::Text(text)
                | Event::Code(text)
                | Event::Html(text)
                | Event::InlineHtml(text) => {
                    table_state.cell.push_str(&text);
                }
                Event::SoftBreak | Event::HardBreak => table_state.cell.push(' '),
                _ => {}
            }
            if finish_table {
                if let Some(table_state) = table.take() {
                    lines.extend(render_table(&table_state, width, styles));
                    lines.push(Spans::default());
                }
            }
            continue;
        }

        if let Some((_, body)) = &mut footnote {
            match event {
                Event::End(TagEnd::FootnoteDefinition) => {
                    let (label, body) = footnote.take().unwrap();
                    let n = footnote_refs.entry(label.clone()).or_insert_with(|| {
                        footnote_order.push(label.clone());
                        footnote_order.len()
                    });
                    lines.push(Spans::from(Span::styled(
                        format!("[{n}] {}", body.trim()),
                        styles.link,
                    )));
                }
                Event::Text(text)
                | Event::Code(text)
                | Event::Html(text)
                | Event::InlineHtml(text) => {
                    body.push_str(&text);
                }
                Event::SoftBreak | Event::HardBreak => body.push(' '),
                _ => {}
            }
            continue;
        }

        match event {
            Event::Start(Tag::Table(alignments)) => {
                if !spans.is_empty() {
                    push_line(&mut spans, &mut lines, width);
                }
                table = Some(Table {
                    alignments,
                    ..Table::default()
                });
            }
            Event::Start(Tag::FootnoteDefinition(label)) => {
                footnote = Some((label.to_string(), String::new()));
            }
            Event::Start(Tag::List(start)) => {
                if !spans.is_empty() {
                    push_line(&mut spans, &mut lines, width);
                }
                list_stack.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                list_stack.pop();
                if list_stack.is_empty() && !lines.last().is_some_and(|line| line.0.is_empty()) {
                    lines.push(Spans::default());
                }
            }
            Event::Start(Tag::Item) => {
                let (marker, style) = match list_stack.last_mut().and_then(Option::as_mut) {
                    Some(number) => {
                        let marker = format!("{number}. ");
                        *number += 1;
                        (marker, styles.list)
                    }
                    None => ("• ".to_string(), styles.list),
                };
                spans.push(Span::styled(
                    format!("{}{}", indent(list_stack.len()), marker),
                    style,
                ));
                tags.push(Tag::Item);
            }
            Event::TaskListMarker(checked) => {
                spans.push(Span::styled(
                    if checked { "[x] " } else { "[ ] " }.to_string(),
                    styles.list,
                ));
            }
            Event::Start(Tag::BlockQuote(_)) => {
                quote_depth += 1;
                spans.push(Span::styled(
                    format!("{}> ", "  ".repeat(quote_depth - 1)),
                    styles.quote,
                ));
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                quote_depth = quote_depth.saturating_sub(1);
                if !spans.is_empty() {
                    push_line(&mut spans, &mut lines, width);
                }
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                link_destinations.push(dest_url.to_string());
                tags.push(Tag::Link {
                    link_type: LinkType::Inline,
                    dest_url,
                    title: "".into(),
                    id: "".into(),
                });
            }
            Event::End(TagEnd::Link) => {
                tags.pop();
                if let Some(url) = link_destinations.pop() {
                    spans.push(Span::styled(format!(" ({url})"), styles.link));
                }
            }
            Event::Start(tag) => tags.push(tag),
            Event::End(tag) => {
                tags.pop();
                match tag {
                    TagEnd::Heading(_) | TagEnd::Paragraph | TagEnd::CodeBlock | TagEnd::Item => {
                        if !spans.is_empty() {
                            push_line(&mut spans, &mut lines, width);
                        }
                    }
                    _ => {}
                }
                if matches!(
                    tag,
                    TagEnd::Heading(_) | TagEnd::Paragraph | TagEnd::CodeBlock
                ) && !lines.last().is_some_and(|line| line.0.is_empty())
                {
                    lines.push(Spans::default());
                }
            }
            Event::Text(text) => {
                if let Some(Tag::CodeBlock(kind)) = tags.last() {
                    let language = match kind {
                        CodeBlockKind::Fenced(language) => language.as_ref(),
                        CodeBlockKind::Indented => "",
                    };
                    let highlighted = highlighted_code_block(&text, language, theme, loader, None);
                    for line in highlighted.lines {
                        lines.extend(wrap_spans(spans_into_static(line), width));
                    }
                } else {
                    let mut style = base_style;
                    for tag in &tags {
                        style = match tag {
                            Tag::Heading { level, .. } => heading_style(*level, theme, styles),
                            Tag::Emphasis => styles.italic,
                            Tag::Strong => styles.bold,
                            Tag::Strikethrough => styles.strike,
                            Tag::Link { .. } => styles.link,
                            _ => style,
                        };
                    }
                    spans.push(Span::styled(text.to_string(), style));
                }
            }
            Event::Code(text) => {
                spans.push(Span::styled(text.to_string(), styles.code));
            }
            Event::Html(text) | Event::InlineHtml(text) => {
                spans.push(Span::styled(text.to_string(), base_style));
            }
            Event::FootnoteReference(label) => {
                let n = *footnote_refs.entry(label.to_string()).or_insert_with(|| {
                    footnote_order.push(label.to_string());
                    footnote_order.len()
                });
                spans.push(Span::styled(format!("[{n}]"), styles.link));
            }
            Event::SoftBreak | Event::HardBreak => {
                push_line(&mut spans, &mut lines, width);
                if quote_depth > 0 {
                    spans.push(Span::styled(
                        format!("{}> ", "  ".repeat(quote_depth - 1)),
                        styles.quote,
                    ));
                }
            }
            Event::Rule => {
                let rule = "─".repeat(width.min(24).max(3));
                lines.push(Spans::from(Span::styled(rule, styles.separator)));
                lines.push(Spans::default());
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                spans.push(Span::styled(text.to_string(), styles.code));
            }
        }
    }

    if !spans.is_empty() {
        lines.extend(wrap_spans(Spans::from(spans), width));
    }
    while lines.last().is_some_and(|line| line.0.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(Spans::from(Span::styled(String::new(), base_style)));
    }
    lines
}

fn heading_style(level: HeadingLevel, theme: Option<&Theme>, styles: &MarkdownLineStyles) -> Style {
    let index = match level {
        HeadingLevel::H1 => 0,
        HeadingLevel::H2 => 1,
        HeadingLevel::H3 => 2,
        HeadingLevel::H4 => 3,
        HeadingLevel::H5 => 4,
        HeadingLevel::H6 => 5,
    };
    theme
        .map(|theme| theme.get(Markdown::HEADING_STYLES[index]))
        .unwrap_or(styles.heading)
}

fn wrap_spans(line: Spans<'static>, width: usize) -> Vec<Spans<'static>> {
    if width == 0 {
        return Vec::new();
    }
    if width >= usize::MAX / 8 || line.width() <= width {
        return vec![line];
    }
    if line.0.len() == 1 {
        let span = &line.0[0];
        return text_layout::wrap_to_width(&span.content, width)
            .into_iter()
            .map(|line| Spans::from(Span::styled(line, span.style)))
            .collect();
    }

    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut used = 0usize;
    for span in line.0 {
        for grapheme in text_layout::visible_graphemes(&span.content, usize::MAX / 4) {
            if used > 0 && used + grapheme.width > width {
                lines.push(Spans::from(std::mem::take(&mut spans)));
                used = 0;
            }
            spans.push(Span::styled(grapheme.text.to_string(), span.style));
            used += grapheme.width;
        }
    }
    if !spans.is_empty() {
        lines.push(Spans::from(spans));
    }
    lines
}

fn spans_into_static(line: Spans<'_>) -> Spans<'static> {
    Spans::from(
        line.0
            .into_iter()
            .map(|span| Span::styled(span.content.into_owned(), span.style))
            .collect::<Vec<_>>(),
    )
}

fn render_table(table: &Table, width: usize, styles: &MarkdownLineStyles) -> Vec<Spans<'static>> {
    if table.rows.is_empty() {
        return Vec::new();
    }
    let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 {
        return Vec::new();
    }

    let gap = 3usize;
    let gap_total = gap.saturating_mul(columns.saturating_sub(1));
    let available = width.saturating_sub(gap_total).max(columns);
    let per_column = (available / columns).max(1);
    let mut widths = vec![1usize; columns];
    for row in &table.rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index]
                .max(text_layout::display_width(cell).min(per_column))
                .min(per_column);
        }
    }

    let mut lines = Vec::new();
    for (row_index, row) in table.rows.iter().enumerate() {
        let mut spans = Vec::new();
        for column in 0..columns {
            if column > 0 {
                spans.push(Span::styled(" | ".to_string(), styles.separator));
            }
            let cell = row.get(column).map(String::as_str).unwrap_or("");
            let cell = text_layout::truncate(cell, widths[column], TruncateAt::End);
            let align = match table
                .alignments
                .get(column)
                .copied()
                .unwrap_or(Alignment::None)
            {
                Alignment::Right => Align::Right,
                Alignment::Center => Align::Center,
                Alignment::None | Alignment::Left => Align::Left,
            };
            spans.push(Span::styled(
                text_layout::pad(&cell, widths[column], align),
                styles.code,
            ));
        }
        lines.push(Spans::from(spans));

        if row_index == 0 {
            let mut spans = Vec::new();
            for (column, width) in widths.iter().copied().enumerate() {
                if column > 0 {
                    spans.push(Span::styled("-+-".to_string(), styles.separator));
                }
                spans.push(Span::styled("-".repeat(width), styles.separator));
            }
            lines.push(Spans::from(spans));
        }
    }
    lines
}

fn content_hash(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

impl Component for Markdown {
    fn render(&mut self, area: Rect, surface: &mut crate::render::CellSurface, cx: &RenderContext) {
        let margin = Margin::all(1);
        let area = area.inner(margin);
        let theme = cx.theme();
        let base = theme.get(Self::TEXT_STYLE);
        let styles = Self::styles(Some(theme));
        let doc = Doc::new(self.contents.clone());
        let lines = self.cache.layout(
            &doc,
            area.width as usize,
            base,
            &styles,
            Some(theme),
            &self.config_loader.load(),
        );
        render_to_surface(
            surface,
            area,
            &lines,
            cx.scroll().unwrap_or_default() as u16,
        );
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        let padding = 2;
        let max_text_width = (viewport.0.saturating_sub(padding)).min(120);
        let base = Style::default();
        let styles = Self::styles(None);
        let doc = Doc::new(self.contents.clone());
        let lines = self.cache.layout(
            &doc,
            max_text_width as usize,
            base,
            &styles,
            None,
            &self.config_loader.load(),
        );
        let contents = Text::from(lines);
        let (width, height) = crate::ui::text::required_size(&contents, max_text_width);

        Some((width + padding, height + padding))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_view::theme::{Modifier, Style};

    fn styles() -> MarkdownLineStyles {
        MarkdownLineStyles {
            heading: Style::default().add_modifier(Modifier::BOLD),
            code: Style::default().add_modifier(Modifier::DIM),
            bold: Style::default().add_modifier(Modifier::BOLD),
            italic: Style::default().add_modifier(Modifier::ITALIC),
            strike: Style::default().add_modifier(Modifier::CROSSED_OUT),
            link: Style::default().add_modifier(Modifier::DIM),
            quote: Style::default().add_modifier(Modifier::ITALIC),
            list: Style::default().add_modifier(Modifier::BOLD),
            separator: Style::default().add_modifier(Modifier::DIM),
        }
    }

    fn render(text: &str, width: usize) -> Vec<String> {
        let loader = helix_core::config::default_lang_loader();
        Doc::new(text)
            .layout(width, Style::default(), &styles(), None, &loader)
            .iter()
            .map(|line| line.0.iter().map(|span| span.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn table_layout_truncates_at_narrow_widths() {
        let lines = render(
            "| name | value |\n| --- | --- |\n| alpha | betabetabeta |\n",
            12,
        );
        assert!(lines.iter().any(|line| line.contains("…")));
        assert!(lines.iter().any(|line| line.contains("|")));
    }

    #[test]
    fn task_list_and_nested_list_indent() {
        let lines = render("- [x] done\n  - child\n", 40);
        let text = lines.join("\n");
        assert!(text.contains("[x] done"));
        assert!(text.contains("  • child"));
    }

    #[test]
    fn footnote_reference_adds_trailing_definition() {
        let lines = render("hello[^a]\n\n[^a]: note body\n", 40);
        let text = lines.join("\n");
        assert!(text.contains("hello[1]"));
        assert!(text.contains("[1] note body"));
    }

    #[test]
    fn streaming_cache_reuses_complete_prefix() {
        let loader = helix_core::config::default_lang_loader();
        let mut cache = MarkdownCache::default();
        let complete = Doc::new("done paragraph\n\nstream");
        let _ = cache.layout(&complete, 40, Style::default(), &styles(), None, &loader);
        let before = cache.stats();
        let appended = Doc::new("done paragraph\n\nstreaming tail");
        let _ = cache.layout(&appended, 40, Style::default(), &styles(), None, &loader);
        let after = cache.stats();
        assert!(after.hits > before.hits);
    }

    #[test]
    fn unknown_code_fence_falls_back_to_plain_code() {
        let lines = render("```not-a-language\nlet x = 1;\n```\n", 40);
        assert!(lines.iter().any(|line| line.contains("let x = 1;")));
    }

    #[test]
    fn completed_prefix_stays_reusable_during_incremental_append() {
        let complete = "done paragraph\n\n";
        let appended = format!("{complete}streaming tail");

        assert_eq!(complete_markdown_prefix_len(complete), complete.len());
        assert_eq!(complete_markdown_prefix_len(&appended), complete.len());
        assert!(appended.starts_with(complete));
    }
}
