use arc_swap::{access::Map, ArcSwap};
use futures_util::Stream;
use helix_core::{pos_at_coords, syntax, Range, Selection};
use helix_lsp::{
    lsp::{self, notification::Notification},
    util::lsp_range_to_range,
    Call, LanguageServerId, LspProgressMap,
};
use helix_stdx::path::get_relative_path;
use helix_view::{
    align_view,
    bench::{enter_bench_run, log_run_event, log_run_phase, BenchRunContext},
    document::{DocumentOpenError, DocumentSavedEventFuture, DocumentSavedEventResult},
    editor::{ConfigEvent, EditorEvent},
    events::EditorConfigDidChange,
    graphics::Rect,
    theme,
    tree::Layout,
    Align, Editor,
};
use serde_json::json;
use tui::backend::Backend;

use crate::{
    args::Args,
    compositor::{Compositor, Event},
    config::Config,
    events::OnModeSwitch,
    handlers,
    keymap::Keymaps,
    runtime::ExitTaskSet,
    ui::{self, overlay::overlaid},
};
use helix_runtime::{channel, Receiver, Runtime, Sender, Work};
use tokio::time::{sleep, Instant, Sleep};
use futures_util::stream::select_all::SelectAll;
use helix_dap::{self as dap, registry::DebugAdapterId};

use crate::runtime::ingress::{self, RuntimeEvent};

use log::{debug, error, info, warn};
use std::{
    borrow::Cow,
    io::{stdin, IsTerminal},
    path::Path,
    pin::Pin,
    sync::Arc,
};

use helix_event::register_hook;
use helix_plugin::{EventData, EventType, PluginConfig, PluginEvent, PluginManager};
use helix_view::events::{DiagnosticsDidChange, DocumentDidOpen, SelectionDidChange};

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

type Terminal = tui::terminal::Terminal<TerminalBackend>;

struct ModalEngineFactory {
    registry: Arc<helix_modal::registry::CommandRegistry>,
}

impl helix_view::engine::EditingEngineFactory for ModalEngineFactory {
    fn create(
        &self,
        config: helix_view::editor::EditingEngineConfig,
    ) -> Box<dyn helix_view::engine::EditingEngine> {
        match config {
            helix_view::editor::EditingEngineConfig::Helix => {
                Box::new(helix_modal::helix::HelixEngine::new(self.registry.clone()))
            }
            helix_view::editor::EditingEngineConfig::Vim => {
                Box::new(helix_modal::vim::VimEngine::new(self.registry.clone()))
            }
        }
    }
}

pub struct Application {
    compositor: Compositor,
    terminal: Terminal,
    pub editor: Editor,

    config: Arc<ArcSwap<Config>>,

    /// Shared async runtime (UI/work/block/clock domains).
    runtime: Runtime,
    /// Send typed events from background/UI tasks into [`Self::event_loop_until_idle`].
    ingress_tx: Sender<RuntimeEvent>,
    ingress_rx: Receiver<RuntimeEvent>,
    _status_bridge: crate::runtime::StatusBridge,

    signals: Signals,
    exit_tasks: ExitTaskSet,
    exit_task_work: Work,
    save_queue: helix_runtime::Receiver<DocumentSavedEventFuture>,
    lsp_incoming: SelectAll<helix_runtime::Receiver<(LanguageServerId, Call)>>,
    debugger_incoming: SelectAll<helix_runtime::Receiver<(DebugAdapterId, dap::Payload)>>,
    config_rx: helix_runtime::Receiver<ConfigEvent>,
    assistant_updates_rx: helix_runtime::Receiver<helix_view::assistant::backend::Update>,
    redraw_rx: helix_runtime::Receiver<()>,
    idle_reset_rx: helix_runtime::Receiver<()>,
    idle_reset_tx: helix_runtime::Sender<()>,
    plugin_event_rx: helix_runtime::Receiver<PluginEvent>,
    plugin_event_tx: helix_runtime::Sender<PluginEvent>,
    redraw_timer: Pin<Box<Sleep>>,
    idle_timer: Pin<Box<Sleep>>,
    lsp_progress: LspProgressMap,
    plugin_manager: Arc<PluginManager>,

    /// Native shutdown channel (Windows: console ctrl; Unix: None, uses signal stream).
    shutdown_rx: Option<tokio::sync::mpsc::UnboundedReceiver<()>>,

    theme_mode: Option<theme::Mode>,
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

// ---------------------------------------------------------------------------
use helix_view::bench::{
    bench_pick_action, enter_bench_command, log_command_phase, BenchCommandContext,
    BenchResetStats, BenchTickTrace, BENCH_INSERT_SNIPPETS,
};

impl Application {
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
        let (ingress_tx, ingress_rx) = channel(ingress::BOUND);
        let status_bridge = crate::runtime::install_status_bridge(ingress_tx.clone());
        let handlers = handlers::setup(config.clone(), ingress_tx.clone(), runtime.clone());
        let mut editor = Editor::new(
            area,
            Arc::new(theme_loader),
            Arc::new(ArcSwap::from_pointee(lang_loader)),
            Arc::new(Map::new(Arc::clone(&config), |config: &Config| {
                &config.editor
            })),
            runtime.clone(),
            handlers,
        );
        editor.set_assistant_history_backend(helix_view::assistant::history::local_backend());
        editor.set_assistant_context_registry(helix_view::assistant::context::core_registry());
        let save_queue = editor.take_save_queue();
        let lsp_incoming = editor.take_lsp_incoming();
        let debugger_incoming = editor.take_debugger_incoming();
        let config_rx = editor.take_config_rx();
        let assistant_updates_rx = editor.take_assistant_updates_rx();
        let redraw_rx = editor.take_redraw_rx();
        let (idle_reset_tx, idle_reset_rx) = helix_runtime::channel(64);
        let (plugin_event_tx, plugin_event_rx) = helix_runtime::channel(256);
        let idle_timeout = editor.config().idle_timeout;
        if editor.assistant_history_backend().is_some() {
            helix_runtime::send_blocking(
                &ingress_tx,
                crate::runtime::RuntimeEvent::Task(
                    crate::runtime::RuntimeTaskEvent::BootstrapAssistantHistory {
                        scope: helix_view::assistant::layout::current_scope(),
                    },
                ),
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
        editor.modal_keymaps = Some(Arc::new(arc_swap::ArcSwap::from_pointee(
            crate::keymap::to_component_modal_keymaps(&config.load().keys),
        )));

        // Build the editing engine based on config
        let registry = std::sync::Arc::new(helix_modal::populate::build_registry());
        editor.engine_factory = Some(Arc::new(ModalEngineFactory {
            registry: registry.clone(),
        }));
        let engine = editor
            .engine_factory
            .as_ref()
            .expect("engine_factory not set")
            .create(config.load().editor.editing_engine);
        log::info!("Editing engine: {}", engine.name());

        let editor_view = Box::new(ui::EditorView::new(Keymaps::new(keys), engine, registry));
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
                        let view_id = editor.tree.focus;
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

        let plugin_manager =
            PluginManager::new(PluginConfig::default()).expect("Failed to create plugin manager");

        let ui_handler = crate::plugin_registry::get_ui_handler(ingress_tx.clone());

        // Register registries
        {
            let engine_arc = plugin_manager.engine();
            let mut engine = engine_arc.write();
            engine.set_builtin_command_registry(crate::plugin_registry::get_registry(
                ingress_tx.clone(),
            ));
            engine.set_ui_handler(ui_handler);
        }

        if plugin_manager.is_enabled() {
            if let Err(e) = plugin_manager.initialize(&mut editor) {
                log::error!("Failed to initialize plugin manager: {}", e);
            } else {
                log::warn!("Plugin system initialized");
                editor.set_status("Plugin system initialized");
            }
        }
        let plugin_manager = Arc::new(plugin_manager);

        #[cfg(windows)]
        let shutdown_rx = crate::shutdown::setup();
        #[cfg(not(windows))]
        let shutdown_rx = None;

        let redraw_ingress = ingress_tx.clone();
        let plugin_events = plugin_event_tx.clone();
        register_hook!(move |event: &mut DocumentDidOpen<'_>| {
            let _ = plugin_events.send(PluginEvent {
                event_type: EventType::OnBufferOpen,
                data: EventData::Buffer {
                    document_id: event.doc,
                    path: Some(event.path.clone()),
                },
            });
            helix_runtime::send_blocking(&redraw_ingress, RuntimeEvent::Redraw);
            Ok(())
        });

        let plugin_events = plugin_event_tx.clone();
        register_hook!(move |event: &mut SelectionDidChange<'_>| {
            let _ = plugin_events.send(PluginEvent {
                event_type: EventType::OnSelectionChange,
                data: EventData::Buffer {
                    document_id: event.doc.id(),
                    path: event
                        .doc
                        .path()
                        .map(|p: &std::path::PathBuf| p.to_path_buf()),
                },
            });
            Ok(())
        });

        let plugin_events = plugin_event_tx.clone();
        register_hook!(move |event: &mut DiagnosticsDidChange<'_>| {
            let _ = plugin_events.send(PluginEvent {
                event_type: EventType::OnLspDiagnostic,
                data: EventData::Buffer {
                    document_id: event.doc,
                    path: None,
                },
            });
            Ok(())
        });

        let redraw_ingress = ingress_tx.clone();
        let plugin_events = plugin_event_tx.clone();
        register_hook!(move |event: &mut OnModeSwitch<'_, '_>| {
            let old_mode = format!("{:?}", event.old_mode);
            let new_mode = format!("{:?}", event.new_mode);
            let _ = plugin_events.send(PluginEvent {
                event_type: EventType::OnModeChange,
                data: EventData::ModeChange { old_mode, new_mode },
            });
            helix_runtime::send_blocking(&redraw_ingress, RuntimeEvent::Redraw);
            Ok(())
        });

        // Fire OnBufferOpen for already opened documents
        let docs: Vec<_> = editor
            .documents()
            .filter_map(|doc| doc.path().map(|p| (doc.id(), p.to_path_buf())))
            .collect();

        for (doc_id, path) in docs {
            if let Err(e) = plugin_manager.fire_event(
                &mut editor,
                PluginEvent {
                    event_type: EventType::OnBufferOpen,
                    data: EventData::Buffer {
                        document_id: doc_id,
                        path: Some(path),
                    },
                },
            ) {
                log::error!("Failed to fire plugin event for startup doc: {}", e);
            }
        }

        let app = Self {
            compositor,
            terminal,
            editor,
            config,
            runtime,
            ingress_tx,
            ingress_rx,
            _status_bridge: status_bridge,
            signals,
            exit_tasks,
            exit_task_work,
            save_queue,
            lsp_incoming,
            debugger_incoming,
            config_rx,
            assistant_updates_rx,
            redraw_rx,
            idle_reset_rx,
            idle_reset_tx,
            plugin_event_rx,
            plugin_event_tx,
            redraw_timer: Box::pin(sleep(std::time::Duration::MAX)),
            idle_timer: Box::pin(sleep(idle_timeout)),
            lsp_progress: LspProgressMap::new(),
            plugin_manager,
            shutdown_rx,
            theme_mode,
        };

        Ok(app)
    }

    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    /// Clone of the sender for [`RuntimeEvent`] delivery into the main loop.
    pub fn ingress_sender(&self) -> Sender<RuntimeEvent> {
        self.ingress_tx.clone()
    }

    #[inline]
    fn queue_redraw(&self) {
        self.editor.request_redraw();
    }

    async fn handle_runtime_event(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::Redraw => {
                self.queue_redraw();
            }
            RuntimeEvent::Status { message, severity } => {
                self.editor.status_msg = Some((Cow::Owned(message), severity));
                self.queue_redraw();
            }
            RuntimeEvent::Timer(id) => {
                log::trace!("runtime timer fired: {:?}", id);
                self.queue_redraw();
            }
            RuntimeEvent::Task(task) => {
                crate::effect::apply_runtime_task_event(
                    &mut self.editor,
                    self.ingress_tx.clone(),
                    self.plugin_manager.clone(),
                    task,
                );
                self.render().await;
            }
            RuntimeEvent::AssistantPermissionResolved {
                thread,
                request,
                decision,
            } => {
                let effects = self.editor.resolve_assistant_permission(thread, request, decision);
                self.editor.apply_assistant_effects(effects);
            }
            RuntimeEvent::Ui(cmd) => {
                crate::runtime::apply_ui_command(
                    &mut self.editor,
                    &mut self.compositor,
                    self.ingress_tx.clone(),
                    self.plugin_manager.clone(),
                    cmd,
                );
                self.render().await;
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
        let ingress = self.ingress_tx.clone();
        for (id, after) in timers {
            let ingress = ingress.clone();
            let timer_task = clock.timer(after);
            let _ = work
                .spawn(async move {
                    if timer_task.await.is_ok() {
                        let _ = ingress.send(RuntimeEvent::Timer(id)).await;
                    }
                })
                .detach();
        }
    }

    async fn render(&mut self) {
        let t0 = std::time::Instant::now();

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
        let mut cx = crate::compositor::Context {
            editor: &mut self.editor,
            exit_tasks: &mut self.exit_tasks,
            exit_task_work: self.exit_task_work.clone(),
            scroll: None,
            ingress: self.ingress_tx.clone(),
            idle_reset_tx: self.idle_reset_tx.clone(),
            plugin_manager: Some(self.plugin_manager.clone()),
        };

        cx.editor.needs_redraw = false;
        let frame_setup_elapsed = frame_setup_start.elapsed();
        log_run_phase("render_setup", "frame_state", frame_setup_elapsed, || {
            format!("needs_redraw_reset={}", !cx.editor.needs_redraw)
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
            format!("cursor_visible={}", pos.is_some())
        });
        self.editor.cursor_cache.reset();

        let t2 = std::time::Instant::now(); // compositor done
        log_run_phase("render", "compositor_total", t2 - t1, || {
            format!("area={}x{}", area.width, area.height)
        });

        let pos = pos.map(|pos| (pos.col as u16, pos.row as u16));
        self.terminal.draw(pos, kind).unwrap();

        let t3 = std::time::Instant::now(); // terminal flush done
        log_run_phase("render", "flush_total", t3 - t2, || {
            format!("cursor_visible={}", pos.is_some())
        });

        // Record render sub-phases when bench is active
        if let Some(bench) = self.editor.bench.as_mut() {
            bench.render_setup.push(t1 - t0);
            bench.render_compositor.push(t2 - t1);
            bench.render_flush.push(t3 - t2);
        }
    }

    /// Render and return the frame duration. Used by benchmarks.
    #[cfg(feature = "integration")]
    pub async fn render_timed(&mut self) -> std::time::Duration {
        let start = std::time::Instant::now();
        self.render().await;
        start.elapsed()
    }

    /// Drive one benchmark action: pick a random action, feed its keys, render, record timing.
    /// Returns `true` if the bench is still running, `false` if it finished.
    fn bench_tick(&mut self) -> bool {
        use helix_view::input::Event as ViewEvent;

        // Check if bench exists and is not expired
        match self.editor.bench.as_mut() {
            None => return false,
            Some(b) if b.is_expired() => {
                let report = b.report();
                self.editor.bench = None;
                eprintln!("{report}");
                self.editor
                    .set_status("Bench complete. Report printed to stderr.");
                return false;
            }
            _ => {}
        }

        // Ensure clean state before each action
        let reset_start = std::time::Instant::now();
        let reset_stats = self.bench_reset_state();
        let reset_dur = reset_start.elapsed();

        // If buffer is too small, force an insert to replenish it
        let force_insert = reset_stats.after_lines < 100;

        // Pick action (re-borrow bench after reset_state released it)
        let bench = self.editor.bench.as_mut().unwrap();
        let (category, macro_str) = if force_insert {
            ("insert", "")
        } else {
            bench_pick_action(bench)
        };

        let action_start = std::time::Instant::now();
        let bench_context = {
            let bench = self.editor.bench.as_ref().unwrap();
            BenchCommandContext {
                seed: bench.seed,
                event_log_path: bench.event_log_path.clone(),
                action_index: bench.actions_executed + 1,
                elapsed_secs: bench.elapsed().as_secs_f64(),
                category,
                macro_str,
                force_insert,
            }
        };
        let _bench_command_guard = enter_bench_command(bench_context);

        if category == "insert" {
            // Direct Transaction insertion — bypasses compositor entirely.
            // This is safe: no mode changes, no overlays, just text insertion.
            self.bench_insert_text();
        } else {
            let keys = match helix_view::input::parse_macro(macro_str) {
                Ok(k) => k,
                Err(_) => return true,
            };

            // Feed keys through compositor
            let mut cx = crate::compositor::Context {
                editor: &mut self.editor,
                exit_tasks: &mut self.exit_tasks,
                exit_task_work: self.exit_task_work.clone(),
                scroll: None,
                ingress: self.ingress_tx.clone(),
                idle_reset_tx: self.idle_reset_tx.clone(),
                plugin_manager: Some(self.plugin_manager.clone()),
            };

            for key in &keys {
                self.compositor.handle_event(&ViewEvent::Key(*key), &mut cx);
            }
        }

        let action_dur = action_start.elapsed();
        let (post_action_lines, post_action_bytes) = self.bench_buffer_stats();

        // Store reset duration for the event loop to read (avoids double-timing)
        if let Some(bench) = self.editor.bench.as_mut() {
            bench.last_reset_dur = reset_dur;
            bench.record_action(category, action_dur);
            bench.log_slow_tick(&BenchTickTrace {
                action_index: bench.actions_executed,
                elapsed_secs: bench.elapsed().as_secs_f64(),
                category,
                macro_str,
                force_insert,
                reset: reset_stats,
                post_action_lines,
                post_action_bytes,
                reset_us: reset_dur.as_micros() as u64,
                action_us: action_dur.as_micros() as u64,
            });
        }

        self.editor.request_redraw();
        true
    }

    /// Insert a random code snippet at the cursor via direct Transaction.
    /// This bypasses the compositor entirely — no mode changes, no overlays.
    fn bench_insert_text(&mut self) {
        use helix_core::{Tendril, Transaction};

        let snippet_idx = self
            .editor
            .bench
            .as_mut()
            .map(|b| b.rand_range(BENCH_INSERT_SNIPPETS.len() as u32) as usize)
            .unwrap_or(0);
        let snippet = BENCH_INSERT_SNIPPETS[snippet_idx];

        let view_id = self.editor.tree.focus;
        let view = self.editor.tree.get(view_id);
        let doc_id = view.doc;

        if let Some(doc) = self.editor.documents.get_mut(&doc_id) {
            let text = doc.text();
            let selection = doc.selection(view_id).clone();
            let before_lines = text.len_lines();
            let before_bytes = text.len_bytes();
            let selection_count = selection.len();
            let build_start = std::time::Instant::now();
            let transaction = Transaction::insert(text, &selection, Tendril::from(snippet));
            let build_dur = build_start.elapsed();
            log_command_phase("bench_insert", "build_transaction", build_dur, || {
                format!(
                    "snippet_idx={} snippet_bytes={} selections={} lines={} bytes={}",
                    snippet_idx,
                    snippet.len(),
                    selection_count,
                    before_lines,
                    before_bytes
                )
            });
            let apply_start = std::time::Instant::now();
            doc.apply(&transaction, view_id);
            let apply_dur = apply_start.elapsed();
            log_command_phase("bench_insert", "apply", apply_dur, || {
                format!(
                    "snippet_idx={} snippet_bytes={} selections={} before_lines={} after_lines={} before_bytes={} after_bytes={}",
                    snippet_idx,
                    snippet.len(),
                    selection_count,
                    before_lines,
                    doc.text().len_lines(),
                    before_bytes,
                    doc.text().len_bytes()
                )
            });
        }
    }

    /// Force the editor into a clean normal-mode state by dismissing all
    /// compositor layers except the base EditorView and sending escapes.
    fn bench_reset_state(&mut self) -> BenchResetStats {
        use helix_view::input::{Event as ViewEvent, KeyCode, KeyEvent as VKeyEvent, KeyModifiers};

        let (before_lines, before_bytes) = self.bench_buffer_stats();
        let mut stats = BenchResetStats {
            before_lines,
            before_bytes,
            ..BenchResetStats::default()
        };

        let esc = VKeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::NONE,
        };

        // Pop all compositor layers except the bottom one (EditorView).
        // This clears prompts, pickers, menus, completion popups, etc.
        while self.compositor.layer_count() > 1 {
            self.compositor.pop();
            stats.layers_popped += 1;
        }

        // Send escapes to exit any non-normal mode (visual, insert, etc.)
        for _ in 0..5 {
            if self.editor.mode == helix_view::document::Mode::Normal {
                break;
            }
            stats.escapes_sent += 1;
            let mut cx = crate::compositor::Context {
                editor: &mut self.editor,
                exit_tasks: &mut self.exit_tasks,
                exit_task_work: self.exit_task_work.clone(),
                scroll: None,
                ingress: self.ingress_tx.clone(),
                idle_reset_tx: self.idle_reset_tx.clone(),
                plugin_manager: Some(self.plugin_manager.clone()),
            };
            self.compositor.handle_event(&ViewEvent::Key(esc), &mut cx);
        }

        // If editor wants to close (e.g., last buffer was closed), reopen scratch
        if self.editor.should_close() {
            let _ = self.editor.new_file(helix_view::editor::Action::Replace);
            stats.reopened_scratch = true;
        }

        // Prevent runaway buffer growth (e.g. paste loops creating millions of lines).
        // Only cap at extreme sizes — the bench should stress real code paths.
        {
            let view_id = self.editor.tree.focus;
            let view = self.editor.tree.get_mut(view_id);
            let doc_id = view.doc;
            if let Some(doc) = self.editor.documents.get_mut(&doc_id) {
                if doc.text().len_lines() > 10_000 {
                    let view = self.editor.tree.get_mut(view_id);
                    for _ in 0..50 {
                        if doc.text().len_lines() <= 5_000 {
                            break;
                        }
                        if !doc.undo(view) {
                            break;
                        }
                        stats.undo_steps += 1;
                    }
                }
            }
        }

        let (after_lines, after_bytes) = self.bench_buffer_stats();
        stats.after_lines = after_lines;
        stats.after_bytes = after_bytes;
        stats
    }

    fn bench_buffer_stats(&self) -> (usize, usize) {
        let view = self.editor.tree.get(self.editor.tree.focus);
        self.editor
            .documents
            .get(&view.doc)
            .map(|d| (d.text().len_lines(), d.text().len_bytes()))
            .unwrap_or((0, 0))
    }

    /// Tight bench loop: batch actions within budget, render once per batch,
    /// poll for Ctrl+C periodically. Used by both `:bench` and `hx-bench`.
    pub async fn bench_run_loop<S>(&mut self, input_stream: &mut S)
    where
        S: Stream<Item = std::io::Result<TerminalEvent>> + Unpin,
    {
        use futures_util::StreamExt;

        let _bench_run_guard = self.editor.bench.as_ref().and_then(|bench| {
            bench.event_log_path.clone().map(|event_log_path| {
                enter_bench_run(BenchRunContext {
                    seed: bench.seed,
                    event_log_path,
                })
            })
        });

        let mut last_poll = std::time::Instant::now();

        while self.editor.bench.is_some() {
            const ACTION_BUDGET: std::time::Duration = std::time::Duration::from_millis(4);

            // Poll for terminal events (Ctrl+C) every ~200ms, not every frame
            let poll_dur = if last_poll.elapsed() >= std::time::Duration::from_millis(200) {
                let poll_start = std::time::Instant::now();
                if let Ok(Some(event)) =
                    tokio::time::timeout(std::time::Duration::ZERO, input_stream.next()).await
                {
                    self.handle_terminal_events(event).await;
                    if self.editor.bench.is_none() {
                        break;
                    }
                }
                last_poll = std::time::Instant::now();
                poll_start.elapsed()
            } else {
                std::time::Duration::ZERO
            };

            let batch_start = std::time::Instant::now();
            let mut total_reset = std::time::Duration::ZERO;
            let mut bench_running = true;

            // Run actions until budget exhausted or bench ends
            while batch_start.elapsed() < ACTION_BUDGET {
                if !self.bench_tick() {
                    bench_running = false;
                    break;
                }
                if let Some(bench) = self.editor.bench.as_ref() {
                    total_reset += bench.last_reset_dur;
                }
            }

            let action_dur = batch_start.elapsed();

            if tokio::time::Instant::now() >= self.idle_timer.deadline() {
                self.service_idle_timeout(false).await;
            }

            // Single render for the whole batch
            let render_start = std::time::Instant::now();
            self.render().await;
            let render_dur = render_start.elapsed();

            let tick_dur = action_dur + render_dur + poll_dur;

            // Record frame + per-phase timing + periodic diagnostic snapshot
            if let Some(bench) = self.editor.bench.as_mut() {
                bench.record_frame(render_dur);
                bench.record_phases(
                    poll_dur,
                    total_reset,
                    action_dur.saturating_sub(total_reset),
                    render_dur,
                    tick_dur,
                );
                bench.last_tick_end = std::time::Instant::now();

                let (buf_lines, buf_bytes) = {
                    let view = self.editor.tree.get(self.editor.tree.focus);
                    self.editor
                        .documents
                        .get(&view.doc)
                        .map(|d| (d.text().len_lines(), d.text().len_bytes()))
                        .unwrap_or((0, 0))
                };
                bench.maybe_snapshot(buf_lines, buf_bytes);
            }

            if !bench_running {
                break;
            }
        }
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

                Some(signal) = self.signals.next() => {
                    if !self.handle_signals(signal).await {
                        return false;
                    };
                }
                _ = match &mut self.shutdown_rx {
                    Some(rx) => Either::Left(rx.recv()),
                    None => Either::Right(pending()),
                } => return false,
                Some(event) = input_stream.next() => {
                    self.handle_terminal_events(event).await;
                }
                Some(evt) = self.ingress_rx.recv() => {
                    self.handle_runtime_event(evt).await;
                }
                Some(event) = self.save_queue.recv() => {
                    self.editor.write_count -= 1;
                    self.handle_document_write(event.await);
                    self.render().await;
                }
                Some((id, call)) = self.lsp_incoming.next() => {
                    self.handle_language_server_message(call, id).await;
                    self.queue_redraw();
                }
                Some((id, payload)) = self.debugger_incoming.next() => {
                    let needs_render = self.editor.handle_debugger_message(id, payload).await;
                    if needs_render {
                        self.render().await;
                    }
                }
                Some(config_event) = self.config_rx.recv() => {
                    self.handle_config_events(config_event);
                    self.render().await;
                }
                Some(update) = self.assistant_updates_rx.recv() => {
                    self.handle_assistant_update(update).await;
                    self.queue_redraw();
                }
                Some(plugin_event) = self.plugin_event_rx.recv() => {
                    if let Err(err) = self.plugin_manager.fire_event(&mut self.editor, plugin_event) {
                        log::error!("Failed to fire plugin event: {}", err);
                    }
                }
                Some(()) = self.redraw_rx.recv() => {
                    if !self.editor.needs_redraw {
                        self.editor.needs_redraw = true;
                        let timeout = Instant::now() + std::time::Duration::from_millis(33);
                        if timeout < self.idle_timer.deadline() && timeout < self.redraw_timer.deadline() {
                            self.redraw_timer.as_mut().reset(timeout);
                        }
                    }
                }
                Some(()) = self.idle_reset_rx.recv() => {
                    let timeout = self.editor.config().idle_timeout;
                    self.idle_timer.as_mut().reset(Instant::now() + timeout);
                }
                _ = &mut self.idle_timer => {
                    self.idle_timer.as_mut().reset(
                        Instant::now() + std::time::Duration::from_secs(86400 * 365 * 30),
                    );
                    self.handle_idle_timeout().await;

                    #[cfg(feature = "integration")]
                    {
                        return true;
                    }
                }
                _ = &mut self.redraw_timer => {
                    self.redraw_timer
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
                Some(res) = self.exit_tasks.next() => {
                    if let Err(err) = crate::effect::apply_exit_task_result(
                        &mut self.editor,
                        self.ingress_tx.clone(),
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
                self.idle_timer.as_mut().reset(Instant::now() + timeout);
            }

            if self.editor.bench.is_some() {
                self.bench_run_loop(input_stream).await;
            }
        }
    }

    pub fn handle_config_events(&mut self, config_event: ConfigEvent) {
        self.editor.config_gen = self.editor.config_gen.wrapping_add(1);
        let old_editor_config = self.editor.config();

        match config_event {
            ConfigEvent::Refresh => self.refresh_config(),

            // Since only the Application can make changes to Editor's config,
            // the Editor must send up a new copy of a modified config so that
            // the Application can apply it.
            ConfigEvent::Update(editor_config) => {
                let mut app_config = (*self.config.load().clone()).clone();
                helix_event::dispatch(EditorConfigDidChange {
                    old_config: &app_config.editor,
                    editor: &mut self.editor,
                });
                app_config.editor = *editor_config;
                if let Err(err) = self.terminal.reconfigure((&app_config.editor).into()) {
                    self.editor.set_error(err.to_string());
                };
                self.config.store(Arc::new(app_config));
                if let Some(modal_keymaps) = &self.editor.modal_keymaps {
                    modal_keymaps.store(Arc::new(crate::keymap::to_component_modal_keymaps(
                        &self.config.load().keys,
                    )));
                }
            }
        }

        // Update all the relevant members in the editor after updating
        // the configuration.
        self.editor.refresh_config(&old_editor_config);

        // reset view position in case softwrap was enabled/disabled
        let scrolloff = self.editor.config().scrolloff;
        for (view, _) in self.editor.tree.views() {
            let doc = doc_mut!(self.editor, &view.doc);
            view.ensure_cursor_in_view(doc, scrolloff);
        }
    }

    fn refresh_config(&mut self) {
        let mut refresh_config = || -> Result<(), Error> {
            let default_config = Config::load_default()
                .map_err(|err| anyhow::anyhow!("Failed to load config: {}", err))?;

            // Update the syntax language loader before setting the theme. Setting the theme will
            // call `Loader::set_scopes` which must be done before the documents are re-parsed for
            // the sake of locals highlighting.
            let lang_loader = helix_core::config::user_lang_loader()?;
            self.editor.syn_loader.store(Arc::new(lang_loader));
            Self::load_configured_theme(
                &mut self.editor,
                &default_config,
                self.terminal.backend().supports_true_color(),
                self.theme_mode,
            );

            // Re-parse any open documents with the new language config.
            let lang_loader = self.editor.syn_loader.load();
            for document in self.editor.documents.values_mut() {
                // Re-detect .editorconfig
                document.detect_editor_config();
                document.detect_language(&lang_loader);
                let diagnostics = Editor::doc_diagnostics(
                    &self.editor.language_servers,
                    &self.editor.diagnostics,
                    document,
                );
                document.replace_diagnostics(diagnostics, &[], None);
            }

            self.terminal.reconfigure((&default_config.editor).into())?;
            // Store new config
            self.config.store(Arc::new(default_config));
            if let Some(modal_keymaps) = &self.editor.modal_keymaps {
                modal_keymaps.store(Arc::new(crate::keymap::to_component_modal_keymaps(
                    &self.config.load().keys,
                )));
            }
            Ok(())
        };

        match refresh_config() {
            Ok(_) => {
                self.editor.set_status("Config refreshed");
            }
            Err(err) => {
                self.editor.set_error(err.to_string());
            }
        }
    }

    /// Load the theme set in configuration
    fn load_configured_theme(
        editor: &mut Editor,
        config: &Config,
        terminal_true_color: bool,
        mode: Option<theme::Mode>,
    ) {
        let true_color = terminal_true_color || config.editor.true_color || crate::true_color();
        let theme = config
            .theme
            .as_ref()
            .and_then(|theme_config| {
                let theme = theme_config.choose(mode);
                editor
                    .theme_loader
                    .load(theme)
                    .map_err(|e| {
                        log::warn!("failed to load theme `{}` - {}", theme, e);
                        e
                    })
                    .ok()
                    .filter(|theme| {
                        let colors_ok = true_color || theme.is_16_color();
                        if !colors_ok {
                            log::warn!(
                                "loaded theme `{}` but cannot use it because true color \
                                support is not enabled",
                                theme.name()
                            );
                        }
                        colors_ok
                    })
            })
            .unwrap_or_else(|| editor.theme_loader.default_theme(true_color));
        editor.set_theme(theme);
    }

    #[cfg(windows)]
    // no signal handling available on windows
    pub async fn handle_signals(&mut self, _signal: ()) -> bool {
        true
    }

    #[cfg(not(windows))]
    pub async fn handle_signals(&mut self, signal: i32) -> bool {
        match signal {
            signal::SIGTSTP => {
                self.restore_term().unwrap();

                // SAFETY:
                //
                // - helix must have permissions to send signals to all processes in its signal
                //   group, either by already having the requisite permission, or by having the
                //   user's UID / EUID / SUID match that of the receiving process(es).
                let res = unsafe {
                    // A pid of 0 sends the signal to the entire process group, allowing the user to
                    // regain control of their terminal if the editor was spawned under another process
                    // (e.g. when running `git commit`).
                    //
                    // We have to send SIGSTOP (not SIGTSTP) to the entire process group, because,
                    // as mentioned above, the terminal will get stuck if `helix` was spawned from
                    // an external process and that process waits for `helix` to complete. This may
                    // be an issue with signal-hook-tokio, but the author of signal-hook believes it
                    // could be a tokio issue instead:
                    // https://github.com/vorner/signal-hook/issues/132
                    libc::kill(0, signal::SIGSTOP)
                };

                if res != 0 {
                    let err = std::io::Error::last_os_error();
                    eprintln!("{}", err);
                    let res = err.raw_os_error().unwrap_or(1);
                    std::process::exit(res);
                }
            }
            signal::SIGCONT => {
                // Copy/Paste from same issue from neovim:
                // https://github.com/neovim/neovim/issues/12322
                // https://github.com/neovim/neovim/pull/13084
                for retries in 1..=10 {
                    match self.terminal.claim() {
                        Ok(()) => break,
                        Err(err) if retries == 10 => panic!("Failed to claim terminal: {}", err),
                        Err(_) => continue,
                    }
                }

                // redraw the terminal
                let area = self.terminal.size();
                self.compositor.resize(area);
                self.terminal.clear().expect("couldn't clear terminal");

                self.render().await;
            }
            signal::SIGUSR1 => {
                self.refresh_config();
                self.render().await;
            }
            signal::SIGTERM | signal::SIGINT | signal::SIGHUP => {
                self.restore_term().unwrap();
                return false;
            }
            _ => unreachable!(),
        }

        true
    }

    async fn service_idle_timeout(&mut self, render_immediately: bool) {
        let mut cx = crate::compositor::Context {
            editor: &mut self.editor,
            exit_tasks: &mut self.exit_tasks,
            exit_task_work: self.exit_task_work.clone(),
            scroll: None,
            ingress: self.ingress_tx.clone(),
            idle_reset_tx: self.idle_reset_tx.clone(),
            plugin_manager: Some(self.plugin_manager.clone()),
        };
        let should_render = self.compositor.handle_event(&Event::IdleTimeout, &mut cx);
        let syntax_refreshed = self.editor.refresh_one_stale_syntax();
        if self.editor.has_stale_syntax() {
            let timeout = self.editor.config().idle_timeout;
            self.idle_timer.as_mut().reset(Instant::now() + timeout);
        }
        if syntax_refreshed || self.editor.has_stale_syntax() {
            log_run_event("bench_idle_service", || {
                format!(
                    "syntax_refreshed={} stale_remaining={} render_immediately={} needs_redraw={}",
                    syntax_refreshed,
                    self.editor.has_stale_syntax(),
                    render_immediately,
                    self.editor.needs_redraw
                )
            });
        }
        if render_immediately && (should_render || syntax_refreshed || self.editor.needs_redraw) {
            self.render().await;
        }
    }

    pub async fn handle_idle_timeout(&mut self) {
        self.service_idle_timeout(true).await;
    }

    pub fn handle_document_write(&mut self, doc_save_event: DocumentSavedEventResult) {
        let doc_save_event = match doc_save_event {
            Ok(event) => event,
            Err(err) => {
                self.editor.set_error(err.to_string());
                return;
            }
        };

        let doc = match self.editor.document_mut(doc_save_event.doc_id) {
            None => {
                warn!(
                    "received document saved event for non-existent doc id: {}",
                    doc_save_event.doc_id
                );

                return;
            }
            Some(doc) => doc,
        };

        debug!(
            "document {:?} saved with revision {}",
            doc.path(),
            doc_save_event.revision
        );

        doc.set_last_saved_revision(doc_save_event.revision, doc_save_event.save_time);

        let lines = doc_save_event.text.len_lines();
        let size = doc_save_event.text.len_bytes();

        enum Size {
            Bytes(u16),
            HumanReadable(f32, &'static str),
        }

        impl std::fmt::Display for Size {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    Self::Bytes(bytes) => write!(f, "{bytes}B"),
                    Self::HumanReadable(size, suffix) => write!(f, "{size:.1}{suffix}"),
                }
            }
        }

        let size = if size < 1024 {
            Size::Bytes(size as u16)
        } else {
            const SUFFIX: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
            let mut size = size as f32;
            let mut i = 0;
            while i < SUFFIX.len() - 1 && size >= 1024.0 {
                size /= 1024.0;
                i += 1;
            }
            Size::HumanReadable(size, SUFFIX[i])
        };

        self.editor
            .set_doc_path(doc_save_event.doc_id, &doc_save_event.path);
        // TODO: fix being overwritten by lsp
        self.editor.set_status(format!(
            "'{}' written, {lines}L {size}",
            get_relative_path(&doc_save_event.path).to_string_lossy(),
        ));

        // Fire OnBufferPostSave event
        if let Err(e) = self.plugin_manager.fire_event(
            &mut self.editor,
            PluginEvent {
                event_type: EventType::OnBufferPostSave,
                data: EventData::Buffer {
                    document_id: doc_save_event.doc_id,
                    path: Some(doc_save_event.path.clone()),
                },
            },
        ) {
            log::error!("Failed to fire plugin event: {}", e);
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
                if self.editor.bench.is_none() {
                    self.render().await;
                }
            }
        }

        false
    }

    pub async fn handle_terminal_events(&mut self, event: std::io::Result<TerminalEvent>) {
        #[cfg(not(windows))]
        use termina::escape::csi;

        // Cancel bench on Ctrl+C
        if self.editor.bench.is_some() {
            let is_cancel = match &event {
                #[cfg(windows)]
                Ok(crossterm::event::Event::Key(crossterm::event::KeyEvent {
                    code: crossterm::event::KeyCode::Char('c'),
                    modifiers,
                    ..
                })) => modifiers.contains(crossterm::event::KeyModifiers::CONTROL),
                #[cfg(not(windows))]
                Ok(termina::Event::Key(termina::event::KeyEvent {
                    code: termina::event::KeyCode::Char('c'),
                    modifiers,
                    ..
                })) => modifiers.contains(termina::event::KeyModifiers::CONTROL),
                _ => false,
            };
            if is_cancel {
                if let Some(mut bench) = self.editor.bench.take() {
                    let report = bench.report();
                    eprintln!("{report}");
                    self.editor
                        .set_status("Bench cancelled (Ctrl+C). Report printed to stderr.");
                    self.render().await;
                }
                return;
            }
        }

        let mut cx = crate::compositor::Context {
            editor: &mut self.editor,
            exit_tasks: &mut self.exit_tasks,
            exit_task_work: self.exit_task_work.clone(),
            scroll: None,
            ingress: self.ingress_tx.clone(),
            idle_reset_tx: self.idle_reset_tx.clone(),
            plugin_manager: Some(self.plugin_manager.clone()),
        };
        // Handle key events
        let should_redraw = match event.unwrap() {
            #[cfg(not(windows))]
            termina::Event::WindowResized(termina::WindowSize { rows, cols, .. }) => {
                self.terminal
                    .resize(Rect::new(0, 0, cols, rows))
                    .expect("Unable to resize terminal");

                let area = self.terminal.size();

                self.compositor.resize(area);

                let res = self
                    .compositor
                    .handle_event(&Event::Resize(cols, rows), &mut cx);
                self.plugin_manager
                    .fire_event(
                        &mut self.editor,
                        PluginEvent {
                            event_type: EventType::OnViewChange,
                            data: EventData::None,
                        },
                    )
                    .ok();
                res
            }
            #[cfg(not(windows))]
            // Ignore keyboard release events.
            termina::Event::Key(termina::event::KeyEvent {
                kind: termina::event::KeyEventKind::Release,
                ..
            }) => false,
            #[cfg(not(windows))]
            termina::Event::Csi(csi::Csi::Mode(csi::Mode::ReportTheme(mode))) => {
                Self::load_configured_theme(
                    &mut self.editor,
                    &self.config.load(),
                    self.terminal.backend().supports_true_color(),
                    Some(mode.into()),
                );
                true
            }
            #[cfg(windows)]
            TerminalEvent::Resize(width, height) => {
                self.terminal
                    .resize(Rect::new(0, 0, width, height))
                    .expect("Unable to resize terminal");

                let area = self.terminal.size();

                self.compositor.resize(area);

                let res = self
                    .compositor
                    .handle_event(&Event::Resize(width, height), &mut cx);
                self.plugin_manager
                    .fire_event(
                        &mut self.editor,
                        PluginEvent {
                            event_type: EventType::OnViewChange,
                            data: EventData::None,
                        },
                    )
                    .ok();
                res
            }
            #[cfg(windows)]
            // Ignore keyboard release events.
            crossterm::event::Event::Key(crossterm::event::KeyEvent {
                kind: crossterm::event::KeyEventKind::Release,
                ..
            }) => false,
            event => {
                let event: helix_view::input::Event = event.into();
                if let helix_view::input::Event::Key(key) = &event {
                    let _ = self.plugin_event_tx.send(PluginEvent {
                        event_type: EventType::OnKeyPress,
                        data: EventData::KeyPress {
                            key: key.to_string(),
                        },
                    });
                }
                self.compositor.handle_event(&event, &mut cx)
            }
        };

        if should_redraw && !self.editor.should_close() {
            self.render().await;
        }
    }

    pub async fn handle_language_server_message(
        &mut self,
        call: helix_lsp::Call,
        server_id: LanguageServerId,
    ) {
        use helix_lsp::{Call, MethodCall, Notification};

        macro_rules! language_server {
            () => {
                match self.editor.language_server_by_id(server_id) {
                    Some(language_server) => language_server,
                    None => {
                        warn!("can't find language server with id `{}`", server_id);
                        return;
                    }
                }
            };
        }

        match call {
            Call::Notification(helix_lsp::jsonrpc::Notification { method, params, .. }) => {
                let notification = match Notification::parse(&method, params) {
                    Ok(notification) => notification,
                    Err(helix_lsp::Error::Unhandled) => {
                        info!("Ignoring Unhandled notification from Language Server");
                        return;
                    }
                    Err(err) => {
                        error!(
                            "Ignoring unknown notification from Language Server: {}",
                            err
                        );
                        return;
                    }
                };

                match notification {
                    Notification::Initialized => {
                        let language_server = language_server!();

                        // Trigger a workspace/didChangeConfiguration notification after initialization.
                        // This might not be required by the spec but Neovim does this as well, so it's
                        // probably a good idea for compatibility.
                        if let Some(config) = language_server.config() {
                            language_server.did_change_configuration(config.clone());
                        }

                        helix_event::dispatch(helix_view::events::LanguageServerInitialized {
                            editor: &mut self.editor,
                            server_id,
                        });
                    }
                    Notification::PublishDiagnostics(params) => {
                        let uri = match helix_core::Uri::try_from(params.uri) {
                            Ok(uri) => uri,
                            Err(err) => {
                                log::error!("{err}");
                                return;
                            }
                        };
                        let language_server = language_server!();
                        if !language_server.is_initialized() {
                            log::error!("Discarding publishDiagnostic notification sent by an uninitialized server: {}", language_server.name());
                            return;
                        }
                        let provider = helix_core::diagnostic::DiagnosticProvider::Lsp {
                            server_id,
                            identifier: None,
                        };
                        self.editor.handle_lsp_diagnostics(
                            &provider,
                            uri,
                            params.version,
                            params.diagnostics,
                        );
                    }
                    Notification::ShowMessage(params) => {
                        self.handle_show_message(params.typ, params.message);
                    }
                    Notification::LogMessage(params) => {
                        log::debug!("window/logMessage: {:?}", params);

                        // Also show as notification if enabled
                        if self.config.load().editor.lsp.display_messages {
                            match params.typ {
                                lsp::MessageType::ERROR => {
                                    self.editor.notify_error(params.message);
                                }
                                lsp::MessageType::WARNING => {
                                    self.editor.notify_warning(params.message);
                                }
                                // Skip info messages to reduce noise from background operations
                                _ => {}
                            };
                        }
                    }
                    Notification::ProgressMessage(params)
                        if !self
                            .compositor
                            .has_component(std::any::type_name::<ui::Prompt>()) =>
                    {
                        let editor_view = self
                            .compositor
                            .find::<ui::EditorView>()
                            .expect("expected at least one EditorView");
                        let lsp::ProgressParams {
                            token,
                            value: lsp::ProgressParamsValue::WorkDone(work),
                        } = params;
                        let (title, message, percentage) = match &work {
                            lsp::WorkDoneProgress::Begin(lsp::WorkDoneProgressBegin {
                                title,
                                message,
                                percentage,
                                ..
                            }) => (Some(title), message, percentage),
                            lsp::WorkDoneProgress::Report(lsp::WorkDoneProgressReport {
                                message,
                                percentage,
                                ..
                            }) => (None, message, percentage),
                            lsp::WorkDoneProgress::End(lsp::WorkDoneProgressEnd { message }) => {
                                if message.is_some() {
                                    (None, message, &None)
                                } else {
                                    self.lsp_progress.end_progress(server_id, &token);
                                    if !self.lsp_progress.is_progressing(server_id) {
                                        editor_view.spinners_mut().get_or_create(server_id).stop();
                                    }
                                    self.editor.clear_status();

                                    // we want to render to clear any leftover spinners or messages
                                    return;
                                }
                            }
                        };

                        if self.editor.config().lsp.display_progress_messages {
                            let title =
                                title.or_else(|| self.lsp_progress.title(server_id, &token));
                            if title.is_some() || percentage.is_some() || message.is_some() {
                                use std::fmt::Write as _;
                                let mut status = format!("{}: ", language_server!().name());
                                if let Some(percentage) = percentage {
                                    write!(status, "{percentage:>2}% ").unwrap();
                                }
                                if let Some(title) = title {
                                    status.push_str(title);
                                }
                                if title.is_some() && message.is_some() {
                                    status.push_str(" ⋅ ");
                                }
                                if let Some(message) = message {
                                    status.push_str(message);
                                }
                                self.editor.set_status(status);
                            }
                        }

                        match work {
                            lsp::WorkDoneProgress::Begin(begin_status) => {
                                self.lsp_progress
                                    .begin(server_id, token.clone(), begin_status);
                            }
                            lsp::WorkDoneProgress::Report(report_status) => {
                                self.lsp_progress
                                    .update(server_id, token.clone(), report_status);
                            }
                            lsp::WorkDoneProgress::End(_) => {
                                self.lsp_progress.end_progress(server_id, &token);
                                if !self.lsp_progress.is_progressing(server_id) {
                                    editor_view.spinners_mut().get_or_create(server_id).stop();
                                };
                            }
                        }
                    }
                    Notification::ProgressMessage(_params) => {
                        // do nothing
                    }
                    Notification::Exit => {
                        self.editor.set_status("Language server exited");

                        // LSPs may produce diagnostics for files that haven't been opened in helix,
                        // we need to clear those and remove the entries from the list if this leads to
                        // an empty diagnostic list for said files
                        for diags in self.editor.diagnostics.values_mut() {
                            diags.retain(|(_, provider)| {
                                provider.language_server_id() != Some(server_id)
                            });
                        }

                        self.editor.diagnostics.retain(|_, diags| !diags.is_empty());
                        self.editor.refresh_workspace_diagnostic_counts();

                        // Clear any diagnostics for documents with this server open.
                        for doc in self.editor.documents_mut() {
                            doc.clear_diagnostics_for_language_server(server_id);
                        }

                        helix_event::dispatch(helix_view::events::LanguageServerExited {
                            editor: &mut self.editor,
                            server_id,
                        });

                        // Remove the language server from the registry.
                        self.editor.language_servers.remove_by_id(server_id);
                    }
                }
            }
            Call::MethodCall(helix_lsp::jsonrpc::MethodCall {
                method, params, id, ..
            }) => {
                let reply = match MethodCall::parse(&method, params) {
                    Err(helix_lsp::Error::Unhandled) => {
                        error!(
                            "Language Server: Method {} not found in request {}",
                            method, id
                        );
                        Err(helix_lsp::jsonrpc::Error {
                            code: helix_lsp::jsonrpc::ErrorCode::MethodNotFound,
                            message: format!("Method not found: {}", method),
                            data: None,
                        })
                    }
                    Err(err) => {
                        log::error!(
                            "Language Server: Received malformed method call {} in request {}: {}",
                            method,
                            id,
                            err
                        );
                        Err(helix_lsp::jsonrpc::Error {
                            code: helix_lsp::jsonrpc::ErrorCode::ParseError,
                            message: format!("Malformed method call: {}", method),
                            data: None,
                        })
                    }
                    Ok(MethodCall::WorkDoneProgressCreate(params)) => {
                        self.lsp_progress.create(server_id, params.token);

                        let editor_view = self
                            .compositor
                            .find::<ui::EditorView>()
                            .expect("expected at least one EditorView");
                        let spinner = editor_view.spinners_mut().get_or_create(server_id);
                        if spinner.is_stopped() {
                            spinner.start();
                        }

                        Ok(serde_json::Value::Null)
                    }
                    Ok(MethodCall::ApplyWorkspaceEdit(params)) => {
                        let language_server = language_server!();
                        if language_server.is_initialized() {
                            let offset_encoding = language_server.offset_encoding();
                            let res = self
                                .editor
                                .apply_workspace_edit(offset_encoding, &params.edit);

                            Ok(json!(lsp::ApplyWorkspaceEditResponse {
                                applied: res.is_ok(),
                                failure_reason: res.as_ref().err().map(|err| err.kind.to_string()),
                                failed_change: res
                                    .as_ref()
                                    .err()
                                    .map(|err| err.failed_change_idx as u32),
                            }))
                        } else {
                            Err(helix_lsp::jsonrpc::Error {
                                code: helix_lsp::jsonrpc::ErrorCode::InvalidRequest,
                                message: "Server must be initialized to request workspace edits"
                                    .to_string(),
                                data: None,
                            })
                        }
                    }
                    Ok(MethodCall::WorkspaceFolders) => {
                        Ok(json!(&*language_server!().workspace_folders().await))
                    }
                    Ok(MethodCall::WorkspaceConfiguration(params)) => {
                        let language_server = language_server!();
                        let result: Vec<_> = params
                            .items
                            .iter()
                            .map(|item| {
                                let mut config = language_server.config()?;
                                if let Some(section) = item.section.as_ref() {
                                    // for some reason some lsps send an empty string (observed in 'vscode-eslint-language-server')
                                    if !section.is_empty() {
                                        for part in section.split('.') {
                                            config = config.get(part)?;
                                        }
                                    }
                                }
                                Some(config)
                            })
                            .collect();
                        Ok(json!(result))
                    }
                    Ok(MethodCall::RegisterCapability(params)) => {
                        if let Some(client) = self.editor.language_servers.get_by_id(server_id) {
                            for reg in params.registrations {
                                match reg.method.as_str() {
                                    lsp::notification::DidChangeWatchedFiles::METHOD => {
                                        let Some(options) = reg.register_options else {
                                            continue;
                                        };
                                        let ops: lsp::DidChangeWatchedFilesRegistrationOptions =
                                            match serde_json::from_value(options) {
                                                Ok(ops) => ops,
                                                Err(err) => {
                                                    log::warn!("Failed to deserialize DidChangeWatchedFilesRegistrationOptions: {err}");
                                                    continue;
                                                }
                                            };
                                        self.editor.language_servers.file_event_handler.register(
                                            client.id(),
                                            Arc::downgrade(client),
                                            reg.id,
                                            ops,
                                        )
                                    }
                                    _ => {
                                        // Language Servers based on the `vscode-languageserver-node` library often send
                                        // client/registerCapability even though we do not enable dynamic registration
                                        // for most capabilities. We should send a MethodNotFound JSONRPC error in this
                                        // case but that rejects the registration promise in the server which causes an
                                        // exit. So we work around this by ignoring the request and sending back an OK
                                        // response.
                                        log::warn!("Ignoring a client/registerCapability request because dynamic capability registration is not enabled. Please report this upstream to the language server");
                                    }
                                }
                            }
                        }

                        Ok(serde_json::Value::Null)
                    }
                    Ok(MethodCall::UnregisterCapability(params)) => {
                        for unreg in params.unregisterations {
                            match unreg.method.as_str() {
                                lsp::notification::DidChangeWatchedFiles::METHOD => {
                                    self.editor
                                        .language_servers
                                        .file_event_handler
                                        .unregister(server_id, unreg.id);
                                }
                                _ => {
                                    log::warn!("Received unregistration request for unsupported method: {}", unreg.method);
                                }
                            }
                        }
                        Ok(serde_json::Value::Null)
                    }
                    Ok(MethodCall::ShowDocument(params)) => {
                        let language_server = language_server!();
                        let offset_encoding = language_server.offset_encoding();

                        let result = self.handle_show_document(params, offset_encoding);
                        Ok(json!(result))
                    }
                    Ok(MethodCall::WorkspaceDiagnosticRefresh) => {
                        let language_server = language_server!().id();

                        let documents: Vec<_> = self
                            .editor
                            .documents
                            .values()
                            .filter(|x| x.supports_language_server(language_server))
                            .map(|x| x.id())
                            .collect();

                        for document in documents {
                            crate::effect::language_server::request_document_diagnostics(
                                &mut self.editor,
                                document,
                                self.ingress_tx.clone(),
                            );
                        }

                        Ok(serde_json::Value::Null)
                    }
                    Ok(MethodCall::ShowMessageRequest(params)) => {
                        if let Some(actions) = params.actions.filter(|a| !a.is_empty()) {
                            let id = id.clone();
                            let select = ui::Select::new(
                                params.message,
                                actions,
                                (),
                                move |editor, action, event| {
                                    let reply = match event {
                                        ui::PromptEvent::Update => return,
                                        ui::PromptEvent::Validate => Some(action.clone()),
                                        ui::PromptEvent::Abort => None,
                                    };
                                    if let Some(language_server) =
                                        editor.language_server_by_id(server_id)
                                    {
                                        if let Err(err) =
                                            language_server.reply(id.clone(), Ok(json!(reply)))
                                        {
                                            log::error!(
                                                "Failed to send reply to server '{}' request {id}: {err}",
                                                language_server.name()
                                            );
                                        }
                                    }
                                },
                            );
                            self.compositor
                                .replace_or_push("lsp-show-message-request", select);
                            // Avoid sending a reply. The `Select` callback above sends the reply.
                            return;
                        } else {
                            self.handle_show_message(params.typ, params.message);
                            Ok(serde_json::Value::Null)
                        }
                    }
                };

                let language_server = language_server!();
                if let Err(err) = language_server.reply(id.clone(), reply) {
                    log::error!(
                        "Failed to send reply to server '{}' request {id}: {err}",
                        language_server.name()
                    );
                }
            }
            Call::Invalid { id } => log::error!("LSP invalid method call id={:?}", id),
        }
    }

    fn handle_show_message(&mut self, message_type: lsp::MessageType, message: String) {
        if self.config.load().editor.lsp.display_messages {
            match message_type {
                lsp::MessageType::ERROR => self.editor.set_error(message),
                lsp::MessageType::WARNING => self.editor.set_warning(message),
                _ => self.editor.set_status(message),
            }
        }
    }

    fn handle_show_document(
        &mut self,
        params: lsp::ShowDocumentParams,
        offset_encoding: helix_lsp::OffsetEncoding,
    ) -> lsp::ShowDocumentResult {
        if let lsp::ShowDocumentParams {
            external: Some(true),
            uri,
            ..
        } = params
        {
            crate::runtime::ingress::spawn_task_event_with_future(
                self.runtime.work().clone(),
                crate::open_external_url_task_event(uri),
                self.ingress_tx.clone(),
            );
            return lsp::ShowDocumentResult { success: true };
        };

        let lsp::ShowDocumentParams {
            uri,
            selection,
            take_focus,
            ..
        } = params;

        let uri = match helix_core::Uri::try_from(uri) {
            Ok(uri) => uri,
            Err(err) => {
                log::error!("{err}");
                return lsp::ShowDocumentResult { success: false };
            }
        };
        // If `Uri` gets another variant other than `Path` this may not be valid.
        let path = uri.as_path().expect("URIs are valid paths");

        let action = match take_focus {
            Some(true) => helix_view::editor::Action::Replace,
            _ => helix_view::editor::Action::VerticalSplit,
        };

        let doc_id = match self.editor.open(path, action) {
            Ok(id) => id,
            Err(err) => {
                log::error!("failed to open path: {:?}: {:?}", uri, err);
                return lsp::ShowDocumentResult { success: false };
            }
        };

        let doc = doc_mut!(self.editor, &doc_id);
        if let Some(range) = selection {
            // TODO: convert inside server
            if let Some(new_range) = lsp_range_to_range(doc.text(), range, offset_encoding) {
                let view = view_mut!(self.editor);

                // we flip the range so that the cursor sits on the start of the symbol
                // (for example start of the function).
                doc.set_selection(view.id, Selection::single(new_range.head, new_range.anchor));
                if action.align_view(view, doc.id()) {
                    align_view(doc, view, Align::Center);
                }
            } else {
                log::warn!("lsp position out of bounds - {:?}", range);
            };
        };
        lsp::ShowDocumentResult { success: true }
    }

    async fn handle_assistant_update(&mut self, update: helix_view::assistant::backend::Update) {
        let outcome = self.editor.apply_assistant_update(update);
        if let Some((thread, request)) = outcome.permission_request {
            crate::runtime::ui::assistant::apply_assistant_command(
                &mut self.editor,
                &mut self.compositor,
                self.ingress_tx.clone(),
                crate::runtime::AssistantCommand::ShowPermissionRequest { thread, request },
            );
        }
        self.editor.apply_assistant_effects(outcome.effects);
    }

    fn restore_term(&mut self) -> std::io::Result<()> {
        use helix_view::graphics::CursorKind;
        self.terminal
            .backend_mut()
            .show_cursor(CursorKind::Block)
            .ok();
        self.terminal.restore()
    }

    #[cfg(all(not(feature = "integration"), not(windows)))]
    pub fn event_stream(&self) -> impl Stream<Item = std::io::Result<TerminalEvent>> + Unpin {
        use termina::{escape::csi, Terminal as _};
        let reader = self.terminal.backend().terminal().event_reader();
        termina::EventStream::new(reader, |event| {
            // Accept either non-escape sequences or theme mode updates.
            !event.is_escape()
                || matches!(
                    event,
                    termina::Event::Csi(csi::Csi::Mode(csi::Mode::ReportTheme(_)))
                )
        })
    }

    #[cfg(all(not(feature = "integration"), windows))]
    pub fn event_stream(&self) -> impl Stream<Item = std::io::Result<TerminalEvent>> + Unpin {
        crossterm::event::EventStream::new()
    }

    #[cfg(feature = "integration")]
    pub fn event_stream(&self) -> impl Stream<Item = std::io::Result<TerminalEvent>> + Unpin {
        use std::{
            pin::Pin,
            task::{Context, Poll},
        };

        /// A dummy stream that never polls as ready.
        pub struct DummyEventStream;

        impl Stream for DummyEventStream {
            type Item = std::io::Result<TerminalEvent>;

            fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
                Poll::Pending
            }
        }

        DummyEventStream
    }

    pub async fn run<S>(&mut self, input_stream: &mut S) -> Result<i32, Error>
    where
        S: Stream<Item = std::io::Result<TerminalEvent>> + Unpin,
    {
        self.terminal.claim()?;

        self.event_loop(input_stream).await;

        let close_errs = self.close().await;

        self.restore_term()?;

        for err in close_errs {
            self.editor.exit_code = 1;
            eprintln!("Error: {}", err);
        }

        Ok(self.editor.exit_code)
    }

    pub async fn close(&mut self) -> Vec<anyhow::Error> {
        // [NOTE] we intentionally do not return early for errors because we
        //        want to try to run as much cleanup as we can, regardless of
        //        errors along the way
        let mut errs = Vec::new();

        log::debug!("waiting on pending exit-bound task work...");
        for result in std::mem::take(&mut self.exit_tasks).drain().await {
            if let Err(err) = crate::effect::apply_exit_task_result(
                &mut self.editor,
                self.ingress_tx.clone(),
                self.plugin_manager.clone(),
                result,
            ) {
                log::error!("Error finishing async UI work: {}", err);
                errs.push(err);
            }
        }

        if let Err(err) = self.editor.flush_writes().await {
            log::error!("Error writing: {}", err);
            errs.push(err);
        }

        errs.extend(self.editor.flush_assistant_persistence().await);

        if self.editor.close_language_servers(None).await.is_err() {
            log::error!("Timed out waiting for language servers to shutdown");
            errs.push(anyhow::format_err!(
                "Timed out waiting for language servers to shutdown"
            ));
        }

        errs
    }
}

impl ui::menu::Item for lsp::MessageActionItem {
    type Data = ();
    fn format(&self, _data: &Self::Data) -> tui::widgets::Row<'_> {
        self.title.as_str().into()
    }
}
