use std::{future::Future, num::NonZeroUsize};

use helix_core::movement::Movement;
use helix_modal::registry::CommandRegistry;
use helix_view::{document::Mode, engine::CommandToken, input::KeyEvent, Editor};

use crate::{
    compositor::{self, Component},
    runtime::{ExitTaskSet, RuntimeTaskEvent, UiCommand},
};

pub type OnKeyCallback = Box<dyn FnOnce(&mut Context, KeyEvent) + Send>;

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum OnKeyCallbackKind {
    PseudoPending,
    Fallback,
}

pub struct Context<'a> {
    pub register: Option<char>,
    pub count: Option<NonZeroUsize>,
    pub editor: &'a mut Editor,
    pub registry: std::sync::Arc<CommandRegistry>,
    pub notifier: crate::handlers::local::Notifier,

    pub callback: Vec<crate::compositor::PostAction>,
    pub on_next_key_callback: Option<(OnKeyCallback, OnKeyCallbackKind)>,
    /// Exit-bound task sink for commands that must complete typed task work before shutdown.
    pub exit_tasks: &'a mut ExitTaskSet,
    pub exit_task_work: helix_runtime::Work,
    /// Mirrors [`compositor::Context::ingress`] when built from the live app.
    pub ingress: crate::runtime::RuntimeIngress,
    pub foreground: crate::runtime::ForegroundEvents,
    pub redraw: helix_runtime::FrameHandle,
    pub idle_reset: crate::runtime::IdleResetHandle,
    pub(crate) plugin_runtime: crate::plugin_registry::PluginRuntime,
}

impl Context<'_> {
    pub fn submit_task(&mut self, task: RuntimeTaskEvent) {
        if let Err(error) = self.foreground.task(task) {
            self.editor.set_error(error.to_string());
        }
    }

    pub fn submit_ui(&mut self, command: UiCommand) {
        if let Err(error) = self.foreground.ui(command) {
            self.editor.set_error(error.to_string());
        }
    }

    /// Push a new component onto the compositor.
    pub fn push_layer(&mut self, component: Box<dyn Component>) {
        self.callback
            .push(crate::compositor::PostAction::PushLayer(component));
    }

    /// Call `replace_or_push` on the Compositor.
    pub fn replace_or_push_layer<T: Component>(&mut self, id: &'static str, component: T) {
        self.callback
            .push(crate::compositor::PostAction::ReplaceOrPushLayer {
                id,
                layer: Box::new(component),
            });
    }

    #[inline]
    pub fn on_next_key(
        &mut self,
        on_next_key_callback: impl FnOnce(&mut Context, KeyEvent) + Send + 'static,
    ) {
        self.on_next_key_callback = Some((
            Box::new(on_next_key_callback),
            OnKeyCallbackKind::PseudoPending,
        ));
    }

    #[inline]
    pub fn on_next_key_fallback(
        &mut self,
        on_next_key_callback: impl FnOnce(&mut Context, KeyEvent) + Send + 'static,
    ) {
        self.on_next_key_callback =
            Some((Box::new(on_next_key_callback), OnKeyCallbackKind::Fallback));
    }

    #[inline]
    pub fn spawn_ui(
        &mut self,
        future: impl Future<Output = anyhow::Result<UiCommand>> + Send + 'static,
    ) {
        crate::runtime::ingress::spawn_ui_command_with_future(
            self.editor.work(),
            future,
            self.ingress.clone(),
        );
    }

    #[inline]
    pub fn spawn_task_event(
        &mut self,
        future: impl Future<Output = anyhow::Result<RuntimeTaskEvent>> + Send + 'static,
    ) {
        crate::runtime::ingress::spawn_task_event_with_future(
            self.editor.work(),
            future,
            self.ingress.clone(),
        );
    }

    pub fn reset_idle_timer(&self) {
        self.idle_reset.request_reset();
    }

    #[inline]
    pub fn exit_task_event(
        &mut self,
        future: impl Future<Output = anyhow::Result<RuntimeTaskEvent>> + Send + 'static,
    ) {
        crate::runtime::schedule_exit_task(self.exit_tasks, &self.exit_task_work, future);
    }

    pub fn compositor_context(&mut self) -> compositor::Context<'_> {
        compositor::Context::with_foreground(
            self.editor,
            self.exit_tasks,
            self.exit_task_work.clone(),
            self.notifier.clone(),
            self.ingress.clone(),
            self.idle_reset.clone(),
            self.plugin_runtime.clone(),
            self.foreground.clone(),
        )
    }

    /// Returns 1 if no explicit count was provided.
    #[inline]
    pub fn count(&self) -> usize {
        self.count.map_or(1, |v| v.get())
    }

    /// Execute an engine command through the registry.
    pub(super) fn execute_engine_command(&mut self, token: CommandToken) {
        use helix_modal::registry::{CharPendingResolution, CommandRef};

        let count = self.count();
        let register = self.register.take();

        let focus = self.editor.focused_view_id();
        let focused_view = self.editor.tree.get(focus);
        let view_id = focused_view.id;
        let doc_id = focused_view.doc;

        let Some(kind) = self.registry.resolve(token) else {
            log::warn!("engine command missing from registry: {}", token.as_str());
            return;
        };

        match kind {
            CommandRef::Motion(m) => {
                let movement = if self.editor.mode() == Mode::Select {
                    Movement::Extend
                } else {
                    Movement::Move
                };
                let motion = m
                    .make
                    .make(Some(NonZeroUsize::new(count).unwrap_or(NonZeroUsize::MIN)));
                motion(self.editor, view_id, doc_id, movement);
            }
            CommandRef::Operator(op) => {
                (op.execute)(self.editor, view_id, doc_id, register);
            }
            CommandRef::Action(a) => {
                (a.execute)(self.editor, view_id, doc_id, count, register);
            }
            CommandRef::TextObject(to) => {
                let obj_fn = (to.make)(count);
                obj_fn(
                    self.editor,
                    view_id,
                    doc_id,
                    helix_core::textobject::TextObject::Around,
                );
            }
            CommandRef::CharPending(cp) => {
                let movement = if self.editor.mode() == Mode::Select {
                    Movement::Extend
                } else {
                    Movement::Move
                };
                let command = cp.id;
                self.on_next_key(move |cx, event| {
                    if let Some(ch) = event.char() {
                        let Some(cp) = cx.registry.char_pending(command) else {
                            return;
                        };
                        match (cp.resolve)(ch, count) {
                            CharPendingResolution::Motion(motion) => {
                                cx.editor
                                    .apply_motion(move |ed| motion(ed, view_id, doc_id, movement));
                            }
                            CharPendingResolution::Action(action) => {
                                action(cx.editor, view_id, doc_id, register);
                            }
                        }
                    }
                });
            }
        }
    }

    /// Waits on all pending async UI work, then tries to flush all pending write
    /// operations for all documents.
    pub fn block_try_flush_writes(&mut self) -> anyhow::Result<()> {
        self.compositor_context().block_try_flush_writes()
    }
}

impl compositor::Context<'_> {
    pub fn command_context(
        &mut self,
        registry: std::sync::Arc<CommandRegistry>,
        register: Option<char>,
        count: Option<NonZeroUsize>,
    ) -> Context<'_> {
        Context {
            register,
            count,
            editor: self.editor,
            registry,
            notifier: self.notifier.clone(),
            callback: Vec::new(),
            on_next_key_callback: None,
            exit_tasks: self.exit_tasks,
            exit_task_work: self.exit_task_work.clone(),
            ingress: self.ingress.clone(),
            foreground: self.foreground.clone(),
            redraw: self.redraw.clone(),
            idle_reset: self.idle_reset.clone(),
            plugin_runtime: self.plugin_runtime.clone(),
        }
    }
}
