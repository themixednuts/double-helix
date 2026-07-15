use std::future::IntoFuture;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::block::Block;
use crate::cancel::Token;
use crate::task::{Local, Task};
use crate::ui::Ui;
use crate::work::Work;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SpawnError {
    #[error("group is closed")]
    Closed,
}

#[derive(Clone)]
pub struct Scope {
    inner: Arc<Inner>,
}

pub struct Group {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct State {
    lifecycle: Lifecycle,
    live: usize,
    parent: Option<Guard>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Lifecycle {
    #[default]
    Open,
    Closing,
    Closed,
}

struct Inner {
    token: Token,
    state: Mutex<State>,
    notify: Notify,
    #[cfg(test)]
    before_join_wait: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

struct Guard {
    inner: Option<Arc<Inner>>,
}

impl Group {
    pub(crate) fn new() -> Self {
        Self::with_token(Token::new(), None)
    }

    fn with_token(token: Token, parent: Option<Guard>) -> Self {
        Self {
            inner: Arc::new(Inner {
                token,
                state: Mutex::new(State {
                    lifecycle: Lifecycle::Open,
                    live: 0,
                    parent,
                }),
                notify: Notify::new(),
                #[cfg(test)]
                before_join_wait: Mutex::new(None),
            }),
        }
    }

    pub fn token(&self) -> &Token {
        &self.inner.token
    }

    pub fn scope(&self) -> Scope {
        Scope {
            inner: self.inner.clone(),
        }
    }

    pub fn child(&self) -> Result<Group, SpawnError> {
        let parent = track(&self.inner)?;
        Ok(Self::with_token(self.token().child(), Some(parent)))
    }

    pub fn cancel(&self) {
        close(&self.inner, true);
    }

    pub async fn join(self) {
        close(&self.inner, false);
        loop {
            let notified = self.inner.notify.notified();
            if is_settled(&self.inner) {
                break;
            }
            #[cfg(test)]
            self.inner.run_before_join_wait();
            notified.await;
        }
    }
}

#[cfg(test)]
impl Inner {
    fn run_before_join_wait(&self) {
        let hook = self.before_join_wait.lock().take();
        if let Some(hook) = hook {
            hook();
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        close(&self.inner, true);
    }
}

impl Work {
    pub fn spawn_in<F>(&self, scope: &Scope, future: F) -> Result<Task<F::Output>, SpawnError>
    where
        F: IntoFuture + Send + 'static,
        F::IntoFuture: Send + 'static,
        F::Output: Send + 'static,
    {
        let guard = track(&scope.inner)?;
        let wrapped = async move {
            let _guard = guard;
            future.into_future().await
        };
        Ok(self.spawn(wrapped))
    }
}

impl Ui {
    pub fn spawn_in<F>(&self, scope: &Scope, future: F) -> Result<Local<F::Output>, SpawnError>
    where
        F: IntoFuture + 'static,
        F::IntoFuture: 'static,
        F::Output: 'static,
    {
        let guard = track(&scope.inner)?;
        let wrapped = async move {
            let _guard = guard;
            future.into_future().await
        };
        Ok(self.spawn(wrapped))
    }
}

impl Block {
    pub fn spawn_in<T>(
        &self,
        scope: &Scope,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Result<Task<T>, SpawnError>
    where
        T: Send + 'static,
    {
        let guard = track(&scope.inner)?;
        let wrapped = move || {
            let _guard = guard;
            f()
        };
        Ok(self.spawn(wrapped))
    }
}

fn track(inner: &Arc<Inner>) -> Result<Guard, SpawnError> {
    let mut state = inner.state.lock();
    if !matches!(state.lifecycle, Lifecycle::Open) {
        return Err(SpawnError::Closed);
    }
    state.live += 1;
    Ok(Guard {
        inner: Some(inner.clone()),
    })
}

fn close(inner: &Arc<Inner>, cancel: bool) {
    if cancel {
        inner.token.cancel();
    }

    let parent = {
        let mut state = inner.state.lock();
        if matches!(state.lifecycle, Lifecycle::Closed) {
            return;
        }
        state.lifecycle = Lifecycle::Closing;
        if state.live == 0 {
            state.lifecycle = Lifecycle::Closed;
            state.parent.take()
        } else {
            None
        }
    };
    inner.notify.notify_waiters();
    drop(parent);
}

fn is_settled(inner: &Arc<Inner>) -> bool {
    let state = inner.state.lock();
    matches!(state.lifecycle, Lifecycle::Closed)
        || (matches!(state.lifecycle, Lifecycle::Closing) && state.live == 0)
}

impl Drop for Guard {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        let parent = {
            let mut state = inner.state.lock();
            state.live = state.live.saturating_sub(1);
            if state.live == 0 && matches!(state.lifecycle, Lifecycle::Closing) {
                state.lifecycle = Lifecycle::Closed;
                state.parent.take()
            } else {
                None
            }
        };
        inner.notify.notify_waiters();
        drop(parent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::RuntimeTest;
    use std::time::Duration;

    #[test]
    fn closed_scope_rejects_spawn() {
        let rt = RuntimeTest::default();
        let runtime = rt.runtime();
        let group = runtime.group();
        let scope = group.scope();
        group.cancel();
        let result = runtime.work().spawn_in(&scope, async { 1 });
        assert_eq!(result.err(), Some(SpawnError::Closed));
    }

    #[test]
    fn join_waits_for_child_tasks() {
        let rt = RuntimeTest::new_paused();
        let runtime = rt.runtime();
        let group = runtime.group();
        let scope = group.scope();
        rt.block_on(async {
            let _task = runtime
                .work()
                .spawn_in(&scope, async {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                })
                .unwrap();
            tokio::time::advance(Duration::from_millis(10)).await;
            group.join().await;
        });
    }

    #[test]
    fn join_does_not_lose_final_guard_notification() {
        let rt = RuntimeTest::new_paused();
        let group = Group::new();
        let guard = track(&group.inner).unwrap();
        group
            .inner
            .before_join_wait
            .lock()
            .replace(Box::new(move || drop(guard)));

        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(1), group.join())
                .await
                .expect("join lost the final guard notification");
        });
    }
}
