use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, VecDeque},
    path::PathBuf,
    sync::Arc,
};

use crate::{
    document::{DocumentSaveLock, DocumentSavedTask, Mode},
    handlers::Handlers,
    info::Info,
    input::KeyEvent,
    register::Registers,
    theme::{self, Theme},
    tree::Tree,
    view::ComponentViewState,
    Document, DocumentId, ViewId,
};

pub(crate) struct PendingDocumentSave {
    pub doc_id: DocumentId,
    pub task: DocumentSavedTask,
}
use helix_core::{auto_pairs::AutoPairs, syntax, Range, Selection};
use helix_dap::{self as dap};
use helix_runtime::{Receiver as RuntimeReceiver, Runtime, Sender as RuntimeSender};
use helix_vcs::DiffProviderRegistry;

use arc_swap::{access::DynAccess, ArcSwap};

use super::{
    types::{
        AssistantFollowSnapshot, Breakpoint, CompleteAction, ConfigEvent, Diagnostics, Motion,
    },
    Config, CursorCache, NotificationManager, Severity, WorkspaceDiagnosticCounts,
};

pub(crate) struct AssistantServices {
    pub(crate) terminals: Arc<helix_acp::TerminalManager>,
    pub(crate) history: Option<crate::assistant::history::Backend>,
    pub(crate) context: crate::assistant::context::Registry,
}

pub(crate) struct AssistantRuntimeState {
    pub(crate) backends: BTreeMap<crate::assistant::backend::Id, crate::assistant::BackendHandle>,
    pub(crate) updates_tx: RuntimeSender<crate::assistant::backend::Update>,
    pub(crate) updates_rx: RuntimeReceiver<crate::assistant::backend::Update>,
}

#[derive(Default)]
pub(crate) struct PackagedAssistantAgentCache {
    pub(crate) generation: u64,
    pub(crate) agents: Arc<BTreeMap<String, crate::editor::AgentConfig>>,
}

pub(crate) struct AssistantPersistenceState {
    pub(crate) saves: BTreeMap<crate::assistant::thread::Id, helix_runtime::Debounce>,
    pub(crate) layout_save: helix_runtime::Debounce,
}

pub(crate) struct AssistantFollowState {
    pub(crate) snapshot: Option<AssistantFollowSnapshot>,
    pub(crate) suppress_pause: bool,
}

pub struct FrontendState {
    pub focused_modal_input: crate::engine::ModalInputState,
    pub assistant_panel_theme: Option<Arc<Theme>>,
    pub engine_factory: std::sync::Arc<dyn crate::engine::EditingEngineFactory>,
    pub modal_keymaps: std::sync::Arc<
        arc_swap::ArcSwap<std::collections::HashMap<Mode, crate::keymap::ModalKeyTrie>>,
    >,
    pub semantic_modal_keymaps: std::sync::Arc<
        arc_swap::ArcSwap<std::collections::HashMap<Mode, crate::keymap::ModalIntentTrie>>,
    >,
}

pub struct Editor {
    pub mode: Mode,
    pub tree: Tree,
    pub next_document_id: DocumentId,
    pub documents: BTreeMap<DocumentId, Document>,
    pub component_docs: BTreeMap<DocumentId, Document>,
    pub(super) next_virtual_view_idx: u32,
    pub component_views: BTreeMap<ViewId, ComponentViewState>,

    pub(crate) save_locks: HashMap<DocumentId, DocumentSaveLock>,
    pub(crate) save_queue: VecDeque<PendingDocumentSave>,
    pub(crate) write_count: usize,

    pub registers: Registers,
    pub macro_recording: Option<(char, Vec<KeyEvent>)>,
    pub macro_replaying: Vec<char>,
    pub language_servers: helix_lsp::Registry,
    pub(crate) language_server_supervisor:
        super::language_server_supervisor::LanguageServerSupervisor,
    pub diagnostics: Diagnostics,
    pub(crate) diagnostics_revision: u64,
    pub(crate) diagnostic_summaries:
        std::collections::BTreeMap<helix_core::Uri, WorkspaceDiagnosticCounts>,
    pub(crate) diagnostic_path_summaries:
        std::collections::BTreeMap<PathBuf, WorkspaceDiagnosticCounts>,
    pub workspace_diagnostic_counts: WorkspaceDiagnosticCounts,
    pub diff_providers: DiffProviderRegistry,

    pub debug_adapters: dap::registry::Registry,
    pub breakpoints: HashMap<PathBuf, Vec<Breakpoint>>,

    pub(super) runtime: Runtime,

    pub syn_loader: Arc<ArcSwap<syntax::Loader>>,
    pub theme_loader: Arc<theme::Loader>,
    pub last_theme: Option<Arc<Theme>>,
    pub theme: Arc<Theme>,
    pub theme_generation: u64,

    pub last_selection: Option<Selection>,

    pub status_msg: Option<(Cow<'static, str>, Severity)>,
    pub notifications: NotificationManager,
    pub autoinfo: Option<Info>,

    pub config: Arc<dyn DynAccess<Config> + Send + Sync>,
    pub auto_pairs: Option<AutoPairs>,

    pub(super) last_motion: Option<Motion>,
    pub last_completion: Option<CompleteAction>,
    pub(super) last_cwd: Option<PathBuf>,

    pub exit_code: i32,

    pub config_events: (RuntimeSender<ConfigEvent>, RuntimeReceiver<ConfigEvent>),
    pub frame_gate: helix_runtime::FrameGate,
    pub needs_redraw: bool,
    pub config_gen: u64,
    pub handlers: Handlers,
    pub(crate) lifecycle: std::sync::Arc<super::hooks::LifecycleBus>,

    pub file_watcher: Option<crate::file_watcher::FileWatcher>,
    pub(crate) file_operations: super::file_operation::FileOperationJournal,
    pub(crate) prepared_document_opens: super::document_io::PreparedDocumentOpenCache,

    pub mouse_down_range: Option<Range>,
    pub cursor_cache: CursorCache,

    pub model: crate::model::Model,
    pub surface_registry: crate::collab::Registry,
    pub collab: crate::collab::Store,
    pub assistant: crate::assistant::Store,
    pub frontend: FrontendState,
    pub(crate) assistant_services: AssistantServices,
    pub(crate) assistant_persistence: AssistantPersistenceState,
    pub(crate) assistant_runtime: AssistantRuntimeState,
    pub(crate) assistant_packaged_agents: PackagedAssistantAgentCache,
    pub(crate) assistant_follow: AssistantFollowState,

    pub bench: Option<crate::bench::BenchState>,
}
