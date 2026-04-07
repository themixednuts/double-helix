use std::future::IntoFuture;

use crate::task::Task;

#[derive(Clone)]
pub struct Work {
    handle: tokio::runtime::Handle,
}

impl std::fmt::Debug for Work {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Work").finish()
    }
}

impl Work {
    pub(crate) fn new(handle: tokio::runtime::Handle) -> Self {
        Self { handle }
    }

    pub fn spawn<F>(&self, future: F) -> Task<F::Output>
    where
        F: IntoFuture + Send + 'static,
        F::IntoFuture: Send + 'static,
        F::Output: Send + 'static,
    {
        Task::new(self.handle.spawn(future.into_future()))
    }
}
