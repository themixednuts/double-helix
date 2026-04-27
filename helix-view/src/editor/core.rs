use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::Arc,
};

use crate::{
    document::{DocumentSavedTask, Mode},
    handlers::Handlers,
    info::Info,
    input::KeyEvent,
    register::Registers,
    theme::{self, Theme},
    tree::Tree,
    view::ComponentViewState,
    Document, DocumentId, ViewId,
};
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
    pub assistant_panel_theme: Option<Theme>,
    pub engine_factory: Option<std::sync::Arc<dyn crate::engine::EditingEngineFactory>>,
    pub modal_keymaps: Option<
        std::sync::Arc<
            arc_swap::ArcSwap<std::collections::HashMap<Mode, crate::keymap::ModalKeyTrie>>,
        >,
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

    pub saves: HashMap<DocumentId, RuntimeSender<DocumentSavedTask>>,
    pub(super) save_tx: RuntimeSender<DocumentSavedTask>,
    pub save_queue: RuntimeReceiver<DocumentSavedTask>,
    pub write_count: usize,

    pub registers: Registers,
    pub macro_recording: Option<(char, Vec<KeyEvent>)>,
    pub macro_replaying: Vec<char>,
    pub language_servers: helix_lsp::Registry,
    pub diagnostics: Diagnostics,
    pub workspace_diagnostic_counts: WorkspaceDiagnosticCounts,
    pub diff_providers: DiffProviderRegistry,

    pub debug_adapters: dap::registry::Registry,
    pub breakpoints: HashMap<PathBuf, Vec<Breakpoint>>,

    pub(super) runtime: Runtime,

    pub syn_loader: Arc<ArcSwap<syntax::Loader>>,
    pub theme_loader: Arc<theme::Loader>,
    pub last_theme: Option<Theme>,
    pub theme: Theme,

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
    pub(crate) assistant_follow: AssistantFollowState,

    pub bench: Option<crate::bench::BenchState>,
}
