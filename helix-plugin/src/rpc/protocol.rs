use crate::contract::errors::ContractError;
use crate::contract::events::{EventKind, PluginEvent};
use crate::contract::handles::*;
use crate::contract::metadata::{ApiMetadata, EventKindInfo};
use crate::contract::pkg::{PkgBackendRequest, PkgBackendResponse};
use crate::contract::requests::*;
use crate::contract::snapshots::*;
use crate::contract::value::DynamicValue;
use crate::types::PluginConfig;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
/// A length-prefixed RPC frame exchanged between the editor and a plugin host.
pub enum Frame<Req, Res> {
    /// A request that expects a matching response with the same id.
    Request { id: u64, body: Req },
    /// A response to a request with the same id.
    Response {
        id: u64,
        result: Result<Res, ContractError>,
    },
    /// A fire-and-forget request.
    Notify { body: Req },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostRequest {
    Init {
        metadata: ApiMetadata,
        config: PluginConfig,
    },
    Event(PluginEvent),
    CommandInvoke {
        name: String,
        args: Vec<String>,
    },
    UiCallback {
        callback_id: u64,
        value: DynamicValue,
    },
    PkgBackend(PkgBackendRequest),
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PluginResponse {
    Unit,
    Commands(Vec<crate::types::CommandMetadata>),
    PkgBackend(PkgBackendResponse),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PluginRequest {
    ApiMetadata,
    FocusedDocument,
    FocusedView,
    ListDocuments,
    ListViews,
    DocumentSnapshot(DocumentHandle),
    ViewSnapshot(ViewHandle),
    WorkspaceSnapshot,
    ThemeSnapshot,
    Diagnostics(DocumentHandle),
    DocumentText(DocumentHandle),
    DocumentLine {
        document: DocumentHandle,
        line: usize,
    },
    OpenDocument(OpenDocumentRequest),
    ApplyEdit(ApplyEditRequest),
    SetSelection(SetSelectionRequest),
    SaveDocument(SaveDocumentRequest),
    FocusView(FocusViewRequest),
    SetAnnotations(SetAnnotationsRequest),
    SetStatus(SetStatusRequest),
    Undo(UndoRequest),
    Redo(RedoRequest),
    SetMode(SetModeRequest),
    CloseView(CloseViewRequest),
    Notify(NotifyRequest),
    Prompt {
        plugin: PluginId,
        request: PromptRequest,
    },
    Confirm {
        plugin: PluginId,
        request: ConfirmRequest,
    },
    Picker {
        plugin: PluginId,
        request: PickerRequest,
    },
    RegisterPanel {
        plugin: PluginId,
        registration: PanelRegistration,
    },
    UpdatePanel {
        plugin: PluginId,
        request: PanelUpdateRequest,
    },
    ClosePanel {
        plugin: PluginId,
        request: PanelCloseRequest,
    },
    TogglePanel {
        plugin: PluginId,
        request: TogglePanelRequest,
    },
    FocusPanel {
        plugin: PluginId,
        request: FocusPanelRequest,
    },
    ResizePanel {
        plugin: PluginId,
        request: ResizePanelRequest,
    },
    ListPanels,
    RegisterCommand {
        plugin: PluginId,
        definition: CommandDefinition,
    },
    UpdateCommand {
        plugin: PluginId,
        request: CommandUpdateRequest,
    },
    RemoveCommand {
        plugin: PluginId,
        request: CommandRemoveRequest,
    },
    RunCommand(RunCommandRequest),
    Subscribe {
        plugin: PluginId,
        kind: EventKind,
    },
    Unsubscribe {
        plugin: PluginId,
        handle: SubscriptionHandle,
    },
    EventCatalog,
    SplitView(SplitViewRequest),
    FocusDirection(FocusDirectionRequest),
    SwapSplit(SwapSplitRequest),
    ResizeSplit(ResizeSplitRequest),
    Transpose(TransposeSplitRequest),
    SplitTree,
    OpenTab(OpenTabRequest),
    CloseTab(CloseTabRequest),
    FocusTab(FocusTabRequest),
    CycleTab(CycleTabRequest),
    ListTabs(Option<ViewHandle>),
    CreateFloat {
        plugin: PluginId,
        request: CreateFloatRequest,
    },
    UpdateFloat(UpdateFloatRequest),
    CloseFloat(CloseFloatRequest),
    ListFloats,
    AssistantSnapshot,
    ThreadSnapshot(ThreadHandle),
    ThreadEntries(ThreadHandle),
    ThreadContext(ThreadHandle),
    SubmitPrompt {
        thread: Option<ThreadHandle>,
        text: String,
    },
    CancelThread(Option<ThreadHandle>),
    WorkspaceDetail,
    Log {
        level: LogLevel,
        plugin: String,
        msg: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostResponse {
    Unit,
    Bool(bool),
    ApiMetadata(ApiMetadata),
    DocumentHandle(DocumentHandle),
    ViewHandle(ViewHandle),
    FloatHandle(FloatHandle),
    PanelHandle(PanelHandle),
    CommandHandle(CommandHandle),
    SubscriptionHandle(SubscriptionHandle),
    UiCallback(u64),
    OptionDocumentHandle(Option<DocumentHandle>),
    OptionViewHandle(Option<ViewHandle>),
    OptionViewHandleResult(Option<ViewHandle>),
    DocumentHandles(Vec<DocumentHandle>),
    ViewHandles(Vec<ViewHandle>),
    DocumentSnapshot(DocumentSnapshot),
    ViewSnapshot(ViewSnapshot),
    WorkspaceSnapshot(WorkspaceSnapshot),
    ThemeSnapshot(ThemeSnapshot),
    DiagnosticSnapshot(DiagnosticSnapshot),
    DocumentText(String),
    DocumentLine(String),
    EventCatalog(Vec<EventKindInfo>),
    PanelSnapshots(Vec<PanelSnapshot>),
    FloatSnapshots(Vec<FloatSnapshot>),
    SplitTree(SplitTreeSnapshot),
    TabGroup(TabGroupSnapshot),
    AssistantSnapshot(AssistantSnapshot),
    AssistantThreadSnapshot(AssistantThreadSnapshot),
    AssistantEntries(Vec<AssistantEntrySnapshot>),
    AssistantContext(Vec<AssistantContextSnapshot>),
    WorkspaceDetail(WorkspaceDetailSnapshot),
}
