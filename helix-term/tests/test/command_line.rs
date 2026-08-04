use super::*;

use helix_core::diagnostic::Severity;

#[cfg(windows)]
type TestTerminalEvent = crossterm::event::Event;
#[cfg(not(windows))]
type TestTerminalEvent = termina::event::Event;

fn send_keys(
    tx: &tokio::sync::mpsc::UnboundedSender<std::io::Result<TestTerminalEvent>>,
    keys: &str,
) -> anyhow::Result<()> {
    for key_event in helix_view::input::parse_macro(keys)? {
        tx.send(Ok(TestTerminalEvent::Key(key_event.into())))?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_accepts_delayed_terminal_input() -> anyhow::Result<()> {
    let mut app = AppBuilder::new().build()?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut events = tokio_stream::wrappers::UnboundedReceiverStream::new(rx);

    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        send_keys(&tx, ":q!<ret>").unwrap();
    });

    let exit_code =
        tokio::time::timeout(std::time::Duration::from_secs(5), app.run(&mut events)).await??;
    assert_eq!(exit_code, 0);
    assert!(app.editor.should_close());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_input_is_not_starved_by_background_handlers() -> anyhow::Result<()> {
    let mut app = AppBuilder::new().build()?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut events = tokio_stream::wrappers::UnboundedReceiverStream::new(rx);

    send_keys(
        &tx,
        ":set cursorline true<ret><space><esc>:set cursorline false<ret>:q!<ret>",
    )?;

    let exit_code =
        tokio::time::timeout(std::time::Duration::from_secs(5), app.run(&mut events)).await??;
    assert_eq!(exit_code, 0);
    assert!(app.editor.should_close());
    assert!(!app.editor.config().cursorline);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_completion() -> anyhow::Result<()> {
    test_key_sequence(
        &mut AppBuilder::new().build()?,
        Some(":asdf<ret>:theme d<C-n><tab>"),
        Some(&|app| {
            assert!(!app.editor.is_err());
        }),
        false,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_reset_anchor() -> anyhow::Result<()> {
    test_key_sequence(
        &mut AppBuilder::new().build()?,
        Some(":string wider than the terminal window causing the anchor location to be non zero which would panic when the line is deleted<C-u>"),
        Some(&|app| {
            assert!(!app.editor.is_err());
        }),
        false,
    )
    .await?;

    Ok(())
}

async fn test_statusline(
    line: &str,
    expected_status: &str,
    expected_severity: Severity,
) -> anyhow::Result<()> {
    test_key_sequence(
        &mut AppBuilder::new().build()?,
        Some(&format!("{line}<ret>")),
        Some(&|app| {
            let (status, &severity) = app.editor.get_status().unwrap();
            assert_eq!(
                severity, expected_severity,
                "'{line}' printed {severity:?}: {status}"
            );
            assert_eq!(status.as_ref(), expected_status);
        }),
        false,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn variable_expansion() -> anyhow::Result<()> {
    test_statusline(r#":echo %{cursor_line}"#, "1", Severity::Info).await?;
    // Double quotes can be used with expansions:
    test_statusline(
        r#":echo "line%{cursor_line}line""#,
        "line1line",
        Severity::Info,
    )
    .await?;
    // Within double quotes you can escape the percent token for an expansion by doubling it.
    test_statusline(
        r#":echo "%%{cursor_line}""#,
        "%{cursor_line}",
        Severity::Info,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unicode_expansion() -> anyhow::Result<()> {
    test_statusline(r#":echo %u{20}"#, " ", Severity::Info).await?;
    test_statusline(r#":echo %u{0020}"#, " ", Severity::Info).await?;
    test_statusline(r#":echo %u{25CF}"#, "●", Severity::Info).await?;
    // Not a valid Unicode codepoint:
    test_statusline(
        r#":echo %u{deadbeef}"#,
        "'echo': could not interpret 'deadbeef' as a Unicode character code",
        Severity::Error,
    )
    .await?;

    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_expansion() -> anyhow::Result<()> {
    test_statusline(
        r#":echo %sh{echo "hello world"}"#,
        "hello world",
        Severity::Info,
    )
    .await?;

    // Shell expansion is recursive.
    test_statusline(":echo %sh{echo '%{cursor_line}'}", "1", Severity::Info).await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn percent_escaping() -> anyhow::Result<()> {
    test_statusline(
        r#":sh echo hello 10%"#,
        "'run-shell-command': '%' was not properly escaped. Please use '%%'",
        Severity::Error,
    )
    .await?;
    Ok(())
}
