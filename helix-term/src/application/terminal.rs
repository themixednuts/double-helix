use futures_util::Stream;

use crate::runtime::PluginNotification;
use helix_plugin_api::events;
use helix_plugin_editor::adapt;
use helix_view::graphics::Rect;

use super::{Application, TerminalEvent};

impl Application {
    fn fire_view_change_event(&mut self) {
        let focused_view_id = self.editor.tree.focus;
        if let Some(view) = self.editor.tree.try_get(focused_view_id) {
            let event = events::PluginEvent::ViewFocused(events::ViewFocusedEvent {
                view: adapt::view_handle(view.id),
                document: adapt::document_handle(view.doc),
            });
            self.plugin_runtime.notify_event(event);
        }
    }

    fn handle_resize_event(&mut self, width: u16, height: u16) -> bool {
        let ingress = self.ingress().tx.clone();
        let idle_reset = self.ingress().idle_reset.clone();
        let redraw = self.editor.redraw_handle();
        let notifier = crate::handlers::local::Notifier {
            redraw: redraw.clone(),
            plugin_events: self.ingress().tx.clone().into(),
        };
        let area = Rect::new(0, 0, width, height);
        self.terminal_state.area = area;
        self.compositor.resize(area);

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
        let should_redraw = self
            .compositor
            .handle_event(&super::Event::Resize(width, height), &mut cx);
        drop(cx);
        self.drain_foreground();
        self.fire_view_change_event();
        should_redraw
    }

    #[cfg(windows)]
    fn is_bench_cancel_event(event: &std::io::Result<TerminalEvent>) -> bool {
        matches!(
            event,
            Ok(crossterm::event::Event::Key(crossterm::event::KeyEvent {
                code: crossterm::event::KeyCode::Char('c'),
                modifiers,
                ..
            })) if modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
        )
    }

    #[cfg(not(windows))]
    fn is_bench_cancel_event(event: &std::io::Result<TerminalEvent>) -> bool {
        matches!(
            event,
            Ok(termina::Event::Key(termina::event::KeyEvent {
                code: termina::event::KeyCode::Char('c'),
                modifiers,
                ..
            })) if modifiers.contains(termina::event::KeyModifiers::CONTROL)
        )
    }

    pub(super) async fn cancel_bench_if_requested(
        &mut self,
        event: &std::io::Result<TerminalEvent>,
    ) -> bool {
        if !self.editor.has_active_bench() || !Self::is_bench_cancel_event(event) {
            return false;
        }

        if let Some(report) = self.editor.cancel_bench() {
            eprintln!("{report}");
            self.editor
                .set_status("Bench cancelled (Ctrl+C). Report printed to stderr.");
            self.invalidate(super::FRAME_INPUT);
        }

        true
    }

    #[cfg(not(windows))]
    fn apply_reported_theme_mode(&mut self, mode: termina::escape::csi::Mode) -> bool {
        Self::load_configured_theme(
            &mut self.editor,
            &self.config.load(),
            self.terminal_state.supports_true_color,
            Some(mode.into()),
        );
        true
    }

    fn dispatch_terminal_input(&mut self, event: helix_view::input::Event) -> bool {
        let focused_before = self.editor.focused_view_id();
        if let helix_view::input::Event::Key(key) = &event {
            if let Err(error) = self.foreground.plugin(PluginNotification::KeyPress {
                key: key.to_string(),
            }) {
                self.editor.set_error(error.to_string());
            }
        }

        let ingress = self.ingress().tx.clone();
        let idle_reset = self.ingress().idle_reset.clone();
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
        let handled = self.compositor.handle_event(&event, &mut cx);
        drop(cx);
        self.drain_foreground();
        if self.editor.focused_view_id() != focused_before {
            self.fire_view_change_event();
        }
        handled
    }

    #[cfg(windows)]
    pub async fn handle_signals(&mut self, _signal: ()) -> bool {
        true
    }

    #[cfg(not(windows))]
    pub async fn handle_signals(&mut self, signal: i32) -> bool {
        match signal {
            signal_hook::consts::signal::SIGTSTP => {
                self.restore_term().await.unwrap();

                let res = unsafe { libc::kill(0, signal_hook::consts::signal::SIGSTOP) };

                if res != 0 {
                    let err = std::io::Error::last_os_error();
                    eprintln!("{}", err);
                    let res = err.raw_os_error().unwrap_or(1);
                    crate::logging::flush();
                    std::process::exit(res);
                }
            }
            signal_hook::consts::signal::SIGCONT => {
                for retries in 1..=10 {
                    match self
                        .presenter
                        .as_ref()
                        .expect("terminal presenter must exist while handling signals")
                        .claim()
                        .await
                    {
                        Ok(area) => {
                            self.terminal_state.area = area;
                            self.compositor.resize(area);
                            break;
                        }
                        Err(err) if retries == 10 => panic!("Failed to claim terminal: {}", err),
                        Err(_) => tokio::task::yield_now().await,
                    }
                }

                self.compositor.full_redraw = true;
                self.invalidate(super::FRAME_CONFIG);
            }
            signal_hook::consts::signal::SIGUSR1 => {
                self.refresh_config();
                self.invalidate(super::FRAME_CONFIG);
            }
            signal_hook::consts::signal::SIGTERM
            | signal_hook::consts::signal::SIGINT
            | signal_hook::consts::signal::SIGHUP => {
                self.restore_term().await.unwrap();
                return false;
            }
            _ => unreachable!(),
        }

        true
    }

    pub async fn handle_terminal_events(&mut self, event: std::io::Result<TerminalEvent>) -> bool {
        #[cfg(not(windows))]
        use termina::escape::csi;

        if self.cancel_bench_if_requested(&event).await {
            return true;
        }

        let event = match event {
            Ok(event) => event,
            Err(error) => {
                self.editor.exit_code = 1;
                log::error!("terminal input failed: {error}");
                return false;
            }
        };
        let should_redraw = match event {
            #[cfg(not(windows))]
            termina::Event::WindowResized(termina::WindowSize { rows, cols, .. }) => {
                self.handle_resize_event(cols, rows)
            }
            #[cfg(not(windows))]
            termina::Event::Key(termina::event::KeyEvent {
                kind: termina::event::KeyEventKind::Release,
                ..
            }) => false,
            #[cfg(not(windows))]
            termina::Event::Csi(csi::Csi::Mode(csi::Mode::ReportTheme(mode))) => {
                self.apply_reported_theme_mode(mode)
            }
            #[cfg(windows)]
            TerminalEvent::Resize(width, height) => self.handle_resize_event(width, height),
            #[cfg(windows)]
            crossterm::event::Event::Key(crossterm::event::KeyEvent {
                kind: crossterm::event::KeyEventKind::Release,
                ..
            }) => false,
            event => {
                let event: helix_view::input::Event = event.into();
                self.dispatch_terminal_input(event)
            }
        };

        if should_redraw && !self.editor.should_close() {
            self.invalidate(super::FRAME_INPUT);
        }
        true
    }

    #[cfg(all(not(feature = "integration"), not(windows)))]
    pub fn event_stream(&self) -> impl Stream<Item = std::io::Result<TerminalEvent>> + Unpin {
        use termina::{escape::csi, Terminal as _};
        let reader = self
            .terminal
            .as_ref()
            .expect("terminal event stream must be created before application run")
            .backend()
            .terminal()
            .event_reader();
        termina::EventStream::new(reader, |event| {
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

        pub struct DummyEventStream;

        impl Stream for DummyEventStream {
            type Item = std::io::Result<TerminalEvent>;

            fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
                Poll::Pending
            }
        }

        DummyEventStream
    }
}
