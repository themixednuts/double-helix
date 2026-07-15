use std::{
    fs,
    io::{Read, Seek, Write},
    ops::RangeInclusive,
    path::Path,
};

use helix_core::diagnostic::Severity;
use helix_stdx::path;
use helix_view::input::parse_macro;
use tokio_stream::wrappers::UnboundedReceiverStream;

use super::*;

#[cfg(windows)]
use crossterm::event::{Event, KeyEvent};
#[cfg(not(windows))]
use termina::{event::KeyEvent, Event};

#[cfg(unix)]
fn create_file_symlink(original: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(original, link)
}

#[cfg(windows)]
fn create_file_symlink(original: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(original, link)
}

fn is_missing_symlink_privilege(err: &std::io::Error) -> bool {
    cfg!(windows) && err.raw_os_error() == Some(1314)
}

fn try_create_file_symlink(original: &Path, link: &Path) -> anyhow::Result<bool> {
    match create_file_symlink(original, link) {
        Ok(()) => Ok(true),
        Err(err) if is_missing_symlink_privilege(&err) => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn assert_current_document_unmodified(app: &Application) {
    let doc = helix_view::focused_ref!(app.editor).1;
    assert!(!doc.is_modified(), "current document remains modified");
}

fn assert_all_documents_unmodified(app: &Application) {
    let modified: Vec<_> = app
        .editor
        .documents()
        .filter(|doc| doc.is_modified())
        .map(|doc| doc.display_name())
        .collect();
    assert!(
        modified.is_empty(),
        "modified documents remain: {modified:?}"
    );
}

async fn send_keys_and_focus_lost(app: &mut Application, keys: &str) -> anyhow::Result<()> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut rx_stream = UnboundedReceiverStream::new(rx);
    for key_event in parse_macro(keys)? {
        tx.send(Ok(Event::Key(KeyEvent::from(key_event))))?;
    }
    tx.send(Ok(Event::FocusLost))?;
    app.event_loop_until_idle(&mut rx_stream).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_exit_w_buffer_w_path() -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    let mut app = helpers::AppBuilder::new()
        .with_file(file.path(), None)
        .build()?;
    // Check for write operation on given path and edited buffer
    test_key_sequence(
        &mut app,
        Some("iBecause of the obvious threat to untold numbers of citizens due to the crisis that is even now developing, this radio station will remain on the air day and night.<ret><esc>:x<ret>"),
        None,
        true,
    )
    .await?;

    reload_file(&mut file).unwrap();
    let mut file_content = String::new();
    file.as_file_mut().read_to_string(&mut file_content)?;

    assert_eq!(
        LineFeedHandling::Native.apply("Because of the obvious threat to untold numbers of citizens due to the crisis that is even now developing, this radio station will remain on the air day and night.\n"),
        file_content
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_exit_wo_buffer_w_path() -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    let mut app = helpers::AppBuilder::new()
        .with_file(file.path(), None)
        .build()?;

    helpers::run_event_loop_until_idle(&mut app).await;

    file.as_file_mut()
        .write_all("extremely important content".as_bytes())?;
    file.as_file_mut().flush()?;
    file.as_file_mut().sync_all()?;

    test_key_sequence(&mut app, Some(":x<ret>"), None, true).await?;

    reload_file(&mut file).unwrap();
    let mut file_content = String::new();
    file.read_to_string(&mut file_content)?;
    // check that nothing is written to file
    assert_eq!("extremely important content", file_content);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_exit_wo_buffer_wo_path() -> anyhow::Result<()> {
    test_key_sequence(
        &mut AppBuilder::new().build()?,
        Some(":x<ret>"),
        Some(&|app| {
            assert!(!app.editor.is_err());
        }),
        true,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_quit_last_view_does_not_render_empty_tree() -> anyhow::Result<()> {
    test_key_sequence(&mut AppBuilder::new().build()?, Some(":q<ret>"), None, true).await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_exit_w_buffer_wo_file() -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    test_key_sequence(
        // try to write without destination
        &mut AppBuilder::new().build()?,
        Some("itest<esc>:x<ret>"),
        None,
        false,
    )
    .await?;
    test_key_sequence(
        // try to write with path succeeds
        &mut AppBuilder::new().build()?,
        Some(format!("iMicCheck<esc>:x {}<ret>", file.path().to_string_lossy()).as_ref()),
        Some(&|app| {
            assert!(!app.editor.is_err());
        }),
        true,
    )
    .await?;

    helpers::assert_file_has_content(&mut file, &LineFeedHandling::Native.apply("MicCheck"))?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_quit_fail() -> anyhow::Result<()> {
    let file = helpers::new_readonly_tempfile()?;
    let mut app = helpers::AppBuilder::new()
        .with_file(file.path(), None)
        .build()?;

    test_key_sequence(
        &mut app,
        Some("ihello<esc>:wq<ret>"),
        Some(&|app| {
            let mut docs: Vec<_> = app.editor.documents().collect();
            assert_eq!(1, docs.len());

            let doc = docs.pop().unwrap();
            assert_eq!(Some(&path::normalize(file.path())), doc.path());
            assert_eq!(&Severity::Error, app.editor.get_status().unwrap().1);
        }),
        false,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_buffer_close_concurrent() -> anyhow::Result<()> {
    test_key_sequences(
        &mut helpers::AppBuilder::new().build()?,
        vec![
            (
                None,
                Some(&|app| {
                    assert_eq!(1, app.editor.documents().count());
                    assert!(!app.editor.is_err());
                }),
            ),
            (
                Some("ihello<esc>:new<ret>"),
                Some(&|app| {
                    assert_eq!(2, app.editor.documents().count());
                    assert!(!app.editor.is_err());
                }),
            ),
            (
                Some(":buffer<minus>close<ret>"),
                Some(&|app| {
                    assert_eq!(1, app.editor.documents().count());
                    assert!(!app.editor.is_err());
                }),
            ),
        ],
        false,
    )
    .await?;

    // verify if writes are queued up, it finishes them before closing the buffer
    let mut file = tempfile::NamedTempFile::new()?;
    let mut command = String::new();
    const RANGE: RangeInclusive<i32> = 1..=1000;

    for i in RANGE {
        let cmd = format!("i{}<esc>:w!<ret>", i);
        command.push_str(&cmd);
    }

    command.push_str(":buffer<minus>close<ret>");

    let mut app = helpers::AppBuilder::new()
        .with_file(file.path(), None)
        .build()?;

    test_key_sequence(
        &mut app,
        Some(&command),
        Some(&|app| {
            assert!(!app.editor.is_err(), "error: {:?}", app.editor.get_status());

            let doc = app.editor.document_by_path(file.path());
            assert!(doc.is_none(), "found doc: {:?}", doc);
        }),
        false,
    )
    .await?;

    let expected = RANGE.map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
    helpers::assert_file_has_content(
        &mut file,
        &LineFeedHandling::Native.apply(&format!("{expected}\n")),
    )?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write() -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    let mut app = helpers::AppBuilder::new()
        .with_file(file.path(), None)
        .build()?;

    test_key_sequence(
        &mut app,
        Some("ithe gostak distims the doshes<ret><esc>:w<ret>"),
        None,
        false,
    )
    .await?;

    reload_file(&mut file).unwrap();
    let mut file_content = String::new();
    file.as_file_mut().read_to_string(&mut file_content)?;

    assert_eq!(
        LineFeedHandling::Native.apply("the gostak distims the doshes"),
        file_content
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_family_plain_write_persists_bytes_and_clears_modified() -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    let mut app = helpers::AppBuilder::new()
        .with_file(file.path(), None)
        .build()?;

    test_key_sequence(
        &mut app,
        Some("iplain write reached disk<esc>:w<ret>"),
        Some(&assert_current_document_unmodified),
        false,
    )
    .await?;

    helpers::assert_file_has_content(
        &mut file,
        &LineFeedHandling::Native.apply("plain write reached disk"),
    )?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_family_save_as_persists_bytes_and_clears_modified() -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;

    test_key_sequence(
        &mut AppBuilder::new().build()?,
        Some(
            format!(
                "isave as reached disk<esc>:w {}<ret>",
                file.path().to_string_lossy()
            )
            .as_ref(),
        ),
        Some(&|app| {
            assert_current_document_unmodified(app);
            let doc = helix_view::focused_ref!(app.editor).1;
            assert_eq!(Some(&path::normalize(file.path())), doc.path());
        }),
        false,
    )
    .await?;

    helpers::assert_file_has_content(
        &mut file,
        &LineFeedHandling::Native.apply("save as reached disk"),
    )?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_family_force_write_persists_bytes_and_clears_modified() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("missing-parent").join("forced.txt");

    test_key_sequence(
        &mut AppBuilder::new().build()?,
        Some(
            format!(
                "iforce write reached disk<esc>:w! {}<ret>",
                path.to_string_lossy()
            )
            .as_ref(),
        ),
        Some(&assert_current_document_unmodified),
        false,
    )
    .await?;

    assert_eq!(
        LineFeedHandling::Native.apply("force write reached disk"),
        fs::read_to_string(&path)?
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_while_file_explorer_focused() -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    let mut app = helpers::AppBuilder::new()
        .with_file(file.path(), None)
        .build()?;

    test_key_sequence(
        &mut app,
        Some("ifile explorer write reaches disk<esc><space>e:w<ret>"),
        None,
        false,
    )
    .await?;

    reload_file(&mut file).unwrap();
    let mut file_content = String::new();
    file.as_file_mut().read_to_string(&mut file_content)?;

    assert_eq!(
        LineFeedHandling::Native.apply("file explorer write reaches disk"),
        file_content
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_overwrite_protection() -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    let mut app = helpers::AppBuilder::new()
        .with_file(file.path(), None)
        .build()?;

    helpers::run_event_loop_until_idle(&mut app).await;

    file.as_file_mut()
        .write_all("extremely important content".as_bytes())?;

    file.as_file_mut().flush()?;
    file.as_file_mut().sync_all()?;

    test_key_sequence(&mut app, Some("iOverwriteData<esc>:x<ret>"), None, false).await?;

    reload_file(&mut file).unwrap();
    let mut file_content = String::new();
    file.read_to_string(&mut file_content)?;

    assert_eq!("extremely important content", file_content);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_quit() -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    let mut app = helpers::AppBuilder::new()
        .with_file(file.path(), None)
        .build()?;

    test_key_sequence(
        &mut app,
        Some("ithe gostak distims the doshes<ret><esc>:wq<ret>"),
        None,
        true,
    )
    .await?;

    reload_file(&mut file).unwrap();

    let mut file_content = String::new();
    file.read_to_string(&mut file_content)?;

    assert_eq!(
        LineFeedHandling::Native.apply("the gostak distims the doshes"),
        file_content
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_family_force_write_quit_persists_before_exit() -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    let mut app = helpers::AppBuilder::new()
        .with_file(file.path(), None)
        .build()?;

    test_key_sequence(
        &mut app,
        Some("iforce write quit reached disk<esc>:wq!<ret>"),
        None,
        true,
    )
    .await?;

    helpers::assert_file_has_content(
        &mut file,
        &LineFeedHandling::Native.apply("force write quit reached disk"),
    )?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_family_write_all_persists_bytes_and_clears_modified() -> anyhow::Result<()> {
    let mut file1 = tempfile::NamedTempFile::new()?;
    let mut file2 = tempfile::NamedTempFile::new()?;
    let mut app = helpers::AppBuilder::new()
        .with_file(file1.path(), None)
        .with_input_text("#[f|]#irst write-all buffer")
        .build()?;

    test_key_sequence(
        &mut app,
        Some(&format!(
            ":o {}<ret>isecond write-all buffer<esc>:wa<ret>",
            file2.path().to_string_lossy()
        )),
        Some(&assert_all_documents_unmodified),
        false,
    )
    .await?;

    helpers::assert_file_has_content(
        &mut file1,
        &LineFeedHandling::Native.apply("first write-all buffer"),
    )?;
    helpers::assert_file_has_content(
        &mut file2,
        &LineFeedHandling::Native.apply("second write-all buffer"),
    )?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_family_write_all_quit_variants_persist_before_exit() -> anyhow::Result<()> {
    for command in [":wqa<ret>", ":xa<ret>", ":wqa!<ret>"] {
        let mut file1 = tempfile::NamedTempFile::new()?;
        let mut file2 = tempfile::NamedTempFile::new()?;
        let mut app = helpers::AppBuilder::new()
            .with_file(file1.path(), None)
            .with_input_text("#[f|]#irst all-quit buffer")
            .build()?;

        test_key_sequence(
            &mut app,
            Some(&format!(
                ":o {}<ret>isecond all-quit buffer<esc>{command}",
                file2.path().to_string_lossy()
            )),
            None,
            true,
        )
        .await?;

        helpers::assert_file_has_content(
            &mut file1,
            &LineFeedHandling::Native.apply("first all-quit buffer"),
        )?;
        helpers::assert_file_has_content(
            &mut file2,
            &LineFeedHandling::Native.apply("second all-quit buffer"),
        )?;
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_family_write_buffer_close_persists_before_close() -> anyhow::Result<()> {
    let mut file1 = tempfile::NamedTempFile::new()?;
    let file2 = tempfile::NamedTempFile::new()?;
    let mut app = helpers::AppBuilder::new()
        .with_file(file1.path(), None)
        .build()?;

    test_key_sequence(
        &mut app,
        Some(&format!(
            "iwrite buffer close reached disk<esc>:o {}<ret>:buffer-previous<ret>:write-buffer-close<ret>",
            file2.path().to_string_lossy()
        )),
        Some(&|app| {
            assert!(app.editor.document_by_path(file1.path()).is_none());
            assert!(app.editor.document_by_path(file2.path()).is_some());
        }),
        false,
    )
    .await?;

    helpers::assert_file_has_content(
        &mut file1,
        &LineFeedHandling::Native.apply("write buffer close reached disk"),
    )?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_family_auto_save_after_delay_persists_bytes() -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    let mut app = helpers::AppBuilder::new()
        .with_config(Config {
            editor: helix_view::editor::Config {
                auto_save: helix_view::editor::AutoSave {
                    after_delay: helix_view::editor::AutoSaveAfterDelay {
                        enable: true,
                        timeout: 1,
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        })
        .with_file(file.path(), None)
        .build()?;

    test_key_sequence(
        &mut app,
        Some("iauto-save after delay reached disk<esc>"),
        Some(&assert_current_document_unmodified),
        false,
    )
    .await?;

    helpers::assert_file_has_content(
        &mut file,
        &LineFeedHandling::Native.apply("auto-save after delay reached disk"),
    )?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_family_auto_save_focus_lost_persists_bytes() -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    let mut app = helpers::AppBuilder::new()
        .with_config(Config {
            editor: helix_view::editor::Config {
                auto_save: helix_view::editor::AutoSave {
                    focus_lost: true,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        })
        .with_file(file.path(), None)
        .build()?;

    send_keys_and_focus_lost(&mut app, "iauto-save focus lost reached disk<esc>").await?;
    assert_current_document_unmodified(&app);

    test_key_sequence(&mut app, None, None, false).await?;

    helpers::assert_file_has_content(
        &mut file,
        &LineFeedHandling::Native.apply("auto-save focus lost reached disk"),
    )?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_concurrent() -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    let mut command = String::new();
    const RANGE: RangeInclusive<i32> = 1..=1000;
    let mut app = helpers::AppBuilder::new()
        .with_file(file.path(), None)
        .build()?;

    for i in RANGE {
        let cmd = format!("i{}<esc>:w!<ret>", i);
        command.push_str(&cmd);
    }

    test_key_sequence(&mut app, Some(&command), None, false).await?;

    reload_file(&mut file).unwrap();
    let mut file_content = String::new();
    file.read_to_string(&mut file_content)?;
    let expected = RANGE.map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
    assert_eq!(
        LineFeedHandling::Native.apply(&format!("{expected}\n")),
        file_content
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_fail_mod_flag() -> anyhow::Result<()> {
    let file = helpers::new_readonly_tempfile()?;
    let mut app = helpers::AppBuilder::new()
        .with_file(file.path(), None)
        .build()?;

    test_key_sequences(
        &mut app,
        vec![
            (
                None,
                Some(&|app| {
                    let doc = helix_view::focused_ref!(app.editor).1;
                    assert!(!doc.is_modified());
                }),
            ),
            (
                Some("ihello<esc>"),
                Some(&|app| {
                    let doc = helix_view::focused_ref!(app.editor).1;
                    assert!(doc.is_modified());
                }),
            ),
            (
                Some(":w<ret>"),
                Some(&|app| {
                    assert_eq!(&Severity::Error, app.editor.get_status().unwrap().1);

                    let doc = helix_view::focused_ref!(app.editor).1;
                    assert!(doc.is_modified());
                }),
            ),
        ],
        false,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_scratch_to_new_path() -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;

    test_key_sequence(
        &mut AppBuilder::new().build()?,
        Some(format!("ihello<esc>:w {}<ret>", file.path().to_string_lossy()).as_ref()),
        Some(&|app| {
            assert!(!app.editor.is_err());

            let mut docs: Vec<_> = app.editor.documents().collect();
            assert_eq!(1, docs.len());

            let doc = docs.pop().unwrap();
            assert_eq!(Some(&path::normalize(file.path())), doc.path());
        }),
        false,
    )
    .await?;

    helpers::assert_file_has_content(&mut file, &LineFeedHandling::Native.apply("hello"))?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_scratch_to_new_path_force_creates_file() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let new_path = dir.path().join("new-file.txt");

    test_key_sequence(
        &mut AppBuilder::new().build()?,
        Some(format!("ihello<esc>:w! {}<ret>", new_path.to_string_lossy()).as_ref()),
        Some(&|app| {
            assert!(!app.editor.is_err());

            let mut docs: Vec<_> = app.editor.documents().collect();
            assert_eq!(1, docs.len());

            let doc = docs.pop().unwrap();
            assert_eq!(Some(&path::normalize(&new_path)), doc.path());
        }),
        false,
    )
    .await?;

    let file_content = std::fs::read_to_string(&new_path)?;
    assert_eq!(file_content, LineFeedHandling::Native.apply("hello"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_scratch_no_path_fails() -> anyhow::Result<()> {
    helpers::test_key_sequence_with_input_text(
        None,
        ("#[\n|]#", "ihello<esc>:w<ret>", "hello#[\n|]#"),
        &|app| {
            assert!(app.editor.is_err());

            let mut docs: Vec<_> = app.editor.documents().collect();
            assert_eq!(1, docs.len());

            let doc = docs.pop().unwrap();
            assert_eq!(None, doc.path());
        },
        false,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_auto_format_fails_still_writes() -> anyhow::Result<()> {
    let mut file = tempfile::Builder::new().suffix(".rs").tempfile()?;

    let lang_conf = indoc! {r#"
            [[language]]
            name = "rust"
            formatter = { command = "bash", args = [ "-c", "exit 1" ] }
        "#};

    let mut app = helpers::AppBuilder::new()
        .with_file(file.path(), None)
        .with_input_text("#[l|]#et foo = 0;\n")
        .with_lang_loader(helpers::test_syntax_loader(Some(lang_conf.into())))
        .build()?;

    test_key_sequences(&mut app, vec![(Some(":w<ret>"), None)], false).await?;

    // file still saves
    helpers::assert_file_has_content(&mut file, "let foo = 0;\n")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_new_path() -> anyhow::Result<()> {
    let mut file1 = tempfile::NamedTempFile::new().unwrap();
    let mut file2 = tempfile::NamedTempFile::new().unwrap();
    let mut app = helpers::AppBuilder::new()
        .with_file(file1.path(), None)
        .build()?;

    test_key_sequences(
        &mut app,
        vec![
            (
                Some("ii can eat glass, it will not hurt me<ret><esc>:w<ret>"),
                Some(&|app| {
                    let doc = helix_view::focused_ref!(app.editor).1;
                    assert!(!app.editor.is_err());
                    assert_eq!(&path::normalize(file1.path()), doc.path().unwrap());
                }),
            ),
            (
                Some(&format!(":w {}<ret>", file2.path().to_string_lossy())),
                Some(&|app| {
                    let doc = helix_view::focused_ref!(app.editor).1;
                    assert!(!app.editor.is_err());
                    assert_eq!(&path::normalize(file2.path()), doc.path().unwrap());
                    assert!(app.editor.document_by_path(file1.path()).is_none());
                }),
            ),
        ],
        false,
    )
    .await?;

    helpers::assert_file_has_content(
        &mut file1,
        &LineFeedHandling::Native.apply("i can eat glass, it will not hurt me\n"),
    )?;

    helpers::assert_file_has_content(
        &mut file2,
        &LineFeedHandling::Native.apply("i can eat glass, it will not hurt me\n"),
    )?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_fail_new_path() -> anyhow::Result<()> {
    let file = helpers::new_readonly_tempfile()?;

    test_key_sequences(
        &mut AppBuilder::new().build()?,
        vec![
            (
                None,
                Some(&|app| {
                    let doc = helix_view::focused_ref!(app.editor).1;
                    assert_ne!(
                        Some(&Severity::Error),
                        app.editor.get_status().map(|status| status.1)
                    );
                    assert_eq!(None, doc.path());
                }),
            ),
            (
                Some(&format!(":w {}<ret>", file.path().to_string_lossy())),
                Some(&|app| {
                    let doc = helix_view::focused_ref!(app.editor).1;
                    assert_eq!(
                        Some(&Severity::Error),
                        app.editor.get_status().map(|status| status.1)
                    );
                    assert_eq!(None, doc.path());
                }),
            ),
        ],
        false,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_utf_bom_file() -> anyhow::Result<()> {
    // "ABC" with utf8 bom
    const UTF8_FILE: [u8; 6] = [0xef, 0xbb, 0xbf, b'A', b'B', b'C'];

    // "ABC" in UTF16 with bom
    const UTF16LE_FILE: [u8; 8] = [0xff, 0xfe, b'A', 0x00, b'B', 0x00, b'C', 0x00];
    const UTF16BE_FILE: [u8; 8] = [0xfe, 0xff, 0x00, b'A', 0x00, b'B', 0x00, b'C'];

    edit_file_with_content(&UTF8_FILE).await?;
    edit_file_with_content(&UTF16LE_FILE).await?;
    edit_file_with_content(&UTF16BE_FILE).await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_trim_trailing_whitespace() -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    let mut app = helpers::AppBuilder::new()
        .with_config(Config {
            editor: helix_view::editor::Config {
                trim_trailing_whitespace: true,
                ..Default::default()
            },
            ..Default::default()
        })
        .with_file(file.path(), None)
        .with_input_text(LineFeedHandling::Native.apply("#[f|]#oo      \n\n \nbar      "))
        .build()?;

    test_key_sequence(&mut app, Some(":w<ret>"), None, false).await?;

    helpers::assert_file_has_content(&mut file, &LineFeedHandling::Native.apply("foo\n\n\nbar"))?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_trim_final_newlines() -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    let mut app = helpers::AppBuilder::new()
        .with_config(Config {
            editor: helix_view::editor::Config {
                trim_final_newlines: true,
                ..Default::default()
            },
            ..Default::default()
        })
        .with_file(file.path(), None)
        .with_input_text(LineFeedHandling::Native.apply("#[f|]#oo\n \n\n\n"))
        .build()?;

    test_key_sequence(&mut app, Some(":w<ret>"), None, false).await?;

    helpers::assert_file_has_content(&mut file, &LineFeedHandling::Native.apply("foo\n \n"))?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_insert_final_newline_added_if_missing() -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    let mut app = helpers::AppBuilder::new()
        .with_file(file.path(), None)
        .with_input_text("#[h|]#ave you tried chamomile tea?")
        .build()?;

    test_key_sequence(&mut app, Some(":w<ret>"), None, false).await?;

    helpers::assert_file_has_content(
        &mut file,
        &LineFeedHandling::Native.apply("have you tried chamomile tea?\n"),
    )?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_insert_final_newline_unchanged_if_empty() -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    let mut app = helpers::AppBuilder::new()
        .with_file(file.path(), None)
        .with_input_text("#[|]#")
        .build()?;

    test_key_sequence(&mut app, Some(":w<ret>"), None, false).await?;

    helpers::assert_file_has_content(&mut file, "")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_insert_final_newline_unchanged_if_not_missing() -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    let mut app = helpers::AppBuilder::new()
        .with_file(file.path(), None)
        .with_input_text(LineFeedHandling::Native.apply("#[t|]#en minutes, please\n"))
        .build()?;

    test_key_sequence(&mut app, Some(":w<ret>"), None, false).await?;

    helpers::assert_file_has_content(
        &mut file,
        &LineFeedHandling::Native.apply("ten minutes, please\n"),
    )?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_insert_final_newline_unchanged_if_missing_and_false() -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    let mut app = helpers::AppBuilder::new()
        .with_config(Config {
            editor: helix_view::editor::Config {
                insert_final_newline: false,
                ..Default::default()
            },
            ..Default::default()
        })
        .with_file(file.path(), None)
        .with_input_text("#[t|]#he quiet rain continued through the night")
        .build()?;

    test_key_sequence(&mut app, Some(":w<ret>"), None, false).await?;

    reload_file(&mut file).unwrap();
    helpers::assert_file_has_content(&mut file, "the quiet rain continued through the night")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_all_insert_final_newline_add_if_missing_and_modified() -> anyhow::Result<()> {
    let mut file1 = tempfile::NamedTempFile::new()?;
    let mut file2 = tempfile::NamedTempFile::new()?;
    let mut app = helpers::AppBuilder::new()
        .with_file(file1.path(), None)
        .with_input_text("#[w|]#e don't serve time travelers here")
        .build()?;

    test_key_sequence(
        &mut app,
        Some(&format!(
            ":o {}<ret>ia time traveler walks into a bar<esc>:wa<ret>",
            file2.path().to_string_lossy()
        )),
        None,
        false,
    )
    .await?;

    helpers::assert_file_has_content(
        &mut file1,
        &LineFeedHandling::Native.apply("we don't serve time travelers here\n"),
    )?;

    helpers::assert_file_has_content(
        &mut file2,
        &LineFeedHandling::Native.apply("a time traveler walks into a bar\n"),
    )?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_write_all_insert_final_newline_do_not_add_if_unmodified() -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    let mut app = helpers::AppBuilder::new()
        .with_file(file.path(), None)
        .build()?;

    file.write_all(b"i lost on Jeopardy!")?;
    file.rewind()?;

    test_key_sequence(&mut app, Some(":wa<ret>"), None, false).await?;

    helpers::assert_file_has_content(&mut file, "i lost on Jeopardy!")?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_symlink_write() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;

    let mut file = tempfile::NamedTempFile::new_in(&dir)?;
    let symlink_path = dir.path().join("linked");
    if !try_create_file_symlink(file.path(), &symlink_path)? {
        return Ok(());
    }

    let mut app = helpers::AppBuilder::new()
        .with_file(&symlink_path, None)
        .build()?;

    test_key_sequence(
        &mut app,
        Some("ithe gostak distims the doshes<ret><esc>:w<ret>"),
        None,
        false,
    )
    .await?;

    reload_file(&mut file).unwrap();
    let mut file_content = String::new();
    file.as_file_mut().read_to_string(&mut file_content)?;

    assert_eq!(
        LineFeedHandling::Native.apply("the gostak distims the doshes"),
        file_content
    );
    assert!(symlink_path.is_symlink());

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_symlink_write_fail() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;

    let file = helpers::new_readonly_tempfile_in_dir(&dir)?;
    let symlink_path = dir.path().join("linked");
    if !try_create_file_symlink(file.path(), &symlink_path)? {
        return Ok(());
    }

    let mut app = helpers::AppBuilder::new()
        .with_file(&symlink_path, None)
        .build()?;

    test_key_sequence(
        &mut app,
        Some("ihello<esc>:wq<ret>"),
        Some(&|app| {
            let mut docs: Vec<_> = app.editor.documents().collect();
            assert_eq!(1, docs.len());

            let doc = docs.pop().unwrap();
            assert_eq!(Some(&path::normalize(&symlink_path)), doc.path());
            assert_eq!(&Severity::Error, app.editor.get_status().unwrap().1);
        }),
        false,
    )
    .await?;

    assert!(symlink_path.is_symlink());

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_symlink_write_relative() -> anyhow::Result<()> {
    // tempdir
    // |- - b
    // |  |- file
    // |- linked (symlink to file)
    let dir = tempfile::tempdir()?;
    let inner_dir = dir.path().join("b");
    std::fs::create_dir(&inner_dir)?;

    let mut file = tempfile::NamedTempFile::new_in(&inner_dir)?;
    let symlink_path = dir.path().join("linked");
    let relative_path = std::path::PathBuf::from("b").join(file.path().file_name().unwrap());
    if !try_create_file_symlink(&relative_path, &symlink_path)? {
        return Ok(());
    }

    let mut app = helpers::AppBuilder::new()
        .with_file(&symlink_path, None)
        .build()?;

    test_key_sequence(
        &mut app,
        Some("ithe gostak distims the doshes<ret><esc>:w<ret>"),
        None,
        false,
    )
    .await?;

    reload_file(&mut file).unwrap();
    let mut file_content = String::new();
    file.as_file_mut().read_to_string(&mut file_content)?;

    assert_eq!(
        LineFeedHandling::Native.apply("the gostak distims the doshes"),
        file_content
    );
    assert!(symlink_path.is_symlink());

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[cfg(not(target_os = "android"))]
async fn test_hardlink_write() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;

    let mut file = tempfile::NamedTempFile::new_in(&dir)?;
    let hardlink_path = dir.path().join("linked");
    std::fs::hard_link(file.path(), &hardlink_path)?;

    let mut app = helpers::AppBuilder::new()
        .with_file(&hardlink_path, None)
        .build()?;

    test_key_sequence(
        &mut app,
        Some("ithe gostak distims the doshes<ret><esc>:w<ret>"),
        None,
        false,
    )
    .await?;

    reload_file(&mut file).unwrap();
    let mut file_content = String::new();
    file.as_file_mut().read_to_string(&mut file_content)?;

    assert_eq!(
        LineFeedHandling::Native.apply("the gostak distims the doshes"),
        file_content
    );
    assert!(helix_stdx::faccess::hardlink_count(&hardlink_path)? > 1);
    assert!(same_file::is_same_file(file.path(), &hardlink_path)?);

    Ok(())
}

async fn edit_file_with_content(file_content: &[u8]) -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;

    file.as_file_mut().write_all(file_content)?;

    helpers::test_key_sequence(
        &mut helpers::AppBuilder::new()
            .with_config(Config {
                editor: helix_view::editor::Config {
                    insert_final_newline: false,
                    ..Default::default()
                },
                ..Default::default()
            })
            .build()?,
        Some(&format!(":o {}<ret>:x<ret>", file.path().to_string_lossy())),
        None,
        true,
    )
    .await?;

    reload_file(&mut file).unwrap();
    let mut new_file_content: Vec<u8> = Vec::new();
    file.read_to_end(&mut new_file_content)?;

    assert_eq!(file_content, new_file_content);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_move_file_when_given_dir_and_filename() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let source_file = tempfile::NamedTempFile::new_in(&dir)?;
    let target_file = dir.path().join("new_name.ext");

    let mut app = helpers::AppBuilder::new()
        .with_file(source_file.path(), None)
        .build()?;

    test_key_sequence(
        &mut app,
        Some(format!(":move {}<ret>", target_file.to_string_lossy()).as_ref()),
        None,
        false,
    )
    .await?;

    assert!(
        target_file.is_file(),
        "target file '{}' should have been created",
        target_file.display()
    );
    assert!(
        !source_file.path().exists(),
        "Source file '{}' should have been removed",
        source_file.path().display()
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_move_file_when_given_dir_only() -> anyhow::Result<()> {
    let source_dir = tempfile::tempdir()?;
    let target_dir = tempfile::tempdir()?;
    let source_file = source_dir.path().join("file.ext");
    std::fs::File::create(&source_file)?;

    let mut app = helpers::AppBuilder::new()
        .with_file(&source_file, None)
        .build()?;

    test_key_sequence(
        &mut app,
        Some(format!(":move {}<ret>", target_dir.path().to_string_lossy()).as_ref()),
        None,
        false,
    )
    .await?;

    let target_file = target_dir.path().join("file.ext");

    assert!(
        target_file.is_file(),
        "target file '{}' should have been created",
        target_file.display()
    );
    assert!(
        !source_file.exists(),
        "Source file '{}' should have been removed",
        source_file.display()
    );

    Ok(())
}
