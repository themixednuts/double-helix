use std::future::{Future, IntoFuture};

use crate::cancel::Token;
use crate::task::Task;
use crate::work::Work;

#[derive(Debug, Default)]
pub struct Latest {
    current: Option<(Token, Task<()>)>,
}

impl Latest {
    pub fn restart<F>(&mut self, work: &Work, future: F)
    where
        F: IntoFuture<Output = ()> + Send + 'static,
        F::IntoFuture: Send + 'static,
    {
        self.cancel();
        let token = Token::new();
        let task = work.spawn(future);
        self.current = Some((token, task));
    }

    pub fn restart_with<F, Fut>(&mut self, work: &Work, f: F)
    where
        F: FnOnce(Token) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.cancel();
        let token = Token::new();
        let child = token.clone();
        let task = work.spawn(async move {
            f(child).await;
        });
        self.current = Some((token, task));
    }

    pub fn cancel(&mut self) {
        if let Some((token, task)) = self.current.take() {
            token.cancel();
            task.cancel();
        }
    }

    pub fn is_running(&self) -> bool {
        self.current
            .as_ref()
            .is_some_and(|(_, task)| !task.is_finished())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::RuntimeTest;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn restart_cancels_previous_task() {
        let rt = RuntimeTest::new_paused();
        let runtime = rt.runtime();
        let hits = Arc::new(AtomicUsize::new(0));
        let mut latest = Latest::default();

        let hits2 = hits.clone();
        rt.block_on(async {
            latest.restart(&runtime.work(), async {
                tokio::time::sleep(Duration::from_secs(10)).await;
            });

            latest.restart(&runtime.work(), async move {
                hits2.fetch_add(1, Ordering::Relaxed);
            });

            tokio::task::yield_now().await;
        });
        assert_eq!(hits.load(Ordering::Relaxed), 1);
    }
}
