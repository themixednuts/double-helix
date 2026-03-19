use crate::doc_formatter::{DocumentFormatter, TextFormat};
use crate::text_annotations::{InlineAnnotation, Overlay, TextAnnotations};
use crate::{Position, Rope};

impl TextFormat {
    fn new_test(softwrap: bool) -> Self {
        TextFormat {
            soft_wrap: softwrap,
            tab_width: 2,
            max_wrap: 3,
            max_indent_retain: 4,
            wrap_indicator_highlight: None,
            // use a prime number to allow lining up too often with repeat
            viewport_width: 17,
            soft_wrap_at_text_width: false,
        }
    }
}

impl<'t> DocumentFormatter<'t> {
    fn collect_to_str(&mut self) -> String {
        use std::fmt::Write;
        let mut res = String::new();
        let viewport_width = self.text_fmt.viewport_width;
        let soft_wrap_at_text_width = self.text_fmt.soft_wrap_at_text_width;
        let mut line = 0;

        for grapheme in self {
            if grapheme.visual_pos.row != line {
                line += 1;
                assert_eq!(grapheme.visual_pos.row, line);
                write!(res, "\n{}", ".".repeat(grapheme.visual_pos.col)).unwrap();
            }
            if !soft_wrap_at_text_width {
                assert!(
                    grapheme.visual_pos.col <= viewport_width as usize,
                    "softwrapped failed {}<={viewport_width}",
                    grapheme.visual_pos.col
                );
            }
            write!(res, "{}", grapheme.raw).unwrap();
        }

        res
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GraphemeSnapshot {
    raw: String,
    visual_pos: Position,
    line_idx: usize,
    char_idx: usize,
}

fn collect_graphemes(formatter: &mut DocumentFormatter<'_>) -> Vec<GraphemeSnapshot> {
    formatter
        .map(|grapheme| GraphemeSnapshot {
            raw: grapheme.raw.to_string(),
            visual_pos: grapheme.visual_pos,
            line_idx: grapheme.line_idx,
            char_idx: grapheme.char_idx,
        })
        .collect()
}

fn softwrap_text(text: &str) -> String {
    DocumentFormatter::new_at_prev_checkpoint(
        text.into(),
        &TextFormat::new_test(true),
        &TextAnnotations::default(),
        0,
    )
    .collect_to_str()
}

#[test]
fn basic_softwrap() {
    assert_eq!(
        softwrap_text(&"foo ".repeat(10)),
        "foo foo foo foo \nfoo foo foo foo \nfoo foo  "
    );
    assert_eq!(
        softwrap_text(&"fooo ".repeat(10)),
        "fooo fooo fooo \nfooo fooo fooo \nfooo fooo fooo \nfooo  "
    );

    // check that we don't wrap unnecessarily
    assert_eq!(softwrap_text("\t\txxxx1xxxx2xx\n"), "    xxxx1xxxx2xx \n ");
}

#[test]
fn softwrap_indentation() {
    assert_eq!(
        softwrap_text("\t\tfoo1 foo2 foo3 foo4 foo5 foo6\n"),
        "    foo1 foo2 \n....foo3 foo4 \n....foo5 foo6 \n "
    );
    assert_eq!(
        softwrap_text("\t\t\tfoo1 foo2 foo3 foo4 foo5 foo6\n"),
        "      foo1 foo2 \nfoo3 foo4 foo5 \nfoo6 \n "
    );
}

#[test]
fn long_word_softwrap() {
    assert_eq!(
        softwrap_text("\t\txxxx1xxxx2xxxx3xxxx4xxxxx5xxxx6xxxx7xxx8xxxx9xxx\n"),
        "    xxxx1xxxx2xxx\n....x3xxxx4xxxxx5\n....xxxx6xxxx7xxx\n....8xxxx9xxx \n "
    );
    assert_eq!(
        softwrap_text("xxxxxxxx1xxxx2xxx\n"),
        "xxxxxxxx1xxxx2xxx\n \n "
    );
    assert_eq!(
        softwrap_text("\t\txxxx1xxxx 2xxxx3xxxx4xxxx5xxxx6xxxx7xxxx8xxxxxxx9xxx\n"),
        "    xxxx1xxxx \n....2xxxx3xxxx4xx\n....xx5xxxx6xxxx7\n....xxxx8xxxxxxx9\n....xxx \n "
    );
    assert_eq!(
        softwrap_text("\t\txxxx1xxx 2xxxx3xxxx4xxxxx5xxxx6xxxx7xxxxx8xxxx9xx\n"),
        "    xxxx1xxx 2xxx\n....x3xxxx4xxxxx5\n....xxxx6xxxx7xxx\n....xx8xxxx9xx \n "
    );
}

#[test]
fn softwrap_multichar_grapheme() {
    assert_eq!(
        softwrap_text("xxxx xxxx xxx a\u{0301}bc\n"),
        "xxxx xxxx xxx \nábc \n "
    )
}

fn softwrap_text_at_text_width(text: &str) -> String {
    let mut text_fmt = TextFormat::new_test(true);
    text_fmt.soft_wrap_at_text_width = true;
    let annotations = TextAnnotations::default();
    let mut formatter =
        DocumentFormatter::new_at_prev_checkpoint(text.into(), &text_fmt, &annotations, 0);
    formatter.collect_to_str()
}
#[test]
fn long_word_softwrap_text_width() {
    assert_eq!(
        softwrap_text_at_text_width("xxxxxxxx1xxxx2xxx\nxxxxxxxx1xxxx2xxx"),
        "xxxxxxxx1xxxx2xxx \nxxxxxxxx1xxxx2xxx "
    );
}

fn overlay_text(text: &str, char_pos: usize, softwrap: bool, overlays: &[Overlay]) -> String {
    DocumentFormatter::new_at_prev_checkpoint(
        text.into(),
        &TextFormat::new_test(softwrap),
        TextAnnotations::default().add_overlay(overlays, None),
        char_pos,
    )
    .collect_to_str()
}

#[test]
fn overlay() {
    assert_eq!(
        overlay_text(
            "foobar",
            0,
            false,
            &[Overlay::new(0, "X"), Overlay::new(2, "\t")],
        ),
        "Xo  bar "
    );
    assert_eq!(
        overlay_text(
            &"foo ".repeat(10),
            0,
            true,
            &[
                Overlay::new(2, "\t"),
                Overlay::new(5, "\t"),
                Overlay::new(16, "X"),
            ]
        ),
        "fo   f  o foo \nfoo Xoo foo foo \nfoo foo foo  "
    );
}

fn annotate_text(text: &str, softwrap: bool, annotations: &[InlineAnnotation]) -> String {
    DocumentFormatter::new_at_prev_checkpoint(
        text.into(),
        &TextFormat::new_test(softwrap),
        TextAnnotations::default().add_inline_annotations(annotations, None),
        0,
    )
    .collect_to_str()
}

#[test]
fn annotation() {
    assert_eq!(
        annotate_text("bar", false, &[InlineAnnotation::new(0, "foo")]),
        "foobar "
    );
    assert_eq!(
        annotate_text(
            &"foo ".repeat(10),
            true,
            &[InlineAnnotation::new(0, "foo ")]
        ),
        "foo foo foo foo \nfoo foo foo foo \nfoo foo foo  "
    );
}

#[test]
fn annotation_and_overlay() {
    let annotations = [InlineAnnotation {
        char_idx: 0,
        text: "fooo".into(),
    }];
    let overlay = [Overlay {
        char_idx: 0,
        grapheme: "\t".into(),
    }];
    assert_eq!(
        DocumentFormatter::new_at_prev_checkpoint(
            "bbar".into(),
            &TextFormat::new_test(false),
            TextAnnotations::default()
                .add_inline_annotations(annotations.as_slice(), None)
                .add_overlay(overlay.as_slice(), None),
            0,
        )
        .collect_to_str(),
        "fooo  bar "
    );
}

#[test]
fn checkpoint_resume_matches_full_formatter_stream() {
    let rope = Rope::from("0123456789\tabcdefghij\nsecond line\n");
    let slice = rope.slice(..);
    let annotations = TextAnnotations::default();
    let fmt = TextFormat::new_test(false);

    let mut full = DocumentFormatter::new_at_prev_checkpoint(slice, &fmt, &annotations, 0);
    let full_graphemes = collect_graphemes(&mut full);
    let checkpoint = &full_graphemes[8];

    let mut resumed = DocumentFormatter::new_at_checkpoint(
        slice,
        &fmt,
        &annotations,
        checkpoint.char_idx,
        checkpoint.visual_pos,
    );

    assert_eq!(
        collect_graphemes(&mut resumed),
        full_graphemes[8..].to_vec()
    );
}

#[test]
fn skip_to_next_line_preserves_formatter_state() {
    let rope = Rope::from("first giant line\twith tab\nsecond line\n");
    let slice = rope.slice(..);
    let annotations = TextAnnotations::default();
    let fmt = TextFormat::new_test(false);

    let mut formatter = DocumentFormatter::new_at_prev_checkpoint(slice, &fmt, &annotations, 0);
    for _ in 0..5 {
        formatter.next().unwrap();
    }

    let second_line_char = slice.line_to_char(1);
    assert_eq!(formatter.skip_to_next_line(), Some(second_line_char));
    assert_eq!(formatter.next_char_pos(), second_line_char);
    assert_eq!(formatter.next_visual_pos(), Position::new(1, 0));

    let mut resumed = DocumentFormatter::new_at_checkpoint(
        slice,
        &fmt,
        &annotations,
        second_line_char,
        Position::new(1, 0),
    );

    assert_eq!(
        collect_graphemes(&mut formatter),
        collect_graphemes(&mut resumed)
    );
}

#[test]
#[ignore = "targeted local repro for giant-line skip_to_next_line performance"]
fn skip_to_next_line_giant_line_repro() {
    let text = format!("{}\n{}", "a".repeat(900_000), "b".repeat(900_000));
    let rope = Rope::from(text);
    let slice = rope.slice(..);
    let annotations = TextAnnotations::default();
    let fmt = TextFormat::new_test(false);
    let mut formatter = DocumentFormatter::new_at_prev_checkpoint(slice, &fmt, &annotations, 0);

    let mut saw_offscreen = false;
    while let Some(grapheme) = formatter.next() {
        if grapheme.visual_pos.col >= fmt.viewport_width as usize {
            saw_offscreen = true;
            let start = std::time::Instant::now();
            let next = formatter.skip_to_next_line();
            eprintln!(
                "skip_to_next_line_giant_line_repro: elapsed_us={} next_char={next:?}",
                start.elapsed().as_micros(),
            );
            break;
        }
    }

    assert!(saw_offscreen);
}

#[test]
#[ignore = "targeted local repro for render-loop offscreen-right skipping on giant lines"]
fn render_loop_skip_right_giant_line_repro() {
    let text = format!("{}\n{}", "a".repeat(900_000), "b".repeat(900_000));
    let rope = Rope::from(text);
    let slice = rope.slice(..);
    let annotations = TextAnnotations::default();
    let fmt = TextFormat::new_test(false);
    let mut formatter = DocumentFormatter::new_at_prev_checkpoint(slice, &fmt, &annotations, 0);
    let viewport_right = 160usize;
    let mut transitions = Vec::new();
    let mut next_calls = 0usize;

    while let Some(grapheme) = formatter.next() {
        next_calls += 1;
        if grapheme.visual_pos.col < viewport_right {
            continue;
        }

        let before_char = grapheme.char_idx;
        let before_row = grapheme.visual_pos.row;
        let next_line_char = formatter.skip_to_next_line();
        let after_char = formatter.next_char_pos();
        let after_pos = formatter.next_visual_pos();
        transitions.push((
            before_row,
            before_char,
            next_line_char,
            after_char,
            after_pos.row,
            after_pos.col,
        ));
        if transitions.len() >= 5 {
            break;
        }
    }

    eprintln!("render_loop_skip_right_giant_line_repro: next_calls={next_calls}");
    for (idx, transition) in transitions.iter().enumerate() {
        eprintln!("  transition[{idx}]={transition:?}");
    }

    assert!(!transitions.is_empty());
    assert!(transitions.len() <= 2);
    assert_eq!(transitions.last().and_then(|transition| transition.2), None);
}

#[test]
fn skip_to_next_line_preserves_state_across_multiple_lines() {
    let rope = Rope::from("first giant line\twith tab\nsecond line\nthird line\n");
    let slice = rope.slice(..);
    let annotations = TextAnnotations::default();
    let fmt = TextFormat::new_test(false);

    let mut formatter = DocumentFormatter::new_at_prev_checkpoint(slice, &fmt, &annotations, 0);
    for _ in 0..5 {
        formatter.next().unwrap();
    }

    let second_line_char = slice.line_to_char(1);
    let third_line_char = slice.line_to_char(2);

    assert_eq!(formatter.skip_to_next_line(), Some(second_line_char));
    assert_eq!(formatter.next_char_pos(), second_line_char);
    assert_eq!(formatter.next_visual_pos(), Position::new(1, 0));

    assert_eq!(formatter.skip_to_next_line(), Some(third_line_char));
    assert_eq!(formatter.next_char_pos(), third_line_char);
    assert_eq!(formatter.next_visual_pos(), Position::new(2, 0));

    let mut resumed = DocumentFormatter::new_at_checkpoint(
        slice,
        &fmt,
        &annotations,
        third_line_char,
        Position::new(2, 0),
    );

    assert_eq!(
        collect_graphemes(&mut formatter),
        collect_graphemes(&mut resumed)
    );
}

#[test]
fn checkpoint_resume_derives_line_from_char_idx() {
    let rope = Rope::from("first line\nsecond line\n");
    let slice = rope.slice(..);
    let annotations = TextAnnotations::default();
    let fmt = TextFormat::new_test(false);
    let second_line_char = slice.line_to_char(1);

    let mut resumed = DocumentFormatter::new_at_checkpoint(
        slice,
        &fmt,
        &annotations,
        second_line_char,
        Position::new(10, 0),
    );

    let grapheme = resumed.next().unwrap();
    assert_eq!(grapheme.line_idx, 1);
    assert_eq!(grapheme.char_idx, second_line_char);
}
