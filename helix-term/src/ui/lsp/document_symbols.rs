//! Document symbol picker (`textDocument/documentSymbol`).

use helix_lsp::lsp;
use helix_view::{icons::ICONS, theme::Style};
use tui::text::Span;

use crate::compositor::Compositor;
use crate::runtime::ui::command::{
    DocumentSymbolPickerItem, LspCallHierarchyDirection, LspHierarchyPickerItem,
    LspHierarchyPrepareItem, LspTypeHierarchyDirection,
};
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

fn symbol_kind_span(kind: lsp::SymbolKind) -> tui::text::Spans<'static> {
    let icons = ICONS.load();
    let name = display_symbol_kind(kind);

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
}

pub fn show_hierarchy_picker(
    editor: &mut helix_view::Editor,
    compositor: &mut Compositor,
    ingress: crate::runtime::RuntimeIngress,
    items: Vec<LspHierarchyPickerItem>,
    empty_message: &'static str,
) {
    if items.is_empty() {
        editor.set_status(empty_message);
        return;
    }

    let columns = [
        ui::PickerColumn::new("kind", |item: &LspHierarchyPickerItem, _| {
            symbol_kind_span(item.kind).into()
        }),
        ui::PickerColumn::new("name", |item: &LspHierarchyPickerItem, _| {
            item.name.as_str().into()
        }),
        ui::PickerColumn::new("detail", |item: &LspHierarchyPickerItem, _| {
            item.detail.as_deref().unwrap_or_default().into()
        }),
        ui::PickerColumn::new("path", |item: &LspHierarchyPickerItem, _| {
            if let Some(path) = item.location.uri.as_path() {
                format!("{}:{}", path.display(), item.location.range.start.line + 1).into()
            } else {
                item.location.uri.to_string().into()
            }
        }),
    ];

    let picker = Picker::new(
        columns,
        1,
        items,
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

pub fn show_hierarchy_prepare_picker(
    editor: &mut helix_view::Editor,
    compositor: &mut Compositor,
    ingress: crate::runtime::RuntimeIngress,
    items: Vec<LspHierarchyPrepareItem>,
    empty_message: &'static str,
) {
    if items.is_empty() {
        editor.set_status(empty_message);
        return;
    }
    if items.len() == 1 {
        let hierarchy_items = hierarchy_items_for_prepare(editor, items[0].clone());
        show_hierarchy_picker(editor, compositor, ingress, hierarchy_items, empty_message);
        return;
    }

    let columns = [
        ui::PickerColumn::new("kind", |item: &LspHierarchyPrepareItem, _| {
            symbol_kind_span(match item {
                LspHierarchyPrepareItem::Call { item, .. } => item.kind,
                LspHierarchyPrepareItem::Type { item, .. } => item.kind,
            })
            .into()
        }),
        ui::PickerColumn::new("name", |item: &LspHierarchyPrepareItem, _| {
            match item {
                LspHierarchyPrepareItem::Call { item, .. } => item.name.as_str(),
                LspHierarchyPrepareItem::Type { item, .. } => item.name.as_str(),
            }
            .into()
        }),
        ui::PickerColumn::new("detail", |item: &LspHierarchyPrepareItem, _| {
            match item {
                LspHierarchyPrepareItem::Call { item, .. } => item.detail.as_deref(),
                LspHierarchyPrepareItem::Type { item, .. } => item.detail.as_deref(),
            }
            .unwrap_or_default()
            .into()
        }),
    ];

    let picker = Picker::new(
        columns,
        1,
        items,
        (),
        crate::ui::PickerRuntime::new(editor),
        ingress.clone(),
        move |cx: &mut crate::compositor::Context, item, _action| {
            let hierarchy_items = hierarchy_items_for_prepare(cx.editor, item.clone());
            ingress.ui(crate::runtime::UiCommand::Lsp(
                crate::runtime::ui::command::LspCommand::Hierarchy {
                    items: hierarchy_items,
                    empty_message,
                },
            ));
        },
    )
    .truncate_start(false);

    compositor.push(Box::new(overlaid(picker)));
}

fn hierarchy_items_for_prepare(
    editor: &mut helix_view::Editor,
    item: LspHierarchyPrepareItem,
) -> Vec<LspHierarchyPickerItem> {
    match item {
        LspHierarchyPrepareItem::Call {
            server_id,
            offset_encoding,
            item,
            direction,
        } => {
            let Some(language_server) = editor.language_server_by_id(server_id) else {
                editor.set_error("Language server is no longer available");
                return Vec::new();
            };
            match direction {
                LspCallHierarchyDirection::Incoming => language_server
                    .call_hierarchy_incoming_calls(item, None)
                    .and_then(|request| helix_lsp::block_on(request).ok())
                    .flatten()
                    .map(|calls| {
                        crate::commands::lsp::call_hierarchy_incoming_picker_items(
                            calls,
                            offset_encoding,
                        )
                    }),
                LspCallHierarchyDirection::Outgoing => language_server
                    .call_hierarchy_outgoing_calls(item, None)
                    .and_then(|request| helix_lsp::block_on(request).ok())
                    .flatten()
                    .map(|calls| {
                        crate::commands::lsp::call_hierarchy_outgoing_picker_items(
                            calls,
                            offset_encoding,
                        )
                    }),
            }
            .unwrap_or_default()
        }
        LspHierarchyPrepareItem::Type {
            server_id,
            offset_encoding,
            item,
            direction,
        } => {
            let Some(language_server) = editor.language_server_by_id(server_id) else {
                editor.set_error("Language server is no longer available");
                return Vec::new();
            };
            let result = match direction {
                LspTypeHierarchyDirection::Supertypes => language_server
                    .type_hierarchy_supertypes(item, None)
                    .and_then(|request| helix_lsp::block_on(request).ok())
                    .flatten(),
                LspTypeHierarchyDirection::Subtypes => language_server
                    .type_hierarchy_subtypes(item, None)
                    .and_then(|request| helix_lsp::block_on(request).ok())
                    .flatten(),
            }
            .unwrap_or_default();
            crate::commands::lsp::type_hierarchy_picker_items(result, offset_encoding)
        }
    }
}
