use crate::task::Task;

#[derive(Clone)]
pub struct Block {
    handle: tokio::runtime::Handle,
}

impl std::fmt::Debug for Block {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Block").finish()
    }
}

impl Block {
    pub(crate) fn new(handle: tokio::runtime::Handle) -> Self {
        Self { handle }
    }

    pub fn spawn<T>(&self, f: impl FnOnce() -> T + Send + 'static) -> Task<T>
    where
        T: Send + 'static,
    {
        Task::new(self.handle.spawn_blocking(f))
    }
}
