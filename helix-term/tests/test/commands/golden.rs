//! Golden tests for command behavior.
//!
//! These tests are intended to catch regressions during the frontend decoupling refactor.
//! They exercise: motion/edit commands, command palette, prompt flows.
//! Update expected output when behavior intentionally changes.

use super::*;

use indoc::indoc;
use std::io::Read;

/// Motion: j/k move cursor vertically.
#[tokio::test(flavor = "multi_thread")]
async fn golden_motion_jk() -> anyhow::Result<()> {
    test((
        indoc! {"\
            #[a|]#
            b
            c"},
        "jj",
        indoc! {"\
            a
            b
            #[c|]#"},
        LineFeedHandling::AsIs,
    ))
    .await?;

    test((
        indoc! {"\
            a
            b
            #[c|]#"},
        "kk",
        indoc! {"\
            #[a|]#
            b
            c"},
        LineFeedHandling::AsIs,
    ))
    .await?;

    Ok(())
}

/// Motion: w moves to next word start.
/// "ww" from "f" moves to end of "bar " (selection 4-8).
#[tokio::test(flavor = "multi_thread")]
async fn golden_motion_w() -> anyhow::Result<()> {
    test((
        "#[f|]#oo bar baz",
        "ww",
        "foo #[bar |]#baz",
        LineFeedHandling::AsIs,
    ))
    .await?;

    Ok(())
}

/// Edit: insert mode, type, escape.
/// After insert, cursor is at end of "hello" (position 5).
#[tokio::test(flavor = "multi_thread")]
async fn golden_edit_insert() -> anyhow::Result<()> {
    test(("#[|]#", "ihello<esc>", "hello#[|]#", LineFeedHandling::AsIs)).await?;

    Ok(())
}

/// Command palette: open (space ?) and dismiss (esc). Verify no error.
#[tokio::test(flavor = "multi_thread")]
async fn golden_command_palette_open_dismiss() -> anyhow::Result<()> {
    test_key_sequence(
        &mut AppBuilder::new().build()?,
        Some("<space>?<esc>"),
        Some(&|app| {
            assert!(
                !app.editor.is_err(),
                "command palette open/dismiss should not error"
            );
        }),
        false,
    )
    .await?;

    Ok(())
}

/// Prompt: :echo and verify status.
#[tokio::test(flavor = "multi_thread")]
async fn golden_prompt_echo() -> anyhow::Result<()> {
    test_key_sequence(
        &mut AppBuilder::new().build()?,
        Some(":echo hello<ret>"),
        Some(&|app| {
            let (status, _) = app.editor.get_status().unwrap();
            assert_eq!(status.as_ref(), "hello");
        }),
        false,
    )
    .await?;

    Ok(())
}

/// Typable command: :write with path (no LSP).
#[tokio::test(flavor = "multi_thread")]
async fn golden_write_quit() -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    let mut app = AppBuilder::new().with_file(file.path(), None).build()?;

    test_key_sequence(&mut app, Some("ihello world<esc>:wq<ret>"), None, true).await?;

    reload_file(&mut file).unwrap();
    let mut content = String::new();
    file.as_file_mut().read_to_string(&mut content)?;
    assert_eq!(
        content,
        LineFeedHandling::Native.apply("hello world\n"),
        "file content after :wq"
    );

    Ok(())
}
