use std::error::Error as _;
use std::path::{Path, PathBuf};

use helix_core::hashmap;
use helix_view::{
    Editor,
    editor::{Action, EditingEngineConfig},
    theme::Style,
};
use tui::text::Span;

use crate::{
    alt, key,
    runtime::{LayerCommand, UiCommand, ui::command::FileExplorerCommand},
};

use super::prompt::Movement;
use super::{
    Picker, PickerColumn, directory_content,
    picker::{PickerKeyHandler, PickerKeyHandlers},
};

/// for each path: (path to item, is the path a directory?)
type ExplorerItem = (PathBuf, bool);
/// (file explorer root, directory style)
type ExplorerData = (PathBuf, Style);

type FileExplorer = Picker<ExplorerItem, ExplorerData>;

type KeyHandler = PickerKeyHandler<ExplorerItem, ExplorerData>;

fn path_prefill(path: &Path) -> String {
    let mut path = path.display().to_string();
    if !path.ends_with(std::path::MAIN_SEPARATOR) && !path.ends_with('/') {
        path.push(std::path::MAIN_SEPARATOR);
    }
    path
}

fn yank_path_handler() -> KeyHandler {
    Box::new(|cx, (path, _), _, _| {
        let register = cx
            .editor
            .frontend()
            .focused_modal_input
            .selected_register
            .unwrap_or(cx.editor.config().default_yank_register);
        let path = helix_stdx::path::get_relative_path(path);
        let path = path.to_string_lossy().to_string();
        let message = format!("Yanked path {} to register {register}", path);

        match cx.editor.registers.write(register, vec![path]) {
            Ok(()) => cx.editor.set_status(message),
            Err(err) => cx.editor.set_error(err.to_string()),
        };
    })
}

fn create_handler() -> KeyHandler {
    Box::new(|cx, _, data, cursor| {
        let prefill = path_prefill(&data.0);
        cx.spawn_ui(async move {
            Ok(UiCommand::FileExplorer(FileExplorerCommand::PromptCreate {
                root: data.0.clone(),
                cursor,
                prefill,
            }))
        });
    })
}

fn move_handler() -> KeyHandler {
    Box::new(|cx, (path, _), data, cursor| {
        let movement = path
            .extension()
            .map(|ext| Movement::BackwardChar(ext.len() + 1));
        let source = path.to_path_buf();
        let prefill = path.display().to_string();
        cx.spawn_ui(async move {
            Ok(UiCommand::FileExplorer(FileExplorerCommand::PromptMove {
                source,
                root: data.0.clone(),
                cursor,
                prefill,
                movement,
            }))
        });
    })
}

fn delete_handler() -> KeyHandler {
    Box::new(|cx, (path, _), data, cursor| {
        let target = path.to_path_buf();
        cx.spawn_ui(async move {
            Ok(UiCommand::FileExplorer(FileExplorerCommand::PromptDelete {
                target,
                root: data.0.clone(),
                cursor,
            }))
        });
    })
}

fn copy_handler() -> KeyHandler {
    Box::new(|cx, (path, _), data, cursor| {
        let source = path.to_path_buf();
        let prefill = path_prefill(&data.0);
        cx.spawn_ui(async move {
            Ok(UiCommand::FileExplorer(FileExplorerCommand::PromptCopy {
                source,
                root: data.0.clone(),
                cursor,
                prefill,
            }))
        });
    })
}

fn refresh_handler() -> KeyHandler {
    Box::new(|cx, _, data, cursor| {
        cx.spawn_ui(async move {
            Ok(UiCommand::Layer(LayerCommand::RefreshFileExplorer {
                cursor,
                root: data.0.clone(),
            }))
        });
    })
}

fn parent_handler() -> KeyHandler {
    Box::new(|cx, _, data, _| {
        let Some(parent) = data.0.parent().map(Path::to_path_buf) else {
            return;
        };
        cx.spawn_ui(async move {
            Ok(UiCommand::Layer(LayerCommand::RefreshFileExplorer {
                cursor: 0,
                root: parent,
            }))
        });
    })
}

fn open_handler() -> KeyHandler {
    Box::new(|cx, (path, is_dir), _, _| {
        if *is_dir {
            let root = helix_stdx::path::normalize(path);
            cx.spawn_ui(async move {
                Ok(UiCommand::Layer(LayerCommand::RefreshFileExplorer {
                    cursor: 0,
                    root,
                }))
            });
        } else if let Err(e) = cx.editor.open(path, Action::Replace) {
            let err = if let Some(err) = e.source() {
                format!("{}", err)
            } else {
                format!("unable to open \"{}\"", path.display())
            };
            cx.editor.set_error(err);
        }
    })
}

fn shared_key_handlers() -> PickerKeyHandlers<ExplorerItem, ExplorerData> {
    hashmap! {
        key!('m') => move_handler(),
        key!('d') => delete_handler(),
        key!('c') => copy_handler(),
        key!('y') => yank_path_handler(),
        key!('u') => refresh_handler(),
        key!('h') => parent_handler(),
        key!(Backspace) => parent_handler(),
        key!('l') => open_handler(),
        key!(Right) => open_handler(),
        key!(Left) => parent_handler(),
        alt!('n') => create_handler(),
        alt!('m') => move_handler(),
        alt!('d') => delete_handler(),
        alt!('c') => copy_handler(),
        alt!('y') => yank_path_handler(),
    }
}

fn key_handlers(
    editing_engine: EditingEngineConfig,
) -> PickerKeyHandlers<ExplorerItem, ExplorerData> {
    let mut handlers = shared_key_handlers();
    match editing_engine {
        EditingEngineConfig::Helix => {
            handlers.insert(key!('n'), create_handler());
        }
        EditingEngineConfig::Vim => {
            handlers.insert(key!('a'), create_handler());
            handlers.insert(key!('r'), move_handler());
        }
    }
    handlers
}

pub fn file_explorer(
    cursor: Option<u32>,
    root: PathBuf,
    editor: &Editor,
    ingress: helix_runtime::Sender<crate::runtime::RuntimeEvent>,
) -> Result<FileExplorer, std::io::Error> {
    let editing_engine = editor.config().editing_engine;
    let directory_style = editor.theme.get("ui.text.directory");
    let directory_content = directory_content(&root, editor)?;

    let columns = [PickerColumn::new(
        "path",
        |(path, is_dir): &ExplorerItem, (root, directory_style): &ExplorerData| {
            let name = path.strip_prefix(root).unwrap_or(path).to_string_lossy();
            if *is_dir {
                Span::styled(format!("{}/", name), *directory_style).into()
            } else {
                name.into()
            }
        },
    )];

    let picker = Picker::new(
        columns,
        0,
        directory_content,
        (root, directory_style),
        crate::ui::PickerRuntime::new(editor.runtime()),
        ingress,
        move |cx: &mut crate::compositor::Context, (path, is_dir): &ExplorerItem, action| {
            if *is_dir {
                let new_root = helix_stdx::path::normalize(path);
                cx.spawn_ui(async move {
                    Ok(UiCommand::Layer(LayerCommand::PushFileExplorer {
                        cursor: None,
                        root: new_root,
                    }))
                });
            } else if let Err(e) = cx.editor.open(path, action) {
                let err = if let Some(err) = e.source() {
                    format!("{}", err)
                } else {
                    format!("unable to open \"{}\"", path.display())
                };
                cx.editor.set_error(err);
            }
        },
    )
    .with_cursor(cursor.unwrap_or_default())
    .with_preview(|_editor, (path, _is_dir): &ExplorerItem| Some((path.as_path().into(), None)))
    .with_key_handlers(key_handlers(editing_engine));

    Ok(picker)
}
