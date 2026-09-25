use super::*;

fn assert_text(expected: &'static str) -> impl Fn(&Application) {
    move |app| {
        let (_, doc) = helix_view::focused_ref!(app.editor);
        assert_eq!(doc.text().to_string().replace("\r\n", "\n"), expected);
    }
}

/// `.` after `c` repeats the change, including the delete, not a bare insert.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeat_change_deletes_again() -> anyhow::Result<()> {
    test_key_sequence_with_input_text(
        None,
        ("#[a|]#aa\nbbb\nccc\n", "xcX<esc>jx.", "#[|]#"),
        &assert_text("X\nX\nccc\n"),
        false,
    )
    .await
}

/// Undo is not a change of its own: `.` still repeats the last insert.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undo_does_not_replace_what_dot_repeats() -> anyhow::Result<()> {
    test_key_sequence_with_input_text(
        None,
        // Starts empty: the harness sets up its input in the same undo step as the keys.
        // If `.` repeated the undo, the document would stay empty.
        ("#[\n|]#", "iX<esc>u.", "#[|]#"),
        &assert_text("X\n"),
        false,
    )
    .await
}

/// Repeating an insert that contains Enter works more than once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeat_insert_with_newline_twice() -> anyhow::Result<()> {
    test_key_sequence_with_input_text(
        None,
        ("#[a|]#\n", "ix<ret><esc>..", "#[|]#"),
        &assert_text("x\nx\nx\na\n"),
        false,
    )
    .await
}

/// Keys the frontend handles in insert mode (Tab here) are part of what `.` repeats.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeat_insert_includes_frontend_keys() -> anyhow::Result<()> {
    test_key_sequence_with_input_text(
        None,
        ("#[a|]#\nb\n", "i<tab>x<esc>jgh.", "#[|]#"),
        &|app| {
            let (_, doc) = helix_view::focused_ref!(app.editor);
            let text = doc.text().to_string().replace("\r\n", "\n");
            let lines: Vec<&str> = text.lines().collect();
            assert!(lines[0].starts_with(char::is_whitespace), "{text:?}");
            assert_eq!(lines[1], lines[0].replace('a', "b"), "{text:?}");
        },
        false,
    )
    .await
}
