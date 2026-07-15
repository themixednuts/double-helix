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
            navigation::jump_to_location(
                cx.editor,
                &cx.ingress,
                &cx.foreground,
                &item.location,
                action,
            );
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
            navigation::jump_to_location(
                cx.editor,
                &cx.ingress,
                &cx.foreground,
                &item.location,
                action,
            );
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
        request_hierarchy_items(editor, ingress, items[0].clone(), empty_message);
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
            request_hierarchy_items(cx.editor, ingress.clone(), item.clone(), empty_message);
        },
    )
    .truncate_start(false);

    compositor.push(Box::new(overlaid(picker)));
}

fn request_hierarchy_items(
    editor: &mut helix_view::Editor,
    ingress: crate::runtime::RuntimeIngress,
    item: LspHierarchyPrepareItem,
    empty_message: &'static str,
) {
    let server_id = match &item {
        LspHierarchyPrepareItem::Call { server_id, .. }
        | LspHierarchyPrepareItem::Type { server_id, .. } => *server_id,
    };
    let Some(language_server) = editor.language_server_client(server_id).cloned() else {
        editor.set_error("Language server is no longer available");
        return;
    };
    editor.set_status("Loading hierarchy...");
    editor
        .work()
        .spawn(async move {
            let result = hierarchy_items_for_prepare(language_server, item).await;
            match result {
                Ok(items) => {
                    let _ = ingress
                        .send_ui(crate::runtime::UiCommand::Lsp(
                            crate::runtime::ui::command::LspCommand::Hierarchy {
                                items,
                                empty_message,
                            },
                        ))
                        .await;
                }
                Err(error) => ingress.status(anyhow::anyhow!("Failed to load hierarchy: {error}")),
            }
        })
        .detach();
}

async fn hierarchy_items_for_prepare(
    language_server: std::sync::Arc<helix_lsp::Client>,
    item: LspHierarchyPrepareItem,
) -> helix_lsp::Result<Vec<LspHierarchyPickerItem>> {
    Ok(match item {
        LspHierarchyPrepareItem::Call {
            offset_encoding,
            item,
            direction,
            ..
        } => match direction {
            LspCallHierarchyDirection::Incoming => {
                let Some(request) = language_server.call_hierarchy_incoming_calls(item, None)
                else {
                    return Ok(Vec::new());
                };
                let calls = request.await?.unwrap_or_default();
                crate::commands::lsp::call_hierarchy_incoming_picker_items(calls, offset_encoding)
            }
            LspCallHierarchyDirection::Outgoing => {
                let Some(request) = language_server.call_hierarchy_outgoing_calls(item, None)
                else {
                    return Ok(Vec::new());
                };
                let calls = request.await?.unwrap_or_default();
                crate::commands::lsp::call_hierarchy_outgoing_picker_items(calls, offset_encoding)
            }
        },
        LspHierarchyPrepareItem::Type {
            offset_encoding,
            item,
            direction,
            ..
        } => match direction {
            LspTypeHierarchyDirection::Supertypes => {
                let Some(request) = language_server.type_hierarchy_supertypes(item, None) else {
                    return Ok(Vec::new());
                };
                let items = request.await?.unwrap_or_default();
                crate::commands::lsp::type_hierarchy_picker_items(items, offset_encoding)
            }
            LspTypeHierarchyDirection::Subtypes => {
                let Some(request) = language_server.type_hierarchy_subtypes(item, None) else {
                    return Ok(Vec::new());
                };
                let items = request.await?.unwrap_or_default();
                crate::commands::lsp::type_hierarchy_picker_items(items, offset_encoding)
            }
        },
    })
}
