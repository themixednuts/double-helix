use std::{collections::HashMap, sync::Arc};

use arc_swap::{access::DynAccess, ArcSwap};

use crate::{document::Mode, graphics::Rect, handlers::Handlers, register::Registers, theme};
use helix_core::syntax;
use helix_dap::{self as dap};
use helix_runtime::Runtime;
use helix_vcs::DiffProviderRegistry;

use super::{
    core::{
        AssistantFollowState, AssistantPersistenceState, AssistantRuntimeState, AssistantServices,
        FrontendState,
    },
    types::Diagnostics,
    Config, CursorCache, Editor, NotificationManager, WorkspaceDiagnosticCounts,
};

impl Editor {
    pub fn new(
        mut area: Rect,
        theme_loader: Arc<theme::Loader>,
        syn_loader: Arc<ArcSwap<syntax::Loader>>,
        config: Arc<dyn DynAccess<Config> + Send + Sync>,
        runtime: Runtime,
        handlers: Handlers,
    ) -> Self {
        let language_servers = helix_lsp::Registry::new(syn_loader.clone());
        let conf = config.load();
        let auto_pairs = (&conf.auto_pairs).into();
        let (assistant_updates_tx, assistant_updates_rx) = helix_runtime::channel(128);

        area.height -= 1;

        let (save_tx, save_queue) = helix_runtime::channel(64);

        Self {
            mode: Mode::Normal,
            tree: crate::tree::Tree::new(area),
            next_document_id: crate::DocumentId::default(),
            documents: std::collections::BTreeMap::new(),
            component_docs: std::collections::BTreeMap::new(),
            next_virtual_view_idx: 0,
            component_views: std::collections::BTreeMap::new(),
            saves: HashMap::new(),
            save_tx,
            save_queue,
            write_count: 0,
            macro_recording: None,
            macro_replaying: Vec::new(),
            theme: theme_loader.default(),
            language_servers,
            diagnostics: Diagnostics::new(),
            workspace_diagnostic_counts: WorkspaceDiagnosticCounts::default(),
            diff_providers: DiffProviderRegistry::new(conf.vcs.provider.into()),
            debug_adapters: dap::registry::Registry::new(),
            breakpoints: HashMap::new(),
            runtime,
            syn_loader,
            theme_loader,
            last_theme: None,
            last_selection: None,
            registers: Registers::new(Box::new(arc_swap::access::Map::new(
                Arc::clone(&config),
                |config: &Config| &config.clipboard_provider,
            ))),
            status_msg: None,
            notifications: NotificationManager::new(conf.notifications.max_history),
            autoinfo: None,
            last_motion: None,
            last_completion: None,
            last_cwd: None,
            config,
            auto_pairs,
            exit_code: 0,
            config_events: helix_runtime::channel(64),
            frame_gate: helix_runtime::FrameGate::new(64),
            needs_redraw: false,
            config_gen: 0,
            handlers,
            lifecycle: std::sync::Arc::new(super::hooks::LifecycleBus::default()),
            file_watcher: None,
            mouse_down_range: None,
            cursor_cache: CursorCache::default(),
            model: crate::model::Model::default(),
            surface_registry: crate::collab::Registry::new(),
            collab: crate::collab::Store::default(),
            assistant: crate::assistant::Store::default(),
            frontend: FrontendState {
                focused_modal_input: crate::engine::ModalInputState::default(),
                assistant_panel_theme: None,
                engine_factory: std::sync::Arc::new(crate::engine::HeadlessEditingEngineFactory),
                modal_keymaps: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(
                    std::collections::HashMap::new(),
                )),
            },
            assistant_services: AssistantServices {
                terminals: std::sync::Arc::new(helix_acp::TerminalManager::new()),
                history: None,
                context: crate::assistant::context::Registry::default(),
            },
            assistant_persistence: AssistantPersistenceState {
                saves: std::collections::BTreeMap::new(),
                layout_save: helix_runtime::Debounce::new(std::time::Duration::from_millis(300)),
            },
            assistant_runtime: AssistantRuntimeState {
                backends: std::collections::BTreeMap::new(),
                updates_tx: assistant_updates_tx,
                updates_rx: assistant_updates_rx,
            },
            assistant_follow: AssistantFollowState {
                snapshot: None,
                suppress_pause: false,
            },
            bench: None,
        }
    }
}
