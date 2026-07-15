use std::{
    collections::VecDeque,
    io,
    sync::{Arc, Condvar, Mutex},
    time::Instant,
};

use helix_view::graphics::Rect;
use tokio::sync::oneshot;
use tui::ratatui::buffer::Buffer;

use super::Terminal;
use crate::render::FramePacket;

pub(super) enum PresenterResync {}

const CONTROL_CAPACITY: usize = 8;
const RECYCLED_BUFFER_CAPACITY: usize = 2;
const SLOW_PRESENT_THRESHOLD: std::time::Duration = std::time::Duration::from_millis(8);

enum Control {
    Claim(oneshot::Sender<io::Result<Rect>>),
    Restore(oneshot::Sender<io::Result<()>>),
    Shutdown(oneshot::Sender<io::Result<()>>),
}

#[derive(Default)]
struct State {
    frame: Option<FramePacket>,
    config: Option<tui::terminal::Config>,
    control: VecDeque<Control>,
    recycled_buffers: Vec<Buffer>,
    closed: bool,
    replaced_frames: u64,
}

struct Shared {
    state: Mutex<State>,
    ready: Condvar,
}

pub(super) struct TerminalPresenter {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Clone)]
pub(super) struct PresenterHandle {
    shared: Arc<Shared>,
}

impl TerminalPresenter {
    pub fn spawn(
        mut terminal: Terminal,
        resync: helix_runtime::PulseHandle<PresenterResync>,
    ) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            ready: Condvar::new(),
        });
        let actor = shared.clone();
        let thread = std::thread::Builder::new()
            .name("helix-terminal-presenter".to_owned())
            .spawn(move || presenter_loop(&mut terminal, actor, resync))
            .expect("failed to spawn terminal presenter");
        Self {
            shared,
            thread: Some(thread),
        }
    }

    pub fn handle(&self) -> PresenterHandle {
        PresenterHandle {
            shared: self.shared.clone(),
        }
    }

    pub fn reconfigure(&self, config: tui::terminal::Config) -> io::Result<()> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "terminal presenter is closed",
            ));
        }
        state.config = Some(config);
        drop(state);
        self.shared.ready.notify_one();
        Ok(())
    }

    pub async fn claim(&self) -> io::Result<Rect> {
        let (tx, rx) = oneshot::channel();
        self.enqueue_control(Control::Claim(tx))?;
        rx.await.map_err(control_ack_error)?
    }

    pub async fn restore(&self) -> io::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.enqueue_control(Control::Restore(tx))?;
        rx.await.map_err(control_ack_error)?
    }

    pub async fn shutdown(mut self) -> io::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.enqueue_control(Control::Shutdown(tx))?;
        let result = rx.await.map_err(control_ack_error)?;
        if let Some(thread) = self.thread.take() {
            thread.join().map_err(|_| {
                io::Error::other("terminal presenter thread panicked during shutdown")
            })?;
        }
        result
    }

    fn enqueue_control(&self, command: Control) -> io::Result<()> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "terminal presenter is closed",
            ));
        }
        if state.control.len() == CONTROL_CAPACITY {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "terminal presenter control queue is full",
            ));
        }
        state.control.push_back(command);
        drop(state);
        self.shared.ready.notify_one();
        Ok(())
    }
}

impl PresenterHandle {
    pub fn submit(&self, frame: FramePacket) -> io::Result<()> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "terminal presenter is closed",
            ));
        }
        if let Some(replaced) = state.frame.replace(frame) {
            state
                .frame
                .as_mut()
                .expect("replacement frame was just inserted")
                .full_redraw |= replaced.full_redraw;
            state.replaced_frames = state.replaced_frames.saturating_add(1);
            recycle_buffer(&mut state.recycled_buffers, replaced.surface);
        }
        drop(state);
        self.shared.ready.notify_one();
        Ok(())
    }

    pub fn take_surface(&self, area: Rect) -> Buffer {
        let expected = tui::ratatui::to_ratatui_rect(area);
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .recycled_buffers
            .retain(|surface| *surface.area() == expected);
        state
            .recycled_buffers
            .pop()
            .unwrap_or_else(|| Buffer::empty(expected))
    }

    pub fn recycle_surface(&self, surface: Buffer) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        recycle_buffer(&mut state.recycled_buffers, surface);
    }
}

fn control_ack_error(_: oneshot::error::RecvError) -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "terminal presenter stopped before control acknowledgement",
    )
}

fn recycle_buffer(buffers: &mut Vec<Buffer>, buffer: Buffer) {
    if buffers.len() < RECYCLED_BUFFER_CAPACITY {
        buffers.push(buffer);
    }
}

impl Drop for TerminalPresenter {
    fn drop(&mut self) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        drop(state);
        self.shared.ready.notify_one();
    }
}

fn presenter_loop(
    terminal: &mut Terminal,
    shared: Arc<Shared>,
    resync: helix_runtime::PulseHandle<PresenterResync>,
) {
    let mut claimed = false;
    let mut force_full_redraw = false;
    loop {
        let (config, control, frame, replaced_frames, closed) = {
            let mut state = shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !state.closed
                && state.config.is_none()
                && state.control.is_empty()
                && state.frame.is_none()
            {
                state = shared
                    .ready
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            let config = state.config.take();
            let control = state.control.pop_front();
            let frame = state.frame.take();
            let replaced_frames = std::mem::take(&mut state.replaced_frames);
            (config, control, frame, replaced_frames, state.closed)
        };

        if let Some(config) = config {
            if let Err(error) = terminal.reconfigure(config) {
                force_full_redraw = true;
                log::error!("terminal presenter reconfiguration failed: {error}");
                resync.request();
            }
        }

        let mut shutdown = false;
        let mut can_present = claimed;
        if let Some(control) = control {
            match control {
                Control::Claim(reply) => {
                    let result = if claimed {
                        Ok(terminal.size())
                    } else {
                        terminal.claim().map(|()| terminal.size())
                    };
                    if result.is_ok() {
                        claimed = true;
                        force_full_redraw = true;
                    }
                    can_present = claimed;
                    let _ = reply.send(result);
                }
                Control::Restore(reply) => {
                    let result = if claimed { terminal.restore() } else { Ok(()) };
                    if result.is_ok() {
                        claimed = false;
                    }
                    can_present = false;
                    let _ = reply.send(result);
                }
                Control::Shutdown(reply) => {
                    let result = if claimed { terminal.restore() } else { Ok(()) };
                    claimed = false;
                    can_present = false;
                    shutdown = true;
                    let _ = reply.send(result);
                }
            }
            if shutdown {
                let mut state = shared
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.closed = true;
                state.frame = None;
                drop(state);
                shared.ready.notify_all();
                return;
            }
        }

        if can_present {
            if let Some(frame) = frame {
                let started_at = Instant::now();
                let generation = frame.generation;
                let result = terminal.present(
                    frame.area,
                    frame.surface,
                    frame.cursor,
                    frame.cursor_kind,
                    frame.full_redraw || force_full_redraw,
                );
                let elapsed = started_at.elapsed();
                match result {
                    Ok(retired) => {
                        force_full_redraw = false;
                        recycle_buffer(
                            &mut shared
                                .state
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .recycled_buffers,
                            retired,
                        );
                        if elapsed >= SLOW_PRESENT_THRESHOLD || replaced_frames > 0 {
                            log::info!(
                                target: crate::ui::picker::PICKER_TRACE_TARGET,
                                "phase=terminal_present generation={generation:?} elapsed_us={} replaced_frames={replaced_frames}",
                                elapsed.as_micros(),
                            );
                        }
                    }
                    Err(error) => {
                        force_full_redraw = true;
                        resync.request();
                        log::error!(
                            "terminal presentation failed generation={generation:?}: {error}"
                        );
                    }
                }
            }
        }

        if closed {
            if claimed {
                let _ = terminal.restore();
            }
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_runtime::{FrameScheduler, FrameSource};
    use helix_view::graphics::CursorKind;

    fn presenter_without_thread() -> TerminalPresenter {
        TerminalPresenter {
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                ready: Condvar::new(),
            }),
            thread: None,
        }
    }

    fn generation() -> helix_runtime::FrameGeneration {
        let mut frames = FrameScheduler::default();
        frames.invalidate(FrameSource::new("presenter-test"));
        frames
            .begin_frame(std::time::Instant::now())
            .expect("test invalidation must produce a generation")
    }

    fn frame(symbol: &str) -> FramePacket {
        let area = Rect::new(0, 0, 1, 1);
        let mut surface = Buffer::empty(tui::ratatui::to_ratatui_rect(area));
        surface.set_string(0, 0, symbol, tui::ratatui::style::Style::default());
        FramePacket {
            generation: generation(),
            area,
            surface,
            cursor: None,
            cursor_kind: CursorKind::Hidden,
            full_redraw: false,
        }
    }

    #[test]
    fn pending_frame_is_latest_only() {
        let presenter = presenter_without_thread();
        let handle = presenter.handle();

        handle.submit(frame("a")).unwrap();
        handle.submit(frame("b")).unwrap();

        let state = presenter
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(state.replaced_frames, 1);
        assert_eq!(state.recycled_buffers.len(), 1);
        assert_eq!(state.frame.as_ref().unwrap().surface[(0, 0)].symbol(), "b");
    }

    #[test]
    fn control_admission_is_hard_bounded() {
        let presenter = presenter_without_thread();
        let mut receivers = Vec::new();
        for _ in 0..CONTROL_CAPACITY {
            let (tx, rx) = oneshot::channel();
            presenter.enqueue_control(Control::Restore(tx)).unwrap();
            receivers.push(rx);
        }

        let (tx, _rx) = oneshot::channel();
        let error = presenter.enqueue_control(Control::Restore(tx)).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(receivers.len(), CONTROL_CAPACITY);
    }
}
