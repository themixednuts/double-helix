//! FFF_UNSAVED_BUFFER_GREP_BLOCKER: Helix greps unsaved editor buffers by
//! overlaying in-memory bytes over indexed files. Byte slices are searched
//! with the same matcher/sink pipeline as `grep_search`, so overlay results
//! line up with on-disk results.

use super::grep::{NeedleFinder, PlainTextMatcher, PlainTextSink, replace_newline_escapes};
use super::regex::{RegexMatcher, RegexSink, build_regex};
use super::sink::SinkState;
use super::types::{GrepMatch, GrepMode, GrepSearchOptions};
use crate::file_picker::FilePicker;
use fff_grep::SearcherBuilder;
use fff_query_parser::FFFQuery;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone)]
pub struct ContentOverlay {
    pub path: PathBuf,
    pub bytes: Arc<[u8]>,
    pub revision: u64,
}

#[derive(Debug, Clone)]
pub struct OwnedGrepMatch {
    pub path: PathBuf,
    /// 1-based line number.
    pub line_number: u64,
    pub line_content: String,
}

#[derive(Debug, Clone, Default)]
pub struct OwnedGrepResult {
    pub matches: Vec<OwnedGrepMatch>,
    pub next_file_offset: usize,
    pub filtered_file_count: usize,
    pub regex_fallback_error: Option<String>,
}

impl FilePicker {
    /// Grep indexed files, replacing on-disk results for every overlaid path
    /// with matches from the overlay bytes (when `include_overlay_matches`).
    pub fn grep_owned(
        &self,
        query: &FFFQuery<'_>,
        options: &GrepSearchOptions,
        content_overlays: &[ContentOverlay],
        include_overlay_matches: bool,
    ) -> OwnedGrepResult {
        let result = self.grep(query, options);
        let overlay_paths: std::collections::HashSet<&Path> = content_overlays
            .iter()
            .map(|overlay| overlay.path.as_path())
            .collect();
        let mut matches = Vec::with_capacity(result.matches.len());

        for item in result.matches {
            let Some(file) = result.files.get(item.file_index) else {
                continue;
            };
            let path = file.absolute_path(self, self.base_path());
            if overlay_paths.contains(path.as_path()) {
                continue;
            }
            matches.push(OwnedGrepMatch {
                path,
                line_number: item.line_number,
                line_content: item.line_content,
            });
        }

        let mut regex_fallback_error = result.regex_fallback_error;
        if include_overlay_matches {
            for overlay in content_overlays {
                let (overlay_matches, overlay_regex_error) =
                    grep_bytes(query, options, &overlay.bytes);
                if regex_fallback_error.is_none() {
                    regex_fallback_error = overlay_regex_error;
                }
                matches.extend(overlay_matches.into_iter().map(|item| OwnedGrepMatch {
                    path: overlay.path.clone(),
                    line_number: item.line_number,
                    line_content: item.line_content,
                }));
            }
        }

        OwnedGrepResult {
            matches,
            next_file_offset: result.next_file_offset,
            filtered_file_count: result.filtered_file_count,
            regex_fallback_error,
        }
    }
}

/// Grep a single in-memory buffer. Returns the matches and, in regex mode, the
/// pattern compilation error (the search then falls back to literal text).
pub fn grep_bytes(
    query: &FFFQuery<'_>,
    options: &GrepSearchOptions,
    bytes: &[u8],
) -> (Vec<GrepMatch>, Option<String>) {
    let grep_text = grep_text_for_query(query);
    if grep_text.is_empty() {
        return (Vec::new(), None);
    }

    let casing = options.effective_casing();
    let case_insensitive = casing.is_insensitive_for(&grep_text);

    let mut regex_fallback_error = None;
    let regex = match options.mode {
        GrepMode::PlainText | GrepMode::Fuzzy => None,
        GrepMode::Regex => build_regex(&grep_text, casing)
            .inspect_err(|err| {
                tracing::warn!("Regex compilation failed for {}. Error {}", grep_text, err);
                regex_fallback_error = Some(err.to_string());
            })
            .ok(),
    };

    let (multiline_segment_len, effective_pattern) = match replace_newline_escapes(&grep_text) {
        Some((replaced, first_newline_pos)) => (Some(first_newline_pos), replaced),
        None => (None, grep_text),
    };
    let is_multiline = multiline_segment_len.is_some();

    // Mirror `grep_search`: a multiline literal expands the after-context to
    // cover every line of the needle.
    let after_context = if is_multiline && regex.is_none() && options.after_context == 0 {
        effective_pattern.bytes().filter(|&b| b == b'\n').count()
    } else {
        options.after_context
    };

    let finder_pattern: Vec<u8> = if case_insensitive {
        effective_pattern.as_bytes().to_ascii_lowercase()
    } else {
        effective_pattern.as_bytes().to_vec()
    };
    let finder = NeedleFinder::new(&finder_pattern, case_insensitive);
    let pattern_len = finder_pattern.len() as u32;
    let plain_matcher = PlainTextMatcher { finder: &finder };
    let searcher = {
        let mut builder = SearcherBuilder::new();
        builder.line_number(true).multi_line(is_multiline);
        builder
    }
    .build();
    let state = SinkState {
        file_index: 0,
        matches: Vec::with_capacity(4),
        max_matches: options.max_matches_per_file,
        before_context: options.before_context,
        after_context,
        classify_definitions: options.classify_definitions,
    };

    let mut matches = match regex {
        Some(ref re) => {
            let regex_matcher = RegexMatcher {
                regex: re,
                is_multiline,
            };
            let mut sink = RegexSink { state, re };
            if let Err(err) = searcher.search_slice(&regex_matcher, bytes, &mut sink) {
                tracing::error!(error = %err, "Overlay grep (regex) search failed");
            }
            sink.state.matches
        }
        None => {
            let mut sink = PlainTextSink {
                state,
                finder: &finder,
                pattern_len,
                multiline_segment_len,
            };
            if let Err(err) = searcher.search_slice(&plain_matcher, bytes, &mut sink) {
                tracing::error!(error = %err, "Overlay grep (plain text) search failed");
            }
            sink.state.matches
        }
    };

    if options.trim_whitespace {
        for item in &mut matches {
            item.trim_leading_whitespace();
        }
    }

    (matches, regex_fallback_error)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ByteSourceGrepCursor {
    pub source: usize,
    pub match_offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSourceGrepMatch {
    pub source: usize,
    /// One-based line number, matching [`GrepMatch::line_number`].
    pub line_number: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ByteSourceGrepPage {
    pub matches: Vec<ByteSourceGrepMatch>,
    pub next: Option<ByteSourceGrepCursor>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ByteSourceGrepError {
    #[error("in-memory search page limit must be greater than zero")]
    ZeroPageLimit,
    #[error("invalid in-memory search cursor")]
    InvalidCursor,
    #[error("in-memory search was canceled")]
    Canceled,
    #[error("failed to compile search pattern: {0}")]
    InvalidPattern(String),
}

/// Page grep matches over lazily loaded in-memory sources.
///
/// A source is searched atomically, then the deadline is checked before the
/// next source. This keeps callers responsive without splitting multiline
/// regex semantics at arbitrary byte boundaries.
#[allow(clippy::too_many_arguments)]
pub fn grep_byte_sources_page<B, F>(
    query: &FFFQuery<'_>,
    options: &GrepSearchOptions,
    source_count: usize,
    cursor: ByteSourceGrepCursor,
    page_limit: usize,
    time_budget: std::time::Duration,
    abort_signal: Option<&AtomicBool>,
    mut load: F,
) -> Result<ByteSourceGrepPage, ByteSourceGrepError>
where
    B: AsRef<[u8]>,
    F: FnMut(usize) -> Option<B>,
{
    if page_limit == 0 {
        return Err(ByteSourceGrepError::ZeroPageLimit);
    }
    if cursor.source > source_count
        || (cursor.source == source_count && cursor.match_offset != 0)
    {
        return Err(ByteSourceGrepError::InvalidCursor);
    }

    let started = std::time::Instant::now();
    let mut source = cursor.source;
    let mut match_offset = cursor.match_offset;
    let mut matches = Vec::with_capacity(page_limit);
    let mut attempted = false;
    while source < source_count {
        if abort_signal.is_some_and(|signal| signal.load(Ordering::Acquire)) {
            return Err(ByteSourceGrepError::Canceled);
        }
        if attempted && started.elapsed() >= time_budget {
            return Ok(ByteSourceGrepPage {
                matches,
                next: Some(ByteSourceGrepCursor {
                    source,
                    match_offset,
                }),
            });
        }
        attempted = true;
        let Some(bytes) = load(source) else {
            source += 1;
            match_offset = 0;
            continue;
        };
        let (source_matches, regex_fallback_error) = grep_bytes(query, options, bytes.as_ref());
        if abort_signal.is_some_and(|signal| signal.load(Ordering::Acquire)) {
            return Err(ByteSourceGrepError::Canceled);
        }
        if let Some(error) = regex_fallback_error {
            return Err(ByteSourceGrepError::InvalidPattern(error));
        }
        if match_offset > source_matches.len() {
            return Err(ByteSourceGrepError::InvalidCursor);
        }
        let remaining = page_limit.saturating_sub(matches.len());
        let available = source_matches.len() - match_offset;
        let take = remaining.min(available);
        matches.extend(
            source_matches[match_offset..match_offset + take]
                .iter()
                .map(|item| ByteSourceGrepMatch {
                    source,
                    line_number: item.line_number,
                }),
        );
        match_offset += take;
        if match_offset < source_matches.len() {
            return Ok(ByteSourceGrepPage {
                matches,
                next: Some(ByteSourceGrepCursor {
                    source,
                    match_offset,
                }),
            });
        }
        source += 1;
        match_offset = 0;
        if matches.len() >= page_limit && source < source_count {
            return Ok(ByteSourceGrepPage {
                matches,
                next: Some(ByteSourceGrepCursor {
                    source,
                    match_offset: 0,
                }),
            });
        }
    }
    Ok(ByteSourceGrepPage {
        matches,
        next: None,
    })
}

fn grep_text_for_query(query: &FFFQuery<'_>) -> String {
    if !matches!(query.fuzzy_query, fff_query_parser::FuzzyQuery::Empty) {
        return query.grep_text();
    }

    let mut text = String::new();
    for constraint in &query.constraints {
        match constraint {
            fff_query_parser::Constraint::Text(term) => {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(term);
            }
            fff_query_parser::Constraint::Parts(parts) => {
                for part in *parts {
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    text.push_str(part);
                }
            }
            _ => {}
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grep::parse_grep_query;

    #[test]
    fn grep_bytes_searches_overlay_content() {
        let query = parse_grep_query("unsaved");
        let options = GrepSearchOptions {
            mode: GrepMode::PlainText,
            page_limit: 100,
            ..Default::default()
        };

        let (matches, regex_error) = grep_bytes(&query, &options, b"first line\nunsaved overlay\n");

        assert!(regex_error.is_none());
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line_number, 2);
        assert_eq!(matches[0].line_content, "unsaved overlay");
    }

    #[test]
    fn grep_bytes_regex_mode_matches_and_reports_bad_patterns() {
        let options = GrepSearchOptions {
            mode: GrepMode::Regex,
            ..Default::default()
        };

        let query = parse_grep_query("fn\\s+main");
        let (matches, regex_error) = grep_bytes(&query, &options, b"// x\nfn  main() {}\n");
        assert!(regex_error.is_none());
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].line_number, 2);

        let query = parse_grep_query("(unclosed");
        let (_, regex_error) = grep_bytes(&query, &options, b"(unclosed\n");
        assert!(regex_error.is_some());
    }

    #[test]
    fn byte_source_grep_pages_without_duplicates_or_gaps() {
        let query = parse_grep_query("needle");
        let options = GrepSearchOptions {
            mode: GrepMode::PlainText,
            ..Default::default()
        };
        let sources: Vec<&[u8]> = vec![
            b"needle one\nneedle two\nneedle three\n",
            b"needle four\nneedle five\n",
        ];
        let mut cursor = ByteSourceGrepCursor::default();
        let mut found = Vec::new();

        loop {
            let page = grep_byte_sources_page(
                &query,
                &options,
                sources.len(),
                cursor,
                2,
                std::time::Duration::MAX,
                None,
                |index| sources.get(index).copied(),
            )
            .unwrap();
            found.extend(
                page.matches
                    .into_iter()
                    .map(|item| (item.source, item.line_number)),
            );
            let Some(next) = page.next else {
                break;
            };
            assert_ne!(next, cursor, "a continuation cursor must make progress");
            cursor = next;
        }

        assert_eq!(found, vec![(0, 1), (0, 2), (0, 3), (1, 1), (1, 2)]);
    }

    #[test]
    fn byte_source_grep_rejects_non_progressing_requests() {
        let query = parse_grep_query("needle");
        let options = GrepSearchOptions::default();
        let source = b"needle\n".as_slice();

        let zero_limit = grep_byte_sources_page(
            &query,
            &options,
            1,
            ByteSourceGrepCursor::default(),
            0,
            std::time::Duration::MAX,
            None,
            |_| Some(source),
        );
        assert_eq!(zero_limit, Err(ByteSourceGrepError::ZeroPageLimit));

        let invalid_cursor = grep_byte_sources_page(
            &query,
            &options,
            1,
            ByteSourceGrepCursor {
                source: 1,
                match_offset: 1,
            },
            1,
            std::time::Duration::MAX,
            None,
            |_| Some(source),
        );
        assert_eq!(invalid_cursor, Err(ByteSourceGrepError::InvalidCursor));
    }

    #[test]
    fn grep_owned_replaces_disk_results_with_overlay_matches() {
        let dir = tempfile::tempdir().unwrap();
        let base = crate::path_utils::canonicalize(dir.path()).unwrap();
        let edited = base.join("edited.txt");
        let untouched = base.join("untouched.txt");
        std::fs::write(&edited, "needle on disk\n").unwrap();
        std::fs::write(&untouched, "needle elsewhere\n").unwrap();

        let mut picker = FilePicker::new(crate::file_picker::FilePickerOptions {
            base_path: base.to_string_lossy().into_owned(),
            watch: false,
            ..Default::default()
        })
        .unwrap();
        picker.collect_files().unwrap();

        let overlays = [ContentOverlay {
            path: edited.clone(),
            bytes: Arc::from(&b"first\nneedle in buffer\n"[..]),
            revision: 1,
        }];
        let query = parse_grep_query("needle");
        let options = GrepSearchOptions::default();

        let without_overlay_matches = picker.grep_owned(&query, &options, &overlays, false);
        assert!(
            without_overlay_matches
                .matches
                .iter()
                .all(|m| m.path.as_path() != edited.as_path()),
            "overlaid paths must not surface stale on-disk matches"
        );
        assert!(
            without_overlay_matches
                .matches
                .iter()
                .any(|m| m.path.as_path() == untouched.as_path())
        );

        let with_overlay_matches = picker.grep_owned(&query, &options, &overlays, true);
        let overlay_hit = with_overlay_matches
            .matches
            .iter()
            .find(|m| m.path.as_path() == edited.as_path())
            .expect("overlay match");
        assert_eq!(overlay_hit.line_number, 2);
        assert_eq!(overlay_hit.line_content, "needle in buffer");
    }
}
