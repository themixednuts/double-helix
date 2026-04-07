//! Explicitly registered one-shot tasks to await before shutdown (see
//! `docs/runtime-executor-architecture-spec.md`).

use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::stream::{FuturesUnordered, StreamExt};
use futures_util::Stream;

use crate::task::{Task, TaskError};

/// Tasks the application must wait for before exit completes.
///
/// This is intentionally separate from [`crate::group::Group`]: groups own
/// cancellation/join trees; `Set<T>` tracks explicitly awaited one-shot completions.
pub struct Set<T> {
    tasks: FuturesUnordered<Task<T>>,
}

impl<T> Set<T> {
    pub fn new() -> Self {
        Self {
            tasks: FuturesUnordered::new(),
        }
    }

    pub fn push(&mut self, task: Task<T>) {
        self.tasks.push(task);
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub async fn next(&mut self) -> Option<Result<T, TaskError>> {
        self.tasks.next().await
    }

    /// Await every registered task in completion order.
    pub async fn drain(mut self) -> Vec<Result<T, TaskError>> {
        let mut out = Vec::with_capacity(self.tasks.len());
        while let Some(task) = self.tasks.next().await {
            out.push(task);
        }
        out
    }
}

impl<T> Default for Set<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Stream for Set<T> {
    type Item = Result<T, TaskError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.tasks).poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::RuntimeTest;
    use std::time::Duration;

    #[test]
    fn drain_runs_all_tasks() {
        let rt = RuntimeTest::default();
        let runtime = rt.runtime();
        rt.block_on(async {
            let mut set = Set::new();
            set.push(runtime.work().spawn(async {
                let _ = 1_u8;
            }));
            set.push(runtime.work().spawn(async {}));
            let results = set.drain().await;
            assert_eq!(results.len(), 2);
            assert!(results.iter().all(|r| r.is_ok()));
        });
    }

    #[test]
    fn next_yields_completion_order() {
        let rt = RuntimeTest::new_paused();
        let runtime = rt.runtime();
        rt.block_on(async {
            let mut set = Set::new();
            set.push(runtime.work().spawn(async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                1_u8
            }));
            set.push(runtime.work().spawn(async {
                tokio::time::sleep(Duration::from_millis(5)).await;
                2_u8
            }));

            tokio::time::advance(Duration::from_millis(5)).await;
            assert_eq!(set.next().await, Some(Ok(2)));
            tokio::time::advance(Duration::from_millis(15)).await;
            assert_eq!(set.next().await, Some(Ok(1)));
            assert_eq!(set.next().await, None);
        });
    }
}
