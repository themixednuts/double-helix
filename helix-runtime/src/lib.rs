pub mod block;
pub mod cancel;
pub mod clock;
pub mod debounce;
pub mod frame;
pub mod gate;
pub mod group;
pub mod latest;
pub mod mailbox;
pub mod task;
pub mod test;
pub mod ui;
pub mod wait;
pub mod work;

pub use block::Block;
pub use cancel::Token;
pub use clock::{Clock, TimerId};
pub use debounce::Debounce;
pub use frame::{FrameGate, FrameHandle};
pub use gate::{Gate, Push};
pub use group::{Group, Scope, SpawnError};
pub use latest::Latest;
pub use mailbox::{channel, Closed, Receiver, Sender, TryRecvError, TrySend};
pub use task::{Local, Task, TaskError};
pub use ui::Ui;
pub use wait::Set as WaitSet;
pub use work::Work;

use std::thread::ThreadId;

#[derive(Clone)]
pub struct Runtime {
    ui: Ui,
    work: Work,
    block: Block,
    clock: Clock,
}

#[derive(Debug, thiserror::Error)]
#[error("no current tokio runtime")]
pub struct MissingRuntime;

impl Runtime {
    pub fn new(handle: tokio::runtime::Handle) -> Self {
        let thread = std::thread::current().id();
        Self {
            ui: Ui::new(thread),
            work: Work::new(handle.clone()),
            block: Block::new(handle.clone()),
            clock: Clock::new(handle),
        }
    }

    pub fn current() -> Result<Self, MissingRuntime> {
        tokio::runtime::Handle::try_current()
            .map(Self::new)
            .map_err(|_| MissingRuntime)
    }

    pub fn ui(&self) -> &Ui {
        &self.ui
    }

    pub fn work(&self) -> &Work {
        &self.work
    }

    pub fn block(&self) -> &Block {
        &self.block
    }

    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    pub fn group(&self) -> Group {
        Group::new()
    }
}

pub(crate) fn current_thread_id() -> ThreadId {
    std::thread::current().id()
}

pub fn send_blocking<T>(tx: &Sender<T>, data: T) {
    use std::time::Duration;

    if let Err(TrySend::Full(data)) = tx.try_send(data) {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                    _ = tx.send(data) => {}
                }
            });
        });
    }
}
