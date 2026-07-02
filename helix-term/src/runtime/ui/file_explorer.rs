use std::{
    path::{Path, PathBuf},
    time::Instant,
};

use crate::{
    compositor::Compositor,
    runtime::{
        ui::command::{FileExplorerCommand, ModifiedBufferCheck},
        ui::snapshot::UiSnapshotRequest,
        UiCommand,
    },
    ui::{FileExplorerPanel, Prompt, PromptEvent, FILE_EXPLORER_ID},
};
use helix_view::{editor::SavePolicy, DocumentId, Editor};

struct FileExplorerApplyContext<'a> {
    editor: &'a mut Editor,
    compositor: &'a mut Compositor,
    ingress: crate::runtime::RuntimeIngress,
}

fn is_directory_input(input: &str) -> bool {
    input.ends_with(std::path::MAIN_SEPARATOR) || input.ends_with('/')
}

fn path_exists_for_prompt(path: &Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

fn spawn_file_explorer_command(cx: &mut crate::compositor::Context, command: FileExplorerCommand) {
    cx.spawn_ui(async move { Ok(UiCommand::FileExplorer(command)) });
}

fn queue_file_explorer_command(
    editor: &Editor,
    ingress: crate::runtime::RuntimeIngress,
    command: FileExplorerCommand,
) {
    editor
        .work()
        .spawn(async move {
            crate::runtime::send_ui_command_with(UiCommand::FileExplorer(command), ingress).await;
        })
        .detach();
}

pub(crate) fn queue_file_explorer_vcs_snapshot(
    editor: &Editor,
    ingress: crate::runtime::RuntimeIngress,
    root: PathBuf,
) {
    if !editor.config().file_explorer.vcs {
        return;
    }

    let root = helix_stdx::path::normalize(root);
    let diff_providers = editor.diff_providers.clone();
    UiSnapshotRequest::new("[file_explorer] vcs_snapshot", root)
        .load_with(move |root| diff_providers.changed_files(&root))
        .apply_with(|root, changes| {
            UiCommand::FileExplorer(FileExplorerCommand::ApplyVcsSnapshot { root, changes })
        })
        .spawn(editor.work(), ingress);
}

fn refresh_file_explorer_panel(
    editor: &mut Editor,
    compositor: &mut Compositor,
    ingress: crate::runtime::RuntimeIngress,
    root: PathBuf,
    cursor: u32,
) {
    let start = Instant::now();
    let requested_root = root.clone();
    let cursor = usize::try_from(cursor).unwrap_or(usize::MAX);
    if let Some(panel) = compositor.find_id::<FileExplorerPanel>(FILE_EXPLORER_ID) {
        if let Err(err) = panel.refresh(editor, Some(root), Some(cursor)) {
            editor.set_error(format!("{err}"));
        }
        log::info!(
            "[file_explorer] runtime_refresh existing_panel=true root={} cursor={} elapsed_us={}",
            requested_root.display(),
            cursor,
            start.elapsed().as_micros()
        );
        queue_file_explorer_vcs_snapshot(editor, ingress, requested_root);
    } else {
        match FileExplorerPanel::new_with_cursor(root, editor, Some(cursor)) {
            Ok(panel) => {
                compositor.push(Box::new(panel));
                log::info!(
                    "[file_explorer] runtime_refresh existing_panel=false root={} cursor={} elapsed_us={}",
                    requested_root.display(),
                    cursor,
                    start.elapsed().as_micros()
                );
                queue_file_explorer_vcs_snapshot(editor, ingress, requested_root);
            }
            Err(err) => {
                log::info!(
                    "[file_explorer] runtime_refresh existing_panel=false root={} cursor={} error={} elapsed_us={}",
                    requested_root.display(),
                    cursor,
                    err,
                    start.elapsed().as_micros()
                );
                editor.set_error(format!("{err}"));
            }
        }
    }
}

fn refresh_file_explorer_panel_selecting_path(
    editor: &mut Editor,
    compositor: &mut Compositor,
    ingress: crate::runtime::RuntimeIngress,
    root: PathBuf,
    path: PathBuf,
    cursor: u32,
) {
    let start = Instant::now();
    let requested_root = root.clone();
    let requested_path = path.clone();
    let cursor = usize::try_from(cursor).unwrap_or(usize::MAX);
    if let Some(panel) = compositor.find_id::<FileExplorerPanel>(FILE_EXPLORER_ID) {
        if let Err(err) = panel.refresh_selecting_path(editor, Some(root), &path, cursor) {
            editor.set_error(format!("{err}"));
        }
        panel.queue_selected_preview(editor, ingress.clone());
        log::info!(
            "[file_explorer] runtime_refresh existing_panel=true root={} select_path={} fallback_cursor={} elapsed_us={}",
            requested_root.display(),
            requested_path.display(),
            cursor,
            start.elapsed().as_micros()
        );
        queue_file_explorer_vcs_snapshot(editor, ingress, requested_root);
    } else {
        match FileExplorerPanel::new_with_cursor(root, editor, Some(cursor)) {
            Ok(mut panel) => {
                panel.queue_selected_preview(editor, ingress.clone());
                compositor.push(Box::new(panel));
                log::info!(
                    "[file_explorer] runtime_refresh existing_panel=false root={} select_path={} fallback_cursor={} elapsed_us={}",
                    requested_root.display(),
                    requested_path.display(),
                    cursor,
                    start.elapsed().as_micros()
                );
                queue_file_explorer_vcs_snapshot(editor, ingress, requested_root);
            }
            Err(err) => {
                log::info!(
                    "[file_explorer] runtime_refresh existing_panel=false root={} select_path={} fallback_cursor={} error={} elapsed_us={}",
                    requested_root.display(),
                    requested_path.display(),
                    cursor,
                    err,
                    start.elapsed().as_micros()
                );
                editor.set_error(format!("{err}"));
            }
        }
    }
}

fn path_affects_document(path: &Path, document_path: &Path) -> bool {
    let path = helix_stdx::path::canonicalize(path);
    let document_path = helix_stdx::path::canonicalize(document_path);
    document_path == path || path.is_dir() && document_path.starts_with(path)
}

fn modified_documents_for_paths(editor: &Editor, paths: &[PathBuf]) -> Vec<DocumentId> {
    let mut documents = Vec::new();
    for doc in editor.documents() {
        if !doc.is_modified() {
            continue;
        }
        let Some(path) = doc.path() else {
            continue;
        };
        if paths
            .iter()
            .any(|operation_path| path_affects_document(operation_path, path))
            && !documents.contains(&doc.id())
        {
            documents.push(doc.id());
        }
    }
    documents
}

fn save_modified_documents(
    cx: &mut crate::compositor::Context,
    documents: &[DocumentId],
) -> anyhow::Result<()> {
    for doc_id in documents.iter().copied() {
        let Some(doc) = cx.editor.document(doc_id) else {
            continue;
        };
        if !doc.is_modified() {
            continue;
        }

        append_document_changes_to_history(cx.editor, doc_id);
        cx.editor.save(doc_id, None::<PathBuf>, SavePolicy::Safe)?;
    }
    tokio::task::block_in_place(|| helix_lsp::block_on(cx.editor.flush_writes()))?;
    Ok(())
}

fn append_document_changes_to_history(editor: &mut Editor, doc_id: DocumentId) {
    let Some(view_id) = editor
        .tree
        .views()
        .find_map(|(view, focused)| (focused && view.doc == doc_id).then_some(view.id))
        .or_else(|| {
            editor
                .tree
                .views()
                .find_map(|(view, _)| (view.doc == doc_id).then_some(view.id))
        })
    else {
        return;
    };

    let view = helix_view::view_mut!(editor, view_id);
    let doc = helix_view::doc_mut!(editor, &doc_id);
    doc.append_changes_to_history(view);
}

fn without_modified_buffer_check(mut command: FileExplorerCommand) -> FileExplorerCommand {
    match &mut command {
        FileExplorerCommand::ApplyCreate {
            modified_buffer_check,
            ..
        }
        | FileExplorerCommand::ApplyMove {
            modified_buffer_check,
            ..
        }
        | FileExplorerCommand::ApplyDelete {
            modified_buffer_check,
            ..
        }
        | FileExplorerCommand::ApplyCopy {
            modified_buffer_check,
            ..
        } => *modified_buffer_check = ModifiedBufferCheck::Skip,
        _ => {}
    }
    command
}

fn prompt_save_before_modified_documents(
    editor: &mut Editor,
    ingress: crate::runtime::RuntimeIngress,
    operation: String,
    paths: &[PathBuf],
    continuation: FileExplorerCommand,
) -> bool {
    let documents = modified_documents_for_paths(editor, paths);
    if documents.is_empty() {
        return false;
    }

    queue_file_explorer_command(
        editor,
        ingress,
        FileExplorerCommand::PromptSaveBefore {
            operation,
            documents,
            continuation: Box::new(continuation),
        },
    );
    true
}

fn apply_create(
    cx: &mut FileExplorerApplyContext<'_>,
    root: PathBuf,
    cursor: u32,
    input: String,
    target: PathBuf,
    modified_buffer_check: ModifiedBufferCheck,
) {
    let command = FileExplorerCommand::ApplyCreate {
        root: root.clone(),
        cursor,
        input: input.clone(),
        target: target.clone(),
        modified_buffer_check,
    };
    if modified_buffer_check == ModifiedBufferCheck::Prompt
        && prompt_save_before_modified_documents(
            cx.editor,
            cx.ingress.clone(),
            format!("creating {}", target.display()),
            std::slice::from_ref(&target),
            command,
        )
    {
        return;
    }

    let is_dir = is_directory_input(&input);
    let result = if is_dir {
        match cx
            .editor
            .create_path_with_history(&target, true)
            .map_err(|err| format!("Unable to create directory {}: {err}", target.display()))
        {
            Ok(()) => {
                refresh_file_explorer_panel_selecting_path(
                    cx.editor,
                    cx.compositor,
                    cx.ingress.clone(),
                    root,
                    target.clone(),
                    cursor,
                );
                Ok(format!("Created directory: {}", target.display()))
            }
            Err(err) => Err(err),
        }
    } else {
        match cx
            .editor
            .create_path_with_history(&target, false)
            .map_err(|err| format!("Unable to create file {}: {err}", target.display()))
        {
            Ok(()) => {
                refresh_file_explorer_panel_selecting_path(
                    cx.editor,
                    cx.compositor,
                    cx.ingress.clone(),
                    root,
                    target.clone(),
                    cursor,
                );
                Ok(format!("Created file: {}", target.display()))
            }
            Err(err) => Err(err),
        }
    };

    cx.editor.set_result(result);
}

fn apply_move(
    cx: &mut FileExplorerApplyContext<'_>,
    source: PathBuf,
    root: PathBuf,
    cursor: u32,
    input: String,
    destination: PathBuf,
    modified_buffer_check: ModifiedBufferCheck,
) {
    let command = FileExplorerCommand::ApplyMove {
        source: source.clone(),
        root: root.clone(),
        cursor,
        input: input.clone(),
        destination: destination.clone(),
        modified_buffer_check,
    };
    if modified_buffer_check == ModifiedBufferCheck::Prompt
        && prompt_save_before_modified_documents(
            cx.editor,
            cx.ingress.clone(),
            format!("moving {}", source.display()),
            &[source.clone(), destination.clone()],
            command,
        )
    {
        return;
    }

    match cx
        .editor
        .move_path_with_history(&source, &destination)
        .map_err(|err| {
            format!(
                "Unable to move {} {} -> {}: {err}",
                if is_directory_input(&input) {
                    "directory"
                } else {
                    "file"
                },
                source.display(),
                destination.display()
            )
        }) {
        Ok(()) => {
            refresh_file_explorer_panel_selecting_path(
                cx.editor,
                cx.compositor,
                cx.ingress.clone(),
                root,
                destination,
                cursor,
            );
            cx.editor.clear_status();
        }
        Err(err) => cx.editor.set_result(Err(err)),
    }
}

fn apply_delete(
    cx: &mut FileExplorerApplyContext<'_>,
    target: PathBuf,
    root: PathBuf,
    cursor: u32,
    modified_buffer_check: ModifiedBufferCheck,
) {
    let command = FileExplorerCommand::ApplyDelete {
        target: target.clone(),
        root: root.clone(),
        cursor,
        modified_buffer_check,
    };
    if modified_buffer_check == ModifiedBufferCheck::Prompt
        && prompt_save_before_modified_documents(
            cx.editor,
            cx.ingress.clone(),
            format!("deleting {}", target.display()),
            std::slice::from_ref(&target),
            command,
        )
    {
        return;
    }

    let result = if target.is_dir() {
        match cx
            .editor
            .trash_path_with_history(&target)
            .map_err(|err| format!("Unable to trash directory {}: {err}", target.display()))
        {
            Ok(()) => {
                refresh_file_explorer_panel(
                    cx.editor,
                    cx.compositor,
                    cx.ingress.clone(),
                    root,
                    cursor,
                );
                Some(Ok(format!(
                    "Moved directory to trash: {}",
                    target.display()
                )))
            }
            Err(err) => Some(Err(err)),
        }
    } else {
        match cx
            .editor
            .trash_path_with_history(&target)
            .map_err(|err| format!("Unable to trash file {}: {err}", target.display()))
        {
            Ok(()) => {
                refresh_file_explorer_panel(
                    cx.editor,
                    cx.compositor,
                    cx.ingress.clone(),
                    root,
                    cursor,
                );
                Some(Ok(format!("Moved file to trash: {}", target.display())))
            }
            Err(err) => Some(Err(err)),
        }
    };

    if let Some(result) = result {
        cx.editor.set_result(result);
    }
}

fn apply_copy(
    cx: &mut FileExplorerApplyContext<'_>,
    source: PathBuf,
    root: PathBuf,
    cursor: u32,
    destination: PathBuf,
    modified_buffer_check: ModifiedBufferCheck,
) {
    let command = FileExplorerCommand::ApplyCopy {
        source: source.clone(),
        root: root.clone(),
        cursor,
        destination: destination.clone(),
        modified_buffer_check,
    };
    if modified_buffer_check == ModifiedBufferCheck::Prompt
        && prompt_save_before_modified_documents(
            cx.editor,
            cx.ingress.clone(),
            format!("copying {}", source.display()),
            &[source.clone(), destination.clone()],
            command,
        )
    {
        return;
    }

    match cx
        .editor
        .copy_path_with_history(&source, &destination)
        .map_err(|err| {
            format!(
                "Unable to copy from file {} to {}: {err}",
                source.display(),
                destination.display()
            )
        }) {
        Ok(_) => {
            refresh_file_explorer_panel_selecting_path(
                cx.editor,
                cx.compositor,
                cx.ingress.clone(),
                root,
                destination.clone(),
                cursor,
            );
            cx.editor.set_result(Ok(format!(
                "Copied contents of file {} to {}",
                source.display(),
                destination.display()
            )));
        }
        Err(err) => cx.editor.set_result(Err(err)),
    }
}

pub(crate) fn apply_file_explorer_command(
    editor: &mut Editor,
    compositor: &mut Compositor,
    ingress: crate::runtime::RuntimeIngress,
    cmd: FileExplorerCommand,
) {
    match cmd {
        FileExplorerCommand::RefreshPanel { root, cursor } => {
            refresh_file_explorer_panel(editor, compositor, ingress.clone(), root, cursor);
        }
        FileExplorerCommand::PreviewSelection { root, path, cursor } => {
            if let Some(panel) = compositor.find_id::<FileExplorerPanel>(FILE_EXPLORER_ID) {
                panel.apply_preview_request(editor, ingress.clone(), root, path, cursor);
            }
        }
        FileExplorerCommand::ApplyVcsSnapshot { root, changes } => {
            if let Some(panel) = compositor.find_id::<FileExplorerPanel>(FILE_EXPLORER_ID) {
                if let Err(err) = panel.apply_vcs_snapshot(editor, root, changes) {
                    editor.set_error(format!("{err}"));
                }
            }
        }
        FileExplorerCommand::PromptDelete {
            target,
            root,
            cursor,
        } => {
            let prompt = Prompt::new(
                format!("Move {} to trash? (y/n): ", target.display()).into(),
                None,
                crate::ui::completers::none,
                move |cx, input: &str, event: PromptEvent| {
                    if event != PromptEvent::Validate {
                        return;
                    }

                    if input != "y" {
                        cx.editor.clear_status();
                        return;
                    }

                    spawn_file_explorer_command(
                        cx,
                        FileExplorerCommand::ApplyDelete {
                            target: target.clone(),
                            root: root.clone(),
                            cursor,
                            modified_buffer_check: ModifiedBufferCheck::Prompt,
                        },
                    );
                },
            );

            compositor.push(Box::new(prompt));
        }
        FileExplorerCommand::PromptCopy {
            source,
            root,
            cursor,
            prefill,
        } => {
            let prompt = Prompt::new(
                format!("Copy {} -> ", source.display()).into(),
                None,
                crate::ui::completers::none,
                move |cx, input: &str, event: PromptEvent| {
                    if event != PromptEvent::Validate {
                        return;
                    }

                    let copy_to_string = input.to_owned();
                    let copy_to = helix_stdx::path::expand_tilde(PathBuf::from(&copy_to_string));

                    if source.is_dir() || is_directory_input(&copy_to_string) {
                        cx.editor.set_result(Err(format!(
                            "Copying directories is not supported: {} is a directory",
                            source.display()
                        )));
                        return;
                    }

                    let copy_to_str = copy_to_string.to_string();
                    let target_exists = match path_exists_for_prompt(&copy_to) {
                        Ok(exists) => exists,
                        Err(err) => {
                            cx.editor.set_result(Err(format!(
                                "Unable to inspect {}: {err}",
                                copy_to.display()
                            )));
                            return;
                        }
                    };
                    if target_exists {
                        let source = source.clone();
                        let root = root.clone();
                        spawn_file_explorer_command(
                            cx,
                            FileExplorerCommand::ConfirmCopy {
                                source,
                                root,
                                cursor,
                                input: copy_to_str,
                                destination: copy_to.to_path_buf(),
                            },
                        );
                        return;
                    }

                    spawn_file_explorer_command(
                        cx,
                        FileExplorerCommand::ApplyCopy {
                            source: source.clone(),
                            root: root.clone(),
                            cursor,
                            destination: copy_to.to_path_buf(),
                            modified_buffer_check: ModifiedBufferCheck::Prompt,
                        },
                    );
                },
            )
            .with_line(prefill, editor);

            compositor.push(Box::new(prompt));
        }
        FileExplorerCommand::ConfirmCreate {
            root,
            cursor,
            input,
            target,
        } => {
            let prompt = Prompt::new(
                format!(
                    "Path {} already exists. Overwrite? (y/n):",
                    target.display()
                )
                .into(),
                None,
                crate::ui::completers::none,
                move |cx, answer: &str, event: PromptEvent| {
                    if event != PromptEvent::Validate || answer != "y" {
                        return;
                    }

                    spawn_file_explorer_command(
                        cx,
                        FileExplorerCommand::ApplyCreate {
                            root: root.clone(),
                            cursor,
                            input: input.clone(),
                            target: target.clone(),
                            modified_buffer_check: ModifiedBufferCheck::Prompt,
                        },
                    );
                },
            );
            compositor.push(Box::new(prompt));
        }
        FileExplorerCommand::ConfirmMove {
            source,
            root,
            cursor,
            input,
            destination,
        } => {
            let prompt = Prompt::new(
                format!(
                    "Path {} already exists. Overwrite? (y/n):",
                    destination.display()
                )
                .into(),
                None,
                crate::ui::completers::none,
                move |cx, answer: &str, event: PromptEvent| {
                    if event != PromptEvent::Validate || answer != "y" {
                        return;
                    }

                    spawn_file_explorer_command(
                        cx,
                        FileExplorerCommand::ApplyMove {
                            source: source.clone(),
                            root: root.clone(),
                            cursor,
                            input: input.clone(),
                            destination: destination.clone(),
                            modified_buffer_check: ModifiedBufferCheck::Prompt,
                        },
                    );
                },
            );
            compositor.push(Box::new(prompt));
        }
        FileExplorerCommand::ConfirmCopy {
            source,
            root,
            cursor,
            input: _,
            destination,
        } => {
            let prompt = Prompt::new(
                format!(
                    "Path {} already exists. Overwrite? (y/n):",
                    destination.display()
                )
                .into(),
                None,
                crate::ui::completers::none,
                move |cx, answer: &str, event: PromptEvent| {
                    if event != PromptEvent::Validate || answer != "y" {
                        return;
                    }

                    spawn_file_explorer_command(
                        cx,
                        FileExplorerCommand::ApplyCopy {
                            source: source.clone(),
                            root: root.clone(),
                            cursor,
                            destination: destination.clone(),
                            modified_buffer_check: ModifiedBufferCheck::Prompt,
                        },
                    );
                },
            );
            compositor.push(Box::new(prompt));
        }
        FileExplorerCommand::ApplyCreate {
            root,
            cursor,
            input,
            target,
            modified_buffer_check,
        } => {
            let mut cx = FileExplorerApplyContext {
                editor,
                compositor,
                ingress,
            };
            apply_create(&mut cx, root, cursor, input, target, modified_buffer_check);
        }
        FileExplorerCommand::ApplyMove {
            source,
            root,
            cursor,
            input,
            destination,
            modified_buffer_check,
        } => {
            let mut cx = FileExplorerApplyContext {
                editor,
                compositor,
                ingress,
            };
            apply_move(
                &mut cx,
                source,
                root,
                cursor,
                input,
                destination,
                modified_buffer_check,
            );
        }
        FileExplorerCommand::ApplyDelete {
            target,
            root,
            cursor,
            modified_buffer_check,
        } => {
            let mut cx = FileExplorerApplyContext {
                editor,
                compositor,
                ingress,
            };
            apply_delete(&mut cx, target, root, cursor, modified_buffer_check);
        }
        FileExplorerCommand::ApplyCopy {
            source,
            root,
            cursor,
            destination,
            modified_buffer_check,
        } => {
            let mut cx = FileExplorerApplyContext {
                editor,
                compositor,
                ingress,
            };
            apply_copy(
                &mut cx,
                source,
                root,
                cursor,
                destination,
                modified_buffer_check,
            );
        }
        FileExplorerCommand::PromptSaveBefore {
            operation,
            documents,
            continuation,
        } => {
            let prompt = Prompt::new(
                format!(
                    "{} modified buffer(s) affected while {}. Save first? (y/n/c): ",
                    documents.len(),
                    operation
                )
                .into(),
                None,
                crate::ui::completers::none,
                move |cx, answer: &str, event: PromptEvent| {
                    if event != PromptEvent::Validate {
                        return;
                    }

                    match answer {
                        "y" => match save_modified_documents(cx, &documents) {
                            Ok(()) => {
                                spawn_file_explorer_command(
                                    cx,
                                    without_modified_buffer_check((*continuation).clone()),
                                );
                            }
                            Err(err) => cx.editor.set_error(format!("{err}")),
                        },
                        "n" => spawn_file_explorer_command(
                            cx,
                            without_modified_buffer_check((*continuation).clone()),
                        ),
                        _ => cx.editor.clear_status(),
                    }
                },
            );
            compositor.push(Box::new(prompt));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_swap::ArcSwap;
    use helix_core::Transaction;
    use helix_view::{
        doc_mut,
        editor::{Action, Config},
        graphics::Rect,
        handlers::Handlers,
        theme, Editor,
    };
    use std::sync::Arc;

    #[test]
    fn path_affects_documents_under_existing_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("src");
        let child = root.join("main.rs");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(&child, "").unwrap();

        assert!(path_affects_document(&root, &child));
    }

    #[test]
    fn path_affects_exact_file_only_for_files() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("main.rs");
        let sibling = temp.path().join("main.rs.bak");
        std::fs::write(&file, "").unwrap();
        std::fs::write(&sibling, "").unwrap();

        assert!(path_affects_document(&file, &file));
        assert!(!path_affects_document(&file, &sibling));
    }

    #[test]
    fn save_prompt_continuation_skips_second_prompt() {
        let command = FileExplorerCommand::ApplyDelete {
            target: PathBuf::from("target"),
            root: PathBuf::from("."),
            cursor: 0,
            modified_buffer_check: ModifiedBufferCheck::Prompt,
        };

        let FileExplorerCommand::ApplyDelete {
            modified_buffer_check,
            ..
        } = without_modified_buffer_check(command)
        else {
            panic!("expected delete command");
        };

        assert_eq!(modified_buffer_check, ModifiedBufferCheck::Skip);
    }

    fn test_editor(runtime: helix_runtime::Runtime) -> Editor {
        let theme_loader = theme::Loader::new(helix_loader::runtime_dirs());
        let syn_loader = helix_core::config::default_lang_loader();
        let config = Arc::new(ArcSwap::from_pointee(Config::default()));
        Editor::new(
            Rect::new(0, 0, 100, 30),
            Arc::new(theme_loader),
            Arc::new(ArcSwap::from_pointee(syn_loader)),
            config,
            runtime,
            Handlers::dummy(),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn save_modified_documents_flushes_to_disk() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("main.rs");
        std::fs::write(&path, "old").unwrap();

        let runtime = helix_runtime::Runtime::new(tokio::runtime::Handle::current());
        let mut editor = test_editor(runtime.clone());
        let doc_id = editor.open(&path, Action::VerticalSplit).unwrap();
        let view_id = editor.focused_view_id();
        let doc = doc_mut!(editor, &doc_id);
        let transaction = Transaction::change(
            doc.text(),
            [(0, doc.text().len_chars(), Some("new".into()))].into_iter(),
        );
        doc.apply(&transaction, view_id);
        assert!(doc.is_modified());

        let (ingress, _ingress_rx) =
            crate::runtime::RuntimeIngress::channel(runtime.work().clone());
        let (plugin_events, _plugin_events_rx) = helix_runtime::channel(16);
        let idle_reset = crate::runtime::IdleResetGate::new().handle();
        let mut exit_tasks = crate::runtime::ExitTaskSet::default();
        let exit_task_work = editor.work();
        let redraw = editor.redraw_handle();
        let notifier = crate::handlers::local::Notifier {
            redraw: redraw.clone(),
            plugin_events,
        };
        let mut cx = crate::compositor::Context::new(
            &mut editor,
            &mut exit_tasks,
            exit_task_work,
            notifier,
            ingress,
            idle_reset,
            None,
        );

        save_modified_documents(&mut cx, &[doc_id]).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert!(!cx.editor.document(doc_id).unwrap().is_modified());
    }
}
