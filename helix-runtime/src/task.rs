use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use tokio::task::{JoinError, JoinHandle};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TaskError {
    #[error("task canceled")]
    Canceled,
    #[error("task panicked")]
    Panic,
}

#[must_use = "tasks should be stored, awaited, or detached explicitly"]
#[derive(Debug)]
pub struct Task<T> {
    handle: Option<JoinHandle<T>>,
    drop_policy: DropPolicy,
}

#[must_use = "local tasks should be stored, awaited, or detached explicitly"]
#[derive(Debug)]
pub struct Local<T> {
    handle: Option<JoinHandle<T>>,
    drop_policy: DropPolicy,
    _not_send: PhantomData<Rc<()>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DropPolicy {
    Abort,
    Detach,
}

impl DropPolicy {
    fn should_abort(self) -> bool {
        matches!(self, Self::Abort)
    }
}

impl<T> Task<T> {
    pub(crate) fn new(handle: JoinHandle<T>) -> Self {
        Self {
            handle: Some(handle),
            drop_policy: DropPolicy::Abort,
        }
    }

    pub fn cancel(&self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }

    pub fn detach(mut self) {
        self.drop_policy = DropPolicy::Detach;
    }

    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl<T> Local<T> {
    pub(crate) fn new(handle: JoinHandle<T>) -> Self {
        Self {
            handle: Some(handle),
            drop_policy: DropPolicy::Abort,
            _not_send: PhantomData,
        }
    }

    pub fn cancel(&self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }

    pub fn detach(mut self) {
        self.drop_policy = DropPolicy::Detach;
    }

    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        if self.drop_policy.should_abort() {
            if let Some(handle) = &self.handle {
                handle.abort();
            }
        }
    }
}

impl<T> Drop for Local<T> {
    fn drop(&mut self) {
        if self.drop_policy.should_abort() {
            if let Some(handle) = &self.handle {
                handle.abort();
            }
        }
    }
}

impl<T> Future for Task<T> {
    type Output = std::result::Result<T, TaskError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let handle = this.handle.as_mut().expect("task polled after detach");
        match Pin::new(handle).poll(cx) {
            Poll::Ready(result) => {
                this.drop_policy = DropPolicy::Detach;
                this.handle.take();
                Poll::Ready(map_join_result(result))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T> Future for Local<T> {
    type Output = std::result::Result<T, TaskError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let handle = this
            .handle
            .as_mut()
            .expect("local task polled after detach");
        match Pin::new(handle).poll(cx) {
            Poll::Ready(result) => {
                this.drop_policy = DropPolicy::Detach;
                this.handle.take();
                Poll::Ready(map_join_result(result))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

fn map_join_result<T>(result: Result<T, JoinError>) -> Result<T, TaskError> {
    result.map_err(|err| {
        if err.is_cancelled() {
            TaskError::Canceled
        } else {
            TaskError::Panic
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::RuntimeTest;

    #[test]
    fn task_cancel_returns_canceled() {
        let rt = RuntimeTest::default();
        rt.block_on(async {
            let task = Task::new(tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }));
            task.cancel();
            assert_eq!(task.await, Err(TaskError::Canceled));
        });
    }

    #[test]
    fn local_task_runs_on_local_set() {
        let rt = RuntimeTest::default();
        rt.block_on(async {
            tokio::task::LocalSet::new()
                .run_until(async {
                    let task = Local::new(tokio::task::spawn_local(async { 7 }));
                    assert_eq!(task.await, Ok(7));
                })
                .await;
        });
    }
}
