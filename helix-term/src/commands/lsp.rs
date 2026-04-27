use futures_util::stream::FuturesOrdered;
use helix_lsp::{
    block_on,
    lsp::{
        self, CodeAction, CodeActionOrCommand, CodeActionTriggerKind, DiagnosticSeverity,
        NumberOrString,
    },
    util::{diagnostic_to_lsp_diagnostic, lsp_range_to_range, range_to_lsp_range},
    Client, LanguageServerId, OffsetEncoding,
};

use tokio_stream::StreamExt;
use tui::{text::Span, widgets::Row};

use super::{Context, Editor};

use helix_core::{
    diagnostic::DiagnosticProvider, syntax::config::LanguageServerFeature,
    text_folding::ropex::RopeSliceFoldExt, Uri,
};
use helix_stdx::path;
use helix_view::{
    document::DocumentInlayHintsId, handlers::lsp::SignatureHelpInvoked, icons::ICONS,
    theme::Style, Document, View,
};

use crate::{
    compositor::{self, Component},
    runtime::ui::command::{
        DocumentSymbolPickerItem, LspCodeActionItem, LspCodeActionPresentation, LspCommand,
        LspHoverDisplay, LspLocation,
    },
    runtime::{RuntimeTaskEvent, UiCommand},
    ui::{
        self, lsp::document_symbols::display_symbol_kind, lsp::navigation, overlay::overlaid,
        Picker, PromptEvent,
    },
};

use std::{cmp::Ordering, collections::HashSet, fmt::Display, future::Future};

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
        crate::ui::PickerRuntime::new(cx.editor.runtime()),
        cx.ingress.clone(),
        move |cx: &mut crate::compositor::Context, diag: &PickerDiagnostic, action| {
            navigation::jump_to_location(cx.editor, &diag.location, action);
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
                       work: helix_runtime::Work| {
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
        crate::ui::PickerRuntime::new(cx.editor.runtime()),
        cx.ingress.clone(),
        move |cx: &mut crate::compositor::Context, item: &DocumentSymbolPickerItem, action| {
            navigation::jump_to_location(cx.editor, &item.location, action);
        },
    )
    .with_preview(|_editor, item| navigation::location_to_file_location(&item.location))
    .with_dynamic_query(get_symbols, None)
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

pub fn signature_help(cx: &mut Context) {
    cx.editor
        .handlers
        .trigger_signature_help(SignatureHelpInvoked::Manual, cx.editor)
}

fn hover_impl(cx: &mut Context, display: LspHoverDisplay) {
    let (view_id, doc) = focused!(cx.editor);
    if doc
        .language_servers_with_feature(LanguageServerFeature::Hover)
        .count()
        == 0
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

    cx.spawn_ui(async move {
        let mut hovers: Vec<(String, lsp::Hover)> = Vec::new();

        while let Some(response) = futures.next().await {
            match response {
                Ok((server_name, Some(hover))) => hovers.push((server_name, hover)),
                Ok(_) => (),
                Err(err) => log::error!("Error requesting hover: {err}"),
            }
        }

        Ok(UiCommand::Lsp(LspCommand::Hover { hovers, display }))
    });
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
                    let (view_id, doc) = focused!(cx.editor);

                    let Some(language_server) = doc
                        .language_servers_with_feature(LanguageServerFeature::RenameSymbol)
                        .find(|ls| language_server_id.is_none_or(|id| id == ls.id()))
                    else {
                        cx.editor
                            .set_error("No configured language server supports symbol renaming");
                        return;
                    };

                    let offset_encoding = language_server.offset_encoding();
                    let pos = doc.position(view_id, offset_encoding);
                    let future = language_server
                        .rename_symbol(doc.identifier(), pos, input.to_string())
                        .unwrap();

                    match block_on(future) {
                        Ok(edits) => {
                            if let Err(err) = cx
                                .editor
                                .apply_workspace_edit(offset_encoding, &edits.unwrap_or_default())
                            {
                                cx.editor.set_error(format!(
                                    "Failed to apply rename edits: {}",
                                    err.kind
                                ));
                            }
                        }
                        Err(err) => cx.editor.set_error(err.to_string()),
                    }
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
                    let (view_id, doc) = focused!(cx.editor);

                    let Some(language_server) = doc
                        .language_servers_with_feature(LanguageServerFeature::RenameSymbol)
                        .find(|ls| language_server_id.is_none_or(|id| id == ls.id()))
                    else {
                        cx.editor
                            .set_error("No configured language server supports symbol renaming");
                        return;
                    };

                    let offset_encoding = language_server.offset_encoding();
                    let pos = doc.position(view_id, offset_encoding);
                    let future = language_server
                        .rename_symbol(doc.identifier(), pos, input.to_string())
                        .unwrap();

                    match block_on(future) {
                        Ok(edits) => {
                            if let Err(err) = cx
                                .editor
                                .apply_workspace_edit(offset_encoding, &edits.unwrap_or_default())
                            {
                                cx.editor.set_error(format!(
                                    "Failed to apply rename edits: {}",
                                    err.kind
                                ));
                            }
                        }
                        Err(err) => cx.editor.set_error(err.to_string()),
                    }
                },
            )
            .with_line(prefill, editor);

            Box::new(prompt)
        }
    }
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
    ingress: helix_runtime::Sender<crate::runtime::RuntimeEvent>,
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
            offset_encoding,
            id: new_doc_inlay_hints_id,
            hints,
        })
    });

    Some(callback)
}
