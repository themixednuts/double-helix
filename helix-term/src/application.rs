use arc_swap::{access::Map, ArcSwap};
use futures_util::{Stream, StreamExt};
#[cfg(not(feature = "integration"))]
use helix_core::find_workspace;
use helix_core::{pos_at_coords, syntax, Range, Uri};
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
const MAX_PENDING_COLLABORATION_DIAGNOSTICS_BYTES: usize = 16 * 1024 * 1024;
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
const FRAME_REMOTE: FrameSource = FrameSource::new("application.remote");

enum FramePipelineReady {}

fn should_defer_frame(frames: &FrameScheduler, pipeline_saturated: bool) -> bool {
    pipeline_saturated && !frames.is_invalidated(FRAME_INPUT)
}

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
    after_document_mutations: Vec<crate::runtime::UiCommand>,
    after_writes: Vec<(Vec<helix_view::DocumentId>, crate::runtime::UiCommand)>,
}

struct TimerState {
    frame: DeadlineTimer,
    idle: DeadlineTimer,
    host_language_servers: DeadlineTimer,
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
    pipeline_ready: helix_runtime::PulseHandle<FramePipelineReady>,
    pipeline_ready_rx: helix_runtime::PulseReceiver<FramePipelineReady>,
}

struct LanguageState {
    progress: LspProgressMap,
    diagnostics_generations: std::collections::HashMap<(LanguageServerId, Uri), u64>,
}

struct CollaborationApplicationSession {
    host: Option<helix_collab::HostHandle>,
    hosted: Option<crate::runtime::collaboration::HostedProject>,
    handle: helix_collab::GuestSessionHandle,
    updates: helix_runtime::Task<()>,
    language_servers: Option<helix_runtime::Task<()>>,
    previous_workspace_backend: Option<helix_view::editor::WorkspaceBackend>,
    pending_invitation: Option<helix_collab::ConnectCode>,
    bootstrap_pending: std::collections::HashSet<helix_view::DocumentId>,
    bootstrap_failed: std::collections::HashSet<helix_view::DocumentId>,
    host_bindings_pending: std::collections::HashSet<helix_view::DocumentId>,
    pending_language_servers: Vec<PendingHostLanguageServerRequest>,
    pending_diagnostics:
        std::collections::HashMap<(helix_workspace::WorkspacePath, String), Vec<u8>>,
    pending_diagnostics_bytes: usize,
}

#[derive(Debug)]
struct PendingHostLanguageServerRequest {
    request: helix_collab::HostLanguageServerRequest,
    document: helix_view::DocumentId,
    deadline: std::time::Instant,
}

fn host_language_server_error(
    code: i64,
    message: impl Into<String>,
) -> helix_collab::LanguageServerResponse {
    helix_collab::LanguageServerResponse {
        result: Err(helix_collab::LanguageServerError {
            code,
            message: message.into(),
            data: None,
        }),
    }
}

fn host_language_server_result(
    result: helix_lsp::Result<serde_json::Value>,
) -> helix_collab::LanguageServerResponse {
    match result {
        Ok(value) => match serde_json::to_vec(&value) {
            Ok(bytes) if bytes.len() <= helix_collab::MAX_LANGUAGE_SERVER_PAYLOAD_BYTES => {
                helix_collab::LanguageServerResponse {
                    result: Ok(bytes.into()),
                }
            }
            Ok(_) => host_language_server_error(-32603, "language-server response is too large"),
            Err(error) => {
                log::warn!("failed to serialize hosted language-server response: {error}");
                host_language_server_error(-32603, "host language-server response was invalid")
            }
        },
        Err(helix_lsp::Error::Rpc(error)) => {
            let data = error.data.and_then(|data| {
                serde_json::to_vec(&data)
                    .ok()
                    .filter(|data| data.len() <= helix_collab::MAX_LANGUAGE_SERVER_PAYLOAD_BYTES)
                    .map(Into::into)
            });
            helix_collab::LanguageServerResponse {
                result: Err(helix_collab::LanguageServerError {
                    code: error.code.code(),
                    message: error.message,
                    data,
                }),
            }
        }
        Err(error) => {
            log::warn!("hosted language-server request failed: {error}");
            host_language_server_error(-32603, "host language-server request failed")
        }
    }
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
    remote: Option<RemoteApplicationSession>,
    collaboration: Option<CollaborationApplicationSession>,
    collaboration_shutdowns: Vec<helix_runtime::Task<()>>,
}

pub struct RemoteApplicationSession {
    pub transport: helix_remote::ssh::SshSession,
    pub workspace: Arc<helix_remote::backend::RemoteWorkspaceClient>,
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
            self.terminal_state.pipeline_ready.clone(),
        );
        let area = presenter.claim().await?;
        self.terminal_state.area = area;
        self.compositor.resize(area);
        self.renderer = Some(render_actor::RenderActor::spawn(
            self.runtime.work().clone(),
            self.runtime.block().clone(),
            presenter.handle(),
            self.terminal_state.pipeline_ready.clone(),
        ));
        self.presenter = Some(presenter);
        Ok(())
    }

    pub fn new(
        args: Args,
        config: Config,
        lang_loader: syntax::Loader,
        runtime: Runtime,
    ) -> Result<Self, Error> {
        Self::new_with_remote(args, config, lang_loader, runtime, None)
    }

    pub fn new_with_remote(
        args: Args,
        config: Config,
        lang_loader: syntax::Loader,
        runtime: Runtime,
        remote: Option<RemoteApplicationSession>,
    ) -> Result<Self, Error> {
        #[cfg(feature = "integration")]
        setup_integration_logging();

        use helix_view::editor::Action;

        #[cfg(not(feature = "integration"))]
        {
            // Package migration and reconciliation has one owner. Complete it before any
            // runtime consumer captures the process-wide activation snapshot.
            if let Err(error) = helix_pkg::Store::open_default().receipts() {
                log::warn!("failed to reconcile package runtime state: {error}");
            }
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
        let mut pipeline_ready = helix_runtime::PulseGate::new();
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
        if let Some(remote) = &remote {
            editor.set_workspace_backend(helix_view::editor::WorkspaceBackend::Remote(
                remote.workspace.clone(),
            ));
        }
        editor
            .lifecycle()
            .set_error_reporter(crate::runtime::status_error_reporter(ingress_tx.clone()));
        crate::handlers::attach(
            &editor,
            &editor.handlers,
            ingress_tx.clone(),
            foreground.clone(),
        );
        #[cfg(not(feature = "integration"))]
        editor.set_assistant_history_backend(helix_view::assistant::history::local_backend());
        editor.set_assistant_context_registry(helix_view::assistant::context::core_registry());
        #[cfg(not(feature = "integration"))]
        {
            crate::effect::refresh_assistant_agent_cache(&editor, ingress_tx.clone());
            let fff_root = find_workspace().0;
            if remote.is_none() && fff_root.exists() {
                let fff_config = editor.config().file_picker.clone();
                runtime
                    .block()
                    .spawn(move || crate::fff::prewarm(&fff_root, &fff_config))
                    .detach();
            }
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
        #[cfg(not(feature = "integration"))]
        if editor.assistant_history_backend().is_some() {
            foreground.task(
                crate::runtime::RuntimeTaskEvent::BootstrapAssistantHistory {
                    scope: helix_view::assistant::layout::current_scope(),
                },
            )?;
        }
        // Initialize OS-native file watcher for auto-reload
        #[cfg(not(feature = "integration"))]
        if remote.is_none() {
            crate::handlers::auto_reload::setup_file_watcher(&mut editor);
        }

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
                let picker = ui::file_picker(&editor, first.into(), ingress_tx.clone())?;
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
        let collaboration_ingress = ingress_tx.clone();
        editor.lifecycle().on_document_open(move |event| {
            plugin_foreground.plugin(PluginNotification::BufferOpen {
                document_id: event.doc,
                resource: Some(event.location.to_string()),
            })?;
            if event.editor.collaboration.is_hosting() {
                let _ = collaboration_ingress.task(
                    crate::runtime::RuntimeTaskEvent::CollaborationHostDocumentOpened {
                        document: event.doc,
                    },
                );
            }
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
        let collaboration_ingress = ingress_tx.clone();
        editor.lifecycle().on_document_close(move |event| {
            let document = event.doc.id();
            plugin_foreground.plugin(PluginNotification::BufferClosed {
                document_id: document,
            })?;
            event.editor.unbind_collaboration_document(document);
            let _ = collaboration_ingress.task(
                crate::runtime::RuntimeTaskEvent::CollaborationHostDocumentClosed { document },
            );
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
            host_language_servers: DeadlineTimer::unarmed(runtime.clock().clone()),
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
                after_document_mutations: Vec::new(),
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
                pipeline_ready: pipeline_ready.handle(),
                pipeline_ready_rx: pipeline_ready.take_receiver(),
            },
            language: LanguageState {
                progress: LspProgressMap::new(),
                diagnostics_generations: std::collections::HashMap::new(),
            },
            foreground,
            plugin_runtime,
            remote,
            collaboration: None,
            collaboration_shutdowns: Vec::new(),
        };

        Ok(app)
    }

    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    fn handle_remote_session_event(&mut self, event: helix_remote::ssh::SshSessionEvent) {
        use helix_remote::{client::ClientEvent, RemoteLogLevel, ServerEvent};

        match event {
            helix_remote::ssh::SshSessionEvent::Diagnostic(line) => {
                log::warn!(target: "helix_remote_ssh", "{line}");
            }
            helix_remote::ssh::SshSessionEvent::Exited(status) => {
                let message = format!("Remote SSH process exited with {status}");
                log::warn!(target: "helix_remote_ssh", "{message}");
            }
            helix_remote::ssh::SshSessionEvent::Reconnecting { attempt } => {
                let message = format!("Reconnecting remote workspace (attempt {attempt})");
                log::info!(target: "helix_remote_ssh", "{message}");
                if !self.editor.should_close() {
                    self.editor.notify_warning(message);
                }
            }
            helix_remote::ssh::SshSessionEvent::ReconnectFailed {
                attempt,
                error,
                retry_in,
            } => {
                log::warn!(
                    target: "helix_remote_ssh",
                    "remote reconnect attempt {attempt} failed: {error}; retrying in {retry_in:?}"
                );
            }
            helix_remote::ssh::SshSessionEvent::Reconnected => {
                log::info!(target: "helix_remote_ssh", "remote workspace reconnected");
                if !self.editor.should_close() {
                    self.editor.notify_info("Remote workspace reconnected");
                }
            }
            helix_remote::ssh::SshSessionEvent::Remote(ClientEvent::TransportLog(message)) => {
                log::debug!(target: "helix_remote", "{message}");
            }
            helix_remote::ssh::SshSessionEvent::Remote(ClientEvent::Remote(event)) => {
                let routed = self
                    .remote
                    .as_ref()
                    .is_some_and(|remote| remote.workspace.route_event(&event));
                match event {
                    ServerEvent::Log(remote) => match remote.level {
                        RemoteLogLevel::Error => {
                            log::error!(target: "helix_remote", "{}: {}", remote.target, remote.message)
                        }
                        RemoteLogLevel::Warn => {
                            log::warn!(target: "helix_remote", "{}: {}", remote.target, remote.message)
                        }
                        RemoteLogLevel::Info => {
                            log::info!(target: "helix_remote", "{}: {}", remote.target, remote.message)
                        }
                        RemoteLogLevel::Debug => {
                            log::debug!(target: "helix_remote", "{}: {}", remote.target, remote.message)
                        }
                        RemoteLogLevel::Trace => {
                            log::trace!(target: "helix_remote", "{}: {}", remote.target, remote.message)
                        }
                    },
                    ServerEvent::WorkspaceInvalidated { reason } => {
                        self.editor
                            .notify_error(format!("Remote workspace invalidated: {reason}"));
                    }
                    ServerEvent::SearchBatch(_) if routed => {}
                    ServerEvent::SearchBatch(_)
                    | ServerEvent::FileChanges(_)
                    | ServerEvent::ProcessOutput(_)
                    | ServerEvent::ProcessExited(_) => {
                        log::trace!(target: "helix_remote", "remote capability event received");
                    }
                }
            }
        }
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

    fn install_collaboration(&mut self, launch: crate::runtime::collaboration::Launch) {
        if self.collaboration.is_some() {
            self.editor
                .notify_warning("Leave the current collaboration session before starting another");
            return;
        }

        let crate::runtime::collaboration::Launch {
            mut session,
            host,
            invitation,
            hosted,
            language_servers,
        } = launch;
        let joined_project = host.is_none();
        let handle = session.handle();
        let previous_workspace_backend = joined_project.then(|| {
            std::mem::replace(
                &mut self.editor.workspace_backend,
                helix_view::editor::WorkspaceBackend::Collaboration(handle.clone()),
            )
        });
        self.editor
            .attach_collaboration_session(handle.clone(), !joined_project);
        let ingress = self.ingress.tx.clone();
        let updates = self.runtime.work().spawn(async move {
            while let Some(update) = session.next_update().await {
                if ingress.collaboration_update(update).await.is_err() {
                    break;
                }
            }
        });
        let language_servers = language_servers.map(|mut requests| {
            let ingress = self.ingress.tx.clone();
            self.runtime.work().spawn(async move {
                while let Some(request) = requests.recv().await {
                    if ingress
                        .send_task(
                            crate::runtime::RuntimeTaskEvent::CollaborationHostLanguageServerRequest(
                                request,
                            ),
                        )
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            })
        });
        self.collaboration = Some(CollaborationApplicationSession {
            host,
            hosted,
            handle,
            updates,
            language_servers,
            previous_workspace_backend,
            pending_invitation: invitation,
            bootstrap_pending: std::collections::HashSet::new(),
            bootstrap_failed: std::collections::HashSet::new(),
            host_bindings_pending: std::collections::HashSet::new(),
            pending_language_servers: Vec::new(),
            pending_diagnostics: std::collections::HashMap::new(),
            pending_diagnostics_bytes: 0,
        });
        self.editor.notify_info("Collaboration session connected");
        if joined_project {
            let root = helix_view::editor::WorkspaceDocumentPath::Collaboration {
                project: self
                    .collaboration
                    .as_ref()
                    .expect("installed collaboration disappeared")
                    .handle
                    .project()
                    .id,
                path: helix_remote::WorkspacePath::root(),
            };
            match crate::ui::file_picker(&self.editor, root, self.ingress.tx.clone()) {
                Ok(picker) => self
                    .compositor
                    .push(Box::new(crate::ui::overlay::overlaid(picker))),
                Err(error) => self
                    .editor
                    .set_error(format!("Failed to open collaboration file picker: {error}")),
            }
        } else {
            let documents = self.editor.document_ids().collect::<Vec<_>>();
            for document in documents {
                self.start_host_document_binding(document, true);
            }
            self.publish_host_invitation_if_ready();
        }
    }

    fn start_host_document_binding(
        &mut self,
        document: helix_view::DocumentId,
        bootstrap: bool,
    ) -> bool {
        let Some((project, handle, hosted)) = self.collaboration.as_ref().and_then(|session| {
            session.hosted.as_ref().map(|hosted| {
                (
                    session.handle.project().id,
                    session.handle.clone(),
                    hosted.clone(),
                )
            })
        }) else {
            return false;
        };
        let Some(path) = self
            .editor
            .document(document)
            .and_then(|doc| doc.location())
            .and_then(|location| hosted.document_path(location))
        else {
            return false;
        };
        if !self.editor.collaboration.begin_host_binding(document) {
            return false;
        }
        if bootstrap {
            self.collaboration
                .as_mut()
                .expect("collaboration disappeared while binding a host document")
                .bootstrap_pending
                .insert(document);
        }
        self.collaboration
            .as_mut()
            .expect("collaboration disappeared while binding a host document")
            .host_bindings_pending
            .insert(document);
        let ingress = self.ingress.tx.clone();
        self.runtime
            .work()
            .spawn(async move {
                let result = handle
                    .open(path.clone())
                    .await
                    .map_err(|error| error.to_string());
                let _ = ingress
                    .send_task(
                        crate::runtime::RuntimeTaskEvent::CollaborationHostBufferOpened {
                            project,
                            document,
                            path,
                            result,
                        },
                    )
                    .await;
            })
            .detach();
        true
    }

    fn finish_host_document_binding(
        &mut self,
        project: helix_collab::ProjectId,
        document: helix_view::DocumentId,
        path: helix_workspace::WorkspacePath,
        result: Result<helix_collab::OpenedBuffer, String>,
    ) {
        let Some((current_project, handle, hosted)) =
            self.collaboration.as_ref().and_then(|session| {
                session.hosted.as_ref().map(|hosted| {
                    (
                        session.handle.project().id,
                        session.handle.clone(),
                        hosted.clone(),
                    )
                })
            })
        else {
            return;
        };
        if current_project != project {
            return;
        }
        let was_bootstrap = self
            .collaboration
            .as_ref()
            .expect("collaboration disappeared while finishing a host binding")
            .bootstrap_pending
            .contains(&document);
        let current_path = self
            .editor
            .document(document)
            .and_then(|doc| doc.location())
            .and_then(|location| hosted.document_path(location));
        if current_path.as_ref() != Some(&path) {
            self.editor.collaboration.cancel_host_binding(document);
            if let Some(session) = self.collaboration.as_mut() {
                session.host_bindings_pending.remove(&document);
            }
            if current_path.is_some() {
                self.start_host_document_binding(document, was_bootstrap);
            } else if let Some(session) = self.collaboration.as_mut() {
                session.bootstrap_pending.remove(&document);
            }
            self.publish_host_invitation_if_ready();
            return;
        }
        match result {
            Ok(opened) => {
                let text = self
                    .editor
                    .document(document)
                    .expect("host document disappeared after its path was checked")
                    .text()
                    .to_string();
                if self
                    .editor
                    .bind_collaboration_buffer(document, opened.buffer, path)
                    && text != opened.text
                {
                    handle.queue_snapshot(opened.buffer, text);
                }
                if let Some(session) = self.collaboration.as_mut() {
                    session.bootstrap_failed.remove(&document);
                }
                let ingress = self.ingress.tx.clone();
                self.runtime
                    .work()
                    .spawn(async move {
                        let result = handle
                            .flush(opened.buffer)
                            .await
                            .map_err(|error| error.to_string());
                        let _ = ingress
                            .send_task(
                                crate::runtime::RuntimeTaskEvent::CollaborationHostBufferReady {
                                    project,
                                    document,
                                    result,
                                },
                            )
                            .await;
                    })
                    .detach();
                return;
            }
            Err(error) => {
                self.editor.collaboration.cancel_host_binding(document);
                if let Some(session) = self.collaboration.as_mut() {
                    session.host_bindings_pending.remove(&document);
                }
                if was_bootstrap {
                    let session = self
                        .collaboration
                        .as_mut()
                        .expect("collaboration disappeared while recording bootstrap failure");
                    session.bootstrap_pending.remove(&document);
                    session.bootstrap_failed.insert(document);
                }
                self.editor
                    .notify_error(format!("Could not share an open document: {error}"));
            }
        }
        self.publish_host_invitation_if_ready();
    }

    fn host_buffer_ready(
        &mut self,
        project: helix_collab::ProjectId,
        document: helix_view::DocumentId,
        result: Result<(), String>,
    ) {
        let Some(session) = self
            .collaboration
            .as_mut()
            .filter(|session| session.handle.project().id == project)
        else {
            return;
        };
        let was_bootstrap = session.bootstrap_pending.remove(&document);
        session.host_bindings_pending.remove(&document);
        match result {
            Ok(()) => {
                session.bootstrap_failed.remove(&document);
            }
            Err(error) => {
                if was_bootstrap {
                    session.bootstrap_failed.insert(document);
                }
                self.editor.unbind_collaboration_document(document);
                self.editor.notify_error(format!(
                    "Could not synchronize an open document for sharing: {error}"
                ));
            }
        }
        self.publish_host_invitation_if_ready();
        self.drive_host_language_server_requests();
    }

    fn host_document_closed(&mut self, document: helix_view::DocumentId) {
        if let Some(session) = self.collaboration.as_mut() {
            session.bootstrap_pending.remove(&document);
            session.bootstrap_failed.remove(&document);
            session.host_bindings_pending.remove(&document);
            let mut retained = Vec::with_capacity(session.pending_language_servers.len());
            for pending in session.pending_language_servers.drain(..) {
                if pending.document == document {
                    pending.request.respond(host_language_server_error(
                        -32603,
                        "host document closed before the language-server request completed",
                    ));
                } else {
                    retained.push(pending);
                }
            }
            session.pending_language_servers = retained;
        }
        self.arm_host_language_server_deadline();
        self.publish_host_invitation_if_ready();
    }

    fn handle_host_language_server_request(
        &mut self,
        request: helix_collab::HostLanguageServerRequest,
    ) {
        if request.is_canceled() {
            return;
        }
        let Some(hosted) = self
            .collaboration
            .as_ref()
            .and_then(|session| session.hosted.clone())
        else {
            request.respond(host_language_server_error(
                -32603,
                "collaboration host is unavailable",
            ));
            return;
        };
        let workspace_path = hosted.workspace_document_path(&request.path);
        if let Some(document) = self.editor.document_id_by_workspace_path(&workspace_path) {
            self.queue_host_language_server_request(request, document);
            return;
        }

        let work = self.editor.prepare_workspace_document_open(
            workspace_path,
            helix_view::editor::DocumentOpenRole::Background,
        );
        let block = self.runtime.block().clone();
        let ingress = self.ingress.tx.clone();
        self.runtime
            .work()
            .spawn(async move {
                let result = match work {
                    helix_view::editor::WorkspaceDocumentOpenWork::Local(work) => block
                        .spawn(move || {
                            work.execute()
                                .map(helix_view::editor::PreparedWorkspaceDocumentOpen::Local)
                        })
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|result| result.map_err(|error| error.to_string())),
                    helix_view::editor::WorkspaceDocumentOpenWork::Remote(work) => work
                        .execute(tokio_util::sync::CancellationToken::new(), false)
                        .await
                        .map(helix_view::editor::PreparedWorkspaceDocumentOpen::Remote)
                        .map_err(|error| error.to_string()),
                    helix_view::editor::WorkspaceDocumentOpenWork::Collaboration(_) => Err(
                        "a collaboration host cannot source documents from another collaboration"
                            .to_owned(),
                    ),
                    helix_view::editor::WorkspaceDocumentOpenWork::Failed { error, .. } => {
                        Err(error.to_string())
                    }
                };
                let _ = ingress
                    .send_task(
                        crate::runtime::RuntimeTaskEvent::CollaborationHostLanguageServerDocumentOpened {
                            request,
                            result: Box::new(result),
                        },
                    )
                    .await;
            })
            .detach();
    }

    fn finish_host_language_server_document_open(
        &mut self,
        mut request: helix_collab::HostLanguageServerRequest,
        result: Result<helix_view::editor::PreparedWorkspaceDocumentOpen, String>,
    ) {
        if request.is_canceled() {
            return;
        }
        let Some(hosted) = self
            .collaboration
            .as_ref()
            .and_then(|session| session.hosted.clone())
        else {
            request.respond(host_language_server_error(
                -32603,
                "collaboration host is unavailable",
            ));
            return;
        };
        let workspace_path = hosted.workspace_document_path(&request.path);
        if let Some(document) = self.editor.document_id_by_workspace_path(&workspace_path) {
            self.queue_host_language_server_request(request, document);
            return;
        }
        let mut prepared = match result {
            Ok(prepared) => prepared,
            Err(error) => {
                log::warn!(
                    "failed to open host document for collaboration language server: {error}"
                );
                request.respond(host_language_server_error(
                    -32603,
                    "host could not open the requested document",
                ));
                return;
            }
        };
        prepared.replace_initial_text(std::mem::take(&mut request.text));
        let document = self
            .editor
            .apply_prepared_workspace_document_open(prepared, helix_view::editor::Action::Load);
        if !self
            .editor
            .bind_collaboration_buffer(document, request.buffer, request.path.clone())
        {
            request.respond(host_language_server_error(
                -32603,
                "host document disappeared while binding collaboration state",
            ));
            return;
        }
        self.queue_host_language_server_request(request, document);
    }

    fn queue_host_language_server_request(
        &mut self,
        mut request: helix_collab::HostLanguageServerRequest,
        document: helix_view::DocumentId,
    ) {
        if request.is_canceled() {
            return;
        }
        request.text.clear();
        let Some(doc) = self.editor.document(document) else {
            request.respond(host_language_server_error(
                -32603,
                "host document is unavailable",
            ));
            return;
        };
        let configured = doc.language_configuration().is_some_and(|language| {
            language
                .language_servers
                .iter()
                .any(|server| server.name == request.server)
        });
        if !configured {
            request.respond(host_language_server_error(
                -32601,
                "language server is not configured for this document",
            ));
            return;
        }

        match self.editor.collaboration.buffer(document) {
            Some(buffer) if buffer != request.buffer => {
                request.respond(host_language_server_error(
                    -32603,
                    "host document is bound to a different collaboration buffer",
                ));
                return;
            }
            Some(_) => {}
            None => {
                self.start_host_document_binding(document, false);
            }
        }
        if self
            .editor
            .document(document)
            .and_then(|doc| doc.language_server_by_name(&request.server))
            .is_some()
            && !self
                .collaboration
                .as_ref()
                .is_some_and(|session| session.host_bindings_pending.contains(&document))
        {
            self.execute_host_language_server_request(request, document);
            return;
        }
        self.editor.refresh_language_servers(document);
        let Some(session) = self.collaboration.as_mut() else {
            request.respond(host_language_server_error(
                -32603,
                "collaboration host is unavailable",
            ));
            return;
        };
        let deadline = self
            .runtime
            .clock()
            .deadline_after(helix_collab::LANGUAGE_SERVER_REQUEST_TIMEOUT);
        session
            .pending_language_servers
            .push(PendingHostLanguageServerRequest {
                request,
                document,
                deadline,
            });
        self.arm_host_language_server_deadline();
    }

    fn drive_host_language_server_requests(&mut self) {
        let Some(session) = self.collaboration.as_mut() else {
            self.timers.host_language_servers.disarm();
            return;
        };
        let now = self.runtime.clock().now();
        let pending = std::mem::take(&mut session.pending_language_servers);
        let mut retained = Vec::with_capacity(pending.len());
        for pending in pending {
            if pending.request.is_canceled() {
                continue;
            }
            if pending.deadline <= now {
                pending.request.respond(host_language_server_error(
                    -32000,
                    "host language-server request timed out",
                ));
                continue;
            }
            let Some(doc) = self.editor.document(pending.document) else {
                pending.request.respond(host_language_server_error(
                    -32603,
                    "host document is unavailable",
                ));
                continue;
            };
            if self.editor.collaboration.buffer(pending.document) != Some(pending.request.buffer) {
                if self.collaboration.as_ref().is_some_and(|session| {
                    session.host_bindings_pending.contains(&pending.document)
                }) {
                    retained.push(pending);
                } else {
                    pending.request.respond(host_language_server_error(
                        -32603,
                        "host document collaboration binding failed",
                    ));
                }
                continue;
            }
            if doc
                .language_server_by_name(&pending.request.server)
                .is_none()
            {
                retained.push(pending);
                continue;
            }
            self.execute_host_language_server_request(pending.request, pending.document);
        }
        if let Some(session) = self.collaboration.as_mut() {
            session.pending_language_servers.extend(retained);
        } else {
            for pending in retained {
                pending.request.respond(host_language_server_error(
                    -32603,
                    "collaboration host is unavailable",
                ));
            }
        }
        self.arm_host_language_server_deadline();
    }

    fn arm_host_language_server_deadline(&mut self) {
        let deadline = self.collaboration.as_ref().and_then(|session| {
            session
                .pending_language_servers
                .iter()
                .map(|pending| pending.deadline)
                .min()
        });
        match deadline {
            Some(deadline) => self.timers.host_language_servers.arm_at(deadline),
            None => self.timers.host_language_servers.disarm(),
        }
    }

    fn execute_host_language_server_request(
        &mut self,
        mut request: helix_collab::HostLanguageServerRequest,
        document: helix_view::DocumentId,
    ) {
        let Some((client, hosted)) = self.editor.document(document).and_then(|doc| {
            let client = doc.language_server_by_name(&request.server)?.clone();
            let hosted = self
                .collaboration
                .as_ref()
                .and_then(|session| session.hosted.clone())?;
            Some((client, hosted))
        }) else {
            request.respond(host_language_server_error(
                -32603,
                "host language server is unavailable",
            ));
            return;
        };
        let method = std::mem::take(&mut request.method);
        let params = std::mem::take(&mut request.params);
        request.text.clear();
        self.runtime
            .work()
            .spawn(async move {
                let operation = async {
                    let mut params = serde_json::from_slice(&params).map_err(|error| {
                        helix_lsp::Error::Rpc(helix_lsp::jsonrpc::Error::invalid_params(
                            error.to_string(),
                        ))
                    })?;
                    hosted
                        .rewrite_language_server_request(&mut params)
                        .map_err(|error| {
                            helix_lsp::Error::Rpc(helix_lsp::jsonrpc::Error::invalid_params(error))
                        })?;
                    client.wait_until_initialized().await?;
                    let mut result = if method == "initialize" {
                        serde_json::json!({ "capabilities": client.capabilities() })
                    } else {
                        client.call_custom(method, params).await?
                    };
                    hosted
                        .rewrite_language_server_response(&mut result)
                        .map_err(|error| helix_lsp::Error::Other(anyhow::Error::msg(error)))?;
                    Ok(result)
                };
                tokio::select! {
                    biased;
                    _ = request.canceled() => {}
                    result = operation => request.respond(host_language_server_result(result)),
                }
            })
            .detach();
    }

    fn queue_collaboration_language_server_diagnostics(
        &mut self,
        diagnostics: helix_collab::LanguageServerDiagnostics,
    ) {
        if diagnostics.params.len() > helix_collab::MAX_LANGUAGE_SERVER_PAYLOAD_BYTES {
            log::warn!("discarding oversized collaboration diagnostics payload");
            return;
        }
        let Some(session) = self
            .collaboration
            .as_mut()
            .filter(|session| session.hosted.is_none())
        else {
            return;
        };
        let key = (diagnostics.path, diagnostics.server);
        let params = diagnostics.params.into_vec();
        if let Some(replaced) = session.pending_diagnostics.insert(key.clone(), params) {
            session.pending_diagnostics_bytes = session
                .pending_diagnostics_bytes
                .saturating_sub(replaced.len());
        }
        session.pending_diagnostics_bytes = session.pending_diagnostics.get(&key).map_or(
            session.pending_diagnostics_bytes,
            |params| {
                session
                    .pending_diagnostics_bytes
                    .saturating_add(params.len())
            },
        );
        while session.pending_diagnostics_bytes > MAX_PENDING_COLLABORATION_DIAGNOSTICS_BYTES {
            let evicted = session
                .pending_diagnostics
                .keys()
                .find(|candidate| **candidate != key)
                .cloned()
                .or_else(|| session.pending_diagnostics.keys().next().cloned());
            let Some(evicted) = evicted else {
                break;
            };
            if let Some(params) = session.pending_diagnostics.remove(&evicted) {
                session.pending_diagnostics_bytes = session
                    .pending_diagnostics_bytes
                    .saturating_sub(params.len());
            }
        }
        self.drive_collaboration_language_server_diagnostics();
    }

    fn drive_collaboration_language_server_diagnostics(&mut self) {
        let Some(session) = self
            .collaboration
            .as_ref()
            .filter(|session| session.hosted.is_none())
        else {
            return;
        };
        let project = session.handle.project().id;
        let ready = session
            .pending_diagnostics
            .keys()
            .filter(|(path, server)| {
                let path = helix_view::editor::WorkspaceDocumentPath::Collaboration {
                    project,
                    path: path.clone(),
                };
                self.editor
                    .document_id_by_workspace_path(&path)
                    .and_then(|document| self.editor.document(document))
                    .and_then(|document| document.language_server_by_name(server))
                    .is_some()
            })
            .cloned()
            .collect::<Vec<_>>();

        for (path, server) in ready {
            let Some(params) = self.collaboration.as_mut().and_then(|session| {
                session
                    .pending_diagnostics
                    .remove(&(path.clone(), server.clone()))
            }) else {
                continue;
            };
            if let Some(session) = self.collaboration.as_mut() {
                session.pending_diagnostics_bytes = session
                    .pending_diagnostics_bytes
                    .saturating_sub(params.len());
            }
            let blocking = self.runtime.block().spawn(move || {
                serde_json::from_slice::<lsp_types::PublishDiagnosticsParams>(&params)
                    .map_err(|error| error.to_string())
            });
            let ingress = self.ingress.tx.clone();
            self.runtime
                .work()
                .spawn(async move {
                    let result = match blocking.await {
                        Ok(result) => result,
                        Err(error) => Err(error.to_string()),
                    };
                    let _ = ingress
                        .send_task(
                            crate::runtime::RuntimeTaskEvent::CollaborationLanguageServerDiagnosticsParsed {
                                project,
                                path,
                                server,
                                result,
                            },
                        )
                        .await;
                })
                .detach();
        }
    }

    fn apply_collaboration_language_server_diagnostics(
        &mut self,
        project: helix_collab::ProjectId,
        path: helix_workspace::WorkspacePath,
        server: String,
        result: Result<lsp_types::PublishDiagnosticsParams, String>,
    ) {
        let Some(_session) = self
            .collaboration
            .as_ref()
            .filter(|session| session.hosted.is_none() && session.handle.project().id == project)
        else {
            return;
        };
        let params = match result {
            Ok(params) => params,
            Err(error) => {
                log::warn!("discarding malformed collaboration diagnostics: {error}");
                return;
            }
        };
        let path = helix_view::editor::WorkspaceDocumentPath::Collaboration { project, path };
        let Some(document) = self.editor.document_id_by_workspace_path(&path) else {
            return;
        };
        let Some((server_id, expected_uri)) = self.editor.document(document).and_then(|document| {
            Some((
                document.language_server_by_name(&server)?.id(),
                document.url()?,
            ))
        }) else {
            return;
        };
        if params.uri != expected_uri {
            log::warn!("discarding collaboration diagnostics with a mismatched document URI");
            return;
        }
        self.queue_lsp_diagnostics(server_id, params);
    }

    fn publish_host_invitation_if_ready(&mut self) {
        let invitation = self.collaboration.as_mut().and_then(|session| {
            (session.bootstrap_pending.is_empty() && session.bootstrap_failed.is_empty())
                .then(|| session.pending_invitation.take())
                .flatten()
        });
        if let Some(invitation) = invitation {
            self.copy_collaboration_invitation(invitation);
        }
    }

    fn stop_collaboration(&mut self) {
        let Some(session) = self.collaboration.take() else {
            self.editor
                .notify_warning("No collaboration session is active");
            return;
        };
        let CollaborationApplicationSession {
            host,
            hosted: _,
            handle,
            updates,
            language_servers,
            previous_workspace_backend,
            pending_invitation: _,
            bootstrap_pending: _,
            bootstrap_failed: _,
            host_bindings_pending: _,
            pending_language_servers,
            pending_diagnostics: _,
            pending_diagnostics_bytes: _,
        } = session;
        for pending in pending_language_servers {
            pending.request.respond(host_language_server_error(
                -32603,
                "collaboration host is shutting down",
            ));
        }
        self.timers.host_language_servers.disarm();
        self.editor.detach_collaboration_session();
        if let Some(previous) = previous_workspace_backend {
            self.editor.set_workspace_backend(previous);
        }
        let shutdown = self.runtime.work().spawn(async move {
            if let Err(error) = handle.leave().await {
                log::warn!("failed to leave collaboration session cleanly: {error}");
            }
            let _ = updates.await;
            if let Some(host) = host {
                if let Err(error) = host.shutdown().await {
                    log::warn!("failed to shut down collaboration host cleanly: {error}");
                }
            }
            if let Some(language_servers) = language_servers {
                let _ = language_servers.await;
            }
        });
        self.collaboration_shutdowns.push(shutdown);
        self.editor.notify_info("Collaboration session closed");
    }

    fn copy_collaboration_invitation(&mut self, code: helix_collab::ConnectCode) {
        use helix_view::clipboard::ClipboardType;

        let code = code.to_string();
        let _ = self.editor.registers.write('"', vec![code.clone()]);
        let provider = self.editor.config().clipboard_provider.clone();
        let copy = self
            .runtime
            .block()
            .spawn(move || provider.set_contents(&code, ClipboardType::Clipboard));
        let ingress = self.ingress.tx.clone();
        self.runtime
            .work()
            .spawn(async move {
                let result = match copy.await {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(error) => Err(error.to_string()),
                };
                let _ = ingress
                    .send_task(
                        crate::runtime::RuntimeTaskEvent::CollaborationInvitationCopied(result),
                    )
                    .await;
            })
            .detach();
    }

    fn handle_runtime_task(&mut self, task: crate::runtime::RuntimeTaskEvent) {
        let task = match task {
            crate::runtime::RuntimeTaskEvent::CollaborationHostDocumentOpened { document } => {
                self.start_host_document_binding(document, false);
                return;
            }
            crate::runtime::RuntimeTaskEvent::CollaborationHostDocumentClosed { document } => {
                self.host_document_closed(document);
                return;
            }
            crate::runtime::RuntimeTaskEvent::CollaborationHostBufferOpened {
                project,
                document,
                path,
                result,
            } => {
                self.finish_host_document_binding(project, document, path, result);
                return;
            }
            crate::runtime::RuntimeTaskEvent::CollaborationHostBufferReady {
                project,
                document,
                result,
            } => {
                self.host_buffer_ready(project, document, result);
                return;
            }
            crate::runtime::RuntimeTaskEvent::CollaborationHostLanguageServerRequest(request) => {
                self.handle_host_language_server_request(request);
                return;
            }
            crate::runtime::RuntimeTaskEvent::CollaborationHostLanguageServerDocumentOpened {
                request,
                result,
            } => {
                self.finish_host_language_server_document_open(request, *result);
                return;
            }
            crate::runtime::RuntimeTaskEvent::CollaborationLanguageServerDiagnosticsParsed {
                project,
                path,
                server,
                result,
            } => {
                self.apply_collaboration_language_server_diagnostics(project, path, server, result);
                return;
            }
            crate::runtime::RuntimeTaskEvent::Collaboration(
                helix_collab::GuestSessionUpdate::LanguageServerDiagnostics(diagnostics),
            ) => {
                self.queue_collaboration_language_server_diagnostics(diagnostics);
                return;
            }
            crate::runtime::RuntimeTaskEvent::Collaboration(
                helix_collab::GuestSessionUpdate::LanguageServerRefresh(refresh),
            ) => {
                self.apply_collaboration_language_server_refresh(refresh);
                return;
            }
            crate::runtime::RuntimeTaskEvent::Collaboration(update) => {
                let project = self
                    .collaboration
                    .as_ref()
                    .map(|session| session.handle.project().id);
                let refresh_files = matches!(
                    &update,
                    helix_collab::GuestSessionUpdate::FilesChanged { .. }
                        | helix_collab::GuestSessionUpdate::WorktreeChanged { .. }
                        | helix_collab::GuestSessionUpdate::ProjectState(_)
                );
                let refresh_participants = matches!(
                    &update,
                    helix_collab::GuestSessionUpdate::ProjectState(_)
                        | helix_collab::GuestSessionUpdate::ParticipantJoined(_)
                        | helix_collab::GuestSessionUpdate::ParticipantLeft(_)
                        | helix_collab::GuestSessionUpdate::RoleChanged { .. }
                        | helix_collab::GuestSessionUpdate::Connection(
                            helix_collab::ConnectionState::Connected(_)
                        )
                );
                self.editor.apply_collaboration_update(update);
                self.drive_collaboration_language_server_diagnostics();
                if let Some(project) = project.filter(|_| refresh_files) {
                    self.compositor.refresh_picker(
                        &mut self.editor,
                        crate::ui::picker::PickerRefreshScope::CollaborationFiles(project),
                    );
                    self.handle_runtime_ui_command(crate::runtime::UiCommand::FileExplorer(
                        crate::runtime::FileExplorerCommand::RefreshCollaboration { project },
                    ));
                }
                if let Some(project) = project.filter(|_| refresh_participants) {
                    self.compositor.refresh_picker(
                        &mut self.editor,
                        crate::ui::picker::PickerRefreshScope::CollaborationParticipants(project),
                    );
                }
                return;
            }
            crate::runtime::RuntimeTaskEvent::CollaborationStarted(launch) => {
                self.install_collaboration(launch);
                return;
            }
            crate::runtime::RuntimeTaskEvent::CollaborationStop => {
                self.stop_collaboration();
                return;
            }
            crate::runtime::RuntimeTaskEvent::CollaborationInvitation(code) => {
                self.copy_collaboration_invitation(code);
                return;
            }
            crate::runtime::RuntimeTaskEvent::CollaborationInvitationCopied(result) => {
                match result {
                    Ok(()) => {
                        self.editor
                            .notify_info("Collaboration invitation copied to clipboard");
                    }
                    Err(error) => {
                        self.editor.notify_warning(format!(
                            "Could not copy collaboration invitation: {error}. It is in the unnamed register"
                        ));
                    }
                }
                return;
            }
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
        let hosted = self
            .collaboration
            .as_ref()
            .and_then(|session| session.hosted.clone());
        crate::effect::apply_runtime_task_event(
            &mut self.editor,
            ingress,
            self.foreground.clone(),
            self.plugin_runtime.clone(),
            hosted,
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
                crate::runtime::UiCommand::AfterDocumentMutations { command }
                    if self.ingress.tx.has_pending_document_mutations() =>
                {
                    self.ingress.after_document_mutations.push(*command);
                    return;
                }
                crate::runtime::UiCommand::AfterDocumentMutations { command } => {
                    cmd = *command;
                }
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
        let mut context = crate::compositor::Context::with_services(
            &mut self.editor,
            &mut self.exit.tasks,
            crate::compositor::ContextServices::new(
                self.exit.work.clone(),
                notifier,
                self.ingress.tx.clone(),
                self.ingress.idle_reset.clone(),
                self.plugin_runtime.clone(),
                self.foreground.clone(),
            ),
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

    fn service_after_document_mutations(&mut self) {
        if self.ingress.tx.has_pending_document_mutations()
            || self.ingress.after_document_mutations.is_empty()
        {
            return;
        }
        let commands = std::mem::take(&mut self.ingress.after_document_mutations);
        for command in commands {
            self.handle_runtime_ui_command(command);
            if self.editor.should_close() {
                break;
            }
        }
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
                .map(|path| helix_stdx::path::display_path(path).into_owned())
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
        let mut cx = crate::compositor::Context::with_services(
            &mut self.editor,
            &mut self.exit.tasks,
            crate::compositor::ContextServices::new(
                self.exit.work.clone(),
                notifier,
                ingress,
                idle_reset,
                self.plugin_runtime.clone(),
                self.foreground.clone(),
            ),
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

        let pipeline_saturated = self
            .renderer
            .as_ref()
            .is_some_and(render_actor::RenderActor::is_saturated);
        if should_defer_frame(&self.frames, pipeline_saturated) {
            self.timers.frame.disarm();
            return false;
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
        #[cfg(feature = "integration")]
        self.timers
            .idle
            .arm_after(self.editor.config().idle_timeout);

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
            let input_barrier_pending = self.ingress.tx.has_pending_input_barriers();
            let document_open_pending = self.ingress.tx.has_pending_interactive_document_open();
            let input_blocked = input_barrier_pending || document_open_pending;
            if input_blocked {
                log::trace!(
                    "terminal input gated: input_barrier_pending={input_barrier_pending} document_open_pending={document_open_pending}"
                );
            }

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
                event = async {
                    if input_blocked {
                        pending().await
                    } else {
                        input_stream.next().await
                    }
                } => {
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
                _ = self.timers.host_language_servers.elapsed() => {
                    self.drive_host_language_server_requests();
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
                    self.drive_host_language_server_requests();
                    self.drive_collaboration_language_server_diagnostics();
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
                Some(_request) = self.terminal_state.pipeline_ready_rx.recv() => {
                    self.arm_frame_timer();
                }
                event = async {
                    match &mut self.remote {
                        Some(remote) => remote.transport.next_event().await,
                        None => pending::<Option<helix_remote::ssh::SshSessionEvent>>().await,
                    }
                } => {
                    match event {
                        Some(event) => self.handle_remote_session_event(event),
                        None => {
                            self.remote = None;
                            self.editor.notify_error("Remote workspace connection closed");
                        }
                    }
                    self.invalidate(FRAME_REMOTE);
                }
                _ = self.timers.idle.elapsed() => {
                    self.timers.idle.disarm();
                    self.handle_idle_timeout().await;

                    #[cfg(feature = "integration")]
                    {
                        let now = self.runtime.clock().now();
                        if self.exit.tasks.is_empty()
                            && !self.editor.has_pending_writes()
                            && self.ingress.tx.is_idle()
                            && !self.editor.is_redraw_pending()
                            && !self.frames.has_pending_frame(now)
                            && !self
                                .renderer
                                .as_ref()
                                .is_some_and(render_actor::RenderActor::is_saturated)
                        {
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

            self.service_after_document_mutations();
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
        if let Some(remote) = self.remote.take() {
            remote.transport.shutdown().await;
        }

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

    #[test]
    fn saturated_pipeline_defers_background_but_never_input() {
        let mut frames = FrameScheduler::new();
        frames.invalidate(FRAME_LSP);
        assert!(should_defer_frame(&frames, true));

        frames.invalidate(FRAME_INPUT);
        assert!(!should_defer_frame(&frames, true));
        assert!(!should_defer_frame(&frames, false));
    }

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
