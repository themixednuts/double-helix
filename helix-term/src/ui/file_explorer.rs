use std::error::Error as _;
use std::{
    path::PathBuf,
};

use helix_core::hashmap;
use helix_view::{theme::Style, Editor};
use tui::text::Span;

use crate::{
    alt,
    runtime::{ui::command::FileExplorerCommand, LayerCommand, UiCommand},
};

use super::prompt::Movement;
use super::{directory_content, picker::PickerKeyHandler, Picker, PickerColumn};

/// for each path: (path to item, is the path a directory?)
type ExplorerItem = (PathBuf, bool);
/// (file explorer root, directory style)
type ExplorerData = (PathBuf, Style);

type FileExplorer = Picker<ExplorerItem, ExplorerData>;

type KeyHandler = PickerKeyHandler<ExplorerItem, ExplorerData>;

pub fn file_explorer(
    cursor: Option<u32>,
    root: PathBuf,
    editor: &Editor,
    ingress: helix_runtime::Sender<crate::runtime::RuntimeEvent>,
) -> Result<FileExplorer, std::io::Error> {
    let directory_style = editor.theme.get("ui.text.directory");
    let directory_content = directory_content(&root, editor)?;

    let yank_path: KeyHandler = Box::new(|cx, (path, _), _, _| {
        let register = cx
            .editor
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
    });

    let create: KeyHandler = Box::new(|cx, (path, _), data, cursor| {
        let prefill = path
            .parent()
            .map(|p| format!("{}{}", p.display(), std::path::MAIN_SEPARATOR))
            .unwrap_or_default();
        cx.spawn_ui(async move {
            Ok(UiCommand::FileExplorer(FileExplorerCommand::PromptCreate {
                root: data.0.clone(),
                cursor,
                prefill,
            }))
        });
    });

    let move_: KeyHandler = Box::new(|cx, (path, _), data, cursor| {
        let movement = path.extension().map(|ext| Movement::BackwardChar(ext.len() + 1));
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
    });

    let delete: KeyHandler = Box::new(|cx, (path, _), data, cursor| {
        let target = path.to_path_buf();
        cx.spawn_ui(async move {
            Ok(UiCommand::FileExplorer(FileExplorerCommand::PromptDelete {
                target,
                root: data.0.clone(),
                cursor,
            }))
        });
    });

    let copy: KeyHandler = Box::new(|cx, (path, _), data, cursor| {
        let source = path.to_path_buf();
        let prefill = path
            .parent()
            .map(|p| format!("{}{}", p.display(), std::path::MAIN_SEPARATOR))
            .unwrap_or_default();
        cx.spawn_ui(async move {
            Ok(UiCommand::FileExplorer(FileExplorerCommand::PromptCopy {
                source,
                root: data.0.clone(),
                cursor,
                prefill,
            }))
        });
    });

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
        editor.runtime().clone(),
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
    .with_key_handlers(hashmap! {
        alt!('n') => create,
        alt!('m') => move_,
        alt!('d') => delete,
        alt!('c') => copy,
        alt!('y') => yank_path,
    });

    Ok(picker)
}
