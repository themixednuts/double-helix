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
        FrontendState, PackagedAssistantAgentCache,
    },
    types::Diagnostics,
    Config, CursorCache, Editor, NotificationManager, WorkspaceDiagnosticCounts,
};

pub struct EditorBuilder {
    area: Rect,
    theme_loader: Arc<theme::Loader>,
    language_loader: Arc<ArcSwap<syntax::Loader>>,
    config: Arc<dyn DynAccess<Config> + Send + Sync>,
    runtime: Runtime,
    handlers: Handlers,
}

impl EditorBuilder {
    #[must_use]
    pub fn new(area: Rect, runtime: Runtime) -> Self {
        Self {
            area,
            theme_loader: Arc::new(theme::Loader::new(&[])),
            language_loader: Arc::new(ArcSwap::from_pointee(
                helix_core::config::default_lang_loader(),
            )),
            config: Arc::new(ArcSwap::from_pointee(Config::default())),
            runtime,
            handlers: Handlers::dummy(),
        }
    }

    #[must_use]
    pub fn theme_loader(mut self, theme_loader: Arc<theme::Loader>) -> Self {
        self.theme_loader = theme_loader;
        self
    }

    #[must_use]
    pub fn language_loader(mut self, language_loader: syntax::Loader) -> Self {
        self.language_loader = Arc::new(ArcSwap::from_pointee(language_loader));
        self
    }

    #[must_use]
    pub fn language_loader_store(mut self, language_loader: Arc<ArcSwap<syntax::Loader>>) -> Self {
        self.language_loader = language_loader;
        self
    }

    #[must_use]
    pub fn config(mut self, config: Config) -> Self {
        self.config = Arc::new(ArcSwap::from_pointee(config));
        self
    }

    #[must_use]
    pub fn config_access(mut self, config: Arc<dyn DynAccess<Config> + Send + Sync>) -> Self {
        self.config = config;
        self
    }

    #[must_use]
    pub fn handlers(mut self, handlers: Handlers) -> Self {
        self.handlers = handlers;
        self
    }

    #[must_use]
    pub fn build(self) -> Editor {
        Editor::new(
            self.area,
            self.theme_loader,
            self.language_loader,
            self.config,
            self.runtime,
            self.handlers,
        )
    }
}

impl Editor {
    pub fn new(
        mut area: Rect,
        theme_loader: Arc<theme::Loader>,
        syn_loader: Arc<ArcSwap<syntax::Loader>>,
        config: Arc<dyn DynAccess<Config> + Send + Sync>,
        runtime: Runtime,
        handlers: Handlers,
    ) -> Self {
        let language_servers = helix_lsp::Registry::new();
        let conf = config.load();
        let auto_pairs = (&conf.auto_pairs).into();
        let (assistant_updates_tx, assistant_updates_rx) = helix_runtime::channel(128);

        area.height = area.height.saturating_sub(1);

        Self {
            mode: Mode::Normal,
            tree: crate::tree::Tree::new(area),
            next_document_id: crate::DocumentId::default(),
            documents: std::collections::BTreeMap::new(),
            component_docs: std::collections::BTreeMap::new(),
            next_virtual_view_idx: 0,
            component_views: std::collections::BTreeMap::new(),
            save_locks: HashMap::new(),
            save_queue: std::collections::VecDeque::new(),
            write_count: 0,
            macro_recording: None,
            macro_replaying: Vec::new(),
            theme: Arc::new(theme_loader.default()),
            theme_generation: 0,
            language_servers,
            language_server_supervisor:
                super::language_server_supervisor::LanguageServerSupervisor::default(),
            diagnostics: Diagnostics::new(),
            diagnostics_revision: 0,
            diagnostic_summaries: Default::default(),
            diagnostic_path_summaries: Default::default(),
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
            frame_gate: helix_runtime::FrameGate::new(),
            needs_redraw: false,
            config_gen: 0,
            handlers,
            lifecycle: std::sync::Arc::new(super::hooks::LifecycleBus::default()),
            file_watcher: None,
            file_operations: super::file_operation::FileOperationJournal::default(),
            prepared_document_opens: super::document_io::PreparedDocumentOpenCache::default(),
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
                semantic_modal_keymaps: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(
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
            assistant_packaged_agents: PackagedAssistantAgentCache::default(),
            assistant_follow: AssistantFollowState {
                snapshot: None,
                suppress_pause: false,
            },
            bench: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_builder_creates_headless_editor() {
        let area = Rect::new(0, 0, 40, 12);
        let runtime = helix_runtime::test::RuntimeTest::default();
        runtime.block_on(async {
            let editor = EditorBuilder::new(area, runtime.runtime()).build();

            assert_eq!(editor.tree.area().width, area.width);
            assert_eq!(editor.tree.area().height, area.height.saturating_sub(1));
            assert_eq!(editor.document_count(), 0);
        });
    }

    #[test]
    fn editor_builder_handles_zero_height_area() {
        let area = Rect::new(0, 0, 40, 0);
        let runtime = helix_runtime::test::RuntimeTest::default();
        runtime.block_on(async {
            let editor = EditorBuilder::new(area, runtime.runtime()).build();

            assert_eq!(editor.tree.area().width, area.width);
            assert_eq!(editor.tree.area().height, 0);
        });
    }
}
