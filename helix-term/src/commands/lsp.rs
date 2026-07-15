use futures_util::stream::FuturesOrdered;
use helix_lsp::{
    lsp::{
        self, CodeAction, CodeActionOrCommand, CodeActionTriggerKind, DiagnosticSeverity,
        NumberOrString,
    },
    util::{diagnostic_to_lsp_diagnostic, lsp_range_to_range, range_to_lsp_range},
    Client, LanguageServerId, OffsetEncoding,
};

use tokio_stream::StreamExt;
use tui::text::Span;

use super::{Context, Editor};

use helix_core::{
    diagnostic::DiagnosticProvider, syntax::config::LanguageServerFeature,
    text_folding::ropex::RopeSliceFoldExt, Uri,
};
use helix_stdx::path;
use helix_view::{
    document::{DocumentInlayHint, DocumentInlayHintsId},
    editor::Action,
    handlers::lsp::SignatureHelpInvoked,
    icons::ICONS,
    theme::Style,
    Document, DocumentId, View,
};

use crate::{
    compositor::{self, Component},
    runtime::ui::command::{
        DocumentSymbolPickerItem, LspCallHierarchyDirection, LspCodeActionItem,
        LspCodeActionPresentation, LspCommand, LspHierarchyPickerItem, LspHierarchyPrepareItem,
        LspHoverDisplay, LspLocation, LspTypeHierarchyDirection,
    },
    runtime::{RuntimeTaskEvent, UiCommand},
    ui::{
        self, lsp::document_symbols::display_symbol_kind, lsp::navigation, menu::Row,
        overlay::overlaid, Picker, PromptEvent,
    },
};

use std::{cmp::Ordering, collections::HashSet, fmt::Display, future::Future};

#[derive(Clone)]
struct LspCodeLensPickerItem {
    doc_id: DocumentId,
    expected_version: i32,
    server_id: LanguageServerId,
    lens: lsp::CodeLens,
}

#[derive(Clone)]
struct LspDocumentLinkPickerItem {
    doc_id: DocumentId,
    expected_version: i32,
    server_id: LanguageServerId,
    link: lsp::DocumentLink,
    text: String,
}

/// Gets the first language server that is attached to a document which supports a specific feature.
/// If there is no configured language server that supports the feature, this displays a status message.
/// Using this macro in a context where the editor automatically queries the LSP
/// (instead of when the user explicitly does so via a keybind like `gd`)
/// will spam the "No configured language server supports \<feature>" status message confusingly.
#[macro_export]
macro_rules! language_server_with_feature {
    ($editor:expr, $doc:expr, $feature:expr) => {{
        let language_server = $doc.language_servers_with_feature($feature).next();
        match language_server {
            Some(language_server) => language_server,
            None => {
                $editor.set_error(format!(
                    "No configured language server supports {}",
                    $feature
                ));
                return;
            }
        }
    }};
}

struct DiagnosticStyles {
    hint: Style,
    info: Style,
    warning: Style,
    error: Style,
}

struct PickerDiagnostic {
    location: LspLocation,
    diag: lsp::Diagnostic,
}

#[derive(Copy, Clone, PartialEq)]
enum DiagnosticsFormat {
    ShowSourcePath,
    HideSourcePath,
}

type DiagnosticsPicker = Picker<PickerDiagnostic, DiagnosticStyles>;

fn diag_picker(
    cx: &Context,
    diagnostics: impl IntoIterator<Item = (Uri, Vec<(lsp::Diagnostic, DiagnosticProvider)>)>,
    format: DiagnosticsFormat,
) -> DiagnosticsPicker {
    // TODO: drop current_path comparison and instead use workspace: bool flag?

    // flatten the map to a vec of (url, diag) pairs
    let mut flat_diag = Vec::new();
    for (uri, diags) in diagnostics {
        flat_diag.reserve(diags.len());

        for (diag, provider) in diags {
            if let Some(ls) = provider
                .language_server_id()
                .and_then(|id| cx.editor.language_server_by_id(id))
            {
                flat_diag.push(PickerDiagnostic {
                    location: LspLocation {
                        uri: uri.clone(),
                        range: diag.range,
                        offset_encoding: ls.offset_encoding(),
                    },
                    diag,
                });
            }
        }
    }

    flat_diag.sort_by(|a, b| {
        a.diag
            .severity
            .unwrap_or(lsp::DiagnosticSeverity::HINT)
            .cmp(&b.diag.severity.unwrap_or(lsp::DiagnosticSeverity::HINT))
    });

    let styles = DiagnosticStyles {
        hint: cx.editor.theme.get("hint"),
        info: cx.editor.theme.get("info"),
        warning: cx.editor.theme.get("warning"),
        error: cx.editor.theme.get("error"),
    };

    let mut columns = vec![
        ui::PickerColumn::new(
            "severity",
            |item: &PickerDiagnostic, styles: &DiagnosticStyles| {
                let icons = ICONS.load();
                match item.diag.severity {
                    Some(DiagnosticSeverity::HINT) => {
                        Span::styled(format!("{} HINT", icons.diagnostic().hint()), styles.hint)
                    }
                    Some(DiagnosticSeverity::INFORMATION) => {
                        Span::styled(format!("{} INFO", icons.diagnostic().info()), styles.info)
                    }
                    Some(DiagnosticSeverity::WARNING) => Span::styled(
                        format!("{} WARN", icons.diagnostic().warning()),
                        styles.warning,
                    ),
                    Some(DiagnosticSeverity::ERROR) => Span::styled(
                        format!("{} ERROR", icons.diagnostic().error()),
                        styles.error,
                    ),
                    _ => Span::raw(""),
                }
                .into()
            },
        ),
        ui::PickerColumn::new("source", |item: &PickerDiagnostic, _| {
            item.diag.source.as_deref().unwrap_or("").into()
        }),
        ui::PickerColumn::new("message", |item: &PickerDiagnostic, _| {
            item.diag.message.as_str().into()
        }),
        ui::PickerColumn::new("code", |item: &PickerDiagnostic, _| {
            match item.diag.code.as_ref() {
                Some(NumberOrString::Number(n)) => n.to_string().into(),
                Some(NumberOrString::String(s)) => s.as_str().into(),
                None => "".into(),
            }
        }),
    ];
    let mut primary_column = 2; // message

    if format == DiagnosticsFormat::ShowSourcePath {
        columns.insert(
            // between message and code
            3,
            ui::PickerColumn::new("path", |item: &PickerDiagnostic, _| {
                if let Some(path) = item.location.uri.as_path() {
                    path::get_truncated_path(path)
                        .to_string_lossy()
                        .to_string()
                        .into()
                } else {
                    Default::default()
                }
            }),
        );
        primary_column += 1;
    }

    Picker::new(
        columns,
        primary_column,
        flat_diag,
        styles,
        crate::ui::PickerRuntime::new(cx.editor),
        cx.ingress.clone(),
        move |cx: &mut crate::compositor::Context, diag: &PickerDiagnostic, action| {
            navigation::jump_to_location(
                cx.editor,
                &cx.ingress,
                &cx.foreground,
                &diag.location,
                action,
            );
            let (view_id, doc) = focused!(cx.editor);
            let view = view_mut!(cx.editor, view_id);
            view.diagnostics_handler
                .immediately_show_diagnostic(doc, view_id);
        },
    )
    .with_preview(move |_editor, diag| navigation::location_to_file_location(&diag.location))
    .truncate_start(false)
}

pub fn symbol_picker(cx: &mut Context) {
    fn nested_to_flat(
        list: &mut Vec<DocumentSymbolPickerItem>,
        file: &lsp::TextDocumentIdentifier,
        uri: &Uri,
        symbol: lsp::DocumentSymbol,
        offset_encoding: OffsetEncoding,
    ) {
        #[allow(deprecated)]
        list.push(DocumentSymbolPickerItem {
            symbol: lsp::SymbolInformation {
                name: symbol.name,
                kind: symbol.kind,
                tags: symbol.tags,
                deprecated: symbol.deprecated,
                location: lsp::Location::new(file.uri.clone(), symbol.selection_range),
                container_name: None,
            },
            location: LspLocation {
                uri: uri.clone(),
                range: symbol.selection_range,
                offset_encoding,
            },
        });
        for child in symbol.children.into_iter().flatten() {
            nested_to_flat(list, file, uri, child, offset_encoding);
        }
    }
    let (_, doc) = focused_ref!(cx.editor);

    let mut seen_language_servers = HashSet::new();

    let mut futures: FuturesOrdered<_> = doc
        .language_servers_with_feature(LanguageServerFeature::DocumentSymbols)
        .filter(|ls| seen_language_servers.insert(ls.id()))
        .map(|language_server| {
            let request = language_server.document_symbols(doc.identifier()).unwrap();
            let offset_encoding = language_server.offset_encoding();
            let doc_id = doc.identifier();
            let doc_uri = doc
                .uri()
                .expect("docs with active language servers must be backed by paths");

            async move {
                let symbols = match request.await? {
                    Some(symbols) => symbols,
                    None => return anyhow::Ok(vec![]),
                };
                // lsp has two ways to represent symbols (flat/nested)
                // convert the nested variant to flat, so that we have a homogeneous list
                let symbols = match symbols {
                    lsp::DocumentSymbolResponse::Flat(symbols) => symbols
                        .into_iter()
                        .map(|symbol| DocumentSymbolPickerItem {
                            location: LspLocation {
                                uri: doc_uri.clone(),
                                range: symbol.location.range,
                                offset_encoding,
                            },
                            symbol,
                        })
                        .collect(),
                    lsp::DocumentSymbolResponse::Nested(symbols) => {
                        let mut flat_symbols = Vec::new();
                        for symbol in symbols {
                            nested_to_flat(
                                &mut flat_symbols,
                                &doc_id,
                                &doc_uri,
                                symbol,
                                offset_encoding,
                            )
                        }
                        flat_symbols
                    }
                };
                Ok(symbols)
            }
        })
        .collect();

    if futures.is_empty() {
        cx.editor
            .set_error("No configured language server supports document symbols");
        return;
    }

    cx.spawn_ui(async move {
        let mut symbols = Vec::new();
        while let Some(response) = futures.next().await {
            match response {
                Ok(mut items) => symbols.append(&mut items),
                Err(err) => log::error!("Error requesting document symbols: {err}"),
            }
        }
        Ok(UiCommand::Lsp(LspCommand::DocumentSymbols { symbols }))
    });
}

pub fn workspace_symbol_picker(cx: &mut Context) {
    use crate::ui::picker::Injector;

    let (_, doc) = focused_ref!(cx.editor);
    if doc
        .language_servers_with_feature(LanguageServerFeature::WorkspaceSymbols)
        .count()
        == 0
    {
        cx.editor
            .set_error("No configured language server supports workspace symbols");
        return;
    }

    let get_symbols = |pattern: &str,
                       editor: &mut Editor,
                       _data,
                       injector: &Injector<_, _>,
                       work: helix_runtime::Work,
                       _block: helix_runtime::Block| {
        let (_, doc) = focused_ref!(editor);
        let mut seen_language_servers = HashSet::new();
        let mut futures: FuturesOrdered<_> = doc
            .language_servers_with_feature(LanguageServerFeature::WorkspaceSymbols)
            .filter(|ls| seen_language_servers.insert(ls.id()))
            .map(|language_server| {
                let request = language_server
                    .workspace_symbols(pattern.to_string())
                    .unwrap();
                let offset_encoding = language_server.offset_encoding();
                async move {
                    let symbols = request
                        .await?
                        .and_then(|resp| match resp {
                            lsp::WorkspaceSymbolResponse::Flat(symbols) => Some(symbols),
                            lsp::WorkspaceSymbolResponse::Nested(_) => None,
                        })
                        .unwrap_or_default();

                    let response: Vec<_> = symbols
                        .into_iter()
                        .filter_map(|symbol| {
                            let uri = match Uri::try_from(&symbol.location.uri) {
                                Ok(uri) => uri,
                                Err(err) => {
                                    log::warn!("discarding symbol with invalid URI: {err}");
                                    return None;
                                }
                            };
                            Some(DocumentSymbolPickerItem {
                                location: LspLocation {
                                    uri,
                                    range: symbol.location.range,
                                    offset_encoding,
                                },
                                symbol,
                            })
                        })
                        .collect();

                    anyhow::Ok(response)
                }
            })
            .collect();

        if futures.is_empty() {
            editor.set_error("No configured language server supports workspace symbols");
        }

        let injector = injector.clone();
        work.spawn(async move {
            while let Some(response) = futures.next().await {
                match response {
                    Ok(items) => {
                        for item in items {
                            injector.push(item)?;
                        }
                    }
                    Err(err) => log::error!("Error requesting workspace symbols: {err}"),
                }
            }
            Ok(())
        })
    };
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
        })
        .without_filtering(),
        ui::PickerColumn::new("container", |item: &DocumentSymbolPickerItem, _| {
            item.symbol
                .container_name
                .as_deref()
                .unwrap_or_default()
                .into()
        }),
        ui::PickerColumn::new("path", |item: &DocumentSymbolPickerItem, _| {
            if let Some(path) = item.location.uri.as_path() {
                path::get_relative_path(path)
                    .to_string_lossy()
                    .to_string()
                    .into()
            } else {
                item.symbol.location.uri.to_string().into()
            }
        }),
    ];

    let picker = Picker::new(
        columns,
        1, // name column
        [],
        (),
        crate::ui::PickerRuntime::new(cx.editor),
        cx.ingress.clone(),
        move |cx: &mut crate::compositor::Context, item: &DocumentSymbolPickerItem, action| {
            navigation::jump_to_location(
                cx.editor,
                &cx.ingress,
                &cx.foreground,
                &item.location,
                action,
            );
        },
    )
    .with_preview(|_editor, item| navigation::location_to_file_location(&item.location))
    .with_dynamic_query(get_symbols, ui::picker::DynamicQuerySchedule::Immediate)
    .truncate_start(false);

    cx.push_layer(Box::new(overlaid(picker)));
}

pub fn diagnostics_picker(cx: &mut Context) {
    let (_, doc) = focused_ref!(cx.editor);
    if let Some(uri) = doc.uri() {
        let diagnostics = cx.editor.document_diagnostics(&uri);
        let picker = diag_picker(cx, [(uri, diagnostics)], DiagnosticsFormat::HideSourcePath);
        cx.push_layer(Box::new(overlaid(picker)));
    }
}

pub fn workspace_diagnostics_picker(cx: &mut Context) {
    // TODO not yet filtered by LanguageServerFeature, need to do something similar as Document::shown_diagnostics here for all open documents
    let diagnostics = cx.editor.diagnostics_snapshot();
    let picker = diag_picker(cx, diagnostics, DiagnosticsFormat::ShowSourcePath);
    cx.push_layer(Box::new(overlaid(picker)));
}

impl ui::menu::Item for LspCodeActionItem {
    type Data = ();
    fn format(&self, _data: &Self::Data) -> Row<'_> {
        match &self.lsp_item {
            lsp::CodeActionOrCommand::CodeAction(action) => action.title.as_str().into(),
            lsp::CodeActionOrCommand::Command(command) => command.title.as_str().into(),
        }
    }
}

/// Determines the category of the `CodeAction` using the `CodeAction::kind` field.
/// Returns a number that represent these categories.
/// Categories with a lower number should be displayed first.
///
///
/// While the `kind` field is defined as open ended in the LSP spec (any value may be used)
/// in practice a closed set of common values (mostly suggested in the LSP spec) are used.
/// VSCode displays each of these categories separately (separated by a heading in the codeactions picker)
/// to make them easier to navigate. Helix does not display these  headings to the user.
/// However it does sort code actions by their categories to achieve the same order as the VScode picker,
/// just without the headings.
///
/// The order used here is modeled after the [vscode sourcecode](https://github.com/microsoft/vscode/blob/eaec601dd69aeb4abb63b9601a6f44308c8d8c6e/src/vs/editor/contrib/codeAction/browser/codeActionWidget.ts>)
fn action_category(action: &CodeActionOrCommand) -> u32 {
    if let CodeActionOrCommand::CodeAction(CodeAction {
        kind: Some(kind), ..
    }) = action
    {
        let mut components = kind.as_str().split('.');
        match components.next() {
            Some("quickfix") => 0,
            Some("refactor") => match components.next() {
                Some("extract") => 1,
                Some("inline") => 2,
                Some("rewrite") => 3,
                Some("move") => 4,
                Some("surround") => 5,
                _ => 7,
            },
            Some("source") => 6,
            _ => 7,
        }
    } else {
        7
    }
}

fn action_preferred(action: &CodeActionOrCommand) -> bool {
    matches!(
        action,
        CodeActionOrCommand::CodeAction(CodeAction {
            is_preferred: Some(true),
            ..
        })
    )
}

fn action_fixes_diagnostics(action: &CodeActionOrCommand) -> bool {
    matches!(
        action,
        CodeActionOrCommand::CodeAction(CodeAction {
            diagnostics: Some(diagnostics),
            ..
        }) if !diagnostics.is_empty()
    )
}

pub fn code_action(cx: &mut Context) {
    code_action_inner(cx, false);
}

pub fn code_action_picker(cx: &mut Context) {
    code_action_inner(cx, true);
}

pub fn code_lens(cx: &mut Context) {
    let (view_id, doc) = focused_ref!(cx.editor);
    let doc_id = doc.id();
    let expected_version = doc.version();
    let text = doc.text();
    let selection_lines: HashSet<_> = doc
        .selection(view_id)
        .iter()
        .flat_map(|range| {
            let (start, end) = range.line_range(text.slice(..));
            start..=end
        })
        .collect();

    let Some(code_lenses) = doc.code_lenses() else {
        cx.editor.set_status("No code lenses available");
        return;
    };

    let mut items: Vec<_> = code_lenses
        .lenses
        .iter()
        .filter(|lens| {
            let line = text.char_to_line(lens.range.from().min(text.len_chars()));
            selection_lines.contains(&line)
        })
        .map(|lens| LspCodeLensPickerItem {
            doc_id,
            expected_version,
            server_id: lens.server_id,
            lens: lens.lens.clone(),
        })
        .collect();

    if items.is_empty() {
        items = code_lenses
            .lenses
            .iter()
            .map(|lens| LspCodeLensPickerItem {
                doc_id,
                expected_version,
                server_id: lens.server_id,
                lens: lens.lens.clone(),
            })
            .collect();
    }

    if items.is_empty() {
        cx.editor.set_status("No code lenses available");
        return;
    }

    let columns = [ui::PickerColumn::new(
        "title",
        |item: &LspCodeLensPickerItem, _| {
            item.lens
                .command
                .as_ref()
                .map(|command| command.title.as_str())
                .unwrap_or("unresolved code lens")
                .into()
        },
    )];
    let picker = Picker::new(
        columns,
        0,
        items,
        (),
        crate::ui::PickerRuntime::new(cx.editor),
        cx.ingress.clone(),
        move |cx: &mut compositor::Context, item: &LspCodeLensPickerItem, _action| {
            let Some(language_server) = cx.editor.language_server_by_id(item.server_id) else {
                cx.editor.set_error("Language Server disappeared");
                return;
            };
            if let Some(command) = item.lens.command.clone() {
                cx.submit_task(RuntimeTaskEvent::ExecuteLspCommand {
                    command,
                    server_id: item.server_id,
                });
                return;
            }
            let Some(resolve) = language_server.resolve_code_lens(&item.lens) else {
                cx.editor
                    .set_error("Code lens did not resolve to a command");
                return;
            };
            let doc_id = item.doc_id;
            let expected_version = item.expected_version;
            let server_id = item.server_id;
            let original = item.lens.clone();
            cx.editor.set_status("Resolving code lens...");
            cx.spawn_task_event(async move {
                let resolved = resolve.await?;
                Ok(RuntimeTaskEvent::ApplyResolvedCodeLens {
                    doc_id,
                    expected_version,
                    server_id,
                    original,
                    resolved,
                })
            });
        },
    );
    cx.push_layer(Box::new(overlaid(picker)));
}

pub fn document_links(cx: &mut Context) {
    let (_, doc) = focused_ref!(cx.editor);
    let doc_id = doc.id();
    let expected_version = doc.version();
    let Some(document_links) = doc.document_links() else {
        cx.editor.set_status("No document links available");
        return;
    };
    let text = doc.text().slice(..);
    let items: Vec<_> = document_links
        .links
        .iter()
        .map(|link| LspDocumentLinkPickerItem {
            doc_id,
            expected_version,
            server_id: link.server_id,
            text: link.range.fragment(text).into(),
            link: link.link.clone(),
        })
        .collect();
    if items.is_empty() {
        cx.editor.set_status("No document links available");
        return;
    }

    let columns = [
        ui::PickerColumn::new("text", |item: &LspDocumentLinkPickerItem, _| {
            item.text.as_str().into()
        }),
        ui::PickerColumn::new("target", |item: &LspDocumentLinkPickerItem, _| {
            item.link
                .target
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "unresolved".to_owned())
                .into()
        }),
    ];
    let picker = Picker::new(
        columns,
        0,
        items,
        (),
        crate::ui::PickerRuntime::new(cx.editor),
        cx.ingress.clone(),
        move |cx: &mut compositor::Context, item: &LspDocumentLinkPickerItem, action| {
            open_document_link(
                cx.editor,
                &cx.foreground,
                cx.ingress.clone(),
                item.clone(),
                action,
            );
        },
    )
    .truncate_start(false);
    cx.push_layer(Box::new(overlaid(picker)));
}

fn open_document_link(
    editor: &mut Editor,
    foreground: &crate::runtime::ForegroundEvents,
    ingress: crate::runtime::RuntimeIngress,
    item: LspDocumentLinkPickerItem,
    action: Action,
) {
    let Some(language_server) = editor.language_server_by_id(item.server_id) else {
        editor.set_error("Language Server disappeared");
        return;
    };
    let doc_id = item.doc_id;
    let expected_version = item.expected_version;
    if let Some(target) = item.link.target {
        if let Err(error) = foreground.task(RuntimeTaskEvent::OpenResolvedDocumentLink {
            doc_id,
            expected_version,
            target,
            action,
        }) {
            editor.set_error(error.to_string());
        }
        return;
    }
    let Some(resolve) = language_server.resolve_document_link(&item.link) else {
        editor.set_error("Document link did not resolve to a target");
        return;
    };
    editor.set_status("Resolving document link...");
    crate::runtime::ingress::spawn_task_event_with_future(
        editor.work(),
        async move {
            let resolved = resolve.await?;
            let target = resolved
                .target
                .ok_or_else(|| anyhow::anyhow!("Document link did not resolve to a target"))?;
            Ok(RuntimeTaskEvent::OpenResolvedDocumentLink {
                doc_id,
                expected_version,
                target,
                action,
            })
        },
        ingress,
    );
}

pub(crate) fn try_open_document_link_at_cursor(cx: &mut Context, action: Action) -> bool {
    let (view_id, doc) = focused_ref!(cx.editor);
    let text = doc.text();
    let cursor = doc.selection(view_id).primary().cursor(text.slice(..));
    let Some(document_links) = doc.document_links() else {
        return false;
    };
    let Some(link) = document_links
        .links
        .iter()
        .find(|link| link.range.contains(cursor))
    else {
        return false;
    };
    open_document_link(
        cx.editor,
        &cx.foreground,
        cx.ingress.clone(),
        LspDocumentLinkPickerItem {
            doc_id: doc.id(),
            expected_version: doc.version(),
            server_id: link.server_id,
            link: link.link.clone(),
            text: String::new(),
        },
        action,
    );
    true
}

pub fn linked_editing_range(cx: &mut Context) {
    let (view_id, doc) = focused_ref!(cx.editor);
    let Some(language_server) = doc
        .language_servers_with_feature(LanguageServerFeature::LinkedEditingRange)
        .next()
    else {
        cx.editor
            .set_error("No configured language server supports linked editing ranges");
        return;
    };
    let offset_encoding = language_server.offset_encoding();
    let pos = doc.position(view_id, offset_encoding);
    let Some(future) =
        language_server.text_document_linked_editing_range(doc.identifier(), pos, None)
    else {
        cx.editor
            .set_error("No configured language server supports linked editing ranges");
        return;
    };
    cx.spawn_task_event(async move {
        let Some(ranges) = future.await? else {
            return Ok(RuntimeTaskEvent::Stub);
        };
        Ok(RuntimeTaskEvent::ApplyLinkedEditingRanges {
            offset_encoding,
            ranges,
        })
    });
}

pub(crate) fn request_on_type_formatting(cx: &mut Context, ch: char) {
    if !cx.editor.config().lsp.on_type_formatting {
        return;
    }
    let (view_id, doc) = focused_ref!(cx.editor);
    let Some(language_server) = doc
        .language_servers_with_feature(LanguageServerFeature::OnTypeFormatting)
        .next()
    else {
        return;
    };
    let offset_encoding = language_server.offset_encoding();
    let pos = doc.position(view_id, offset_encoding);
    let options = lsp::FormattingOptions {
        tab_size: doc.tab_width() as u32,
        insert_spaces: matches!(
            doc.indent_style(),
            helix_core::indent::IndentStyle::Spaces(_)
        ),
        ..Default::default()
    };
    let Some(future) = language_server.text_document_on_type_formatting(
        doc.identifier(),
        pos,
        ch.to_string(),
        options,
    ) else {
        return;
    };
    let doc_id = doc.id();
    let expected_version = doc.version();
    cx.spawn_task_event(async move {
        Ok(RuntimeTaskEvent::ApplyOnTypeFormatting {
            doc_id,
            view_id,
            expected_version,
            offset_encoding,
            edits: future.await?.unwrap_or_default(),
        })
    });
}

pub fn code_action_inner(cx: &mut Context, use_picker: bool) {
    let (view_id, doc) = focused!(cx.editor);

    let selection_range = doc.selection(view_id).primary();

    let mut seen_language_servers = HashSet::new();

    let mut futures: FuturesOrdered<_> = doc
        .language_servers_with_feature(LanguageServerFeature::CodeAction)
        .filter(|ls| seen_language_servers.insert(ls.id()))
        // TODO this should probably already been filtered in something like "language_servers_with_feature"
        .filter_map(|language_server| {
            let offset_encoding = language_server.offset_encoding();
            let language_server_id = language_server.id();
            let range = range_to_lsp_range(doc.text(), selection_range, offset_encoding);
            // Filter and convert overlapping diagnostics
            let code_action_context = lsp::CodeActionContext {
                diagnostics: doc
                    .diagnostics()
                    .iter()
                    .filter(|&diag| {
                        selection_range
                            .overlaps(&helix_core::Range::new(diag.range.start, diag.range.end))
                    })
                    .map(|diag| diagnostic_to_lsp_diagnostic(doc.text(), diag, offset_encoding))
                    .collect(),
                only: None,
                trigger_kind: Some(CodeActionTriggerKind::INVOKED),
            };
            let code_action_request =
                language_server.code_actions(doc.identifier(), range, code_action_context)?;
            Some((code_action_request, language_server_id))
        })
        .map(|(request, ls_id)| async move {
            let Some(mut actions) = request.await? else {
                return anyhow::Ok(Vec::new());
            };

            // remove disabled code actions
            actions.retain(|action| {
                matches!(
                    action,
                    CodeActionOrCommand::Command(_)
                        | CodeActionOrCommand::CodeAction(CodeAction { disabled: None, .. })
                )
            });

            // Sort codeactions into a useful order. This behaviour is only partially described in the LSP spec.
            // Many details are modeled after vscode because language servers are usually tested against it.
            // VScode sorts the codeaction two times:
            //
            // First the codeactions that fix some diagnostics are moved to the front.
            // If both codeactions fix some diagnostics (or both fix none) the codeaction
            // that is marked with `is_preferred` is shown first. The codeactions are then shown in separate
            // submenus that only contain a certain category (see `action_category`) of actions.
            //
            // Below this done in in a single sorting step
            actions.sort_by(|action1, action2| {
                // sort actions by category
                let order = action_category(action1).cmp(&action_category(action2));
                if order != Ordering::Equal {
                    return order;
                }
                // within the categories sort by relevancy.
                // Modeled after the `codeActionsComparator` function in vscode:
                // https://github.com/microsoft/vscode/blob/eaec601dd69aeb4abb63b9601a6f44308c8d8c6e/src/vs/editor/contrib/codeAction/browser/codeAction.ts

                // if one code action fixes a diagnostic but the other one doesn't show it first
                let order = action_fixes_diagnostics(action1)
                    .cmp(&action_fixes_diagnostics(action2))
                    .reverse();
                if order != Ordering::Equal {
                    return order;
                }

                // if one of the codeactions is marked as preferred show it first
                // otherwise keep the original LSP sorting
                action_preferred(action1)
                    .cmp(&action_preferred(action2))
                    .reverse()
            });

            Ok(actions
                .into_iter()
                .map(|lsp_item| LspCodeActionItem {
                    lsp_item,
                    language_server_id: ls_id,
                })
                .collect())
        })
        .collect();

    if futures.is_empty() {
        cx.editor
            .set_error("No configured language server supports code actions");
        return;
    }

    cx.spawn_ui(async move {
        let mut actions = Vec::new();

        while let Some(output) = futures.next().await {
            match output {
                Ok(mut lsp_items) => actions.append(&mut lsp_items),
                Err(err) => log::error!("while gathering code actions: {err}"),
            }
        }

        Ok(UiCommand::Lsp(LspCommand::CodeActions {
            items: actions,
            presentation: if use_picker {
                LspCodeActionPresentation::Picker
            } else {
                LspCodeActionPresentation::Menu
            },
        }))
    });
}

#[derive(Debug)]
pub struct ApplyEditError {
    pub kind: ApplyEditErrorKind,
    pub failed_change_idx: usize,
}

#[derive(Debug)]
pub enum ApplyEditErrorKind {
    DocumentChanged,
    FileNotFound,
    UnknownURISchema,
    IoError(std::io::Error),
    // TODO: check edits before applying and propagate failure
    // InvalidEdit,
}

impl Display for ApplyEditErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyEditErrorKind::DocumentChanged => f.write_str("document has changed"),
            ApplyEditErrorKind::FileNotFound => f.write_str("file not found"),
            ApplyEditErrorKind::UnknownURISchema => f.write_str("URI schema not supported"),
            ApplyEditErrorKind::IoError(err) => f.write_str(&format!("{err}")),
        }
    }
}

fn goto_single_impl<P, F>(cx: &mut Context, feature: LanguageServerFeature, request_provider: P)
where
    P: Fn(&Client, lsp::Position, lsp::TextDocumentIdentifier) -> Option<F>,
    F: Future<Output = helix_lsp::Result<Option<lsp::GotoDefinitionResponse>>> + 'static + Send,
{
    let (view_id, doc) = focused_ref!(cx.editor);
    let mut futures: FuturesOrdered<_> = doc
        .language_servers_with_feature(feature)
        .map(|language_server| {
            let offset_encoding = language_server.offset_encoding();
            let pos = doc.position(view_id, offset_encoding);
            let future = request_provider(language_server, pos, doc.identifier()).unwrap();
            async move { anyhow::Ok((future.await?, offset_encoding)) }
        })
        .collect();

    cx.spawn_ui(async move {
        let mut locations = Vec::new();
        while let Some(response) = futures.next().await {
            match response {
                Ok((response, offset_encoding)) => match response {
                    Some(lsp::GotoDefinitionResponse::Scalar(lsp_location)) => {
                        locations.extend(navigation::lsp_location_to_lsp_location(
                            lsp_location,
                            offset_encoding,
                        ));
                    }
                    Some(lsp::GotoDefinitionResponse::Array(lsp_locations)) => {
                        locations.extend(lsp_locations.into_iter().flat_map(|location| {
                            navigation::lsp_location_to_lsp_location(location, offset_encoding)
                        }));
                    }
                    Some(lsp::GotoDefinitionResponse::Link(lsp_locations)) => {
                        locations.extend(
                            lsp_locations
                                .into_iter()
                                .map(|location_link| {
                                    lsp::Location::new(
                                        location_link.target_uri,
                                        location_link.target_range,
                                    )
                                })
                                .flat_map(|location| {
                                    navigation::lsp_location_to_lsp_location(
                                        location,
                                        offset_encoding,
                                    )
                                }),
                        );
                    }
                    None => (),
                },
                Err(err) => log::error!("Error requesting locations: {err}"),
            }
        }
        let empty_message = match feature {
            LanguageServerFeature::GotoDeclaration => "No declaration found.",
            LanguageServerFeature::GotoDefinition => "No definition found.",
            LanguageServerFeature::GotoTypeDefinition => "No type definition found.",
            LanguageServerFeature::GotoImplementation => "No implementation found.",
            _ => "No location found.",
        };
        Ok(UiCommand::Lsp(LspCommand::Goto {
            locations,
            empty_message,
        }))
    });
}

pub fn goto_declaration(cx: &mut Context) {
    goto_single_impl(
        cx,
        LanguageServerFeature::GotoDeclaration,
        |ls, pos, doc_id| ls.goto_declaration(doc_id, pos, None),
    );
}

pub fn goto_definition(cx: &mut Context) {
    goto_single_impl(
        cx,
        LanguageServerFeature::GotoDefinition,
        |ls, pos, doc_id| ls.goto_definition(doc_id, pos, None),
    );
}

pub fn goto_type_definition(cx: &mut Context) {
    goto_single_impl(
        cx,
        LanguageServerFeature::GotoTypeDefinition,
        |ls, pos, doc_id| ls.goto_type_definition(doc_id, pos, None),
    );
}

pub fn goto_implementation(cx: &mut Context) {
    goto_single_impl(
        cx,
        LanguageServerFeature::GotoImplementation,
        |ls, pos, doc_id| ls.goto_implementation(doc_id, pos, None),
    );
}

pub fn goto_reference(cx: &mut Context) {
    let config = cx.editor.config();
    let (view_id, doc) = focused_ref!(cx.editor);

    let mut futures: FuturesOrdered<_> = doc
        .language_servers_with_feature(LanguageServerFeature::GotoReference)
        .map(|language_server| {
            let offset_encoding = language_server.offset_encoding();
            let pos = doc.position(view_id, offset_encoding);
            let future = language_server
                .goto_reference(
                    doc.identifier(),
                    pos,
                    config.lsp.goto_reference_include_declaration,
                    None,
                )
                .unwrap();
            async move { anyhow::Ok((future.await?, offset_encoding)) }
        })
        .collect();

    cx.spawn_ui(async move {
        let mut locations = Vec::new();
        while let Some(response) = futures.next().await {
            match response {
                Ok((lsp_locations, offset_encoding)) => {
                    locations.extend(lsp_locations.into_iter().flatten().flat_map(|location| {
                        navigation::lsp_location_to_lsp_location(location, offset_encoding)
                    }))
                }
                Err(err) => log::error!("Error requesting references: {err}"),
            }
        }
        Ok(UiCommand::Lsp(LspCommand::Goto {
            locations,
            empty_message: "No references found.",
        }))
    });
}

pub fn call_hierarchy_incoming(cx: &mut Context) {
    call_hierarchy(cx, LspCallHierarchyDirection::Incoming);
}

pub fn call_hierarchy_outgoing(cx: &mut Context) {
    call_hierarchy(cx, LspCallHierarchyDirection::Outgoing);
}

fn call_hierarchy(cx: &mut Context, direction: LspCallHierarchyDirection) {
    let (view_id, doc) = focused_ref!(cx.editor);
    let mut futures: FuturesOrdered<_> = doc
        .language_servers()
        .filter_map(|language_server| {
            let offset_encoding = language_server.offset_encoding();
            let pos = doc.position(view_id, offset_encoding);
            let request = language_server.text_document_prepare_call_hierarchy(
                doc.identifier(),
                pos,
                None,
            )?;
            let server_id = language_server.id();
            Some(async move { anyhow::Ok((server_id, offset_encoding, request.await?)) })
        })
        .collect();

    if futures.is_empty() {
        cx.editor
            .set_error("No configured language server supports call hierarchy");
        return;
    }

    cx.spawn_ui(async move {
        let mut prepared = Vec::new();
        while let Some(response) = futures.next().await {
            match response {
                Ok((server_id, offset_encoding, Some(items))) => {
                    prepared.extend(items.into_iter().map(|item| LspHierarchyPrepareItem::Call {
                        server_id,
                        offset_encoding,
                        item,
                        direction,
                    }));
                }
                Ok(_) => (),
                Err(err) => log::error!("Error preparing call hierarchy: {err}"),
            }
        }

        Ok(UiCommand::Lsp(LspCommand::HierarchyPrepare {
            items: prepared,
            empty_message: "No call hierarchy found.",
        }))
    });
}

pub fn type_hierarchy_super(cx: &mut Context) {
    type_hierarchy(cx, LspTypeHierarchyDirection::Supertypes);
}

pub fn type_hierarchy_sub(cx: &mut Context) {
    type_hierarchy(cx, LspTypeHierarchyDirection::Subtypes);
}

fn type_hierarchy(cx: &mut Context, direction: LspTypeHierarchyDirection) {
    let (view_id, doc) = focused_ref!(cx.editor);
    let mut futures: FuturesOrdered<_> = doc
        .language_servers()
        .filter_map(|language_server| {
            let offset_encoding = language_server.offset_encoding();
            let pos = doc.position(view_id, offset_encoding);
            let request = language_server.text_document_prepare_type_hierarchy(
                doc.identifier(),
                pos,
                None,
            )?;
            let server_id = language_server.id();
            Some(async move { anyhow::Ok((server_id, offset_encoding, request.await?)) })
        })
        .collect();

    if futures.is_empty() {
        cx.editor
            .set_error("No configured language server supports type hierarchy");
        return;
    }

    cx.spawn_ui(async move {
        let mut prepared = Vec::new();
        while let Some(response) = futures.next().await {
            match response {
                Ok((server_id, offset_encoding, Some(items))) => {
                    prepared.extend(items.into_iter().map(|item| LspHierarchyPrepareItem::Type {
                        server_id,
                        offset_encoding,
                        item,
                        direction,
                    }));
                }
                Ok(_) => (),
                Err(err) => log::error!("Error preparing type hierarchy: {err}"),
            }
        }

        Ok(UiCommand::Lsp(LspCommand::HierarchyPrepare {
            items: prepared,
            empty_message: "No type hierarchy found.",
        }))
    });
}

pub fn call_hierarchy_incoming_picker_items(
    calls: Vec<lsp::CallHierarchyIncomingCall>,
    offset_encoding: OffsetEncoding,
) -> Vec<LspHierarchyPickerItem> {
    calls
        .into_iter()
        .filter_map(|call| {
            let range = call
                .from_ranges
                .first()
                .copied()
                .unwrap_or(call.from.selection_range);
            hierarchy_picker_item(
                call.from.name,
                call.from.detail,
                call.from.kind,
                call.from.uri,
                range,
                offset_encoding,
            )
        })
        .collect()
}

pub fn call_hierarchy_outgoing_picker_items(
    calls: Vec<lsp::CallHierarchyOutgoingCall>,
    offset_encoding: OffsetEncoding,
) -> Vec<LspHierarchyPickerItem> {
    calls
        .into_iter()
        .filter_map(|call| {
            hierarchy_picker_item(
                call.to.name,
                call.to.detail,
                call.to.kind,
                call.to.uri,
                call.to.selection_range,
                offset_encoding,
            )
        })
        .collect()
}

pub fn type_hierarchy_picker_items(
    items: Vec<lsp::TypeHierarchyItem>,
    offset_encoding: OffsetEncoding,
) -> Vec<LspHierarchyPickerItem> {
    items
        .into_iter()
        .filter_map(|item| {
            hierarchy_picker_item(
                item.name,
                item.detail,
                item.kind,
                item.uri,
                item.selection_range,
                offset_encoding,
            )
        })
        .collect()
}

fn hierarchy_picker_item(
    name: String,
    detail: Option<String>,
    kind: lsp::SymbolKind,
    uri: lsp::Url,
    range: lsp::Range,
    offset_encoding: OffsetEncoding,
) -> Option<LspHierarchyPickerItem> {
    Some(LspHierarchyPickerItem {
        name,
        detail,
        kind,
        location: LspLocation {
            uri: uri.try_into().ok()?,
            range,
            offset_encoding,
        },
    })
}

pub fn signature_help(cx: &mut Context) {
    cx.editor
        .handlers
        .trigger_signature_help(SignatureHelpInvoked::Manual, cx.editor)
}

fn hover_impl(cx: &mut Context, display: LspHoverDisplay) {
    let (view_id, doc) = focused_ref!(cx.editor);
    let inlay_hints = inlay_hints_at_cursor(cx.editor, view_id, doc);
    if doc
        .language_servers_with_feature(LanguageServerFeature::Hover)
        .count()
        == 0
        && inlay_hints.is_empty()
    {
        cx.editor
            .set_error("No configured language server supports hover");
        return;
    }

    let mut seen_language_servers = HashSet::new();
    let mut futures: FuturesOrdered<_> = doc
        .language_servers_with_feature(LanguageServerFeature::Hover)
        .filter(|ls| seen_language_servers.insert(ls.id()))
        .map(|language_server| {
            let server_name = language_server.name().to_string();
            // TODO: factor out a doc.position_identifier() that returns lsp::TextDocumentPositionIdentifier
            let pos = doc.position(view_id, language_server.offset_encoding());
            let request = language_server
                .text_document_hover(doc.identifier(), pos, None)
                .unwrap();

            async move { anyhow::Ok((server_name, request.await?)) }
        })
        .collect();

    let mut inlay_futures: FuturesOrdered<_> = inlay_hints
        .into_iter()
        .filter_map(|hint| {
            let language_server = cx.editor.language_server_by_id(hint.server_id)?;
            let server_name = language_server.name().to_string();
            let resolve = hint
                .hint
                .tooltip
                .is_none()
                .then(|| language_server.resolve_inlay_hint(&hint.hint))
                .flatten();
            Some(async move {
                let hint = match resolve {
                    Some(resolve) => resolve.await.unwrap_or(hint.hint),
                    None => hint.hint,
                };
                anyhow::Ok((server_name, hint))
            })
        })
        .collect();

    cx.spawn_ui(async move {
        let mut hovers: Vec<(String, lsp::Hover)> = Vec::new();

        while let Some(response) = futures.next().await {
            match response {
                Ok((server_name, Some(hover))) => hovers.push((server_name, hover)),
                Ok(_) => (),
                Err(err) => log::error!("Error requesting hover: {err}"),
            }
        }
        while let Some(response) = inlay_futures.next().await {
            match response {
                Ok((server_name, hint)) => {
                    hovers.extend(
                        inlay_hint_to_hover(hint)
                            .map(|hover| (format!("{server_name} inlay hint"), hover)),
                    );
                }
                Err(err) => log::error!("Error resolving inlay hint: {err}"),
            }
        }

        Ok(UiCommand::Lsp(LspCommand::Hover { hovers, display }))
    });
}

fn inlay_hints_at_cursor(
    editor: &Editor,
    view_id: helix_view::ViewId,
    doc: &Document,
) -> Vec<DocumentInlayHint> {
    let Some(inlay_hints) = doc.inlay_hints(view_id) else {
        return Vec::new();
    };
    let cursor = doc
        .selection(view_id)
        .primary()
        .cursor(doc.text().slice(..));
    inlay_hints
        .lsp_hints
        .iter()
        .filter_map(|hint| {
            let pos = helix_lsp::util::lsp_pos_to_pos(
                doc.text(),
                hint.hint.position,
                hint.offset_encoding,
            )?;
            (pos.abs_diff(cursor) <= 1).then_some(hint.clone())
        })
        .filter(|hint| editor.language_server_by_id(hint.server_id).is_some())
        .collect()
}

fn inlay_hint_to_hover(hint: lsp::InlayHint) -> Option<lsp::Hover> {
    let contents = hint
        .tooltip
        .map(inlay_hint_tooltip_to_hover_contents)
        .or_else(|| match hint.label {
            lsp::InlayHintLabel::String(_) => None,
            lsp::InlayHintLabel::LabelParts(parts) => parts
                .into_iter()
                .find_map(|part| part.tooltip.map(inlay_hint_label_tooltip_to_hover_contents)),
        })?;

    Some(lsp::Hover {
        contents,
        range: None,
    })
}

fn inlay_hint_tooltip_to_hover_contents(tooltip: lsp::InlayHintTooltip) -> lsp::HoverContents {
    match tooltip {
        lsp::InlayHintTooltip::String(value) => lsp::HoverContents::Markup(lsp::MarkupContent {
            kind: lsp::MarkupKind::PlainText,
            value,
        }),
        lsp::InlayHintTooltip::MarkupContent(content) => lsp::HoverContents::Markup(content),
    }
}

fn inlay_hint_label_tooltip_to_hover_contents(
    tooltip: lsp::InlayHintLabelPartTooltip,
) -> lsp::HoverContents {
    match tooltip {
        lsp::InlayHintLabelPartTooltip::String(value) => {
            lsp::HoverContents::Markup(lsp::MarkupContent {
                kind: lsp::MarkupKind::PlainText,
                value,
            })
        }
        lsp::InlayHintLabelPartTooltip::MarkupContent(content) => {
            lsp::HoverContents::Markup(content)
        }
    }
}

pub fn hover(cx: &mut Context) {
    hover_impl(cx, LspHoverDisplay::Popup)
}

pub fn goto_hover(cx: &mut Context) {
    hover_impl(cx, LspHoverDisplay::FileBuffer)
}

fn get_prefill_from_word_boundary(editor: &Editor) -> String {
    let (view_id, doc) = focused_ref!(editor);
    let text = doc.text().slice(..);
    let primary_selection = doc.selection(view_id).primary();
    if primary_selection.len() > 1 {
        primary_selection
    } else {
        use helix_core::textobject::{textobject_word, TextObject};
        textobject_word(text, primary_selection, TextObject::Inside, 1, false)
    }
    .fragment(text)
    .into()
}

fn get_prefill_from_lsp_response(
    text: &helix_core::Rope,
    fallback_prefill: &str,
    offset_encoding: OffsetEncoding,
    response: Option<lsp::PrepareRenameResponse>,
) -> Result<String, &'static str> {
    match response {
        Some(lsp::PrepareRenameResponse::Range(range)) => {
            Ok(lsp_range_to_range(text, range, offset_encoding)
                .ok_or("lsp sent invalid selection range for rename")?
                .fragment(text.slice(..))
                .into())
        }
        Some(lsp::PrepareRenameResponse::RangeWithPlaceholder { placeholder, .. }) => {
            Ok(placeholder)
        }
        Some(lsp::PrepareRenameResponse::DefaultBehavior { .. }) => Ok(fallback_prefill.to_owned()),
        None => Err("lsp did not respond to prepare rename request"),
    }
}

pub(crate) fn create_rename_prompt(
    editor: &Editor,
    prefill: String,
    history_register: Option<char>,
    language_server_id: Option<LanguageServerId>,
) -> Box<dyn Component> {
    match editor.config().cmdline.style {
        helix_view::editor::CmdlineStyle::Popup => {
            let cmdline = ui::CmdlinePopup::new(
                "rename-to:".into(),
                history_register,
                ui::completers::none,
                move |cx: &mut compositor::Context, input: &str, event: PromptEvent| {
                    if event != PromptEvent::Validate {
                        return;
                    }
                    submit_rename(cx, input, language_server_id);
                },
                helix_view::editor::CmdlineStyle::Popup,
            )
            .with_line(prefill, editor);

            Box::new(cmdline)
        }
        helix_view::editor::CmdlineStyle::Bottom => {
            let prompt = ui::Prompt::new(
                "rename-to:".into(),
                history_register,
                ui::completers::none,
                move |cx: &mut compositor::Context, input: &str, event: PromptEvent| {
                    if event != PromptEvent::Validate {
                        return;
                    }
                    submit_rename(cx, input, language_server_id);
                },
            )
            .with_line(prefill, editor);

            Box::new(prompt)
        }
    }
}

fn submit_rename(
    cx: &mut compositor::Context,
    input: &str,
    language_server_id: Option<LanguageServerId>,
) {
    let (view_id, doc) = focused!(cx.editor);
    let Some(language_server) = doc
        .language_servers_with_feature(LanguageServerFeature::RenameSymbol)
        .find(|server| language_server_id.is_none_or(|id| id == server.id()))
    else {
        cx.editor
            .set_error("No configured language server supports symbol renaming");
        return;
    };

    let doc_id = doc.id();
    let expected_version = doc.version();
    let offset_encoding = language_server.offset_encoding();
    let position = doc.position(view_id, offset_encoding);
    let Some(request) = language_server.rename_symbol(doc.identifier(), position, input.to_owned())
    else {
        cx.editor
            .set_error("Language server does not support symbol renaming");
        return;
    };
    cx.editor.set_status("Renaming symbol...");
    cx.spawn_task_event(async move {
        let workspace_edit = request.await?;
        Ok(RuntimeTaskEvent::ApplyRenameEdit {
            doc_id,
            expected_version,
            offset_encoding,
            workspace_edit,
        })
    });
}

pub fn rename_symbol(cx: &mut Context) {
    let history_register = cx.register;

    let (has_rename_support, prepare_rename) = {
        let (view_id, doc) = focused_ref!(cx.editor);
        let mut language_servers =
            doc.language_servers_with_feature(LanguageServerFeature::RenameSymbol);
        let has_rename_support = language_servers.next().is_some();
        let prepare_rename = doc
            .language_servers_with_feature(LanguageServerFeature::RenameSymbol)
            .find(|ls| {
                matches!(
                    ls.capabilities().rename_provider,
                    Some(lsp::OneOf::Right(lsp::RenameOptions {
                        prepare_provider: Some(true),
                        ..
                    }))
                )
            })
            .map(|language_server| {
                let ls_id = language_server.id();
                let offset_encoding = language_server.offset_encoding();
                let pos = doc.position(view_id, offset_encoding);
                let text = doc.text().clone();
                let future = language_server
                    .prepare_rename(doc.identifier(), pos)
                    .unwrap();
                (ls_id, offset_encoding, text, future)
            });
        (has_rename_support, prepare_rename)
    };

    if !has_rename_support {
        cx.editor
            .set_error("No configured language server supports symbol renaming");
        return;
    }

    if let Some((ls_id, offset_encoding, text, future)) = prepare_rename {
        let fallback_prefill = get_prefill_from_word_boundary(cx.editor);
        cx.spawn_ui(async move {
            let response = future.await?;
            let prefill =
                get_prefill_from_lsp_response(&text, &fallback_prefill, offset_encoding, response)
                    .map_err(anyhow::Error::msg)?;
            Ok(UiCommand::Lsp(LspCommand::PrepareRename {
                prefill,
                history_register,
                language_server_id: Some(ls_id),
            }))
        });
    } else {
        let prefill = get_prefill_from_word_boundary(cx.editor);
        let prompt = create_rename_prompt(cx.editor, prefill, history_register, None);
        cx.push_layer(prompt);
    }
}

pub fn select_references_to_symbol_under_cursor(cx: &mut Context) {
    let (view_id, doc) = focused!(cx.editor);
    let language_server =
        language_server_with_feature!(cx.editor, doc, LanguageServerFeature::DocumentHighlight);
    let offset_encoding = language_server.offset_encoding();
    let pos = doc.position(view_id, offset_encoding);
    let future = language_server
        .text_document_document_highlight(doc.identifier(), pos, None)
        .unwrap();

    cx.spawn_task_event(async move {
        let highlights = future.await?.unwrap_or_default();
        Ok(RuntimeTaskEvent::SelectDocumentHighlights {
            highlights,
            offset_encoding,
        })
    });
}

pub fn compute_inlay_hints_for_all_views(
    editor: &mut Editor,
    ingress: crate::runtime::RuntimeIngress,
) {
    if !editor.config().lsp.display_inlay_hints {
        return;
    }

    editor.for_each_view_document(|view, doc| {
        if let Some(callback) = compute_inlay_hints_for_view(view, doc) {
            crate::runtime::ingress::spawn_task_event_with_future(
                editor.work(),
                callback,
                ingress.clone(),
            );
        }
    });
}

fn compute_inlay_hints_for_view(
    view: &View,
    doc: &Document,
) -> Option<
    std::pin::Pin<
        Box<impl Future<Output = Result<crate::runtime::RuntimeTaskEvent, anyhow::Error>>>,
    >,
> {
    let view_id = view.id;
    let doc_id = view.doc;

    let language_server = doc
        .language_servers_with_feature(LanguageServerFeature::InlayHints)
        .next()?;
    let server_id = language_server.id();

    let doc_text = doc.text();
    let annotations = &view.fold_annotations(doc);

    // Compute ~3 times the current view height of inlay hints, that way some scrolling
    // will not show half the view with hints and half without while still being faster
    // than computing all the hints for the full file (which could be dozens of time
    // longer than the view is).
    let view_height = view.inner_height();
    let first_visible_line =
        doc_text.char_to_line(doc.view_offset(view_id).anchor.min(doc_text.len_chars()));
    let first_line = first_visible_line.saturating_sub(view_height);
    let last_line = doc_text.slice(..).nth_next_folded_line(
        annotations,
        first_visible_line,
        view_height.saturating_mul(2),
    );

    let new_doc_inlay_hints_id = DocumentInlayHintsId {
        first_line,
        last_line,
    };
    // Don't recompute the annotations in case nothing has changed about the view
    if !doc.inlay_hints_outdated()
        && doc
            .inlay_hints(view_id)
            .is_some_and(|dih| dih.id == new_doc_inlay_hints_id)
    {
        return None;
    }

    let doc_slice = doc_text.slice(..);
    let first_char_in_range = doc_slice.line_to_char(first_line);
    let last_char_in_range = doc_slice.line_to_char(last_line);

    let range = helix_lsp::util::range_to_lsp_range(
        doc_text,
        helix_core::Range::new(first_char_in_range, last_char_in_range),
        language_server.offset_encoding(),
    );

    let request = language_server.text_document_range_inlay_hints(doc.identifier(), range, None)?;
    let offset_encoding = language_server.offset_encoding();

    let callback = Box::pin(async move {
        let hints = request.await?.unwrap_or_default();
        Ok(RuntimeTaskEvent::ApplyInlayHints {
            view_id,
            doc_id,
            server_id,
            offset_encoding,
            id: new_doc_inlay_hints_id,
            hints,
        })
    });

    Some(callback)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url() -> lsp::Url {
        // Drive-letter form so Url::to_file_path works on Windows too.
        lsp::Url::parse("file:///C:/test.rs").expect("url")
    }

    fn range(start: u32, end: u32) -> lsp::Range {
        lsp::Range::new(lsp::Position::new(0, start), lsp::Position::new(0, end))
    }

    fn call_item(name: &str, selection_range: lsp::Range) -> lsp::CallHierarchyItem {
        lsp::CallHierarchyItem {
            name: name.to_owned(),
            kind: lsp::SymbolKind::FUNCTION,
            tags: None,
            detail: Some("detail".to_owned()),
            uri: url(),
            range: selection_range,
            selection_range,
            data: None,
        }
    }

    fn type_item(name: &str, selection_range: lsp::Range) -> lsp::TypeHierarchyItem {
        lsp::TypeHierarchyItem {
            name: name.to_owned(),
            kind: lsp::SymbolKind::CLASS,
            tags: None,
            detail: None,
            uri: url(),
            range: selection_range,
            selection_range,
            data: None,
        }
    }

    #[test]
    fn incoming_call_picker_items_jump_to_first_call_site() {
        let calls = vec![lsp::CallHierarchyIncomingCall {
            from: call_item("caller", range(1, 7)),
            from_ranges: vec![range(10, 14)],
        }];

        let items = call_hierarchy_incoming_picker_items(calls, OffsetEncoding::Utf8);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "caller");
        assert_eq!(items[0].location.range, range(10, 14));
    }

    #[test]
    fn outgoing_call_picker_items_jump_to_callee_selection() {
        let calls = vec![lsp::CallHierarchyOutgoingCall {
            to: call_item("callee", range(3, 9)),
            from_ranges: vec![range(10, 14)],
        }];

        let items = call_hierarchy_outgoing_picker_items(calls, OffsetEncoding::Utf8);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "callee");
        assert_eq!(items[0].location.range, range(3, 9));
    }

    #[test]
    fn type_hierarchy_picker_items_map_selection_range() {
        let items =
            type_hierarchy_picker_items(vec![type_item("Base", range(2, 6))], OffsetEncoding::Utf8);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "Base");
        assert_eq!(items[0].location.range, range(2, 6));
    }

    #[test]
    fn inlay_hint_tooltip_becomes_hover_content() {
        let hover = inlay_hint_to_hover(lsp::InlayHint {
            position: lsp::Position::new(0, 1),
            label: lsp::InlayHintLabel::String(": i32".to_owned()),
            kind: Some(lsp::InlayHintKind::TYPE),
            text_edits: None,
            tooltip: Some(lsp::InlayHintTooltip::String("resolved tooltip".to_owned())),
            padding_left: None,
            padding_right: None,
            data: None,
        })
        .expect("hover");

        assert!(matches!(
            hover.contents,
            lsp::HoverContents::Markup(lsp::MarkupContent { value, .. }) if value == "resolved tooltip"
        ));
    }
}
