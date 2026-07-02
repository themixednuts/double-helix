use std::path::{Path, PathBuf};

use super::context;

pub const CONTEXT_ID_PREFIX: &str = "mention:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub text: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Active {
    pub query: String,
    pub start: usize,
    pub end: usize,
}

#[must_use]
pub fn tokens(input: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut iter = input.char_indices().peekable();
    while let Some((start, ch)) = iter.next() {
        if ch != '@' {
            continue;
        }
        let mut end = start + ch.len_utf8();
        while let Some(&(idx, next)) = iter.peek() {
            if next.is_whitespace() {
                break;
            }
            end = idx + next.len_utf8();
            iter.next();
        }
        if end > start + 1 {
            out.push(Token {
                text: input[start + 1..end].to_string(),
                start,
                end,
            });
        }
    }
    out
}

#[must_use]
pub fn active_token(input: &str, cursor: usize) -> Option<Token> {
    tokens(input)
        .into_iter()
        .find(|token| token.start < cursor && cursor <= token.end)
}

#[must_use]
pub fn active_query(input: &str, cursor: usize) -> Option<Active> {
    let cursor = cursor.min(input.len());
    let prefix = &input[..cursor];
    let mut start = None;
    for (idx, ch) in prefix.char_indices().rev() {
        if ch == '@' {
            start = Some(idx);
            break;
        }
        if ch.is_whitespace() {
            break;
        }
    }
    let start = start?;
    if input[start + 1..cursor].chars().any(char::is_whitespace) {
        return None;
    }
    Some(Active {
        query: input[start + 1..cursor].to_string(),
        start,
        end: cursor,
    })
}

#[must_use]
pub fn context_id(key: &str) -> context::Id {
    context::Id::new(format!("{CONTEXT_ID_PREFIX}{key}"))
}

#[must_use]
pub fn is_context_id(id: &context::Id) -> bool {
    id.as_str().starts_with(CONTEXT_ID_PREFIX)
}

#[must_use]
pub fn key_for_kind(kind: &context::Kind) -> String {
    match kind {
        context::Kind::Selection(_) => "selection".to_string(),
        context::Kind::Symbol(symbol) => format!("symbol:{}", normalize_path(&symbol.path)),
        context::Kind::File(file) => format!("file:{}", normalize_path(&file.path)),
        context::Kind::Diagnostics(diagnostics) => {
            format!("diagnostics:{}", normalize_path(&diagnostics.path))
        }
        context::Kind::Diff(diff) => format!("diff:{}", normalize_path(&diff.path)),
    }
}

fn normalize_path(path: &Path) -> String {
    path.components()
        .collect::<PathBuf>()
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_collect_mentions() {
        let found = tokens("look at @src/main.rs and @diagnostics\nthen @diff");
        assert_eq!(
            found
                .iter()
                .map(|token| token.text.as_str())
                .collect::<Vec<_>>(),
            ["src/main.rs", "diagnostics", "diff"]
        );
    }

    #[test]
    fn tokens_drop_deleted_mentions() {
        let found = tokens("look at src/main.rs and @diff");
        assert_eq!(
            found
                .iter()
                .map(|token| token.text.as_str())
                .collect::<Vec<_>>(),
            ["diff"]
        );
    }

    #[test]
    fn active_token_tracks_cursor_inside_token() {
        let input = "ask @src/lib.rs now";
        let cursor = input.find("lib").expect("token") + 1;
        assert_eq!(
            active_token(input, cursor).map(|token| token.text),
            Some("src/lib.rs".to_string())
        );
    }

    #[test]
    fn active_query_allows_empty_at_token() {
        assert_eq!(
            active_query("ask @", "ask @".len()).map(|active| active.query),
            Some(String::new())
        );
    }
}
