use tui::text::{Span, Spans};

pub(super) struct MarkdownLineStyles {
    pub(super) heading: helix_view::graphics::Style,
    pub(super) code: helix_view::graphics::Style,
    pub(super) bold: helix_view::graphics::Style,
    pub(super) italic: helix_view::graphics::Style,
    pub(super) separator: helix_view::graphics::Style,
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
) {
    let mut in_code_block = false;

    for line in text.lines() {
        if line.starts_with("```") {
            in_code_block = !in_code_block;
            lines.push(Spans::from(Span::styled(
                "────────".to_string(),
                styles.code,
            )));
            continue;
        }

        if in_code_block {
            lines.push(Spans::from(Span::styled(format!("  {line}"), styles.code)));
            continue;
        }

        let trimmed = line.trim();
        if (trimmed.starts_with("---") || trimmed.starts_with("***") || trimmed.starts_with("___"))
            && trimmed
                .chars()
                .all(|c| c == '-' || c == '*' || c == '_' || c == ' ')
            && trimmed.len() >= 3
        {
            lines.push(Spans::from(Span::styled(
                "───".to_string(),
                styles.separator,
            )));
            continue;
        }

        if let Some(stripped) = line.strip_prefix("# ") {
            lines.push(Spans::from(Span::styled(
                stripped.to_string(),
                styles.heading,
            )));
            continue;
        }
        if let Some(stripped) = line.strip_prefix("## ") {
            lines.push(Spans::from(Span::styled(
                stripped.to_string(),
                styles.heading,
            )));
            continue;
        }
        if let Some(stripped) = line.strip_prefix("### ") {
            lines.push(Spans::from(Span::styled(
                stripped.to_string(),
                styles.heading,
            )));
            continue;
        }

        let spans =
            parse_inline_markdown(line, base_style, styles.bold, styles.italic, styles.code);
        lines.push(Spans::from(spans));
    }
}

fn parse_inline_markdown(
    line: &str,
    base: helix_view::graphics::Style,
    bold: helix_view::graphics::Style,
    italic: helix_view::graphics::Style,
    code: helix_view::graphics::Style,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = line.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if chars[i] == '`' {
            if !current.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut current), base));
            }
            i += 1;
            let mut code_text = String::new();
            while i < len && chars[i] != '`' {
                code_text.push(chars[i]);
                i += 1;
            }
            if i < len {
                i += 1;
            }
            spans.push(Span::styled(code_text, code));
            continue;
        }

        if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            if !current.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut current), base));
            }
            i += 2;
            let mut bold_text = String::new();
            while i + 1 < len && !(chars[i] == '*' && chars[i + 1] == '*') {
                bold_text.push(chars[i]);
                i += 1;
            }
            if i + 1 < len {
                i += 2;
            }
            spans.push(Span::styled(bold_text, bold));
            continue;
        }

        if chars[i] == '*' {
            if !current.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut current), base));
            }
            i += 1;
            let mut italic_text = String::new();
            while i < len && chars[i] != '*' {
                italic_text.push(chars[i]);
                i += 1;
            }
            if i < len {
                i += 1;
            }
            spans.push(Span::styled(italic_text, italic));
            continue;
        }

        current.push(chars[i]);
        i += 1;
    }

    if !current.is_empty() {
        spans.push(Span::styled(current, base));
    }

    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }

    spans
}
