//! LSP code action menu / picker (typed [`crate::runtime::ui::command::LspCommand::CodeActions`]).

use helix_lsp::lsp;
use helix_view::Editor;

use crate::compositor::Compositor;
use crate::runtime::ui::command::LspCodeActionItem;
use crate::runtime::RuntimeEvent;
use crate::ui::{self, overlay::overlaid, Popup, PromptEvent};

pub fn apply_code_action_item(
    editor: &mut Editor,
    ingress: helix_runtime::Sender<RuntimeEvent>,
    item: &LspCodeActionItem,
) {
    crate::effect::language_server::request_apply_code_action(editor, item.clone(), ingress);
}

pub fn show_code_action_menu(
    editor: &mut Editor,
    compositor: &mut Compositor,
    ingress: helix_runtime::Sender<RuntimeEvent>,
    items: Vec<LspCodeActionItem>,
) {
    if items.is_empty() {
        editor.set_error("No code actions available");
        return;
    }
    let mut picker = ui::Menu::new(items, (), move |editor, action, event| {
        if event != PromptEvent::Validate {
            return;
        }
        let action = action.unwrap();
        apply_code_action_item(editor, ingress.clone(), action);
    });
    picker.move_down();

    let popup = Popup::new("code-action", picker).with_scrollbar(false);
    compositor.replace_or_push("code-action", popup);
}

pub fn show_code_action_picker(
    editor: &mut Editor,
    compositor: &mut Compositor,
    ingress: helix_runtime::Sender<crate::runtime::RuntimeEvent>,
    items: Vec<LspCodeActionItem>,
) {
    if items.is_empty() {
        editor.set_error("No code actions available");
        return;
    }
    let columns = [ui::PickerColumn::new(
        "action",
        |item: &LspCodeActionItem, _| match &item.lsp_item {
            lsp::CodeActionOrCommand::CodeAction(action) => action.title.as_str().into(),
            lsp::CodeActionOrCommand::Command(command) => command.title.as_str().into(),
        },
    )];

    let picker = ui::Picker::new(
        columns,
        0,
        items,
        (),
        ui::PickerRuntime::new(editor.runtime()),
        ingress.clone(),
        move |cx: &mut crate::compositor::Context, lsp_item, _action| {
            apply_code_action_item(cx.editor, cx.ingress.clone(), lsp_item);
        },
    );
    compositor.push(Box::new(overlaid(picker)));
}
