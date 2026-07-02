use helix_core::unicode::{segmentation::UnicodeSegmentation, width::UnicodeWidthStr};

// Later migration candidate: widgets/picker_table.rs still has local cell truncation.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncateAt {
    Start,
    Middle,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grapheme<'a> {
    pub byte: usize,
    pub text: &'a str,
    pub width: usize,
}

#[must_use]
pub fn display_width(text: &str) -> usize {
    text.width()
}

#[must_use]
pub fn visible_graphemes(text: &str, max_width: usize) -> Vec<Grapheme<'_>> {
    let mut width = 0usize;
    let mut out = Vec::new();
    for (byte, text) in text.grapheme_indices(true) {
        let grapheme_width = text.width();
        if grapheme_width == 0 {
            continue;
        }
        if width + grapheme_width > max_width {
            break;
        }
        out.push(Grapheme {
            byte,
            text,
            width: grapheme_width,
        });
        width += grapheme_width;
    }
    out
}

#[must_use]
pub fn truncate(text: &str, width: usize, at: TruncateAt) -> String {
    if width == 0 {
        return String::new();
    }
    if display_width(text) <= width {
        return text.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }

    match at {
        TruncateAt::End => {
            let mut out = take_prefix(text, width - 1);
            out.push('…');
            out
        }
        TruncateAt::Start => {
            let mut out = String::from("…");
            out.push_str(&take_suffix(text, width - 1));
            out
        }
        TruncateAt::Middle => {
            let left = (width - 1) / 2;
            let right = width - 1 - left;
            let mut out = take_prefix(text, left);
            out.push('…');
            out.push_str(&take_suffix(text, right));
            out
        }
    }
}

#[must_use]
pub fn pad(text: &str, width: usize, align: Align) -> String {
    let text_width = display_width(text);
    if text_width >= width {
        return text.to_string();
    }

    let padding = width - text_width;
    let (left, right) = match align {
        Align::Left => (0, padding),
        Align::Right => (padding, 0),
        Align::Center => (padding / 2, padding - padding / 2),
    };
    format!("{}{}{}", " ".repeat(left), text, " ".repeat(right))
}

#[must_use]
pub fn wrap_to_width(text: &str, max_width: usize) -> Vec<String> {
    let mut out = Vec::new();
    if max_width == 0 {
        return out;
    }

    for line in text.lines() {
        if line.is_empty() {
            out.push(String::new());
            continue;
        }

        let mut current = String::new();
        let mut current_width = 0usize;
        for word in line.split_whitespace() {
            let word_width = display_width(word);
            if word_width > max_width {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                    current_width = 0;
                }
                push_broken_word(&mut out, word, max_width);
                continue;
            }

            let gap = usize::from(current_width > 0);
            if current_width > 0 && current_width + gap + word_width > max_width {
                out.push(std::mem::take(&mut current));
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
            out.push(current);
        }
    }

    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn take_prefix(text: &str, max_width: usize) -> String {
    visible_graphemes(text, max_width)
        .into_iter()
        .map(|g| g.text)
        .collect()
}

fn take_suffix(text: &str, max_width: usize) -> String {
    let mut width = 0usize;
    let mut parts = Vec::new();
    for text in text.graphemes(true).rev() {
        let grapheme_width = text.width();
        if grapheme_width == 0 {
            continue;
        }
        if width + grapheme_width > max_width {
            break;
        }
        parts.push(text);
        width += grapheme_width;
    }
    parts.into_iter().rev().collect()
}

fn push_broken_word(out: &mut Vec<String>, word: &str, max_width: usize) {
    let mut current = String::new();
    let mut width = 0usize;
    for grapheme in word.graphemes(true) {
        let grapheme_width = grapheme.width();
        if grapheme_width == 0 {
            continue;
        }
        if width > 0 && width + grapheme_width > max_width {
            out.push(std::mem::take(&mut current));
            width = 0;
        }
        if grapheme_width > max_width {
            out.push(grapheme.to_string());
            continue;
        }
        current.push_str(grapheme);
        width += grapheme_width;
    }
    if !current.is_empty() {
        out.push(current);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_cjk_width() {
        assert_eq!(display_width("a界"), 3);
    }

    #[test]
    fn truncates_ellipsis_edges() {
        assert_eq!(truncate("abcdef", 0, TruncateAt::End), "");
        assert_eq!(truncate("abcdef", 1, TruncateAt::End), "…");
        assert_eq!(truncate("abcdef", 2, TruncateAt::End), "a…");
        assert_eq!(truncate("abcdef", 2, TruncateAt::Start), "…f");
        assert_eq!(truncate("abcdef", 5, TruncateAt::Middle), "ab…ef");
    }

    #[test]
    fn wraps_long_unbreakable_token() {
        assert_eq!(wrap_to_width("abcdef", 2), vec!["ab", "cd", "ef"]);
    }
}
