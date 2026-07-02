use tui::text::{Span, Spans};

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd};

pub(super) struct MarkdownLineStyles {
    pub(super) heading: helix_view::graphics::Style,
    pub(super) code: helix_view::graphics::Style,
    pub(super) bold: helix_view::graphics::Style,
    pub(super) italic: helix_view::graphics::Style,
    pub(super) strike: helix_view::graphics::Style,
    pub(super) link: helix_view::graphics::Style,
    pub(super) quote: helix_view::graphics::Style,
    pub(super) list: helix_view::graphics::Style,
    pub(super) separator: helix_view::graphics::Style,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MarkdownCacheKey {
    pub(super) len: usize,
    pub(super) complete_len: usize,
}

impl MarkdownCacheKey {
    #[must_use]
    pub(super) fn new(text: &str) -> Self {
        Self {
            len: text.len(),
            complete_len: complete_markdown_prefix_len(text),
        }
    }
}

/// Compute the ideal bubble width for `text`: fit to the longest wrapped
/// line, then clamp to [min_w, max_w].
pub(super) fn fit_bubble_width(text: &str, min_w: usize, max_w: usize) -> usize {
    let max_w = max_w.max(4);
    let min_w = min_w.min(max_w);
    let inner_max = max_w.saturating_sub(4).max(1);
    let wrapped = wrap_text(text, inner_max);
    let longest = wrapped.iter().map(|l| l.len()).max().unwrap_or(0);
    (longest + 4).clamp(min_w, max_w)
}

/// Render markdown-ish text into styled Spans lines.
/// Handles headings, emphasis, inline code, code blocks, and simple horizontal rules.
pub(super) fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    let mut result = Vec::new();
    if max_width == 0 {
        return result;
    }
    for line in text.lines() {
        if line.is_empty() {
            result.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut current_width = 0;
        for word in line.split_whitespace() {
            let word_width = word.len();
            if current_width > 0 && current_width + 1 + word_width > max_width {
                result.push(current);
                current = String::new();
                current_width = 0;
            }
            if current_width > 0 {
                current.push(' ');
                current_width += 1;
            }
            current.push_str(word);
            current_width += word_width;
        }
        if !current.is_empty() || line.ends_with(' ') {
            result.push(current);
        }
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}

pub(super) fn render_markdown_lines<'a>(
    text: &str,
    lines: &mut Vec<Spans<'a>>,
    base_style: helix_view::graphics::Style,
    styles: &MarkdownLineStyles,
    theme: Option<&helix_view::Theme>,
    loader: &helix_core::syntax::Loader,
) {
    lines.extend(render_markdown(text, base_style, styles, theme, loader));
}

pub(super) fn complete_markdown_prefix_len(text: &str) -> usize {
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

fn render_markdown(
    text: &str,
    base_style: helix_view::graphics::Style,
    styles: &MarkdownLineStyles,
    theme: Option<&helix_view::Theme>,
    loader: &helix_core::syntax::Loader,
) -> Vec<Spans<'static>> {
    fn push_line(spans: &mut Vec<Span<'static>>, lines: &mut Vec<Spans<'static>>) {
        lines.push(Spans::from(std::mem::take(spans)));
    }

    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    let parser = Parser::new_ext(text, options);
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut tags = Vec::new();
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut quote_depth = 0usize;
    let mut link_destinations = Vec::new();

    let indent = |level: usize| "  ".repeat(level.saturating_sub(1));

    for event in parser {
        match event {
            Event::Start(Tag::List(start)) => {
                if !spans.is_empty() {
                    push_line(&mut spans, &mut lines);
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
                    push_line(&mut spans, &mut lines);
                }
            }
            Event::Start(Tag::Link {
                link_type: LinkType::Inline,
                dest_url,
                ..
            })
            | Event::Start(Tag::Link { dest_url, .. }) => {
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
            Event::Start(tag) => {
                tags.push(tag);
            }
            Event::End(tag) => {
                tags.pop();
                match tag {
                    TagEnd::Heading(_) | TagEnd::Paragraph | TagEnd::CodeBlock | TagEnd::Item => {
                        if !spans.is_empty() {
                            push_line(&mut spans, &mut lines);
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
                    let highlighted = crate::ui::markdown::highlighted_code_block(
                        &text, language, theme, loader, None,
                    );
                    lines.extend(highlighted.lines);
                } else {
                    let mut style = base_style;
                    for tag in &tags {
                        style = match tag {
                            Tag::Heading { level, .. } => match level {
                                HeadingLevel::H1
                                | HeadingLevel::H2
                                | HeadingLevel::H3
                                | HeadingLevel::H4
                                | HeadingLevel::H5
                                | HeadingLevel::H6 => styles.heading,
                            },
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
            Event::Code(text) | Event::Html(text) => {
                spans.push(Span::styled(text.to_string(), styles.code));
            }
            Event::SoftBreak | Event::HardBreak => {
                push_line(&mut spans, &mut lines);
                if quote_depth > 0 {
                    spans.push(Span::styled(
                        format!("{}> ", "  ".repeat(quote_depth - 1)),
                        styles.quote,
                    ));
                }
            }
            Event::Rule => {
                lines.push(Spans::from(Span::styled(
                    "───".to_string(),
                    styles.separator,
                )));
                lines.push(Spans::default());
            }
            _ => {}
        }
    }

    if !spans.is_empty() {
        lines.push(Spans::from(spans));
    }
    while lines.last().is_some_and(|line| line.0.is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(Spans::from(Span::styled(String::new(), base_style)));
    }
    lines
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

    #[test]
    fn renders_lists_links_and_code_blocks() {
        let loader = helix_core::config::default_lang_loader();
        let mut lines = Vec::new();
        render_markdown_lines(
            "- **item** with [link](https://example.com)\n\n```rust\nlet x = 1;\n```",
            &mut lines,
            Style::default(),
            &styles(),
            None,
            &loader,
        );

        let rendered = lines
            .iter()
            .map(|line| {
                line.0
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("• "));
        assert!(rendered.contains("item"));
        assert!(rendered.contains("(https://example.com)"));
        assert!(rendered.contains("let x = 1;"));
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
