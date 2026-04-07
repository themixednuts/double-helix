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
    open: bool,
    live: usize,
    finalized: bool,
    parent: Option<Guard>,
}

struct Inner {
    token: Token,
    state: Mutex<State>,
    notify: Notify,
}

struct Guard {
    inner: Arc<Inner>,
    active: bool,
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
                    open: true,
                    live: 0,
                    finalized: false,
                    parent,
                }),
                notify: Notify::new(),
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
            if is_settled(&self.inner) {
                break;
            }
            self.inner.notify.notified().await;
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
    if !state.open {
        return Err(SpawnError::Closed);
    }
    state.live += 1;
    Ok(Guard {
        inner: inner.clone(),
        active: true,
    })
}

fn close(inner: &Arc<Inner>, cancel: bool) {
    if cancel {
        inner.token.cancel();
    }

    let parent = {
        let mut state = inner.state.lock();
        if state.finalized {
            return;
        }
        state.open = false;
        if state.live == 0 {
            state.finalized = true;
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
    state.finalized || (!state.open && state.live == 0)
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let parent = {
            let mut state = self.inner.state.lock();
            state.live = state.live.saturating_sub(1);
            if state.live == 0 && !state.open && !state.finalized {
                state.finalized = true;
                state.parent.take()
            } else {
                None
            }
        };
        self.inner.notify.notify_waiters();
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
}
