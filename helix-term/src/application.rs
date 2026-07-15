use arc_swap::{access::Map, ArcSwap};
use futures_util::{Stream, StreamExt};
use helix_core::{find_workspace, pos_at_coords, syntax, Range, Uri};
use helix_lsp::{
    lsp::{self as lsp_types},
    LanguageServerId, LspProgressMap,
};
use helix_view::{
    align_view,
    bench::log_run_phase,
    document::DocumentOpenError,
    editor::{ConfigEvent, EditorBuilder, EditorEvent},
    theme,
    tree::Layout,
    Align, Editor,
};
use tui::backend::Backend;

use crate::{
    args::Args,
    compositor::{Compositor, Event, FrameDeadlines},
    config::Config,
    handlers,
    keymap::Keymaps,
    runtime::{ExitTaskSet, PluginNotification},
    ui::{self, overlay::overlaid},
};
use helix_runtime::{FrameReceiver, FrameScheduler, FrameSource, Runtime, Work};

use crate::runtime::{RuntimeDelivery, RuntimeIngressReceiver};

use std::{
    borrow::Cow,
    io::{stdin, IsTerminal},
    sync::Arc,
};

use helix_plugin::PluginConfig;

#[cfg_attr(windows, allow(unused_imports))]
use anyhow::{Context, Error};

#[cfg(not(windows))]
use {signal_hook::consts::signal, signal_hook_tokio::Signals};
#[cfg(windows)]
type Signals = futures_util::stream::Empty<()>;

#[cfg(all(not(windows), not(feature = "integration")))]
use tui::backend::TerminaBackend;

#[cfg(all(windows, not(feature = "integration")))]
use tui::backend::CrosstermBackend;

#[cfg(feature = "integration")]
use tui::backend::TestBackend;

#[cfg(all(not(windows), not(feature = "integration")))]
type TerminalBackend = TerminaBackend;
#[cfg(all(windows, not(feature = "integration")))]
type TerminalBackend = CrosstermBackend<std::io::Stdout>;
#[cfg(feature = "integration")]
type TerminalBackend = TestBackend;

#[cfg(not(windows))]
type TerminalEvent = termina::Event;
#[cfg(windows)]
type TerminalEvent = crossterm::event::Event;

fn plugin_config(config: &Config) -> Result<PluginConfig, Error> {
    #[cfg(feature = "integration")]
    return Ok(config.plugins.clone());

    #[cfg(not(feature = "integration"))]
    let mut plugins = config.plugins.clone();
    #[cfg(not(feature = "integration"))]
    if plugins.enabled {
        plugins.hosts.insert(
            0,
            helix_plugin::PluginHostConfig {
                name: "local-lua".into(),
                command: std::env::current_exe()
                    .context("resolve current executable for plugin host")?,
                args: vec!["--plugin-host".into()],
                plugin_dirs: Vec::new(),
            },
        );
    }
    #[cfg(not(feature = "integration"))]
    Ok(plugins)
}

const SLOW_RENDER_LOG_THRESHOLD: std::time::Duration = std::time::Duration::from_millis(8);
const SLOW_REDRAW_LAG_THRESHOLD: std::time::Duration = std::time::Duration::from_millis(8);
const SLOW_LSP_EVENT_THRESHOLD: std::time::Duration = std::time::Duration::from_millis(4);
const FRAME_STARTUP: FrameSource = FrameSource::new("application.startup");
const FRAME_EDITOR: FrameSource = FrameSource::new("application.editor");
const FRAME_RUNTIME: FrameSource = FrameSource::new("application.runtime");
const FRAME_INPUT: FrameSource = FrameSource::new("application.input");
const FRAME_LSP: FrameSource = FrameSource::new("application.lsp");
const FRAME_DEBUGGER: FrameSource = FrameSource::new("application.debugger");
const FRAME_CONFIG: FrameSource = FrameSource::new("application.config");
const FRAME_ASSISTANT: FrameSource = FrameSource::new("application.assistant");
const FRAME_SAVE: FrameSource = FrameSource::new("application.save");
const FRAME_TIMER: FrameSource = FrameSource::new("application.timer");
const FRAME_EXIT_TASK: FrameSource = FrameSource::new("application.exit-task");
const FRAME_PRESENTER: FrameSource = FrameSource::new("application.presenter-resync");

type Terminal = ratatui_terminal::AppTerminal<TerminalBackend>;

mod assistant_events;
mod bench;
mod config;
mod dap_events;
mod lifecycle;
mod lsp;
mod lsp_events;
mod ratatui_terminal;
mod render_actor;
mod terminal;
mod terminal_presenter;

struct IngressState {
    tx: crate::runtime::RuntimeIngress,
    rx: RuntimeIngressReceiver,
    lsp_events: lsp_events::LspEvents,
    lsp_events_rx: lsp_events::LspEventReceiver,
    dap_events: dap_events::DapEvents,
    dap_events_rx: dap_events::DapEventReceiver,
    config_rx: helix_runtime::Receiver<ConfigEvent>,
    assistant_events_rx: assistant_events::AssistantEventReceiver,
    language_server_supervisor_rx:
        helix_runtime::Receiver<helix_view::editor::LanguageServerSupervisorEvent>,
    redraw_rx: FrameReceiver,
    idle_reset_rx: crate::runtime::IdleResetReceiver,
    idle_reset: crate::runtime::IdleResetHandle,
    after_writes: Vec<(Vec<helix_view::DocumentId>, crate::runtime::UiCommand)>,
}

struct TimerState {
    frame: DeadlineTimer,
    idle: DeadlineTimer,
}

struct DeadlineTimer {
    clock: helix_runtime::Clock,
    deadline: Option<std::time::Instant>,
    task: Option<helix_runtime::Task<()>>,
}

impl DeadlineTimer {
    fn unarmed(clock: helix_runtime::Clock) -> Self {
        Self {
            clock,
            deadline: None,
            task: None,
        }
    }

    fn after(clock: helix_runtime::Clock, duration: std::time::Duration) -> Self {
        let mut timer = Self::unarmed(clock);
        timer.arm_after(duration);
        timer
    }

    fn arm_after(&mut self, duration: std::time::Duration) {
        self.arm_at(self.clock.deadline_after(duration));
    }

    fn arm_at(&mut self, deadline: std::time::Instant) {
        if self.deadline == Some(deadline) {
            return;
        }
        let now = self.clock.now();
        self.deadline = Some(deadline);
        self.task = (deadline > now).then(|| self.clock.timer_at(deadline));
    }

    fn disarm(&mut self) {
        self.deadline = None;
        self.task = None;
    }

    fn deadline(&self) -> Option<std::time::Instant> {
        self.deadline
    }

    fn is_due(&self, now: std::time::Instant) -> bool {
        self.deadline.is_some_and(|deadline| now >= deadline)
    }

    async fn elapsed(&mut self) {
        if self.is_due(self.clock.now()) {
            return;
        }
        match &mut self.task {
            Some(task) => {
                let _ = task.await;
            }
            None => futures_util::future::pending().await,
        }
    }
}

struct LoopState {
    signals: Signals,
    /// Native shutdown channel (Windows: console ctrl; Unix: None, uses signal stream).
    shutdown_rx: Option<tokio::sync::mpsc::Receiver<()>>,
}

fn sync_editor_streams(
    editor: &mut Editor,
    lsp_events: &lsp_events::LspEvents,
    dap_events: &dap_events::DapEvents,
    work: Work,
) {
    let incoming = editor.take_lsp_incoming();
    if !incoming.is_empty() {
        lsp_events.attach(work.clone(), incoming);
    }
    let incoming = editor.take_debugger_incoming();
    if !incoming.is_empty() {
        dap_events.attach(work, incoming);
    }
}

struct ExitState {
    tasks: ExitTaskSet,
    work: Work,
}

struct TerminalState {
    theme_mode: Option<theme::Mode>,
    area: helix_view::graphics::Rect,
    supports_true_color: bool,
    resync: helix_runtime::PulseHandle<terminal_presenter::PresenterResync>,
    resync_rx: helix_runtime::PulseReceiver<terminal_presenter::PresenterResync>,
}

struct LanguageState {
    progress: LspProgressMap,
    diagnostics_generations: std::collections::HashMap<(LanguageServerId, Uri), u64>,
}

pub struct Application {
    compositor: Compositor,
    terminal: Option<Terminal>,
    renderer: Option<render_actor::RenderActor>,
    presenter: Option<terminal_presenter::TerminalPresenter>,
    pub editor: Editor,

    config: Arc<ArcSwap<Config>>,

    /// Shared async runtime (UI/work/block/clock domains).
    runtime: Runtime,
    ingress: IngressState,

    exit: ExitState,
    loop_state: LoopState,
    timers: TimerState,
    frames: FrameScheduler,
    ui_timers: std::collections::HashMap<helix_runtime::TimerId, helix_runtime::Task<()>>,
    terminal_state: TerminalState,
    language: LanguageState,
    foreground: crate::runtime::ForegroundEvents,
    plugin_runtime: crate::plugin_registry::PluginRuntime,
}

#[cfg(feature = "integration")]
fn setup_integration_logging() {
    let level = std::env::var("HELIX_LOG_LEVEL")
        .map(|lvl| lvl.parse().unwrap())
        .unwrap_or(log::LevelFilter::Info);

    // Separate file config so we can include year, month and day in file logs
    let _ = fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{} {} [{}] {}",
                chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f"),
                record.target(),
                record.level(),
                message
            ))
        })
        .level(level)
        .chain(std::io::stdout())
        .apply();
}

impl Application {
    async fn ensure_terminal_presenter(&mut self) -> std::io::Result<()> {
        if self.presenter.is_some() {
            return Ok(());
        }
        let terminal = self
            .terminal
            .take()
            .expect("application terminal may only be transferred once");
        let presenter = terminal_presenter::TerminalPresenter::spawn(
            terminal,
            self.terminal_state.resync.clone(),
        );
        let area = presenter.claim().await?;
        self.terminal_state.area = area;
        self.compositor.resize(area);
        self.renderer = Some(render_actor::RenderActor::spawn(
            self.runtime.work().clone(),
            self.runtime.block().clone(),
            presenter.handle(),
        ));
        self.presenter = Some(presenter);
        Ok(())
    }

    fn make_compositor_context<'a>(
        editor: &'a mut Editor,
        exit_tasks: &'a mut ExitTaskSet,
        exit_task_work: Work,
        notifier: crate::handlers::local::Notifier,
        ingress: crate::runtime::RuntimeIngress,
        idle_reset: crate::runtime::IdleResetHandle,
        plugin_runtime: crate::plugin_registry::PluginRuntime,
        foreground: crate::runtime::ForegroundEvents,
    ) -> crate::compositor::Context<'a> {
        crate::compositor::Context::with_foreground(
            editor,
            exit_tasks,
            exit_task_work,
            notifier,
            ingress,
            idle_reset,
            plugin_runtime,
            foreground,
        )
    }

    pub fn new(
        args: Args,
        config: Config,
        lang_loader: syntax::Loader,
        runtime: Runtime,
    ) -> Result<Self, Error> {
        #[cfg(feature = "integration")]
        setup_integration_logging();

        use helix_view::editor::Action;

        // Package migration and reconciliation has one owner. Complete it before any
        // runtime consumer captures the process-wide activation snapshot.
        if let Err(error) = helix_pkg::Store::open_default().receipts() {
            log::warn!("failed to reconcile package runtime state: {error}");
        }

        let theme_loader = theme::Loader::new(&[helix_loader::config_dir()])
            .with_runtime_assets(helix_loader::runtime_assets()?.clone());

        #[cfg(all(not(windows), not(feature = "integration")))]
        let backend = TerminaBackend::new((&config.editor).into())
            .context("failed to create terminal backend")?;
        #[cfg(all(windows, not(feature = "integration")))]
        let backend = CrosstermBackend::new(std::io::stdout(), (&config.editor).into());

        #[cfg(feature = "integration")]
        let backend = TestBackend::new(120, 150);

        let theme_mode = backend.get_theme_mode();
        let terminal = Terminal::new(backend)?;
        let area = terminal.size();
        let supports_true_color = terminal.backend().supports_true_color();
        let mut presenter_resync = helix_runtime::PulseGate::new();
        let mut compositor = Compositor::new(area);
        let config = Arc::new(ArcSwap::from_pointee(config));
        let (ingress_tx, ingress_rx) = crate::runtime::RuntimeIngress::channel(runtime.clone());
        let foreground = crate::runtime::ForegroundEvents::new();
        let handlers = handlers::setup(config.clone(), ingress_tx.clone(), runtime.clone());
        let mut editor = EditorBuilder::new(area, runtime.clone())
            .theme_loader(Arc::new(theme_loader))
            .language_loader(lang_loader)
            .config_access(Arc::new(Map::new(
                Arc::clone(&config),
                |config: &Config| &config.editor,
            )))
            .handlers(handlers)
            .build();
        editor
            .lifecycle()
            .set_error_reporter(crate::runtime::status_error_reporter(ingress_tx.clone()));
        crate::handlers::attach(
            &editor,
            &editor.handlers,
            ingress_tx.clone(),
            foreground.clone(),
        );
        editor.set_assistant_history_backend(helix_view::assistant::history::local_backend());
        editor.set_assistant_context_registry(helix_view::assistant::context::core_registry());
        crate::effect::refresh_assistant_agent_cache(&editor, ingress_tx.clone());
        let fff_root = find_workspace().0;
        if fff_root.exists() {
            let fff_config = editor.config().file_picker.clone();
            runtime
                .block()
                .spawn(move || crate::fff::prewarm(&fff_root, &fff_config))
                .detach();
        }
        let (lsp_events, lsp_events_rx) = lsp_events::LspEvents::channel();
        let lsp_incoming = editor.take_lsp_incoming();
        if !lsp_incoming.is_empty() {
            lsp_events.attach(runtime.work().clone(), lsp_incoming);
        }
        let (dap_events, dap_events_rx) = dap_events::DapEvents::channel();
        let debugger_incoming = editor.take_debugger_incoming();
        if !debugger_incoming.is_empty() {
            dap_events.attach(runtime.work().clone(), debugger_incoming);
        }
        let config_rx = editor.take_config_rx();
        let assistant_updates_rx = editor.take_assistant_updates_rx();
        let (assistant_events, assistant_events_rx) = assistant_events::AssistantEvents::channel();
        assistant_events.attach(runtime.work().clone(), assistant_updates_rx);
        let language_server_supervisor_rx = editor.take_language_server_supervisor_rx();
        let redraw_rx = editor.take_redraw_rx();
        let mut idle_reset_gate = crate::runtime::IdleResetGate::new();
        let idle_reset = idle_reset_gate.handle();
        let idle_reset_rx = idle_reset_gate.take_receiver();
        let idle_timeout = editor.config().idle_timeout;
        if editor.assistant_history_backend().is_some() {
            foreground.task(
                crate::runtime::RuntimeTaskEvent::BootstrapAssistantHistory {
                    scope: helix_view::assistant::layout::current_scope(),
                },
            )?;
        }
        // Initialize OS-native file watcher for auto-reload
        crate::handlers::auto_reload::setup_file_watcher(&mut editor);

        Self::load_configured_theme(&mut editor, &config.load(), supports_true_color, theme_mode);

        let keys = config.load().keys.clone();
        editor.frontend_mut().modal_keymaps = Arc::new(arc_swap::ArcSwap::from_pointee(
            crate::keymap::to_component_modal_keymaps(&config.load().keys),
        ));
        editor.frontend_mut().semantic_modal_keymaps = Arc::new(arc_swap::ArcSwap::from_pointee(
            crate::keymap::to_semantic_modal_keymaps(&config.load().keys),
        ));

        let modal_engines = Arc::new(helix_modal::ModalEngineFactory::with_builtins());
        modal_engines.install(&mut editor);
        let engine_config = config.load().editor.editing_engine;
        let editor_view = Box::new(ui::EditorView::from_modal_factory(
            Keymaps::new(keys),
            &modal_engines,
            engine_config,
        ));
        compositor.push(editor_view);

        let exit_task_work = runtime.work().clone();
        let exit_tasks = ExitTaskSet::new();

        if args.load_tutor {
            let tutor = helix_loader::runtime_assets()?.require_file("tutor")?;
            editor.open(&tutor.path, Action::VerticalSplit)?;
            // Unset path to prevent accidentally saving to the original tutor file.
            focused!(editor).1.set_path(None);
        } else if !args.files.is_empty() {
            let mut files_it = args.files.into_iter().peekable();

            // If the first file is a directory, skip it and open a picker
            if let Some((first, _)) = files_it.next_if(|(p, _)| p.is_dir()) {
                let picker = ui::file_picker(&editor, first, ingress_tx.clone());
                compositor.push(Box::new(overlaid(picker)));
            }

            // If there are any more files specified, open them
            if files_it.peek().is_some() {
                let mut nr_of_files = 0;
                for (file, pos) in files_it {
                    nr_of_files += 1;
                    if file.is_dir() {
                        return Err(anyhow::anyhow!(
                            "expected a path to file, but found a directory: {file:?}. (to open a directory pass it as first argument)"
                        ));
                    } else {
                        // If the user passes in either `--vsplit` or
                        // `--hsplit` as a command line argument, all the given
                        // files will be opened according to the selected
                        // option. If neither of those two arguments are passed
                        // in, just load the files normally.
                        let action = match args.split {
                            _ if nr_of_files == 1 => Action::VerticalSplit,
                            Some(Layout::Vertical) => Action::VerticalSplit,
                            Some(Layout::Horizontal) => Action::HorizontalSplit,
                            None => Action::Load,
                        };
                        let old_id = editor.document_id_by_path(&file);
                        let doc_id = match editor.open(&file, action) {
                            // Ignore irregular files during application init.
                            Err(
                                DocumentOpenError::IrregularFile | DocumentOpenError::Directory,
                            ) => {
                                nr_of_files -= 1;
                                continue;
                            }
                            Err(err) => return Err(anyhow::anyhow!(err)),
                            // We can't open more than 1 buffer for 1 file, in this case we already have opened this file previously
                            Ok(doc_id) if old_id == Some(doc_id) => {
                                nr_of_files -= 1;
                                doc_id
                            }
                            Ok(doc_id) => {
                                ui::default_folding(&mut editor);
                                doc_id
                            }
                        };
                        // with Action::Load all documents have the same view
                        // NOTE: this isn't necessarily true anymore. If
                        // `--vsplit` or `--hsplit` are used, the file which is
                        // opened last is focused on.
                        let view_id = editor.focused_view_id();
                        let doc = doc_mut!(editor, &doc_id);
                        let selection = pos
                            .into_iter()
                            .map(|coords| {
                                Range::point(pos_at_coords(doc.text().slice(..), coords, true))
                            })
                            .collect();
                        doc.set_selection(view_id, selection);
                    }
                }

                // if all files were invalid, replace with empty buffer
                if nr_of_files == 0 {
                    editor.new_file(Action::VerticalSplit);
                } else {
                    editor.set_status(format!(
                        "Loaded {} file{}.",
                        nr_of_files,
                        if nr_of_files == 1 { "" } else { "s" } // avoid "Loaded 1 files." grammo
                    ));
                    // align the view to center after all files are loaded,
                    // does not affect views without pos since it is at the top
                    let (view_id, doc) = focused!(editor);
                    let view = view!(editor, view_id);
                    align_view(doc, view, Align::Center);
                }
            } else {
                editor.new_file(Action::VerticalSplit);
            }
        } else if stdin().is_terminal() || cfg!(feature = "integration") {
            editor.new_file_welcome();
        } else {
            editor
                .new_file_from_stdin(Action::VerticalSplit)
                .unwrap_or_else(|_| editor.new_file_welcome());
        }

        #[cfg(windows)]
        let signals = futures_util::stream::empty();
        #[cfg(not(windows))]
        let signals = Signals::new([
            signal::SIGTSTP,
            signal::SIGCONT,
            signal::SIGUSR1,
            signal::SIGTERM,
            signal::SIGINT,
            signal::SIGHUP, // terminal closed (macOS Terminal.app, Linux, SSH disconnect)
        ])
        .context("build signal handler")?;

        let plugin_config = plugin_config(&config.load())?;
        let plugin_runtime = crate::plugin_registry::spawn_plugin_runtime(
            &plugin_config,
            ingress_tx.clone(),
            foreground.clone(),
            editor.work().clone(),
        )?;

        #[cfg(windows)]
        let shutdown_rx = crate::shutdown::setup();
        #[cfg(not(windows))]
        let shutdown_rx = None;

        let redraw = editor.redraw_handle();
        let plugin_foreground = foreground.clone();
        editor.lifecycle().on_document_open(move |event| {
            plugin_foreground.plugin(PluginNotification::BufferOpen {
                document_id: event.doc,
                path: Some(event.path.clone()),
            })?;
            redraw.request_redraw();
            Ok(())
        });

        let plugin_foreground = foreground.clone();
        editor.lifecycle().on_document_change(move |event| {
            plugin_foreground.plugin(PluginNotification::BufferChanged {
                document_id: event.doc.id(),
            })?;
            Ok(())
        });

        let plugin_foreground = foreground.clone();
        editor.lifecycle().on_document_close(move |event| {
            plugin_foreground.plugin(PluginNotification::BufferClosed {
                document_id: event.doc.id(),
            })?;
            Ok(())
        });

        let plugin_foreground = foreground.clone();
        editor.lifecycle().on_selection_change(move |event| {
            plugin_foreground.plugin(PluginNotification::SelectionChange {
                document_id: event.doc.id(),
                path: event
                    .doc
                    .path()
                    .map(|p: &std::path::PathBuf| p.to_path_buf()),
            })?;
            Ok(())
        });

        let plugin_foreground = foreground.clone();
        editor.lifecycle().on_diagnostics_change(move |event| {
            plugin_foreground.plugin(PluginNotification::LspDiagnostic {
                document_id: event.doc,
                diagnostic_count: event.diagnostic_count,
            })?;
            Ok(())
        });

        // Fire DocumentOpened for already opened documents
        {
            use helix_plugin_api::events;
            use helix_plugin_editor::adapt;
            let docs: Vec<_> = editor
                .documents()
                .filter_map(|doc| {
                    Some((
                        doc.id(),
                        doc.path()?.to_path_buf(),
                        doc.language_name().map(|s| s.to_string()),
                    ))
                })
                .collect();

            for (doc_id, path, lang) in docs {
                let event = events::PluginEvent::DocumentOpened(events::DocumentOpenedEvent {
                    document: adapt::document_handle(doc_id),
                    path: Some(path.to_string_lossy().into_owned()),
                    language: lang,
                });
                plugin_runtime.notify_event(event);
            }
        }

        let timers = TimerState {
            frame: DeadlineTimer::unarmed(runtime.clock().clone()),
            idle: DeadlineTimer::after(runtime.clock().clone(), idle_timeout),
        };
        let app = Self {
            compositor,
            terminal: Some(terminal),
            renderer: None,
            presenter: None,
            editor,
            config,
            runtime,
            ingress: IngressState {
                tx: ingress_tx,
                rx: ingress_rx,
                lsp_events,
                lsp_events_rx,
                dap_events,
                dap_events_rx,
                config_rx,
                assistant_events_rx,
                language_server_supervisor_rx,
                redraw_rx,
                idle_reset_rx,
                idle_reset,
                after_writes: Vec::new(),
            },
            exit: ExitState {
                tasks: exit_tasks,
                work: exit_task_work,
            },
            loop_state: LoopState {
                signals,
                shutdown_rx,
            },
            timers,
            frames: FrameScheduler::default(),
            ui_timers: std::collections::HashMap::new(),
            terminal_state: TerminalState {
                theme_mode,
                area,
                supports_true_color,
                resync: presenter_resync.handle(),
                resync_rx: presenter_resync.take_receiver(),
            },
            language: LanguageState {
                progress: LspProgressMap::new(),
                diagnostics_generations: std::collections::HashMap::new(),
            },
            foreground,
            plugin_runtime,
        };

        Ok(app)
    }

    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    /// Clone of the typed ingress for deliveries into the main loop.
    pub fn ingress_sender(&self) -> crate::runtime::RuntimeIngress {
        self.ingress.tx.clone()
    }

    fn invalidate(&mut self, source: FrameSource) {
        self.frames.invalidate(source);
        self.arm_frame_timer();
    }

    #[inline]
    fn queue_redraw(&mut self) {
        self.invalidate(FRAME_RUNTIME);
    }

    fn arm_frame_timer(&mut self) {
        let now = self.runtime.clock().now();
        if let Some(deadline) = self.frames.next_deadline(now) {
            self.timers.frame.arm_at(deadline);
        } else {
            self.timers.frame.disarm();
        }
    }

    fn handle_runtime_status(&mut self, message: String, severity: helix_view::editor::Severity) {
        self.editor.status_msg = Some((Cow::Owned(message), severity));
    }

    fn handle_runtime_timer(&mut self, id: helix_runtime::TimerId) {
        self.ui_timers.remove(&id);
        log::trace!("runtime timer fired: {:?}", id);
    }

    fn handle_runtime_task(&mut self, task: crate::runtime::RuntimeTaskEvent) {
        let task = match task {
            crate::runtime::RuntimeTaskEvent::ApplyConfigReload(prepared) => {
                self.apply_prepared_config_reload(prepared);
                return;
            }
            crate::runtime::RuntimeTaskEvent::ConfigReloadFailed { request, message } => {
                if request == self.editor.config_gen {
                    self.editor.set_error(message);
                }
                return;
            }
            crate::runtime::RuntimeTaskEvent::ApplyPreparedLspDiagnostics {
                server_id,
                uri,
                generation,
                prepared,
            } => {
                self.apply_prepared_lsp_diagnostics(server_id, uri, generation, prepared);
                return;
            }
            task => task,
        };
        if let crate::runtime::RuntimeTaskEvent::PkgEvent(event) = &task {
            if let Some(editor_view) = self.compositor.find::<ui::EditorView>() {
                editor_view.pkg_progress_mut().apply(event);
            }
            if let Some(manager) = self
                .compositor
                .find_id::<ui::overlay::Overlay<ui::pkg::PkgManager>>(ui::pkg::ID)
            {
                manager.content.apply_progress_event(event);
            }
        }
        if let crate::runtime::RuntimeTaskEvent::PkgOperationFinished(outcome) = &task {
            if let Some(manager) = self
                .compositor
                .find_id::<ui::overlay::Overlay<ui::pkg::PkgManager>>(ui::pkg::ID)
            {
                manager
                    .content
                    .apply_operation_outcome(&self.editor, outcome);
            }
        }
        let ingress = self.ingress().tx.clone();
        crate::effect::apply_runtime_task_event(
            &mut self.editor,
            ingress,
            self.foreground.clone(),
            self.plugin_runtime.clone(),
            task,
        );
    }

    fn handle_runtime_assistant_permission(
        &mut self,
        thread: helix_view::assistant::thread::Id,
        request: helix_view::assistant::permission::RequestId,
        decision: helix_view::assistant::permission::Decision,
    ) {
        let effects = self
            .editor
            .resolve_assistant_permission(thread, request, decision);
        self.editor.apply_assistant_effects(effects);
    }

    fn handle_runtime_ui_command(&mut self, cmd: crate::runtime::UiCommand) {
        let mut cmd = cmd;
        loop {
            match cmd {
                crate::runtime::UiCommand::AfterWrites { documents, command }
                    if self.write_barrier_pending() =>
                {
                    self.ingress.after_writes.push((documents, *command));
                    return;
                }
                crate::runtime::UiCommand::AfterWrites { documents, command } => {
                    if documents.iter().any(|document| {
                        self.editor
                            .document(*document)
                            .is_some_and(helix_view::Document::is_modified)
                    }) {
                        self.editor.set_error(
                            "File operation cancelled because a document could not be saved",
                        );
                        return;
                    }
                    cmd = *command;
                }
                command => {
                    cmd = command;
                    break;
                }
            }
        }
        let notifier = crate::handlers::local::Notifier {
            redraw: self.editor.redraw_handle(),
            plugin_events: self.ingress.tx.clone().into(),
        };
        let mut context = Self::make_compositor_context(
            &mut self.editor,
            &mut self.exit.tasks,
            self.exit.work.clone(),
            notifier,
            self.ingress.tx.clone(),
            self.ingress.idle_reset.clone(),
            self.plugin_runtime.clone(),
            self.foreground.clone(),
        );
        crate::runtime::apply_ui_command(&mut self.compositor, &mut context, cmd);
    }

    fn drain_foreground(&mut self) {
        while let Some(delivery) = self.foreground.pop() {
            self.handle_runtime_delivery(delivery);
            if self.editor.should_close() {
                break;
            }
        }
    }

    fn write_barrier_pending(&self) -> bool {
        self.editor.has_pending_writes() || !self.exit.tasks.is_empty()
    }

    fn service_after_writes(&mut self) {
        if self.write_barrier_pending() || self.ingress.after_writes.is_empty() {
            return;
        }
        let commands = std::mem::take(&mut self.ingress.after_writes);
        for (documents, command) in commands {
            if documents.iter().any(|document| {
                self.editor
                    .document(*document)
                    .is_some_and(helix_view::Document::is_modified)
            }) {
                self.editor
                    .set_error("Operation cancelled because a document could not be saved");
            } else {
                self.handle_runtime_ui_command(command);
                if self.editor.should_close() {
                    break;
                }
            }
        }
    }

    fn handle_runtime_delivery(&mut self, delivery: RuntimeDelivery) {
        if let RuntimeDelivery::Ui(crate::runtime::UiCommand::Picker(cmd)) = &delivery {
            log::info!(
                target: crate::ui::picker::PICKER_TRACE_TARGET,
                "phase=runtime_event event=Ui::Picker command={cmd:?}",
            );
        }
        match delivery {
            RuntimeDelivery::Status { message, severity } => {
                self.handle_runtime_status(message, severity);
            }
            RuntimeDelivery::Timer(id) => {
                self.handle_runtime_timer(id);
            }
            RuntimeDelivery::Task(task) => {
                self.handle_runtime_task(*task);
            }
            RuntimeDelivery::AssistantPermissionResolved {
                thread,
                request,
                decision,
            } => {
                self.handle_runtime_assistant_permission(thread, request, decision);
            }
            RuntimeDelivery::Ui(cmd) => {
                self.handle_runtime_ui_command(cmd);
            }
            RuntimeDelivery::Plugin(notification) => {
                if let Some(event) =
                    crate::effect::plugin::notification_to_event(&notification, &self.editor)
                {
                    self.plugin_runtime.notify_event(event);
                }
            }
        }
        self.invalidate(FRAME_RUNTIME);
    }

    /// Schedule UI timer requests collected during compositor render via [`UiHost::request_timer`](crate::host::UiHost::request_timer).
    fn schedule_pending_timers(&mut self) {
        let timers = self.compositor.take_pending_timers();
        if timers.is_empty() {
            return;
        }
        let work = self.runtime.work().clone();
        let clock = self.runtime.clock().clone();
        let ingress = self.ingress().tx.clone();
        for (id, after) in timers {
            let ingress = ingress.clone();
            let timer_task = clock.timer(after);
            let task = work.spawn(async move {
                if timer_task.await.is_ok() {
                    ingress.send_timer(id).await;
                }
            });
            self.ui_timers.insert(id, task);
        }
    }

    async fn render_frame(&mut self, generation: helix_runtime::FrameGeneration) -> FrameDeadlines {
        let t0 = std::time::Instant::now();
        if log::log_enabled!(
            target: crate::ui::picker::PICKER_TRACE_TARGET,
            log::Level::Trace
        ) {
            let focused_doc_path = self
                .editor
                .focused_document()
                .and_then(|doc| doc.path())
                .map(|path| path.display().to_string().replace('\\', "/"))
                .unwrap_or_else(|| String::from("<scratch>"));
            log::trace!(
                target: crate::ui::picker::PICKER_TRACE_TARGET,
                "phase=app_render_start redraw_pending={} full_redraw={} focused_view={:?} focused_doc={:?} focused_path={} documents={} component_documents={}",
                self.editor.is_redraw_pending(),
                self.compositor.full_redraw,
                self.editor.focused_view_id(),
                self.editor.focused_document_id(),
                focused_doc_path,
                self.editor.document_count(),
                self.editor.component_docs.len(),
            );
        }
        let ingress = self.ingress().tx.clone();
        let idle_reset = self.ingress().idle_reset.clone();

        self.editor.pause_assistant_follow_if_local_change();

        let full_redraw = std::mem::take(&mut self.compositor.full_redraw);

        let frame_setup_start = std::time::Instant::now();
        let redraw = self.editor.redraw_handle();
        let notifier = crate::handlers::local::Notifier {
            redraw: redraw.clone(),
            plugin_events: self.ingress().tx.clone().into(),
        };
        let mut cx = Self::make_compositor_context(
            &mut self.editor,
            &mut self.exit.tasks,
            self.exit.work.clone(),
            notifier,
            ingress,
            idle_reset,
            self.plugin_runtime.clone(),
            self.foreground.clone(),
        );

        cx.editor.clear_redraw_request();
        let frame_setup_elapsed = frame_setup_start.elapsed();
        log_run_phase("render_setup", "frame_state", frame_setup_elapsed, || {
            format!("needs_redraw_reset={}", !cx.editor.is_redraw_pending())
        });

        let area = self.terminal_state.area;

        let t1 = std::time::Instant::now(); // setup done

        let surface = self
            .renderer
            .as_ref()
            .expect("render actor must be running while rendering")
            .take_surface(area);

        let frame_preparation = self.compositor.prepare_frame(area, &mut cx);
        self.schedule_pending_timers();
        let render_done = std::time::Instant::now();
        log_run_phase("render", "compositor_render_only", render_done - t1, || {
            format!("area={}x{}", area.width, area.height)
        });
        let cursor_start = std::time::Instant::now();
        let (pos, kind) = self.compositor.cursor(area, &self.editor);
        let cursor_elapsed = cursor_start.elapsed();
        log_run_phase("render", "cursor_total", cursor_elapsed, || {
            format!("cursor_pos_present={} cursor_kind={kind:?}", pos.is_some())
        });
        log::trace!(
            target: crate::ui::picker::PICKER_TRACE_TARGET,
            "phase=app_cursor_resolved pos={} kind={:?} elapsed_us={}",
            pos.map(|pos| format!("{},{}", pos.col, pos.row))
                .unwrap_or_else(|| String::from("<none>")),
            kind,
            cursor_elapsed.as_micros(),
        );
        self.editor.cursor_cache.reset();

        let t2 = std::time::Instant::now(); // compositor done
        log_run_phase("render", "compositor_total", t2 - t1, || {
            format!("area={}x{}", area.width, area.height)
        });

        let pos = pos.map(|pos| (pos.col as u16, pos.row as u16));
        let mut render_plan = crate::render::RenderPlan::seeded(area, surface);
        render_plan.extend([crate::render::RenderStep::paint(
            "frame_clear",
            |surface, cancellation| {
                if !cancellation.is_cancelled() {
                    surface.reset();
                }
            },
        )]);
        render_plan.extend(frame_preparation.render_steps);
        let submit_result = self
            .renderer
            .as_ref()
            .expect("render actor must be running while rendering")
            .submit(render_actor::PreparedFrame::new(
                generation,
                render_plan,
                pos,
                kind,
                full_redraw,
            ));
        if let Err(error) = submit_result {
            self.compositor.full_redraw = true;
            log::error!("failed to submit terminal frame: {error}");
        }

        let t3 = std::time::Instant::now(); // presenter submission done
        log_run_phase("render", "present_submit", t3 - t2, || {
            format!("cursor_pos_present={} cursor_kind={kind:?}", pos.is_some())
        });
        let total_elapsed = t3 - t0;
        let compositor_elapsed = t2 - t1;
        let submit_elapsed = t3 - t2;
        if total_elapsed >= SLOW_RENDER_LOG_THRESHOLD {
            log::info!(
                target: crate::ui::picker::PICKER_TRACE_TARGET,
                "phase=app_render_slow total_us={} compositor_us={} present_submit_us={} cursor_pos_present={} cursor_kind={:?}",
                total_elapsed.as_micros(),
                compositor_elapsed.as_micros(),
                submit_elapsed.as_micros(),
                pos.is_some(),
                kind,
            );
        }
        log::trace!(
            target: crate::ui::picker::PICKER_TRACE_TARGET,
            "phase=app_render_done total_us={} compositor_us={} present_submit_us={} cursor_pos_present={} cursor_kind={:?}",
            total_elapsed.as_micros(),
            compositor_elapsed.as_micros(),
            submit_elapsed.as_micros(),
            pos.is_some(),
            kind,
        );

        // Record render sub-phases when bench is active
        self.editor
            .record_bench_render_phases(t1 - t0, t2 - t1, t3 - t2);

        frame_preparation.deadlines
    }

    async fn render_if_due(&mut self) -> bool {
        let now = self.runtime.clock().now();
        if self.editor.is_redraw_pending() && !self.frames.has_pending_frame(now) {
            self.frames.invalidate(FRAME_EDITOR);
        }

        let Some(generation) = self.frames.begin_frame(now) else {
            self.arm_frame_timer();
            return false;
        };
        self.ensure_terminal_presenter()
            .await
            .expect("failed to start terminal presenter");
        let deadlines = self.render_frame(generation).await;
        self.frames.replace_deadlines(deadlines);
        self.frames.end_frame(generation);

        // Sync/render code may invalidate editor state while the current generation
        // is being drawn. Preserve that signal for exactly one following frame.
        if self.editor.is_redraw_pending() {
            self.frames.invalidate(FRAME_EDITOR);
        }
        self.arm_frame_timer();
        true
    }

    pub async fn event_loop<S>(&mut self, input_stream: &mut S)
    where
        S: Stream<Item = std::io::Result<TerminalEvent>> + Unpin,
    {
        self.invalidate(FRAME_STARTUP);
        self.render_if_due().await;
        loop {
            if !self.event_loop_until_idle(input_stream).await {
                break;
            }
        }
    }

    pub async fn event_loop_until_idle<S>(&mut self, input_stream: &mut S) -> bool
    where
        S: Stream<Item = std::io::Result<TerminalEvent>> + Unpin,
    {
        loop {
            if self.editor.should_close() {
                return false;
            }
            sync_editor_streams(
                &mut self.editor,
                &self.ingress.lsp_events,
                &self.ingress.dap_events,
                self.runtime.work().clone(),
            );

            use futures_util::future::{pending, Either};

            tokio::select! {
                biased;
                Some(signal) = self.loop_state.signals.next() => {
                    if !self.handle_signals(signal).await {
                        return false;
                    };
                }
                shutdown = match &mut self.loop_state.shutdown_rx {
                    Some(rx) => Either::Left(rx.recv()),
                    None => Either::Right(pending()),
                } => {
                    if shutdown.is_none() {
                        self.editor.exit_code = 1;
                        log::error!("native shutdown channel closed unexpectedly");
                    }
                    return false;
                },
                event = input_stream.next() => {
                    let Some(event) = event else {
                        self.editor.exit_code = 1;
                        log::error!("terminal input stream closed unexpectedly");
                        return false;
                    };
                    if !self.handle_terminal_events(event).await {
                        return false;
                    }
                }
                _ = self.timers.frame.elapsed() => {
                    let now = self.runtime.clock().now();
                    let lag = self
                        .timers
                        .frame
                        .deadline()
                        .and_then(|deadline| now.checked_duration_since(deadline))
                        .unwrap_or_default();
                    if lag >= SLOW_REDRAW_LAG_THRESHOLD {
                        log::info!(
                            target: crate::ui::picker::PICKER_TRACE_TARGET,
                            "phase=ui_redraw_late lag_us={} lsp_streams={} redraw_pending={}",
                            lag.as_micros(),
                            self.ingress.lsp_events.active_streams(),
                            self.frames.has_pending_frame(now),
                        );
                    }
                }
                Some(delivery) = self.ingress.rx.recv() => {
                    self.handle_runtime_delivery(delivery);
                    self.drain_foreground();
                }
                Some(result) = self.editor.recv_save_result() => {
                    self.handle_document_write(result);
                    self.invalidate(FRAME_SAVE);
                }
                Some(event) = self.ingress.language_server_supervisor_rx.recv() => {
                    self.editor.handle_language_server_supervisor_event(event);
                    sync_editor_streams(
                        &mut self.editor,
                        &self.ingress.lsp_events,
                        &self.ingress.dap_events,
                        self.runtime.work().clone(),
                    );
                    self.invalidate(FRAME_LSP);
                }
                Some(event) = self.ingress.lsp_events_rx.recv() => {
                    self.handle_language_server_message(event.event, event.server_id);
                    self.invalidate(FRAME_LSP);
                }
                Some(event) = self.ingress.dap_events_rx.recv() => {
                    let needs_render = crate::effect::dap::handle_message(
                        &mut self.editor,
                        self.ingress.tx.clone(),
                        event.client_id,
                        event.event,
                    );
                    if needs_render {
                        self.invalidate(FRAME_DEBUGGER);
                    }
                }
                Some(config_event) = self.ingress.config_rx.recv() => {
                    self.handle_config_events(config_event);
                    self.invalidate(FRAME_CONFIG);
                }
                Some(update) = self.ingress.assistant_events_rx.recv() => {
                    self.handle_assistant_update(update);
                    self.invalidate(FRAME_ASSISTANT);
                }
                Some(_request) = self.ingress.redraw_rx.recv() => {
                    self.queue_redraw();
                }
                Some(_request) = self.ingress.idle_reset_rx.recv() => {
                    let timeout = self.editor.config().idle_timeout;
                    self.timers.idle.arm_after(timeout);
                }
                Some(_request) = self.terminal_state.resync_rx.recv() => {
                    self.compositor.full_redraw = true;
                    self.invalidate(FRAME_PRESENTER);
                }
                _ = self.timers.idle.elapsed() => {
                    self.timers.idle.disarm();
                    self.handle_idle_timeout().await;

                    #[cfg(feature = "integration")]
                    {
                        if self.exit.tasks.is_empty() && !self.editor.has_pending_writes() {
                            return true;
                        }
                    }
                }
                Some(res) = self.exit.tasks.next() => {
                    let ingress = self.ingress().tx.clone();
                    if let Err(err) = crate::runtime::apply_exit_task(
                        &mut self.editor,
                        ingress,
                        self.foreground.clone(),
                        self.plugin_runtime.clone(),
                        res,
                    ) {
                        self.editor.set_error(format!("Async task failed: {}", err));
                    }
                    self.invalidate(FRAME_EXIT_TASK);
                }
            }

            self.service_after_writes();
            self.drain_foreground();

            if self.editor.should_close() {
                return false;
            }

            if !self.editor.has_active_bench() {
                self.render_if_due().await;
            }

            // for integration tests only, reset the idle timer after every
            // event to signal when test events are done processing
            #[cfg(feature = "integration")]
            {
                let timeout = self.editor.config().idle_timeout;
                self.timers.idle.arm_after(timeout);
            }

            if self.editor.has_active_bench() {
                self.bench_run_loop(input_stream).await;
            }
        }
    }

    #[inline(always)]
    pub async fn handle_editor_event(&mut self, event: EditorEvent) -> bool {
        log::debug!("received editor event: {:?}", event);

        match event {
            EditorEvent::CursorMoved
            | EditorEvent::Scrolled
            | EditorEvent::Edited
            | EditorEvent::BufferSwitched => {}
            EditorEvent::Redraw => {
                self.queue_redraw();
            }
        }

        false
    }

    pub async fn run<S>(&mut self, input_stream: &mut S) -> Result<i32, Error>
    where
        S: Stream<Item = std::io::Result<TerminalEvent>> + Unpin,
    {
        self.ensure_terminal_presenter().await?;

        self.event_loop(input_stream).await;
        self.plugin_runtime.shutdown().await;
        let close_errs = self.close().await;

        self.presenter
            .take()
            .expect("terminal presenter must exist during shutdown")
            .shutdown()
            .await?;

        for err in close_errs {
            self.editor.exit_code = 1;
            eprintln!("Error: {}", err);
        }

        Ok(self.editor.exit_code)
    }
}

impl Application {
    fn ingress(&self) -> &IngressState {
        &self.ingress
    }
}

impl ui::menu::Item for lsp_types::MessageActionItem {
    type Data = ();
    fn format(&self, _data: &Self::Data) -> ui::menu::Row<'_> {
        self.title.as_str().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_view::graphics::Rect;

    #[cfg(not(windows))]
    fn empty_signals() -> Signals {
        Signals::new([signal::SIGTERM]).expect("signals")
    }

    #[cfg(windows)]
    fn empty_signals() -> Signals {
        futures_util::stream::empty()
    }

    #[test]
    fn deadline_timer_spawns_only_for_future_deadlines() {
        let tokio = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = tokio.enter();
        let runtime = Runtime::new(tokio.handle().clone());
        let clock = runtime.clock().clone();
        let now = clock.now();
        let mut timer = DeadlineTimer::unarmed(clock);

        timer.arm_at(now);
        assert!(timer.is_due(timer.clock.now()));
        assert!(timer.task.is_none());

        timer.arm_at(now + std::time::Duration::from_secs(1));
        assert!(timer.task.is_some());

        timer.disarm();
        assert!(timer.deadline.is_none());
        assert!(timer.task.is_none());
    }

    #[test]
    fn domain_adapters_pick_up_streams_registered_after_initial_take() {
        let tokio = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = tokio.enter();
        let runtime = Runtime::new(tokio.handle().clone());
        let mut editor = EditorBuilder::new(Rect::new(0, 0, 80, 24), runtime.clone()).build();
        let (lsp_events, _lsp_events_rx) = lsp_events::LspEvents::channel();
        let (dap_events, _dap_events_rx) = dap_events::DapEvents::channel();

        let _loop_state = LoopState {
            signals: empty_signals(),
            shutdown_rx: None,
        };
        let _ = editor.take_lsp_incoming();
        let _ = editor.take_debugger_incoming();

        let (_lsp_tx, lsp_rx) = helix_runtime::channel(1);
        editor.language_servers.incoming.push(lsp_rx);
        let (_dap_tx, dap_rx) = helix_runtime::channel(1);
        editor.debug_adapters.incoming.push(dap_rx);

        assert_eq!(lsp_events.active_streams(), 0);
        assert_eq!(dap_events.active_streams(), 0);

        sync_editor_streams(
            &mut editor,
            &lsp_events,
            &dap_events,
            runtime.work().clone(),
        );

        assert_eq!(lsp_events.active_streams(), 1);
        assert_eq!(dap_events.active_streams(), 1);
    }
}
