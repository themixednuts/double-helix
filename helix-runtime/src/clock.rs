use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::task::Task;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimerId(pub u64);

#[derive(Clone)]
pub struct Clock {
    handle: tokio::runtime::Handle,
    ids: Arc<AtomicU64>,
}

impl std::fmt::Debug for Clock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Clock").finish()
    }
}

impl Clock {
    pub(crate) fn new(handle: tokio::runtime::Handle) -> Self {
        Self {
            handle,
            ids: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn now(&self) -> Instant {
        Instant::now()
    }

    pub(crate) fn deadline_after(&self, after: Duration) -> tokio::time::Instant {
        let _guard = self.handle.enter();
        tokio::time::Instant::now() + after
    }

    pub fn next_id(&self) -> TimerId {
        TimerId(self.ids.fetch_add(1, Ordering::Relaxed))
    }

    pub fn timer(&self, after: Duration) -> Task<()> {
        Task::new(self.handle.spawn(async move {
            tokio::time::sleep(after).await;
        }))
    }

    pub(crate) fn timer_at(&self, deadline: tokio::time::Instant) -> Task<()> {
        Task::new(self.handle.spawn(async move {
            tokio::time::sleep_until(deadline).await;
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::RuntimeTest;

    #[test]
    fn timer_progresses_under_fake_time() {
        let rt = RuntimeTest::new_paused();
        let runtime = rt.runtime();
        let task = runtime.clock().timer(Duration::from_secs(5));

        rt.advance(Duration::from_secs(4));
        assert!(!task.is_finished());

        rt.advance(Duration::from_secs(1));
        rt.block_on(async {
            assert_eq!(task.await, Ok(()));
        });
    }
}
