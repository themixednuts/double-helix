use std::{
    fs,
    path::PathBuf,
};

use crate::{
    compositor::Compositor,
    runtime::{ui::command::FileExplorerCommand, RuntimeEvent, UiCommand},
    ui::{Prompt, PromptEvent},
};
use helix_view::Editor;
fn refresh_file_explorer(cursor: u32, cx: &mut crate::compositor::Context, root: PathBuf) {
    let ingress = cx.ingress.clone();
    cx.editor.runtime().work().clone().spawn(async move {
        crate::runtime::send_ui_command_with(
            UiCommand::Layer(crate::runtime::LayerCommand::RefreshFileExplorer { cursor, root }),
            ingress,
        )
        .await;
    }).detach();
}

pub(crate) fn apply_file_explorer_command(
    editor: &mut Editor,
    compositor: &mut Compositor,
    _ingress: helix_runtime::Sender<RuntimeEvent>,
    cmd: FileExplorerCommand,
) {
    match cmd {
        FileExplorerCommand::PromptCreate {
            root,
            cursor,
            prefill,
        } => {
            let prompt = Prompt::new(
                "Create: ".into(),
                None,
                crate::ui::completers::none,
                move |cx, input: &str, event: PromptEvent| {
                    if event != PromptEvent::Validate {
                        return;
                    }

                    let to_create_string = input.to_owned();
                    let to_create = helix_stdx::path::expand_tilde(PathBuf::from(&to_create_string));
                    if to_create.exists() {
                        let root = root.clone();
                        cx.spawn_ui(async move {
                            Ok(UiCommand::FileExplorer(FileExplorerCommand::ConfirmCreate {
                                root,
                                cursor,
                                input: to_create_string,
                                target: to_create.to_path_buf(),
                            }))
                        });
                        return;
                    }

                    let result = if to_create_string.ends_with(std::path::MAIN_SEPARATOR) {
                        match fs::create_dir_all(&to_create).map_err(|err| {
                            format!("Unable to create directory {}: {err}", to_create.display())
                        }) {
                            Ok(()) => {
                                refresh_file_explorer(cursor, cx, root.clone());
                                Ok(format!("Created directory: {}", to_create.display()))
                            }
                            Err(err) => Err(err),
                        }
                    } else {
                        let Some(to_create_parent) = to_create.parent() else {
                            cx.editor.set_result(Err(format!(
                                "Failed to get parent directory of {}",
                                to_create.display()
                            )));
                            return;
                        };

                        if let Err(err_create_parent) = fs::create_dir_all(to_create_parent) {
                            cx.editor.set_result(Err(format!(
                                "Could not create intermediate directories: {err_create_parent}"
                            )));
                            return;
                        }

                        match fs::File::create(&to_create).map_err(|err| {
                            format!("Unable to create file {}: {err}", to_create.display())
                        }) {
                            Ok(_) => {
                                refresh_file_explorer(cursor, cx, root.clone());
                                Ok(format!("Created file: {}", to_create.display()))
                            }
                            Err(err) => Err(err),
                        }
                    };

                    cx.editor.set_result(result);
                },
            )
            .with_line(prefill, editor);

            compositor.push(Box::new(prompt));
        }
        FileExplorerCommand::PromptMove {
            source,
            root,
            cursor,
            prefill,
            movement,
        } => {
            let mut prompt = Prompt::new(
                format!("Move {} -> ", source.display()).into(),
                None,
                crate::ui::completers::none,
                move |cx, input: &str, event: PromptEvent| {
                    if event != PromptEvent::Validate {
                        return;
                    }

                    let move_to_string = input.to_owned();
                    let move_to = helix_stdx::path::expand_tilde(PathBuf::from(&move_to_string));
                    if move_to.exists() {
                        let source = source.clone();
                        let root = root.clone();
                        cx.spawn_ui(async move {
                            Ok(UiCommand::FileExplorer(FileExplorerCommand::ConfirmMove {
                                source,
                                root,
                                cursor,
                                input: move_to_string,
                                destination: move_to.to_path_buf(),
                            }))
                        });
                        return;
                    }

                    match cx.editor.move_path(&source, &move_to).map_err(|err| {
                        format!(
                            "Unable to move {} {} -> {}: {err}",
                            if move_to_string.ends_with(std::path::MAIN_SEPARATOR) {
                                "directory"
                            } else {
                                "file"
                            },
                            source.display(),
                            move_to.display()
                        )
                    }) {
                        Ok(()) => {
                            refresh_file_explorer(cursor, cx, root.clone());
                            cx.editor.clear_status();
                        }
                        Err(err) => cx.editor.set_result(Err(err)),
                    }
                },
            );

            prompt.set_line(prefill, editor);
            if let Some(movement) = movement {
                prompt.move_cursor(movement);
            }
            compositor.push(Box::new(prompt));
        }
        FileExplorerCommand::PromptDelete {
            target,
            root,
            cursor,
        } => {
            let prompt = Prompt::new(
                format!("Delete {}? (y/n): ", target.display()).into(),
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

                    let result = if !target.exists() {
                        Some(Err(format!("Path {} does not exist", target.display())))
                    } else if target.is_dir() {
                        match fs::remove_dir_all(&target).map_err(|err| {
                            format!("Unable to delete directory {}: {err}", target.display())
                        }) {
                            Ok(()) => {
                                refresh_file_explorer(cursor, cx, root.clone());
                                Some(Ok(format!("Deleted directory: {}", target.display())))
                            }
                            Err(err) => Some(Err(err)),
                        }
                    } else {
                        match fs::remove_file(&target).map_err(|err| {
                            format!("Unable to delete file {}: {err}", target.display())
                        }) {
                            Ok(()) => {
                                refresh_file_explorer(cursor, cx, root.clone());
                                Some(Ok(format!("Deleted file: {}", target.display())))
                            }
                            Err(err) => Some(Err(err)),
                        }
                    };

                    if let Some(result) = result {
                        cx.editor.set_result(result);
                    }
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

                    if source.is_dir() || copy_to_string.ends_with(std::path::MAIN_SEPARATOR) {
                        cx.editor.set_result(Err(format!(
                            "Copying directories is not supported: {} is a directory",
                            source.display()
                        )));
                        return;
                    }

                    let copy_to_str = copy_to_string.to_string();
                    if copy_to.exists() {
                        let source = source.clone();
                        let root = root.clone();
                        cx.spawn_ui(async move {
                            Ok(UiCommand::FileExplorer(FileExplorerCommand::ConfirmCopy {
                                source,
                                root,
                                cursor,
                                input: copy_to_str,
                                destination: copy_to.to_path_buf(),
                            }))
                        });
                        return;
                    }

                    match std::fs::copy(&source, &copy_to).map_err(|err| {
                        format!(
                            "Unable to copy from file {} to {}: {err}",
                            source.display(),
                            copy_to.display()
                        )
                    }) {
                        Ok(_) => {
                            refresh_file_explorer(cursor, cx, root.clone());
                            cx.editor.set_result(Ok(format!(
                                "Copied contents of file {} to {}",
                                source.display(),
                                copy_to.display()
                            )));
                        }
                        Err(err) => cx.editor.set_result(Err(err)),
                    }
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
                format!("Path {} already exists. Ovewrite? (y/n):", target.display()).into(),
                None,
                crate::ui::completers::none,
                move |cx, answer: &str, event: PromptEvent| {
                    if event != PromptEvent::Validate || answer != "y" {
                        return;
                    }

                    let result = if input.ends_with(std::path::MAIN_SEPARATOR) {
                        match fs::create_dir_all(&target)
                            .map_err(|err| format!("Unable to create directory {}: {err}", target.display()))
                        {
                            Ok(()) => {
                                refresh_file_explorer(cursor, cx, root.clone());
                                Ok(format!("Created directory: {}", target.display()))
                            }
                            Err(err) => Err(err),
                        }
                    } else {
                        let Some(parent) = target.parent() else {
                            cx.editor.set_result(Err(format!(
                                "Failed to get parent directory of {}",
                                target.display()
                            )));
                            return;
                        };
                        if let Err(err) = fs::create_dir_all(parent) {
                            cx.editor.set_result(Err(format!(
                                "Could not create intermediate directories: {err}"
                            )));
                            return;
                        }
                        match fs::File::create(&target)
                            .map_err(|err| format!("Unable to create file {}: {err}", target.display()))
                        {
                            Ok(_) => {
                                refresh_file_explorer(cursor, cx, root.clone());
                                Ok(format!("Created file: {}", target.display()))
                            }
                            Err(err) => Err(err),
                        }
                    };
                    cx.editor.set_result(result);
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
                format!("Path {} already exists. Ovewrite? (y/n):", destination.display()).into(),
                None,
                crate::ui::completers::none,
                move |cx, answer: &str, event: PromptEvent| {
                    if event != PromptEvent::Validate || answer != "y" {
                        return;
                    }

                    match cx.editor.move_path(&source, &destination).map_err(|err| {
                        format!(
                            "Unable to move {} {} -> {}: {err}",
                            if input.ends_with(std::path::MAIN_SEPARATOR) {
                                "directory"
                            } else {
                                "file"
                            },
                            source.display(),
                            destination.display()
                        )
                    }) {
                        Ok(()) => {
                            refresh_file_explorer(cursor, cx, root.clone());
                            cx.editor.clear_status();
                        }
                        Err(err) => cx.editor.set_result(Err(err)),
                    }
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
                format!("Path {} already exists. Ovewrite? (y/n):", destination.display()).into(),
                None,
                crate::ui::completers::none,
                move |cx, answer: &str, event: PromptEvent| {
                    if event != PromptEvent::Validate || answer != "y" {
                        return;
                    }

                    match std::fs::copy(&source, &destination).map_err(|err| {
                        format!(
                            "Unable to copy from file {} to {}: {err}",
                            source.display(),
                            destination.display()
                        )
                    }) {
                        Ok(_) => {
                            refresh_file_explorer(cursor, cx, root.clone());
                            cx.editor.set_result(Ok(format!(
                                "Copied contents of file {} to {}",
                                source.display(),
                                destination.display()
                            )));
                        }
                        Err(err) => cx.editor.set_result(Err(err)),
                    }
                },
            );
            compositor.push(Box::new(prompt));
        }
    }
}
