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

/// The workspace-trust check a diff provider registry asks before letting a repository use its
/// own `.git/config`.
pub(crate) fn repo_trust(
    trust: &helix_loader::workspace_trust::WorkspaceTrust,
) -> helix_vcs::RepoTrust {
    let trust = trust.clone();
    std::sync::Arc::new(move |path: &std::path::Path| {
        let dir = if path.is_dir() {
            path
        } else {
            path.parent().unwrap_or(path)
        };
        let workspace = helix_loader::find_workspace_in(dir).0;
        trust
            .query(&workspace, helix_loader::workspace_trust::TrustQuery::Git)
            .is_trusted()
    })
}

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
        let language_servers = helix_lsp::Registry::new(&runtime);
        let conf = config.load();
        let workspace_trust =
            helix_loader::workspace_trust::WorkspaceTrust::new((&conf.workspace_trust).into());
        let auto_pairs = (&conf.auto_pairs).into();
        let (assistant_updates_tx, assistant_updates_rx) = helix_runtime::channel(128);
        let lifecycle = std::sync::Arc::new(super::hooks::LifecycleBus::default());
        let open_buffers = crate::open_buffers::OpenBuffers::default();
        lifecycle.on_document_change({
            let open_buffers = open_buffers.clone();
            move |event| {
                if let Some(path) = event.doc.path() {
                    open_buffers.changed(path, event.doc.text());
                }
                Ok(())
            }
        });
        lifecycle.on_document_close({
            let open_buffers = open_buffers.clone();
            move |event| {
                if let Some(path) = event.doc.path() {
                    open_buffers.forget(path);
                }
                Ok(())
            }
        });
        let collaboration = crate::collab::Replication::default();

        area.height = area.height.saturating_sub(1);

        let editor = Self {
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
            diff_providers: DiffProviderRegistry::new(conf.vcs.provider.into())
                .with_repo_trust(repo_trust(&workspace_trust)),
            workspace_trust,
            open_buffers,
            debug_adapters: dap::registry::Registry::new(),
            breakpoints: HashMap::new(),
            runtime,
            workspace_backend: super::WorkspaceBackend::Local,
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
            dir_stack: std::collections::VecDeque::with_capacity(super::DIR_STACK_CAP),
            config,
            auto_pairs,
            exit_code: 0,
            config_events: helix_runtime::channel(64),
            frame_gate: helix_runtime::FrameGate::new(),
            needs_redraw: false,
            config_gen: 0,
            handlers,
            lifecycle: lifecycle.clone(),
            file_watcher: None,
            file_operations: super::file_operation::FileOperationJournal::default(),
            prepared_document_opens: super::document_io::PreparedDocumentOpenCache::default(),
            mouse_down_range: None,
            cursor_cache: CursorCache::default(),
            model: crate::model::Model::default(),
            surface_registry: crate::collab::Registry::new(),
            collab: crate::collab::Store::default(),
            collaboration: collaboration.clone(),
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
                layout_key: None,
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
        };
        lifecycle.on_document_change(move |event| {
            collaboration.document_changed(event);
            Ok(())
        });
        let collaboration = editor.collaboration.clone();
        lifecycle.on_selection_change(move |event| {
            collaboration.selection_changed(event);
            Ok(())
        });
        editor
    }
}

impl Editor {
    /// A diff provider registry for `provider` that asks this editor's workspace trust before
    /// letting a repository use its own config.
    pub fn diff_provider_registry(&self, provider: helix_vcs::VcsProvider) -> DiffProviderRegistry {
        DiffProviderRegistry::new(provider).with_repo_trust(repo_trust(&self.workspace_trust))
    }

    /// Replace the workspace trust state (tests, embedders); diff providers follow it.
    pub fn set_workspace_trust(&mut self, trust: helix_loader::workspace_trust::WorkspaceTrust) {
        self.workspace_trust = trust;
        self.diff_providers = self.diff_provider_registry(self.diff_providers.provider());
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
