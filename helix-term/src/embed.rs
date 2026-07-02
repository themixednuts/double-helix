//! Host-facing embedding API for non-`Application` integrations.
//!
//! This module collects the pieces a TUI or GUI host needs to drive Helix's
//! terminal-style UI without constructing the built-in terminal application.

use std::{borrow::Cow, sync::Arc};

use anyhow::Context as _;
use arc_swap::{access::Map, ArcSwap};
use helix_core::syntax;
use helix_plugin::{PluginConfig, PluginManager, PluginNotification};
use helix_runtime::{Receiver, Runtime, Sender, Work};
use helix_view::{editor::EditorBuilder, graphics::Rect, theme, Editor};

use crate::{application::Application, config::Config, handlers, keymap::Keymaps, ui::EditorView};

pub use crate::compositor::{Component, Compositor, Context, Event, EventResult, RenderContext};
pub use crate::host::{Invalidation, TermHost, TimerId, UiHost};
pub use crate::render::{
    CacheStore, CellSurface, RenderCell, RenderCellRun, RenderCellRuns, RenderOutput, RenderRow,
    RenderRun, RenderScene,
};
pub use crate::runtime::{
    IdleResetGate, IdleResetHandle, IdleResetReceiver, IdleResetRequest, RuntimeDelivery,
    RuntimeIngress, RuntimeIngressReceiver,
};
pub use helix_modal::{CommandRegistry, ModalEngineFactory};

/// Builds an [`EmbeddedEditor`] without constructing the terminal [`Application`].
pub struct EmbeddedEditorBuilder {
    area: Rect,
    runtime: Runtime,
    config: Config,
    language_loader: syntax::Loader,
    theme_loader: Arc<theme::Loader>,
    terminal_true_color: bool,
    theme_mode: Option<theme::Mode>,
    modal_factory: Arc<ModalEngineFactory>,
    plugin_config: PluginConfig,
}

impl EmbeddedEditorBuilder {
    #[must_use]
    pub fn new(area: Rect, runtime: Runtime) -> Self {
        let mut theme_parent_dirs = vec![helix_loader::config_dir()];
        theme_parent_dirs.extend(helix_loader::runtime_dirs().iter().cloned());

        Self {
            area,
            runtime,
            config: Config::default(),
            language_loader: helix_core::config::default_lang_loader(),
            theme_loader: Arc::new(theme::Loader::new(&theme_parent_dirs)),
            terminal_true_color: false,
            theme_mode: None,
            modal_factory: Arc::new(ModalEngineFactory::with_builtins()),
            plugin_config: PluginConfig {
                enabled: false,
                ..PluginConfig::default()
            },
        }
    }

    #[must_use]
    pub fn config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    #[must_use]
    pub fn language_loader(mut self, language_loader: syntax::Loader) -> Self {
        self.language_loader = language_loader;
        self
    }

    #[must_use]
    pub fn theme_loader(mut self, theme_loader: Arc<theme::Loader>) -> Self {
        self.theme_loader = theme_loader;
        self
    }

    #[must_use]
    pub fn terminal_true_color(mut self, terminal_true_color: bool) -> Self {
        self.terminal_true_color = terminal_true_color;
        self
    }

    #[must_use]
    pub fn theme_mode(mut self, theme_mode: Option<theme::Mode>) -> Self {
        self.theme_mode = theme_mode;
        self
    }

    #[must_use]
    pub fn modal_factory(mut self, modal_factory: Arc<ModalEngineFactory>) -> Self {
        self.modal_factory = modal_factory;
        self
    }

    #[must_use]
    pub fn plugin_config(mut self, plugin_config: PluginConfig) -> Self {
        self.plugin_config = plugin_config;
        self
    }

    pub fn build(self) -> anyhow::Result<EmbeddedEditor> {
        let config = Arc::new(ArcSwap::from_pointee(self.config));
        let (ingress, ingress_rx) = RuntimeIngress::channel(self.runtime.work().clone());
        let handlers = handlers::setup(config.clone(), ingress.clone(), self.runtime.clone());
        let mut editor = EditorBuilder::new(self.area, self.runtime.clone())
            .theme_loader(self.theme_loader)
            .language_loader(self.language_loader)
            .config_access(Arc::new(Map::new(
                Arc::clone(&config),
                |config: &Config| &config.editor,
            )))
            .handlers(handlers)
            .build();

        editor
            .lifecycle()
            .set_error_reporter(crate::runtime::status_error_reporter(ingress.clone()));
        handlers::attach(&editor, &editor.handlers, ingress.clone());
        editor.set_assistant_history_backend(helix_view::assistant::history::local_backend());
        editor.set_assistant_context_registry(helix_view::assistant::context::core_registry());

        Application::load_configured_theme(
            &mut editor,
            &config.load(),
            self.terminal_true_color,
            self.theme_mode,
        );

        let keys = Box::new(Map::new(Arc::clone(&config), |config: &Config| {
            &config.keys
        }));
        editor.set_modal_keymaps(crate::keymap::to_component_modal_keymaps(
            &config.load().keys,
        ));
        editor.set_semantic_modal_keymaps(crate::keymap::to_semantic_modal_keymaps(
            &config.load().keys,
        ));
        self.modal_factory.install(&mut editor);

        let mut compositor = Compositor::new(self.area);
        compositor.push(Box::new(EditorView::from_modal_factory(
            Keymaps::new(keys),
            &self.modal_factory,
            config.load().editor.editing_engine,
        )));

        let mut idle_reset_gate = IdleResetGate::new();
        let idle_reset = idle_reset_gate.handle();
        let idle_reset_rx = idle_reset_gate.take_receiver();
        let (plugin_events, plugin_event_rx) = helix_runtime::channel(256);
        let plugin_manager = PluginManager::new(self.plugin_config)
            .context("failed to create embedded plugin manager")?;
        {
            let engine = plugin_manager.engine();
            let mut engine = engine.write();
            engine.set_ui_host(crate::plugin_registry::get_ui_host(ingress.clone()));
            engine.set_panel_host(crate::plugin_registry::get_panel_host(ingress.clone()));
            engine.set_command_host(crate::plugin_registry::get_command_host(ingress.clone()));
            engine.set_event_host(crate::plugin_registry::get_event_host());
        }
        if plugin_manager.is_enabled() {
            plugin_manager
                .initialize(&mut editor)
                .context("failed to initialize embedded plugin manager")?;
        }

        Ok(EmbeddedEditor {
            editor,
            compositor,
            config,
            ingress,
            ingress_rx,
            exit_tasks: crate::runtime::ExitTaskSet::new(),
            exit_task_work: self.runtime.work().clone(),
            idle_reset,
            idle_reset_rx,
            plugin_events,
            plugin_event_rx,
            plugin_manager: Arc::new(plugin_manager),
        })
    }
}

/// Editor/compositor state owned by an embedding host.
pub struct EmbeddedEditor {
    editor: Editor,
    compositor: Compositor,
    config: Arc<ArcSwap<Config>>,
    ingress: RuntimeIngress,
    ingress_rx: RuntimeIngressReceiver,
    exit_tasks: crate::runtime::ExitTaskSet,
    exit_task_work: Work,
    idle_reset: IdleResetHandle,
    idle_reset_rx: IdleResetReceiver,
    plugin_events: Sender<PluginNotification>,
    plugin_event_rx: Receiver<PluginNotification>,
    plugin_manager: Arc<PluginManager>,
}

impl EmbeddedEditor {
    pub fn editor(&self) -> &Editor {
        &self.editor
    }

    pub fn editor_mut(&mut self) -> &mut Editor {
        &mut self.editor
    }

    pub fn compositor(&self) -> &Compositor {
        &self.compositor
    }

    pub fn compositor_mut(&mut self) -> &mut Compositor {
        &mut self.compositor
    }

    pub fn config(&self) -> Arc<Config> {
        self.config.load_full()
    }

    pub fn ingress(&self) -> RuntimeIngress {
        self.ingress.clone()
    }

    pub fn deliveries_mut(&mut self) -> &mut RuntimeIngressReceiver {
        &mut self.ingress_rx
    }

    pub fn idle_resets_mut(&mut self) -> &mut IdleResetReceiver {
        &mut self.idle_reset_rx
    }

    pub fn plugin_notifications_mut(&mut self) -> &mut Receiver<PluginNotification> {
        &mut self.plugin_event_rx
    }

    pub fn plugin_manager(&self) -> &PluginManager {
        self.plugin_manager.as_ref()
    }

    pub fn apply_delivery(&mut self, delivery: RuntimeDelivery) {
        match delivery {
            RuntimeDelivery::Status { message, severity } => {
                self.editor.status_msg = Some((Cow::Owned(message), severity));
                self.editor.mark_redraw_pending();
            }
            RuntimeDelivery::Timer(_id) => {
                self.editor.mark_redraw_pending();
            }
            RuntimeDelivery::Task(task) => {
                crate::effect::apply_runtime_task_event(
                    &mut self.editor,
                    self.ingress.clone(),
                    self.plugin_manager.clone(),
                    task,
                );
            }
            RuntimeDelivery::AssistantPermissionResolved {
                thread,
                request,
                decision,
            } => {
                let effects = self
                    .editor
                    .resolve_assistant_permission(thread, request, decision);
                self.editor.apply_assistant_effects(effects);
            }
            RuntimeDelivery::Ui(cmd) => {
                crate::runtime::apply_ui_command(
                    &mut self.editor,
                    &mut self.compositor,
                    self.ingress.clone(),
                    self.plugin_manager.clone(),
                    cmd,
                );
            }
        }
    }

    pub async fn apply_next_delivery(&mut self) -> bool {
        let Some(delivery) = self.ingress_rx.recv().await else {
            return false;
        };
        self.apply_delivery(delivery);
        true
    }

    pub fn try_apply_next_delivery(&mut self) -> Result<bool, helix_runtime::TryRecvError> {
        match self.ingress_rx.try_recv() {
            Ok(delivery) => {
                self.apply_delivery(delivery);
                Ok(true)
            }
            Err(helix_runtime::TryRecvError::Empty) => Ok(false),
            Err(err) => Err(err),
        }
    }

    pub fn try_apply_pending_deliveries(&mut self) -> Result<usize, helix_runtime::TryRecvError> {
        let mut applied = 0;
        while self.try_apply_next_delivery()? {
            applied += 1;
        }
        Ok(applied)
    }

    pub async fn recv_idle_reset(&mut self) -> Option<IdleResetRequest> {
        self.idle_reset_rx.recv().await
    }

    pub fn try_recv_idle_reset(
        &mut self,
    ) -> Result<Option<IdleResetRequest>, helix_runtime::TryRecvError> {
        match self.idle_reset_rx.try_recv() {
            Ok(request) => Ok(Some(request)),
            Err(helix_runtime::TryRecvError::Empty) => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub async fn recv_plugin_notification(&mut self) -> Option<PluginNotification> {
        self.plugin_event_rx.recv().await
    }

    pub fn try_recv_plugin_notification(
        &mut self,
    ) -> Result<Option<PluginNotification>, helix_runtime::TryRecvError> {
        match self.plugin_event_rx.try_recv() {
            Ok(notification) => Ok(Some(notification)),
            Err(helix_runtime::TryRecvError::Empty) => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub fn resize(&mut self, area: Rect) -> bool {
        self.compositor.resize(area);
        self.with_context(|compositor, context| {
            compositor.handle_event(&Event::Resize(area.width, area.height), context)
        })
    }

    pub fn with_context<R>(&mut self, f: impl FnOnce(&mut Compositor, &mut Context<'_>) -> R) -> R {
        let notifier = handlers::local::Notifier {
            redraw: self.editor.redraw_handle(),
            plugin_events: self.plugin_events.clone(),
        };
        let mut context = Context::new(
            &mut self.editor,
            &mut self.exit_tasks,
            self.exit_task_work.clone(),
            notifier,
            self.ingress.clone(),
            self.idle_reset.clone(),
            Some(self.plugin_manager.clone()),
        );
        f(&mut self.compositor, &mut context)
    }

    pub fn handle_event(&mut self, event: &Event) -> bool {
        self.with_context(|compositor, context| compositor.handle_event(event, context))
    }

    pub fn render_frame(&mut self, area: Rect) -> RenderOutput {
        if self.compositor.size() != area {
            self.resize(area);
        }
        self.with_context(|compositor, context| compositor.render_frame(area, context))
    }

    pub fn render_scene(&mut self, area: Rect) -> RenderScene {
        self.render_frame(area).to_scene()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_view::editor::Action;

    #[test]
    fn embedded_editor_builds_and_renders_cell_frame() {
        let area = Rect::new(0, 0, 40, 12);
        let runtime = helix_runtime::test::RuntimeTest::default();
        runtime.block_on(async {
            let mut embedded = EmbeddedEditorBuilder::new(area, runtime.runtime())
                .build()
                .unwrap();
            embedded.editor_mut().new_file(Action::VerticalSplit);

            let output = embedded.render_frame(area);

            assert_eq!(output.area(), area);
            assert_eq!(output.surface().area().width, area.width);
            assert_eq!(output.surface().area().height, area.height);
            assert_eq!(output.cells().count(), area.area());
        });
    }

    #[test]
    fn embedded_editor_applies_runtime_status_delivery() {
        let area = Rect::new(0, 0, 40, 12);
        let runtime = helix_runtime::test::RuntimeTest::default();
        runtime.block_on(async {
            let mut embedded = EmbeddedEditorBuilder::new(area, runtime.runtime())
                .build()
                .unwrap();
            embedded.ingress().status("ready");

            assert!(embedded.apply_next_delivery().await);

            let (message, _) = embedded.editor().status_msg.as_ref().unwrap();
            assert_eq!(message.as_ref(), "ready");
            assert!(embedded.editor().is_redraw_pending());
        });
    }

    #[test]
    fn embedded_editor_applies_pending_runtime_deliveries() {
        let area = Rect::new(0, 0, 40, 12);
        let runtime = helix_runtime::test::RuntimeTest::default();
        runtime.block_on(async {
            let mut embedded = EmbeddedEditorBuilder::new(area, runtime.runtime())
                .build()
                .unwrap();
            embedded.ingress().status("first");
            embedded.ingress().status("second");

            let applied = embedded.try_apply_pending_deliveries().unwrap();

            assert_eq!(applied, 2);
            let (message, _) = embedded.editor().status_msg.as_ref().unwrap();
            assert_eq!(message.as_ref(), "second");
        });
    }

    #[test]
    fn embedded_editor_resize_updates_compositor_and_frame_area() {
        let area = Rect::new(0, 0, 40, 12);
        let next_area = Rect::new(0, 0, 50, 14);
        let runtime = helix_runtime::test::RuntimeTest::default();
        runtime.block_on(async {
            let mut embedded = EmbeddedEditorBuilder::new(area, runtime.runtime())
                .build()
                .unwrap();
            embedded.editor_mut().new_file(Action::VerticalSplit);

            embedded.resize(next_area);
            let output = embedded.render_frame(next_area);

            assert_eq!(embedded.compositor().size(), next_area);
            assert_eq!(output.area(), next_area);
            assert_eq!(output.surface().area().width, next_area.width);
            assert_eq!(output.surface().area().height, next_area.height);
        });
    }

    #[test]
    fn embedded_editor_renders_scene_display_list() {
        let area = Rect::new(0, 0, 40, 12);
        let runtime = helix_runtime::test::RuntimeTest::default();
        runtime.block_on(async {
            let mut embedded = EmbeddedEditorBuilder::new(area, runtime.runtime())
                .build()
                .unwrap();
            embedded.editor_mut().new_file(Action::VerticalSplit);

            let scene = embedded.render_scene(area);

            assert_eq!(scene.area(), area);
            assert_eq!(scene.rows().len(), area.height as usize);
            assert!(scene.runs().next().is_some());
        });
    }
}
