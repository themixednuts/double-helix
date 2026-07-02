use arc_swap::{access::Map, ArcSwap};
use futures_util::Stream;
use helix_core::{find_workspace, pos_at_coords, syntax, Range};
use helix_lsp::{
    lsp::{self as lsp_types},
    Call, LanguageServerId, LspProgressMap,
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
    compositor::{Compositor, Event},
    config::Config,
    handlers,
    keymap::Keymaps,
    runtime::ExitTaskSet,
    ui::{self, overlay::overlaid},
};
use futures_util::stream::select_all::SelectAll;
use helix_dap::{self as dap, registry::DebugAdapterId};
use helix_runtime::{FrameReceiver, Runtime, Work};
use tokio::time::{sleep, Instant, Sleep};

use crate::runtime::{RuntimeDelivery, RuntimeIngressReceiver};

use std::{
    borrow::Cow,
    io::{stdin, IsTerminal},
    path::Path,
    pin::Pin,
    sync::Arc,
};

use helix_plugin::{PluginConfig, PluginManager, PluginNotification};

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

type Terminal = ratatui_terminal::AppTerminal<TerminalBackend>;

mod bench;
mod config;
mod lifecycle;
mod lsp;
mod ratatui_terminal;
mod terminal;

struct IngressState {
    tx: crate::runtime::RuntimeIngress,
    rx: RuntimeIngressReceiver,
    config_rx: helix_runtime::Receiver<ConfigEvent>,
    assistant_updates_rx: helix_runtime::Receiver<helix_view::assistant::backend::Update>,
    redraw_rx: FrameReceiver,
    idle_reset_rx: crate::runtime::IdleResetReceiver,
    idle_reset: crate::runtime::IdleResetHandle,
    plugin_event_rx: helix_runtime::Receiver<PluginNotification>,
    plugin_event_tx: helix_runtime::Sender<PluginNotification>,
}

struct TimerState {
    redraw: Pin<Box<Sleep>>,
    idle: Pin<Box<Sleep>>,
}

struct LoopState {
    signals: Signals,
    lsp_incoming: SelectAll<helix_runtime::Receiver<(LanguageServerId, Call)>>,
    debugger_incoming: SelectAll<helix_runtime::Receiver<(DebugAdapterId, dap::Payload)>>,
    /// Native shutdown channel (Windows: console ctrl; Unix: None, uses signal stream).
    shutdown_rx: Option<tokio::sync::mpsc::UnboundedReceiver<()>>,
}

struct ExitState {
    tasks: ExitTaskSet,
    work: Work,
}

struct TerminalState {
    theme_mode: Option<theme::Mode>,
}

struct LanguageState {
    progress: LspProgressMap,
}

pub struct Application {
    compositor: Compositor,
    terminal: Terminal,
    pub editor: Editor,

    config: Arc<ArcSwap<Config>>,

    /// Shared async runtime (UI/work/block/clock domains).
    runtime: Runtime,
    ingress: IngressState,

    exit: ExitState,
    loop_state: LoopState,
    timers: TimerState,
    terminal_state: TerminalState,
    language: LanguageState,
    plugin_manager: Arc<PluginManager>,
    remote_plugin_hosts: crate::plugin_registry::RemotePluginHosts,
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
    fn make_compositor_context<'a>(
        editor: &'a mut Editor,
        exit_tasks: &'a mut ExitTaskSet,
        exit_task_work: Work,
        notifier: crate::handlers::local::Notifier,
        ingress: crate::runtime::RuntimeIngress,
        idle_reset: crate::runtime::IdleResetHandle,
        plugin_manager: Arc<PluginManager>,
    ) -> crate::compositor::Context<'a> {
        crate::compositor::Context::new(
            editor,
            exit_tasks,
            exit_task_work,
            notifier,
            ingress,
            idle_reset,
            Some(plugin_manager),
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

        let mut theme_parent_dirs = vec![helix_loader::config_dir()];
        theme_parent_dirs.extend(helix_loader::runtime_dirs().iter().cloned());
        let theme_loader = theme::Loader::new(&theme_parent_dirs);

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
        let mut compositor = Compositor::new(area);
        let config = Arc::new(ArcSwap::from_pointee(config));
        let (ingress_tx, ingress_rx) =
            crate::runtime::RuntimeIngress::channel(runtime.work().clone());
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
        crate::handlers::attach(&editor, &editor.handlers, ingress_tx.clone());
        editor.set_assistant_history_backend(helix_view::assistant::history::local_backend());
        editor.set_assistant_context_registry(helix_view::assistant::context::core_registry());
        let fff_root = find_workspace().0;
        if fff_root.exists() {
            let fff_config = editor.config().file_picker.clone();
            runtime
                .block()
                .spawn(move || crate::fff::prewarm(&fff_root, &fff_config))
                .detach();
        }
        let lsp_incoming = editor.take_lsp_incoming();
        let debugger_incoming = editor.take_debugger_incoming();
        let config_rx = editor.take_config_rx();
        let assistant_updates_rx = editor.take_assistant_updates_rx();
        let redraw_rx = editor.take_redraw_rx();
        let mut idle_reset_gate = crate::runtime::IdleResetGate::new();
        let idle_reset = idle_reset_gate.handle();
        let idle_reset_rx = idle_reset_gate.take_receiver();
        let (plugin_event_tx, plugin_event_rx) = helix_runtime::channel(256);
        let idle_timeout = editor.config().idle_timeout;
        if editor.assistant_history_backend().is_some() {
            ingress_tx.task(
                crate::runtime::RuntimeTaskEvent::BootstrapAssistantHistory {
                    scope: helix_view::assistant::layout::current_scope(),
                },
            );
        }
        // Initialize OS-native file watcher for auto-reload
        crate::handlers::auto_reload::setup_file_watcher(&mut editor);

        Self::load_configured_theme(
            &mut editor,
            &config.load(),
            terminal.backend().supports_true_color(),
            theme_mode,
        );

        let keys = Box::new(Map::new(Arc::clone(&config), |config: &Config| {
            &config.keys
        }));
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
            let path = helix_loader::runtime_file(Path::new("tutor"));
            editor.open(&path, Action::VerticalSplit)?;
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
                            Err(DocumentOpenError::IrregularFile) => {
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

        let plugin_config = PluginConfig::default();
        let plugin_manager =
            PluginManager::new(plugin_config.clone()).expect("Failed to create plugin manager");

        {
            let engine_arc = plugin_manager.engine();
            let mut engine = engine_arc.write();
            engine.set_ui_host(crate::plugin_registry::get_ui_host(ingress_tx.clone()));
            engine.set_panel_host(crate::plugin_registry::get_panel_host(ingress_tx.clone()));
            engine.set_command_host(crate::plugin_registry::get_command_host(ingress_tx.clone()));
            engine.set_event_host(crate::plugin_registry::get_event_host());
        }

        if plugin_manager.is_enabled() {
            if let Err(e) = plugin_manager.initialize(&mut editor) {
                log::error!("Failed to initialize plugin manager: {}", e);
            } else {
                log::info!("Plugin system initialized");
                editor.set_status("Plugin system initialized");
            }
        }
        let plugin_manager = Arc::new(plugin_manager);
        let remote_plugin_hosts =
            crate::plugin_registry::spawn_remote_hosts(&plugin_config, ingress_tx.clone());

        #[cfg(windows)]
        let shutdown_rx = crate::shutdown::setup();
        #[cfg(not(windows))]
        let shutdown_rx = None;

        let redraw = editor.redraw_handle();
        let plugin_events = plugin_event_tx.clone();
        editor.lifecycle().on_document_open(move |event| {
            helix_runtime::send_blocking(
                &plugin_events,
                PluginNotification::BufferOpen {
                    document_id: event.doc,
                    path: Some(event.path.clone()),
                },
            );
            redraw.request_redraw();
            Ok(())
        });

        let plugin_events = plugin_event_tx.clone();
        editor.lifecycle().on_selection_change(move |event| {
            helix_runtime::send_blocking(
                &plugin_events,
                PluginNotification::SelectionChange {
                    document_id: event.doc.id(),
                    path: event
                        .doc
                        .path()
                        .map(|p: &std::path::PathBuf| p.to_path_buf()),
                },
            );
            Ok(())
        });

        let plugin_events = plugin_event_tx.clone();
        editor.lifecycle().on_diagnostics_change(move |event| {
            helix_runtime::send_blocking(
                &plugin_events,
                PluginNotification::LspDiagnostic {
                    document_id: event.doc,
                    diagnostic_count: event.diagnostic_count,
                },
            );
            Ok(())
        });

        // Fire DocumentOpened for already opened documents
        {
            use helix_plugin::contract::{adapt, events};
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
                if let Err(e) = plugin_manager.fire_event(&mut editor, &event) {
                    log::error!("Failed to fire plugin event for startup doc: {}", e);
                }
                remote_plugin_hosts.notify_event(event);
            }
        }

        let app = Self {
            compositor,
            terminal,
            editor,
            config,
            runtime,
            ingress: IngressState {
                tx: ingress_tx,
                rx: ingress_rx,
                config_rx,
                assistant_updates_rx,
                redraw_rx,
                idle_reset_rx,
                idle_reset,
                plugin_event_rx,
                plugin_event_tx,
            },
            exit: ExitState {
                tasks: exit_tasks,
                work: exit_task_work,
            },
            loop_state: LoopState {
                signals,
                lsp_incoming,
                debugger_incoming,
                shutdown_rx,
            },
            timers: TimerState {
                redraw: Box::pin(sleep(std::time::Duration::MAX)),
                idle: Box::pin(sleep(idle_timeout)),
            },
            terminal_state: TerminalState { theme_mode },
            language: LanguageState {
                progress: LspProgressMap::new(),
            },
            plugin_manager,
            remote_plugin_hosts,
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

    #[inline]
    fn queue_redraw(&mut self) {
        if self.editor.is_redraw_pending() {
            return;
        }

        self.editor.mark_redraw_pending();
        let timeout = Instant::now() + std::time::Duration::from_millis(33);
        if timeout < self.timers.idle.deadline() && timeout < self.timers.redraw.deadline() {
            self.timers.redraw.as_mut().reset(timeout);
        }
    }

    fn handle_runtime_status(&mut self, message: String, severity: helix_view::editor::Severity) {
        self.editor.status_msg = Some((Cow::Owned(message), severity));
        self.queue_redraw();
    }

    fn handle_runtime_timer(&mut self, id: helix_runtime::TimerId) {
        log::trace!("runtime timer fired: {:?}", id);
        self.queue_redraw();
    }

    async fn handle_runtime_task(&mut self, task: crate::runtime::RuntimeTaskEvent) {
        let ingress = self.ingress().tx.clone();
        crate::effect::apply_runtime_task_event(
            &mut self.editor,
            ingress,
            self.plugin_manager.clone(),
            task,
        );
        self.render().await;
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

    async fn handle_runtime_ui_command(&mut self, cmd: crate::runtime::UiCommand) {
        let ingress = self.ingress().tx.clone();
        crate::runtime::apply_ui_command(
            &mut self.editor,
            &mut self.compositor,
            ingress,
            self.plugin_manager.clone(),
            cmd,
        );
        self.render().await;
    }

    async fn handle_runtime_delivery(&mut self, delivery: RuntimeDelivery) {
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
                self.handle_runtime_task(task).await;
            }
            RuntimeDelivery::AssistantPermissionResolved {
                thread,
                request,
                decision,
            } => {
                self.handle_runtime_assistant_permission(thread, request, decision);
            }
            RuntimeDelivery::Ui(cmd) => {
                self.handle_runtime_ui_command(cmd).await;
            }
        }
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
            work.spawn(async move {
                if timer_task.await.is_ok() {
                    ingress.send_timer(id).await;
                }
            })
            .detach();
        }
    }

    async fn render(&mut self) {
        let t0 = std::time::Instant::now();
        let focused_doc_id = self.editor.focused_document_id();
        let focused_doc_path = self
            .editor
            .focused_document()
            .and_then(|doc| doc.path())
            .map(|path| path.display().to_string().replace('\\', "/"))
            .unwrap_or_else(|| String::from("<scratch>"));
        log::info!(
            target: crate::ui::picker::PICKER_TRACE_TARGET,
            "phase=app_render_start redraw_pending={} full_redraw={} focused_view={:?} focused_doc={:?} focused_path={} documents={} component_documents={}",
            self.editor.is_redraw_pending(),
            self.compositor.full_redraw,
            self.editor.focused_view_id(),
            focused_doc_id,
            focused_doc_path,
            self.editor.document_count(),
            self.editor.component_docs.len(),
        );
        let ingress = self.ingress().tx.clone();
        let idle_reset = self.ingress().idle_reset.clone();

        self.editor.pause_assistant_follow_if_local_change();

        let clear_start = std::time::Instant::now();
        let did_full_redraw_clear = self.compositor.full_redraw;
        if self.compositor.full_redraw {
            self.terminal.clear().expect("Cannot clear the terminal");
            self.compositor.full_redraw = false;
        }
        let clear_elapsed = clear_start.elapsed();
        log_run_phase("render_setup", "full_redraw_clear", clear_elapsed, || {
            format!("did_clear={did_full_redraw_clear}")
        });

        let frame_setup_start = std::time::Instant::now();
        let redraw = self.editor.redraw_handle();
        let notifier = crate::handlers::local::Notifier {
            redraw: redraw.clone(),
            plugin_events: self.ingress().plugin_event_tx.clone(),
        };
        let mut cx = Self::make_compositor_context(
            &mut self.editor,
            &mut self.exit.tasks,
            self.exit.work.clone(),
            notifier,
            ingress,
            idle_reset,
            self.plugin_manager.clone(),
        );

        cx.editor.clear_redraw_request();
        let frame_setup_elapsed = frame_setup_start.elapsed();
        log_run_phase("render_setup", "frame_state", frame_setup_elapsed, || {
            format!("needs_redraw_reset={}", !cx.editor.is_redraw_pending())
        });

        let autoresize_start = std::time::Instant::now();
        let previous_area = self.terminal.viewport_area();
        let area = self
            .terminal
            .autoresize()
            .expect("Unable to determine terminal size");
        let autoresize_elapsed = autoresize_start.elapsed();
        log_run_phase(
            "render_setup",
            "terminal_autoresize",
            autoresize_elapsed,
            || {
                format!(
                    "prev={}x{} next={}x{} changed={}",
                    previous_area.width,
                    previous_area.height,
                    area.width,
                    area.height,
                    previous_area != area
                )
            },
        );

        let t1 = std::time::Instant::now(); // setup done

        let surface_start = std::time::Instant::now();
        let surface = self.terminal.current_buffer_mut();
        let surface_elapsed = surface_start.elapsed();
        log_run_phase("render_setup", "surface_prepare", surface_elapsed, || {
            format!("width={} height={}", area.width, area.height)
        });

        self.compositor.render(area, surface, &mut cx);
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
        log::info!(
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
        self.terminal.draw(pos, kind).unwrap();

        let t3 = std::time::Instant::now(); // terminal flush done
        log_run_phase("render", "flush_total", t3 - t2, || {
            format!("cursor_pos_present={} cursor_kind={kind:?}", pos.is_some())
        });
        log::info!(
            target: crate::ui::picker::PICKER_TRACE_TARGET,
            "phase=app_render_done total_us={} compositor_us={} flush_us={} cursor_pos_present={} cursor_kind={:?}",
            (t3 - t0).as_micros(),
            (t2 - t1).as_micros(),
            (t3 - t2).as_micros(),
            pos.is_some(),
            kind,
        );

        // Record render sub-phases when bench is active
        self.editor
            .record_bench_render_phases(t1 - t0, t2 - t1, t3 - t2);
    }

    pub async fn event_loop<S>(&mut self, input_stream: &mut S)
    where
        S: Stream<Item = std::io::Result<TerminalEvent>> + Unpin,
    {
        self.render().await;

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

            use futures_util::future::{pending, Either};
            use futures_util::StreamExt;

            tokio::select! {
                biased;

                Some(signal) = self.loop_state.signals.next() => {
                    if !self.handle_signals(signal).await {
                        return false;
                    };
                }
                _ = match &mut self.loop_state.shutdown_rx {
                    Some(rx) => Either::Left(rx.recv()),
                    None => Either::Right(pending()),
                } => return false,
                Some(event) = input_stream.next() => {
                    self.handle_terminal_events(event).await;
                }
                Some(delivery) = self.ingress.rx.recv() => {
                    self.handle_runtime_delivery(delivery).await;
                }
                Some(result) = self.editor.recv_save_result() => {
                    self.handle_document_write(result);
                    self.render().await;
                }
                Some((id, call)) = self.loop_state.lsp_incoming.next() => {
                    self.handle_language_server_message(call, id).await;
                    self.queue_redraw();
                }
                Some((id, payload)) = self.loop_state.debugger_incoming.next() => {
                    let needs_render = self.editor.handle_debugger_message(id, payload).await;
                    if needs_render {
                        self.render().await;
                    }
                }
                Some(config_event) = self.ingress.config_rx.recv() => {
                    self.handle_config_events(config_event);
                    self.render().await;
                }
                Some(update) = self.ingress.assistant_updates_rx.recv() => {
                    self.handle_assistant_update(update).await;
                    self.queue_redraw();
                }
                Some(notification) = self.ingress.plugin_event_rx.recv() => {
                    if let Some(event) = crate::effect::plugin::notification_to_event(&notification, &self.editor) {
                        if let Err(err) = self.plugin_manager.fire_event(&mut self.editor, &event) {
                            log::error!("Failed to fire plugin event: {}", err);
                        }
                        self.remote_plugin_hosts.notify_event(event);
                    }
                }
                Some(_request) = self.ingress.redraw_rx.recv() => {
                    self.queue_redraw();
                }
                Some(_request) = self.ingress.idle_reset_rx.recv() => {
                    let timeout = self.editor.config().idle_timeout;
                    self.timers.idle.as_mut().reset(Instant::now() + timeout);
                }
                _ = &mut self.timers.idle => {
                    self.timers.idle.as_mut().reset(
                        Instant::now() + std::time::Duration::from_secs(86400 * 365 * 30),
                    );
                    self.handle_idle_timeout().await;

                    #[cfg(feature = "integration")]
                    {
                        if self.exit.tasks.is_empty() && !self.editor.has_pending_writes() {
                            return true;
                        }
                    }
                }
                _ = &mut self.timers.redraw => {
                    self.timers
                        .redraw
                        .as_mut()
                        .reset(Instant::now() + std::time::Duration::from_secs(86400 * 365 * 30));
                    let _idle_handled = self.handle_editor_event(EditorEvent::Redraw).await;
                    #[cfg(feature = "integration")]
                    {
                        if _idle_handled {
                            return true;
                        }
                    }
                }
                Some(res) = self.exit.tasks.next() => {
                    let ingress = self.ingress().tx.clone();
                    if let Err(err) = crate::runtime::apply_exit_task(
                        &mut self.editor,
                        ingress,
                        self.plugin_manager.clone(),
                        res,
                    ) {
                        self.editor.set_error(format!("Async task failed: {}", err));
                    }
                    self.render().await;
                }
            }

            // for integration tests only, reset the idle timer after every
            // event to signal when test events are done processing
            #[cfg(feature = "integration")]
            {
                let timeout = self.editor.config().idle_timeout;
                self.timers.idle.as_mut().reset(Instant::now() + timeout);
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
                // Skip render here when bench is active — the bench tick
                // does its own render, avoiding double-render per iteration.
                if !self.editor.has_active_bench() {
                    self.render().await;
                }
            }
        }

        false
    }

    pub async fn run<S>(&mut self, input_stream: &mut S) -> Result<i32, Error>
    where
        S: Stream<Item = std::io::Result<TerminalEvent>> + Unpin,
    {
        self.terminal.claim()?;

        self.event_loop(input_stream).await;

        self.remote_plugin_hosts.shutdown();
        let close_errs = self.close().await;

        self.restore_term()?;

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
