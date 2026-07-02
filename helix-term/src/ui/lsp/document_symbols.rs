//! Document symbol picker (`textDocument/documentSymbol`).

use helix_lsp::lsp;
use helix_view::{icons::ICONS, theme::Style};
use tui::text::Span;

use crate::compositor::Compositor;
use crate::runtime::ui::command::DocumentSymbolPickerItem;
use crate::ui::{self, lsp::navigation, overlay::overlaid, Picker};

pub fn display_symbol_kind(kind: lsp::SymbolKind) -> &'static str {
    match kind {
        lsp::SymbolKind::FILE => "file",
        lsp::SymbolKind::MODULE => "module",
        lsp::SymbolKind::NAMESPACE => "namespace",
        lsp::SymbolKind::PACKAGE => "package",
        lsp::SymbolKind::CLASS => "class",
        lsp::SymbolKind::METHOD => "method",
        lsp::SymbolKind::PROPERTY => "property",
        lsp::SymbolKind::FIELD => "field",
        lsp::SymbolKind::CONSTRUCTOR => "construct",
        lsp::SymbolKind::ENUM => "enum",
        lsp::SymbolKind::INTERFACE => "interface",
        lsp::SymbolKind::FUNCTION => "function",
        lsp::SymbolKind::VARIABLE => "variable",
        lsp::SymbolKind::CONSTANT => "constant",
        lsp::SymbolKind::STRING => "string",
        lsp::SymbolKind::NUMBER => "number",
        lsp::SymbolKind::BOOLEAN => "boolean",
        lsp::SymbolKind::ARRAY => "array",
        lsp::SymbolKind::OBJECT => "object",
        lsp::SymbolKind::KEY => "key",
        lsp::SymbolKind::NULL => "null",
        lsp::SymbolKind::ENUM_MEMBER => "enum_member",
        lsp::SymbolKind::STRUCT => "struct",
        lsp::SymbolKind::EVENT => "event",
        lsp::SymbolKind::OPERATOR => "operator",
        lsp::SymbolKind::TYPE_PARAMETER => "typeparam",
        _ => {
            log::warn!("Unknown symbol kind: {:?}", kind);
            ""
        }
    }
}

pub fn show_document_symbol_picker(
    editor: &mut helix_view::Editor,
    compositor: &mut Compositor,
    ingress: crate::runtime::RuntimeIngress,
    symbols: Vec<DocumentSymbolPickerItem>,
) {
    if symbols.is_empty() {
        editor.set_status("No document symbols found.");
        return;
    }

    let columns = [
        ui::PickerColumn::new("kind", |item: &DocumentSymbolPickerItem, _| {
            let icons = ICONS.load();
            let name = display_symbol_kind(item.symbol.kind);

            if let Some(icon) = icons.kind().get(name) {
                if let Some(color) = icon.color() {
                    Span::styled(
                        format!("{}  {name}", icon.glyph()),
                        Style::default().fg(color),
                    )
                    .into()
                } else {
                    format!("{}  {name}", icon.glyph()).into()
                }
            } else {
                name.into()
            }
        }),
        ui::PickerColumn::new("name", |item: &DocumentSymbolPickerItem, _| {
            item.symbol.name.as_str().into()
        }),
        ui::PickerColumn::new("container", |item: &DocumentSymbolPickerItem, _| {
            item.symbol
                .container_name
                .as_deref()
                .unwrap_or_default()
                .into()
        }),
    ];

    let picker = Picker::new(
        columns,
        1,
        symbols,
        (),
        crate::ui::PickerRuntime::new(editor),
        ingress,
        move |cx: &mut crate::compositor::Context, item, action| {
            navigation::jump_to_location(cx.editor, &item.location, action);
        },
    )
    .with_preview(move |_editor, item| navigation::location_to_file_location(&item.location))
    .truncate_start(false);

    compositor.push(Box::new(overlaid(picker)));
}
