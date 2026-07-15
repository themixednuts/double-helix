mod client;
pub mod file_event;
mod file_operations;
pub mod jsonrpc;
mod transport;

pub use client::Client;
pub use futures_executor::block_on;
pub use helix_lsp_types as lsp;
pub use lsp::{Position, Url};

use futures_util::stream::select_all::SelectAll;
use helix_core::syntax::config::{LanguageConfiguration, LanguageServerConfiguration, RootMarkers};
use helix_runtime::Receiver;
use helix_stdx::path;
use slotmap::{SecondaryMap, SlotMap};

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use thiserror::Error;

pub type Result<T, E = Error> = core::result::Result<T, E>;
pub type LanguageServerName = String;
pub use helix_core::diagnostic::LanguageServerId;

#[derive(Error, Debug)]
pub enum Error {
    #[error("protocol error: {0}")]
    Rpc(#[from] jsonrpc::Error),
    #[error("failed to parse: {0}")]
    Parse(Box<dyn std::error::Error + Send + Sync>),
    #[error("IO Error: {0}")]
    IO(#[from] std::io::Error),
    #[error("request {0} timed out")]
    Timeout(jsonrpc::Id),
    #[error("server closed the stream")]
    StreamClosed,
    #[error("language server protocol header exceeded {limit} bytes")]
    HeaderTooLarge { limit: usize },
    #[error("language server message was {size} bytes, exceeding the {limit} byte limit")]
    MessageTooLarge { size: usize, limit: usize },
    #[error("language-server outbound control queue is full")]
    OutboundControlQueueFull,
    #[error("language-server outbound queue is full")]
    OutboundQueueFull,
    #[error("Unhandled")]
    Unhandled,
    #[error(transparent)]
    ExecutableNotFound(#[from] helix_stdx::env::ExecutableNotFoundError),
    #[error(transparent)]
    RuntimeAssets(#[from] helix_loader::RuntimeAssetsError),
    #[error("command '{command}' not found")]
    CommandNotFound { command: String, generation: u64 },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl Error {
    /// Whether launching failed because no provider exists for the configured command.
    /// Broken managed activations are deliberately excluded so callers can surface repair errors.
    pub const fn is_missing_launch_command(&self) -> bool {
        matches!(
            self,
            Self::ExecutableNotFound(_) | Self::CommandNotFound { .. }
        )
    }

    pub const fn runtime_generation(&self) -> Option<u64> {
        match self {
            Self::CommandNotFound { generation, .. } => Some(*generation),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Self::Parse(Box::new(value))
    }
}

impl From<sonic_rs::Error> for Error {
    fn from(value: sonic_rs::Error) -> Self {
        Self::Parse(Box::new(value))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OffsetEncoding {
    /// UTF-8 code units aka bytes
    Utf8,
    /// UTF-32 code units aka chars
    Utf32,
    /// UTF-16 code units
    #[default]
    Utf16,
}

pub mod util {
    use super::*;
    use helix_core::line_ending::{line_end_byte_index, line_end_char_index};
    use helix_core::snippets::{RenderedSnippet, Snippet, SnippetRenderCtx};
    use helix_core::{chars, RopeSlice};
    use helix_core::{diagnostic::NumberOrString, Range, Rope, Selection, Tendril, Transaction};

    /// Converts a diagnostic in the document to [`lsp::Diagnostic`].
    ///
    /// Panics when [`pos_to_lsp_pos`] would for an invalid range on the diagnostic.
    pub fn diagnostic_to_lsp_diagnostic(
        doc: &Rope,
        diag: &helix_core::diagnostic::Diagnostic,
        offset_encoding: OffsetEncoding,
    ) -> lsp::Diagnostic {
        use helix_core::diagnostic::Severity::*;

        let range = Range::new(diag.range.start, diag.range.end);
        let severity = diag.severity.map(|s| match s {
            Hint => lsp::DiagnosticSeverity::HINT,
            Info => lsp::DiagnosticSeverity::INFORMATION,
            Warning => lsp::DiagnosticSeverity::WARNING,
            Error => lsp::DiagnosticSeverity::ERROR,
        });

        let code = match diag.code.clone() {
            Some(x) => match x {
                NumberOrString::Number(x) => Some(lsp::NumberOrString::Number(x)),
                NumberOrString::String(x) => Some(lsp::NumberOrString::String(x)),
            },
            None => None,
        };

        let new_tags: Vec<_> = diag
            .tags
            .iter()
            .map(|tag| match tag {
                helix_core::diagnostic::DiagnosticTag::Unnecessary => {
                    lsp::DiagnosticTag::UNNECESSARY
                }
                helix_core::diagnostic::DiagnosticTag::Deprecated => lsp::DiagnosticTag::DEPRECATED,
            })
            .collect();

        let tags = if !new_tags.is_empty() {
            Some(new_tags)
        } else {
            None
        };

        lsp::Diagnostic {
            range: range_to_lsp_range(doc, range, offset_encoding),
            severity,
            code,
            source: diag.source.clone(),
            message: diag.message.to_owned(),
            related_information: None,
            tags,
            data: diag.data.to_owned(),
            ..Default::default()
        }
    }

    /// Converts [`lsp::Position`] to a position in the document.
    ///
    /// Returns `None` if position.line is out of bounds or an overflow occurs
    pub fn lsp_pos_to_pos(
        doc: &Rope,
        pos: lsp::Position,
        offset_encoding: OffsetEncoding,
    ) -> Option<usize> {
        let pos_line = pos.line as usize;
        if pos_line > doc.len_lines() - 1 {
            // If it extends past the end, truncate it to the end. This is because the
            // way the LSP describes the range including the last newline is by
            // specifying a line number after what we would call the last line.
            log::warn!("LSP position {pos:?} out of range assuming EOF");
            return Some(doc.len_chars());
        }

        // We need to be careful here to fully comply ith the LSP spec.
        // Two relevant quotes from the spec:
        //
        // https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#position
        // > If the character value is greater than the line length it defaults back
        // >  to the line length.
        //
        // https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocuments
        // > To ensure that both client and server split the string into the same
        // > line representation the protocol specifies the following end-of-line sequences:
        // > ‘\n’, ‘\r\n’ and ‘\r’. Positions are line end character agnostic.
        // > So you can not specify a position that denotes \r|\n or \n| where | represents the character offset.
        //
        // This means that while the line must be in bounds the `character`
        // must be capped to the end of the line.
        // Note that the end of the line here is **before** the line terminator
        // so we must use `line_end_char_index` instead of `doc.line_to_char(pos_line + 1)`
        //
        // FIXME: Helix does not fully comply with the LSP spec for line terminators.
        // The LSP standard requires that line terminators are ['\n', '\r\n', '\r'].
        // Without the unicode-linebreak feature disabled, the `\r` terminator is not handled by helix.
        // With the unicode-linebreak feature, helix recognizes multiple extra line break chars
        // which means that positions will be decoded/encoded incorrectly in their presence

        let line = match offset_encoding {
            OffsetEncoding::Utf8 => {
                let line_start = doc.line_to_byte(pos_line);
                let line_end = line_end_byte_index(&doc.slice(..), pos_line);
                line_start..line_end
            }
            OffsetEncoding::Utf16 => {
                // TODO directly translate line index to char-idx
                // ropey can do this just as easily as utf-8 byte translation
                // but the functions are just missing.
                // Translate to char first and then utf-16 as a workaround
                let line_start = doc.line_to_char(pos_line);
                let line_end = line_end_char_index(&doc.slice(..), pos_line);
                doc.char_to_utf16_cu(line_start)..doc.char_to_utf16_cu(line_end)
            }
            OffsetEncoding::Utf32 => {
                let line_start = doc.line_to_char(pos_line);
                let line_end = line_end_char_index(&doc.slice(..), pos_line);
                line_start..line_end
            }
        };

        // The LSP spec demands that the offset is capped to the end of the line
        let pos = line
            .start
            .checked_add(pos.character as usize)
            .unwrap_or(line.end)
            .min(line.end);

        match offset_encoding {
            OffsetEncoding::Utf8 => doc.try_byte_to_char(pos).ok(),
            OffsetEncoding::Utf16 => doc.try_utf16_cu_to_char(pos).ok(),
            OffsetEncoding::Utf32 => Some(pos),
        }
    }

    /// Converts position in the document to [`lsp::Position`].
    ///
    /// Panics when `pos` is out of `doc` bounds or operation overflows.
    pub fn pos_to_lsp_pos(
        doc: &Rope,
        pos: usize,
        offset_encoding: OffsetEncoding,
    ) -> lsp::Position {
        match offset_encoding {
            OffsetEncoding::Utf8 => {
                let line = doc.char_to_line(pos);
                let line_start = doc.line_to_byte(line);
                let col = doc.char_to_byte(pos) - line_start;

                lsp::Position::new(line as u32, col as u32)
            }
            OffsetEncoding::Utf16 => {
                let line = doc.char_to_line(pos);
                let line_start = doc.char_to_utf16_cu(doc.line_to_char(line));
                let col = doc.char_to_utf16_cu(pos) - line_start;

                lsp::Position::new(line as u32, col as u32)
            }
            OffsetEncoding::Utf32 => {
                let line = doc.char_to_line(pos);
                let line_start = doc.line_to_char(line);
                let col = pos - line_start;

                lsp::Position::new(line as u32, col as u32)
            }
        }
    }

    /// Converts a range in the document to [`lsp::Range`].
    pub fn range_to_lsp_range(
        doc: &Rope,
        range: Range,
        offset_encoding: OffsetEncoding,
    ) -> lsp::Range {
        let start = pos_to_lsp_pos(doc, range.from(), offset_encoding);
        let end = pos_to_lsp_pos(doc, range.to(), offset_encoding);

        lsp::Range::new(start, end)
    }

    pub fn lsp_range_to_range(
        doc: &Rope,
        mut range: lsp::Range,
        offset_encoding: OffsetEncoding,
    ) -> Option<Range> {
        // This is sort of an edgecase. It's not clear from the spec how to deal with
        // ranges where end < start. They don't make much sense but vscode simply caps start to end
        // and because it's not specified quite a few LS rely on this as a result (for example the TS server)
        if range.start > range.end {
            log::error!(
                "Invalid LSP range start {:?} > end {:?}, using an empty range at the end instead",
                range.start,
                range.end
            );
            range.start = range.end;
        }
        let start = lsp_pos_to_pos(doc, range.start, offset_encoding)?;
        let end = lsp_pos_to_pos(doc, range.end, offset_encoding)?;

        Some(Range::new(start, end))
    }

    /// If the LS did not provide a range for the completion or the range of the
    /// primary cursor can not be used for the secondary cursor, this function
    /// can be used to find the completion range for a cursor
    fn find_completion_range(text: RopeSlice, replace_mode: bool, cursor: usize) -> (usize, usize) {
        let start = cursor
            - text
                .chars_at(cursor)
                .reversed()
                .take_while(|ch| chars::char_is_word(*ch))
                .count();
        let mut end = cursor;
        if replace_mode {
            end += text
                .chars_at(cursor)
                .take_while(|ch| chars::char_is_word(*ch))
                .count();
        }
        (start, end)
    }
    fn completion_range(
        text: RopeSlice,
        edit_offset: Option<(i128, i128)>,
        replace_mode: bool,
        cursor: usize,
    ) -> Option<(usize, usize)> {
        let res = match edit_offset {
            Some((start_offset, end_offset)) => {
                let start_offset = cursor as i128 + start_offset;
                if start_offset < 0 {
                    return None;
                }
                let end_offset = cursor as i128 + end_offset;
                if end_offset > text.len_chars() as i128 {
                    return None;
                }
                (start_offset as usize, end_offset as usize)
            }
            None => find_completion_range(text, replace_mode, cursor),
        };
        Some(res)
    }

    /// Creates a [Transaction] from the [lsp::TextEdit] in a completion response.
    /// The transaction applies the edit to all cursors.
    pub fn generate_transaction_from_completion_edit(
        doc: &Rope,
        selection: &Selection,
        edit_offset: Option<(i128, i128)>,
        replace_mode: bool,
        new_text: String,
    ) -> Transaction {
        let replacement: Option<Tendril> = if new_text.is_empty() {
            None
        } else {
            Some(new_text.into())
        };

        let text = doc.slice(..);
        let (removed_start, removed_end) = completion_range(
            text,
            edit_offset,
            replace_mode,
            selection.primary().cursor(text),
        )
        .expect("transaction must be valid for primary selection");
        let removed_text = text.slice(removed_start..removed_end);

        let (transaction, mut selection) = Transaction::change_by_selection_ignore_overlapping(
            doc,
            selection,
            |range| {
                let cursor = range.cursor(text);
                completion_range(text, edit_offset, replace_mode, cursor)
                    .filter(|(start, end)| text.slice(start..end) == removed_text)
                    .unwrap_or_else(|| find_completion_range(text, replace_mode, cursor))
            },
            |_, _| replacement.clone(),
        );
        if transaction.changes().is_empty() {
            return transaction;
        }
        selection = selection.map(transaction.changes());
        transaction.with_selection(selection)
    }

    /// Creates a [Transaction] from the [Snippet] in a completion response.
    /// The transaction applies the edit to all cursors.
    pub fn generate_transaction_from_snippet(
        doc: &Rope,
        selection: &Selection,
        edit_offset: Option<(i128, i128)>,
        replace_mode: bool,
        snippet: Snippet,
        cx: &mut SnippetRenderCtx,
    ) -> (Transaction, RenderedSnippet) {
        let text = doc.slice(..);
        let (removed_start, removed_end) = completion_range(
            text,
            edit_offset,
            replace_mode,
            selection.primary().cursor(text),
        )
        .expect("transaction must be valid for primary selection");
        let removed_text = text.slice(removed_start..removed_end);
        let (transaction, mapped_selection, snippet) = snippet.render(
            doc,
            selection,
            |range| {
                let cursor = range.cursor(text);
                completion_range(text, edit_offset, replace_mode, cursor)
                    .filter(|(start, end)| text.slice(start..end) == removed_text)
                    .unwrap_or_else(|| find_completion_range(text, replace_mode, cursor))
            },
            cx,
        );
        let transaction = transaction.with_selection(snippet.first_selection(
            // we keep the direction of the old primary selection in case it changed during mapping
            // but use the primary idx from the mapped selection in case ranges had to be merged
            selection.primary().direction(),
            mapped_selection.primary_index(),
        ));
        (transaction, snippet)
    }

    pub fn generate_transaction_from_edits(
        doc: &Rope,
        mut edits: Vec<lsp::TextEdit>,
        offset_encoding: OffsetEncoding,
    ) -> Transaction {
        // Sort edits by start range, since some LSPs (Omnisharp) send them
        // in reverse order.
        edits.sort_by_key(|edit| edit.range.start);

        // Generate a diff if the edit is a full document replacement.
        #[allow(clippy::collapsible_if)]
        if edits.len() == 1 {
            let is_document_replacement = edits.first().and_then(|edit| {
                let start = lsp_pos_to_pos(doc, edit.range.start, offset_encoding)?;
                let end = lsp_pos_to_pos(doc, edit.range.end, offset_encoding)?;
                Some(start..end)
            }) == Some(0..doc.len_chars());
            if is_document_replacement {
                let new_text = Rope::from(edits.pop().unwrap().new_text);
                return helix_core::diff::compare_ropes(doc, &new_text);
            }
        }

        Transaction::change(
            doc,
            edits.into_iter().map(|edit| {
                // simplify "" into None for cleaner changesets
                let replacement = if !edit.new_text.is_empty() {
                    Some(edit.new_text.into())
                } else {
                    None
                };

                let start =
                    if let Some(start) = lsp_pos_to_pos(doc, edit.range.start, offset_encoding) {
                        start
                    } else {
                        return (0, 0, None);
                    };
                let end = if let Some(end) = lsp_pos_to_pos(doc, edit.range.end, offset_encoding) {
                    end
                } else {
                    return (0, 0, None);
                };

                if start > end {
                    log::error!(
                        "Invalid LSP text edit start {:?} > end {:?}, discarding",
                        start,
                        end
                    );
                    return (0, 0, None);
                }

                (start, end, replacement)
            }),
        )
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum MethodCall {
    WorkDoneProgressCreate(lsp::WorkDoneProgressCreateParams),
    ApplyWorkspaceEdit(lsp::ApplyWorkspaceEditParams),
    WorkspaceFolders,
    WorkspaceConfiguration(lsp::ConfigurationParams),
    RegisterCapability(lsp::RegistrationParams),
    UnregisterCapability(lsp::UnregistrationParams),
    ShowDocument(lsp::ShowDocumentParams),
    WorkspaceDiagnosticRefresh,
    SemanticTokensRefresh,
    CodeLensRefresh,
    InlayHintRefresh,
    InlineValueRefresh,
    ShowMessageRequest(lsp::ShowMessageRequestParams),
}

impl MethodCall {
    pub fn parse(method: &str, params: jsonrpc::Params) -> Result<MethodCall> {
        use lsp::request::Request;
        let request = match method {
            lsp::request::WorkDoneProgressCreate::METHOD => {
                let params: lsp::WorkDoneProgressCreateParams = params.parse()?;
                Self::WorkDoneProgressCreate(params)
            }
            lsp::request::ApplyWorkspaceEdit::METHOD => {
                let params: lsp::ApplyWorkspaceEditParams = params.parse()?;
                Self::ApplyWorkspaceEdit(params)
            }
            lsp::request::WorkspaceFoldersRequest::METHOD => Self::WorkspaceFolders,
            lsp::request::WorkspaceConfiguration::METHOD => {
                let params: lsp::ConfigurationParams = params.parse()?;
                Self::WorkspaceConfiguration(params)
            }
            lsp::request::RegisterCapability::METHOD => {
                let params: lsp::RegistrationParams = params.parse()?;
                Self::RegisterCapability(params)
            }
            lsp::request::UnregisterCapability::METHOD => {
                let params: lsp::UnregistrationParams = params.parse()?;
                Self::UnregisterCapability(params)
            }
            lsp::request::ShowDocument::METHOD => {
                let params: lsp::ShowDocumentParams = params.parse()?;
                Self::ShowDocument(params)
            }
            lsp::request::WorkspaceDiagnosticRefresh::METHOD => Self::WorkspaceDiagnosticRefresh,
            lsp::request::SemanticTokensRefresh::METHOD => Self::SemanticTokensRefresh,
            lsp::request::CodeLensRefresh::METHOD => Self::CodeLensRefresh,
            lsp::request::InlayHintRefreshRequest::METHOD => Self::InlayHintRefresh,
            lsp::request::InlineValueRefreshRequest::METHOD => Self::InlineValueRefresh,
            lsp::request::ShowMessageRequest::METHOD => {
                let params: lsp::ShowMessageRequestParams = params.parse()?;
                Self::ShowMessageRequest(params)
            }
            _ => {
                return Err(Error::Unhandled);
            }
        };
        Ok(request)
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum Notification {
    // we inject this notification to signal the LSP is ready
    Initialized,
    // and this notification to signal that the LSP exited
    Exit,
    PublishDiagnostics(lsp::PublishDiagnosticsParams),
    ShowMessage(lsp::ShowMessageParams),
    LogMessage(lsp::LogMessageParams),
    ProgressMessage(lsp::ProgressParams),
}

impl Notification {
    pub fn parse(method: &str, params: jsonrpc::Params) -> Result<Notification> {
        use lsp::notification::Notification as _;

        let notification = match method {
            lsp::notification::Initialized::METHOD => Self::Initialized,
            lsp::notification::Exit::METHOD => Self::Exit,
            lsp::notification::PublishDiagnostics::METHOD => {
                let params: lsp::PublishDiagnosticsParams = params.parse()?;
                Self::PublishDiagnostics(params)
            }

            lsp::notification::ShowMessage::METHOD => {
                let params: lsp::ShowMessageParams = params.parse()?;
                Self::ShowMessage(params)
            }
            lsp::notification::LogMessage::METHOD => {
                let params: lsp::LogMessageParams = params.parse()?;
                Self::LogMessage(params)
            }
            lsp::notification::Progress::METHOD => {
                let params: lsp::ProgressParams = params.parse()?;
                Self::ProgressMessage(params)
            }
            _ => {
                return Err(Error::Unhandled);
            }
        };

        Ok(notification)
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum ServerRequestError {
    MethodNotFound,
    Malformed(String),
}

#[derive(Debug, PartialEq, Clone)]
pub struct ServerRequest {
    pub id: jsonrpc::Id,
    pub method: String,
    pub request: std::result::Result<MethodCall, ServerRequestError>,
}

/// Parsed domain message delivered by the language-server transport.
///
/// Raw JSON-RPC never crosses into editor or terminal state. Parsing and
/// admission happen in the transport actor, where backpressure cannot stall
/// foreground input handling.
#[derive(Debug, PartialEq, Clone)]
pub enum ServerEvent {
    Notification(Notification),
    Request(ServerRequest),
    Invalid { id: jsonrpc::Id },
}

impl ServerEvent {
    pub(crate) fn from_call(call: jsonrpc::Call) -> Option<Self> {
        match call {
            jsonrpc::Call::Notification(jsonrpc::Notification { method, params, .. }) => {
                match Notification::parse(&method, params) {
                    Ok(notification) => Some(Self::Notification(notification)),
                    Err(Error::Unhandled) => {
                        log::info!("ignoring unhandled language-server notification '{method}'");
                        None
                    }
                    Err(error) => {
                        log::error!(
                            "ignoring malformed language-server notification '{method}': {error}"
                        );
                        None
                    }
                }
            }
            jsonrpc::Call::MethodCall(jsonrpc::MethodCall {
                method, params, id, ..
            }) => {
                let request = match MethodCall::parse(&method, params) {
                    Ok(request) => Ok(request),
                    Err(Error::Unhandled) => Err(ServerRequestError::MethodNotFound),
                    Err(error) => Err(ServerRequestError::Malformed(error.to_string())),
                };
                Some(Self::Request(ServerRequest {
                    id,
                    method,
                    request,
                }))
            }
            jsonrpc::Call::Invalid { id } => Some(Self::Invalid { id }),
        }
    }
}

#[derive(Debug)]
pub struct Registry {
    ids: SlotMap<LanguageServerId, ()>,
    inner: SecondaryMap<LanguageServerId, Arc<Client>>,
    inner_by_name: HashMap<LanguageServerName, Vec<Arc<Client>>>,
    manually_stopped: HashSet<LanguageServerName>,
    initialized_dispatched: HashSet<LanguageServerId>,
    pub incoming: SelectAll<Receiver<(LanguageServerId, ServerEvent)>>,
    pub file_event_handler: file_event::Handler,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            ids: SlotMap::with_key(),
            inner: SecondaryMap::new(),
            inner_by_name: HashMap::new(),
            manually_stopped: HashSet::new(),
            initialized_dispatched: HashSet::new(),
            incoming: SelectAll::new(),
            file_event_handler: file_event::Handler::new(),
        }
    }

    pub fn get_by_id(&self, id: LanguageServerId) -> Option<&Arc<Client>> {
        self.inner.get(id)
    }

    pub fn reserve_id(&mut self) -> LanguageServerId {
        self.ids.insert(())
    }

    pub fn release_reserved_id(&mut self, id: LanguageServerId) {
        if self.inner.get(id).is_none() {
            self.ids.remove(id);
        }
    }

    pub fn install_spawned(&mut self, spawned: SpawnedLanguageServer) -> Result<Arc<Client>> {
        let SpawnedLanguageServer(client, incoming) = spawned;
        let id = client.id();
        if !self.ids.contains_key(id) || self.inner.get(id).is_some() {
            return Err(Error::Other(anyhow::anyhow!(
                "language-server id {id:?} was not reserved for installation"
            )));
        }
        self.incoming.push(incoming);
        self.inner.insert(id, client.clone());
        self.inner_by_name
            .entry(client.name().to_owned())
            .or_default()
            .push(client.clone());
        Ok(client)
    }

    pub fn is_manually_stopped(&self, name: &str) -> bool {
        self.manually_stopped.contains(name)
    }

    pub fn compatible_prepared_client(
        &mut self,
        prepared: &PreparedClientLaunch,
    ) -> Option<Arc<Client>> {
        let identity = prepared.identity();
        self.inner_by_name
            .get(prepared.name())?
            .iter()
            .enumerate()
            .find(|(index, client)| {
                client.launch_identity() == &identity
                    && client.try_add_prepared_doc(
                        prepared.root_path(),
                        prepared.root_uri().cloned(),
                        *index == 0,
                    )
            })
            .map(|(_, client)| client.clone())
    }

    pub fn mark_initialization_dispatched(&mut self, id: LanguageServerId) -> bool {
        self.inner.get(id).is_some() && self.initialized_dispatched.insert(id)
    }

    pub fn initialization_was_dispatched(&self, id: LanguageServerId) -> bool {
        self.initialized_dispatched.contains(&id)
    }

    pub fn remove_by_id(&mut self, id: LanguageServerId) {
        let Some(client) = self.inner.remove(id) else {
            if self.ids.remove(id).is_none() {
                log::debug!("client was already removed");
            }
            self.initialized_dispatched.remove(&id);
            return;
        };
        self.file_event_handler.remove_client(id);
        self.initialized_dispatched.remove(&id);
        let instances = self
            .inner_by_name
            .get_mut(client.name())
            .expect("inner and inner_by_name must be synced");
        instances.retain(|ls| id != ls.id());
        if instances.is_empty() {
            self.inner_by_name.remove(client.name());
        }
        self.ids.remove(id);
    }

    pub fn stop(&mut self, name: &str) {
        self.manually_stopped.insert(name.to_owned());
        if let Some(clients) = self.inner_by_name.remove(name) {
            for client in clients {
                self.file_event_handler.remove_client(client.id());
                self.initialized_dispatched.remove(&client.id());
                self.inner.remove(client.id());
                self.ids.remove(client.id());
                tokio::spawn(async move {
                    let _ = client.force_shutdown().await;
                });
            }
        }
    }

    /// Removes all clients for `name` while preserving explicit stop policy.
    pub fn invalidate(&mut self, name: &str) -> bool {
        let Some(clients) = self.inner_by_name.remove(name) else {
            return false;
        };
        for client in clients {
            self.file_event_handler.remove_client(client.id());
            self.initialized_dispatched.remove(&client.id());
            self.inner.remove(client.id());
            self.ids.remove(client.id());
            tokio::spawn(async move {
                let _ = client.force_shutdown().await;
            });
        }
        true
    }

    pub fn clear_manual_stop(&mut self, name: &str) {
        self.manually_stopped.remove(name);
    }

    pub fn iter_clients(&self) -> impl Iterator<Item = &Arc<Client>> {
        self.inner.values()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub enum ProgressStatus {
    Created,
    Started {
        title: String,
        progress: lsp::WorkDoneProgress,
    },
}

impl ProgressStatus {
    pub fn progress(&self) -> Option<&lsp::WorkDoneProgress> {
        match &self {
            ProgressStatus::Created => None,
            ProgressStatus::Started { title: _, progress } => Some(progress),
        }
    }
}

#[derive(Default, Debug)]
/// Acts as a container for progress reported by language servers. Each server
/// has a unique id assigned at creation through [`Registry`]. This id is then used
/// to store the progress in this map.
pub struct LspProgressMap(HashMap<LanguageServerId, HashMap<lsp::ProgressToken, ProgressStatus>>);

impl LspProgressMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a map of all tokens corresponding to the language server with `id`.
    pub fn progress_map(
        &self,
        id: LanguageServerId,
    ) -> Option<&HashMap<lsp::ProgressToken, ProgressStatus>> {
        self.0.get(&id)
    }

    pub fn is_progressing(&self, id: LanguageServerId) -> bool {
        self.0.get(&id).map(|it| !it.is_empty()).unwrap_or_default()
    }

    /// Returns last progress status for a given server with `id` and `token`.
    pub fn progress(
        &self,
        id: LanguageServerId,
        token: &lsp::ProgressToken,
    ) -> Option<&ProgressStatus> {
        self.0.get(&id).and_then(|values| values.get(token))
    }

    pub fn title(&self, id: LanguageServerId, token: &lsp::ProgressToken) -> Option<&String> {
        self.progress(id, token).and_then(|p| match p {
            ProgressStatus::Created => None,
            ProgressStatus::Started { title, .. } => Some(title),
        })
    }

    /// Checks if progress `token` for server with `id` is created.
    pub fn is_created(&mut self, id: LanguageServerId, token: &lsp::ProgressToken) -> bool {
        self.0
            .get(&id)
            .map(|values| values.get(token).is_some())
            .unwrap_or_default()
    }

    pub fn create(&mut self, id: LanguageServerId, token: lsp::ProgressToken) {
        self.0
            .entry(id)
            .or_default()
            .insert(token, ProgressStatus::Created);
    }

    /// Ends the progress by removing the `token` from server with `id`, if removed returns the value.
    pub fn end_progress(
        &mut self,
        id: LanguageServerId,
        token: &lsp::ProgressToken,
    ) -> Option<ProgressStatus> {
        self.0.get_mut(&id).and_then(|vals| vals.remove(token))
    }

    /// Updates the progress of `token` for server with `id` to begin state `status`
    pub fn begin(
        &mut self,
        id: LanguageServerId,
        token: lsp::ProgressToken,
        status: lsp::WorkDoneProgressBegin,
    ) {
        self.0.entry(id).or_default().insert(
            token,
            ProgressStatus::Started {
                title: status.title.clone(),
                progress: lsp::WorkDoneProgress::Begin(status),
            },
        );
    }

    /// Updates the progress of `token` for server with `id` to report state `status`.
    pub fn update(
        &mut self,
        id: LanguageServerId,
        token: lsp::ProgressToken,
        status: lsp::WorkDoneProgressReport,
    ) {
        self.0
            .entry(id)
            .or_default()
            .entry(token)
            .and_modify(|e| match e {
                ProgressStatus::Created => (),
                ProgressStatus::Started { progress, .. } => {
                    *progress = lsp::WorkDoneProgress::Report(status)
                }
            });
    }
}

#[derive(Debug, Clone)]
pub struct LanguageServerLaunchRequest {
    pub name: String,
    pub language: Arc<LanguageConfiguration>,
    pub server: LanguageServerConfiguration,
    pub doc_path: Option<PathBuf>,
    pub root_dirs: Vec<PathBuf>,
    pub enable_snippets: bool,
}

#[derive(Debug)]
pub enum PreparedLanguageServerLaunch {
    Ready(Box<PreparedClientLaunch>),
    NoRequiredRoot,
}

#[derive(Debug)]
pub struct PreparedClientLaunch {
    name: String,
    root_path: PathBuf,
    root_uri: Option<lsp::Url>,
    enable_snippets: bool,
    launch: helix_loader::ResolvedLaunch,
    args: Vec<String>,
    environment: HashMap<String, String>,
    config: Option<serde_json::Value>,
    timeout: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientLaunchIdentity {
    program: PathBuf,
    resolved_args: Vec<String>,
    resolved_environment: BTreeMap<String, String>,
    origin: helix_loader::Origin,
    configured_environment: HashMap<String, String>,
    config: Option<serde_json::Value>,
    timeout: u64,
    enable_snippets: bool,
}

impl PreparedClientLaunch {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    pub fn root_uri(&self) -> Option<&lsp::Url> {
        self.root_uri.as_ref()
    }

    pub fn runtime_generation(&self) -> u64 {
        self.launch.generation
    }

    pub fn identity(&self) -> ClientLaunchIdentity {
        let mut resolved_args = self.launch.prefix_args.clone();
        resolved_args.extend(self.launch.default_args.clone());
        resolved_args.extend(self.args.clone());
        ClientLaunchIdentity {
            program: self.launch.program.clone(),
            resolved_args,
            resolved_environment: self.launch.env.clone(),
            origin: self.launch.origin.clone(),
            configured_environment: self.environment.clone(),
            config: self.config.clone(),
            timeout: self.timeout,
            enable_snippets: self.enable_snippets,
        }
    }
}

pub struct SpawnedLanguageServer(Arc<Client>, Receiver<(LanguageServerId, ServerEvent)>);

impl std::fmt::Debug for SpawnedLanguageServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SpawnedLanguageServer")
            .field("id", &self.0.id())
            .field("name", &self.0.name())
            .finish_non_exhaustive()
    }
}

impl SpawnedLanguageServer {
    pub fn id(&self) -> LanguageServerId {
        self.0.id()
    }

    pub async fn force_shutdown(self) {
        let _ = self.0.force_shutdown().await;
    }
}

pub fn prepare_language_server_launch(
    request: LanguageServerLaunchRequest,
) -> Result<PreparedLanguageServerLaunch> {
    prepare_language_server_launch_parts(
        request.name,
        &request.language,
        &request.server,
        request.doc_path.as_ref(),
        &request.root_dirs,
        request.enable_snippets,
    )
}

fn prepare_language_server_launch_parts(
    name: String,
    language: &LanguageConfiguration,
    server: &LanguageServerConfiguration,
    doc_path: Option<&PathBuf>,
    root_dirs: &[PathBuf],
    enable_snippets: bool,
) -> Result<PreparedLanguageServerLaunch> {
    let (workspace, workspace_is_cwd) = helix_loader::find_workspace();
    let workspace = path::normalize(workspace);
    let root = find_lsp_workspace(
        doc_path
            .and_then(|x| x.parent().and_then(|x| x.to_str()))
            .unwrap_or("."),
        &language.roots,
        language.workspace_lsp_roots.as_deref().unwrap_or(root_dirs),
        &workspace,
        workspace_is_cwd,
    );

    // `root_uri` and `workspace_folder` can be empty in case there is no workspace
    // `root_url` can not, use `workspace` as a fallback
    let root_path = root.clone().unwrap_or_else(|| workspace.clone());
    let root_uri = root.and_then(|root| lsp::Url::from_file_path(root).ok());

    if let Some(globset) = &server.required_root_patterns {
        if !root_path
            .read_dir()?
            .flatten()
            .map(|entry| entry.file_name())
            .any(|entry| globset.is_match(entry))
        {
            return Ok(PreparedLanguageServerLaunch::NoRequiredRoot);
        }
    }

    let runtime = helix_loader::runtime_assets()?.snapshot();
    let runtime_generation = runtime.generation();
    let launch =
        runtime
            .resolve_command(&server.command)?
            .ok_or_else(|| Error::CommandNotFound {
                command: server.command.clone(),
                generation: runtime_generation,
            })?;

    Ok(PreparedLanguageServerLaunch::Ready(Box::new(
        PreparedClientLaunch {
            name,
            root_path,
            root_uri,
            enable_snippets,
            launch,
            args: server.args.clone(),
            environment: server.environment.clone(),
            config: server.config.clone(),
            timeout: server.timeout,
        },
    )))
}

pub fn spawn_language_server(
    id: LanguageServerId,
    prepared: PreparedClientLaunch,
) -> Result<SpawnedLanguageServer> {
    let identity = prepared.identity();
    let PreparedClientLaunch {
        name,
        root_path,
        root_uri,
        enable_snippets,
        launch,
        args,
        environment,
        config,
        timeout,
    } = prepared;
    let (client, incoming) = Client::start_with_launch(
        launch,
        &args,
        config,
        &environment,
        root_path,
        root_uri,
        id,
        name,
        identity,
        timeout,
    )?;

    let client = Arc::new(client);

    // Initialize the client asynchronously
    let _client = client.clone();
    tokio::spawn(async move {
        use futures_util::TryFutureExt;
        let value = _client
            .capabilities
            .get_or_try_init(|| {
                _client
                    .initialize(enable_snippets)
                    .map_ok(|response| response.capabilities)
            })
            .await;

        match value {
            Ok(_) => {
                if let Err(error) = _client.finish_initialization().await {
                    log::error!("failed to finish language server initialization: {error}");
                }
            }
            Err(error) => {
                log::error!("failed to initialize language server: {error}");
                _client.fail_initialization(error.to_string());
            }
        }
    });

    Ok(SpawnedLanguageServer(client, incoming))
}

/// Find an LSP workspace of a file using the following mechanism:
/// * if the file is outside `workspace` return `None`
/// * start at `file` and search the file tree upward
/// * stop the search at the first `root_dirs` entry that contains `file`
/// * if no `root_dirs` matches `file` stop at workspace
/// * Returns the top most directory that contains a `root_marker`
/// * If no root marker and we stopped at a `root_dirs` entry, return the directory we stopped at
/// * If we stopped at `workspace` instead and `workspace_is_cwd == false` return `None`
/// * If we stopped at `workspace` instead and `workspace_is_cwd == true` return `workspace`
pub fn find_lsp_workspace(
    file: &str,
    root_markers: &RootMarkers,
    root_dirs: &[PathBuf],
    workspace: &Path,
    workspace_is_cwd: bool,
) -> Option<PathBuf> {
    let file = std::path::Path::new(file);
    let mut file = if file.is_absolute() {
        file.to_path_buf()
    } else {
        let current_dir = helix_stdx::env::current_working_dir();
        current_dir.join(file)
    };
    file = path::normalize(&file);

    if !file.starts_with(workspace) {
        return None;
    }

    let mut top_marker = None;
    for ancestor in file.ancestors() {
        let Ok(mut dir) = fs::read_dir(ancestor) else {
            continue;
        };

        if dir.any(|entry| {
            if let Ok(entry) = entry {
                return root_markers.is_match(entry.file_name());
            }
            false
        }) {
            top_marker = Some(ancestor);
        }

        if root_dirs
            .iter()
            .any(|root_dir| path::normalize(workspace.join(root_dir)) == ancestor)
        {
            // if the worskapce is the cwd do not search any higher for workspaces
            // but specify
            return Some(top_marker.unwrap_or(workspace).to_owned());
        }
        if ancestor == workspace {
            // if the workspace is the CWD, let the LSP decide what the workspace
            // is
            return top_marker
                .or_else(|| (!workspace_is_cwd).then_some(workspace))
                .map(Path::to_owned);
        }
    }

    debug_assert!(false, "workspace must be an ancestor of <file>");
    None
}

#[cfg(test)]
mod tests {
    use super::{lsp, util::*, Error, OffsetEncoding, Registry};
    use helix_core::Rope;

    #[test]
    fn converts_lsp_pos_to_pos() {
        macro_rules! test_case {
            ($doc:expr, ($x:expr, $y:expr) => $want:expr) => {
                let doc = Rope::from($doc);
                let pos = lsp::Position::new($x, $y);
                assert_eq!($want, lsp_pos_to_pos(&doc, pos, OffsetEncoding::Utf16));
                assert_eq!($want, lsp_pos_to_pos(&doc, pos, OffsetEncoding::Utf8))
            };
        }

        test_case!("", (0, 0) => Some(0));
        test_case!("", (0, 1) => Some(0));
        test_case!("", (1, 0) => Some(0));
        test_case!("\n\n", (0, 0) => Some(0));
        test_case!("\n\n", (1, 0) => Some(1));
        test_case!("\n\n", (1, 1) => Some(1));
        test_case!("\n\n", (2, 0) => Some(2));
        test_case!("\n\n", (3, 0) => Some(2));
        test_case!("test\n\n\n\ncase", (4, 3) => Some(11));
        test_case!("test\n\n\n\ncase", (4, 4) => Some(12));
        test_case!("test\n\n\n\ncase", (4, 5) => Some(12));
        test_case!("", (u32::MAX, u32::MAX) => Some(0));
    }

    #[test]
    fn emoji_format_gh_4791() {
        use lsp::{Position, Range, TextEdit};

        let edits = vec![
            TextEdit {
                range: Range {
                    start: Position {
                        line: 0,
                        character: 1,
                    },
                    end: Position {
                        line: 1,
                        character: 0,
                    },
                },
                new_text: "\n  ".to_string(),
            },
            TextEdit {
                range: Range {
                    start: Position {
                        line: 1,
                        character: 7,
                    },
                    end: Position {
                        line: 2,
                        character: 0,
                    },
                },
                new_text: "\n  ".to_string(),
            },
        ];

        let mut source = Rope::from_str("[\n\"🇺🇸\",\n\"🎄\",\n]");

        let transaction = generate_transaction_from_edits(&source, edits, OffsetEncoding::Utf16);
        assert!(transaction.apply(&mut source));
        assert_eq!(source, "[\n  \"🇺🇸\",\n  \"🎄\",\n]");
    }

    #[test]
    fn missing_launch_classification_excludes_broken_managed_assets() {
        assert!(Error::CommandNotFound {
            command: "rust-analyzer".into(),
            generation: 7,
        }
        .is_missing_launch_command());

        let broken = Error::RuntimeAssets(helix_loader::RuntimeAssetsError::BrokenManaged {
            kind: helix_loader::RuntimeAssetKind::Command,
            key: "rust-analyzer".into(),
            package: Box::new(helix_loader::ActivePackage::new(
                "lsp",
                "rust-analyzer",
                "test",
            )),
            path: "missing-rust-analyzer".into(),
        });
        assert!(!broken.is_missing_launch_command());
    }

    #[tokio::test]
    async fn manual_stop_policy_is_explicit_and_survives_invalidation() {
        let mut registry = Registry::new();

        registry.stop("rust-analyzer");
        assert!(registry.is_manually_stopped("rust-analyzer"));
        assert!(!registry.invalidate("rust-analyzer"));
        assert!(registry.is_manually_stopped("rust-analyzer"));

        registry.clear_manual_stop("rust-analyzer");
        assert!(!registry.is_manually_stopped("rust-analyzer"));
    }
}
