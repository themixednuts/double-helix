use helix_core::Uri;
use helix_lsp::{lsp, LanguageServerId, OffsetEncoding};
use helix_view::handlers::lsp::{SignatureHelpInvoked, SignatureHelpRequestId};

/// LSP text position for navigation (goto / picker); mirrors `lsp::Location` with `Uri` + encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspLocation {
    pub uri: Uri,
    pub range: lsp::Range,
    pub offset_encoding: OffsetEncoding,
}

/// How to show multi-server hover content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspHoverDisplay {
    Popup,
    /// Open aggregated markdown in a scratch buffer (`goto_hover`).
    FileBuffer,
}

/// Menu vs list picker for code actions (`code_action` vs `code_action_picker`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspCodeActionPresentation {
    Menu,
    Picker,
}

/// One row for code-action menu/picker (async gather -> typed UI).
#[derive(Debug, Clone)]
pub struct LspCodeActionItem {
    pub lsp_item: lsp::CodeActionOrCommand,
    pub language_server_id: LanguageServerId,
}

/// Document symbol picker row (`textDocument/documentSymbol`).
#[derive(Debug, Clone)]
pub struct DocumentSymbolPickerItem {
    pub location: LspLocation,
    pub symbol: lsp::SymbolInformation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspCallHierarchyDirection {
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspTypeHierarchyDirection {
    Supertypes,
    Subtypes,
}

#[derive(Debug, Clone)]
pub enum LspHierarchyPrepareItem {
    Call {
        server_id: LanguageServerId,
        offset_encoding: OffsetEncoding,
        item: lsp::CallHierarchyItem,
        direction: LspCallHierarchyDirection,
    },
    Type {
        server_id: LanguageServerId,
        offset_encoding: OffsetEncoding,
        item: lsp::TypeHierarchyItem,
        direction: LspTypeHierarchyDirection,
    },
}

#[derive(Debug, Clone)]
pub struct LspHierarchyPickerItem {
    pub name: String,
    pub detail: Option<String>,
    pub kind: lsp::SymbolKind,
    pub location: LspLocation,
}

/// LSP-driven compositor UI.
#[derive(Debug)]
pub enum LspCommand {
    /// Picker or single-file jump; if `locations` is empty, show `empty_message`.
    Goto {
        locations: Vec<LspLocation>,
        empty_message: &'static str,
    },
    /// Hover popup or scratch buffer; empty `hovers` -> status in apply.
    Hover {
        hovers: Vec<(String, lsp::Hover)>,
        display: LspHoverDisplay,
    },
    /// Code action menu or picker; empty `items` -> error in apply.
    CodeActions {
        items: Vec<LspCodeActionItem>,
        presentation: LspCodeActionPresentation,
    },
    /// Document symbols picker; empty `symbols` -> status in apply.
    DocumentSymbols {
        symbols: Vec<DocumentSymbolPickerItem>,
    },
    HierarchyPrepare {
        items: Vec<LspHierarchyPrepareItem>,
        empty_message: &'static str,
    },
    Hierarchy {
        items: Vec<LspHierarchyPickerItem>,
        empty_message: &'static str,
    },
    SignatureHelp {
        invoked: SignatureHelpInvoked,
        request: SignatureHelpRequestId,
        response: Option<lsp::SignatureHelp>,
    },
    PrepareRename {
        prefill: String,
        history_register: Option<char>,
        language_server_id: Option<LanguageServerId>,
    },
}
