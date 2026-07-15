//! Application events and the thread-affine foreground transaction queue.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::ingress::RuntimeDelivery;

const FOREGROUND_BOUND: usize = 1024;

#[derive(Debug)]
pub enum AppEvent {
    Runtime(RuntimeDelivery),
}

#[derive(Debug, thiserror::Error)]
pub enum ForegroundAdmissionError {
    #[error("foreground events may only be submitted by their owner thread")]
    WrongThread(RuntimeDelivery),
    #[error("foreground transaction queue is full")]
    Full(RuntimeDelivery),
}

#[derive(Debug)]
struct ForegroundState {
    queue: VecDeque<RuntimeDelivery>,
}

/// Bounded queue for effects produced while the application already owns the foreground.
///
/// The handle is `Send + Sync` so editor lifecycle hooks can retain it, but submission is
/// rejected away from the creating thread. Background work must use [`super::RuntimeIngress`].
#[derive(Clone)]
pub struct ForegroundEvents {
    owner: std::thread::ThreadId,
    state: Arc<Mutex<ForegroundState>>,
}

impl std::fmt::Debug for ForegroundEvents {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ForegroundEvents")
            .field("is_owner", &self.is_owner())
            .field("pending", &self.pending_len())
            .finish()
    }
}

impl ForegroundEvents {
    pub fn new() -> Self {
        Self {
            owner: std::thread::current().id(),
            state: Arc::new(Mutex::new(ForegroundState {
                queue: VecDeque::with_capacity(FOREGROUND_BOUND),
            })),
        }
    }

    pub fn submit(&self, delivery: RuntimeDelivery) -> Result<(), ForegroundAdmissionError> {
        if !self.is_owner() {
            return Err(ForegroundAdmissionError::WrongThread(delivery));
        }

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.queue.len() == FOREGROUND_BOUND {
            return Err(ForegroundAdmissionError::Full(delivery));
        }
        state.queue.push_back(delivery);
        Ok(())
    }

    pub fn task(&self, task: super::RuntimeTaskEvent) -> Result<(), ForegroundAdmissionError> {
        self.submit(RuntimeDelivery::Task(Box::new(task)))
    }

    pub fn ui(&self, command: super::UiCommand) -> Result<(), ForegroundAdmissionError> {
        self.submit(RuntimeDelivery::Ui(command))
    }

    pub fn plugin(
        &self,
        notification: super::PluginNotification,
    ) -> Result<(), ForegroundAdmissionError> {
        self.submit(RuntimeDelivery::Plugin(notification))
    }

    pub fn assistant_permission_resolved(
        &self,
        thread: helix_view::assistant::thread::Id,
        request: helix_view::assistant::permission::RequestId,
        decision: helix_view::assistant::permission::Decision,
    ) -> Result<(), ForegroundAdmissionError> {
        self.submit(RuntimeDelivery::AssistantPermissionResolved {
            thread,
            request,
            decision,
        })
    }

    pub(crate) fn pop(&self) -> Option<RuntimeDelivery> {
        debug_assert!(self.is_owner());
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queue
            .pop_front()
    }

    pub fn pending_len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queue
            .len()
    }

    fn is_owner(&self) -> bool {
        self.owner == std::thread::current().id()
    }
}

impl Default for ForegroundEvents {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreground_events_preserve_order() {
        let events = ForegroundEvents::new();
        events
            .task(super::super::RuntimeTaskEvent::DapRestarted)
            .unwrap();
        events
            .ui(super::super::UiCommand::Layer(
                super::super::LayerCommand::DismissPromptIfPresent,
            ))
            .unwrap();

        assert!(matches!(events.pop(), Some(RuntimeDelivery::Task(_))));
        assert!(matches!(events.pop(), Some(RuntimeDelivery::Ui(_))));
        assert!(events.pop().is_none());
    }

    #[test]
    fn foreground_events_reject_other_threads() {
        let events = ForegroundEvents::new();
        let result =
            std::thread::spawn(move || events.task(super::super::RuntimeTaskEvent::DapRestarted))
                .join()
                .unwrap();

        assert!(matches!(
            result,
            Err(ForegroundAdmissionError::WrongThread(_))
        ));
    }
}
