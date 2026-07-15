use std::sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    Arc,
};

use futures_util::{Stream, StreamExt};
use helix_dap::{registry::DebugAdapterId, ServerEvent};
use helix_runtime::{Receiver, RingReceiver, RingSender, Sender, Work};

const RELIABLE_CAPACITY: usize = 256;
const OUTPUT_CAPACITY: usize = 1024;

#[derive(Debug)]
pub(super) struct DapEvent {
    pub client_id: DebugAdapterId,
    pub event: ServerEvent,
}

#[derive(Clone, Debug)]
pub(super) struct DapEvents {
    reliable: Sender<DapEvent>,
    output: RingSender<DapEvent>,
    dropped_output: Arc<AtomicU64>,
    active_streams: Arc<AtomicUsize>,
}

#[derive(Debug)]
pub(super) struct DapEventReceiver {
    reliable: Receiver<DapEvent>,
    output: RingReceiver<DapEvent>,
    reliable_open: bool,
    output_open: bool,
}

impl DapEvents {
    pub fn channel() -> (Self, DapEventReceiver) {
        let (reliable, reliable_rx) = helix_runtime::channel(RELIABLE_CAPACITY);
        let (output, output_rx) = helix_runtime::ring(OUTPUT_CAPACITY);
        (
            Self {
                reliable,
                output,
                dropped_output: Arc::default(),
                active_streams: Arc::default(),
            },
            DapEventReceiver {
                reliable: reliable_rx,
                output: output_rx,
                reliable_open: true,
                output_open: true,
            },
        )
    }

    pub fn attach<S>(&self, work: Work, mut incoming: S)
    where
        S: Stream<Item = (DebugAdapterId, ServerEvent)> + Send + Unpin + 'static,
    {
        self.active_streams.fetch_add(1, Ordering::Relaxed);
        let events = self.clone();
        work.spawn(async move {
            while let Some((client_id, event)) = incoming.next().await {
                let event = DapEvent { client_id, event };
                if is_output(&event.event) {
                    match events.output.push(event) {
                        Ok(Some(_)) => {
                            let dropped =
                                events.dropped_output.fetch_add(1, Ordering::Relaxed) + 1;
                            if dropped.is_power_of_two() {
                                log::debug!(
                                    "debug-adapter output ring overwrote old records; dropped={dropped}"
                                );
                            }
                        }
                        Ok(None) => {}
                        Err(_) => break,
                    }
                } else if events.reliable.send(event).await.is_err() {
                    break;
                }
            }
            events.active_streams.fetch_sub(1, Ordering::Relaxed);
        })
        .detach();
    }

    #[cfg(test)]
    pub fn active_streams(&self) -> usize {
        self.active_streams.load(Ordering::Relaxed)
    }
}

impl DapEventReceiver {
    pub async fn recv(&mut self) -> Option<DapEvent> {
        loop {
            if !self.reliable_open && !self.output_open {
                return None;
            }
            tokio::select! {
                event = self.reliable.recv(), if self.reliable_open => match event {
                    Some(event) => return Some(event),
                    None => self.reliable_open = false,
                },
                event = self.output.recv(), if self.output_open => match event {
                    Some(event) => return Some(event),
                    None => self.output_open = false,
                },
            }
        }
    }
}

fn is_output(event: &ServerEvent) -> bool {
    matches!(
        event,
        ServerEvent::Event(helix_dap::ServerAdapterEvent {
            event: Ok(helix_dap::Event::Output(_)),
            ..
        })
    )
}
