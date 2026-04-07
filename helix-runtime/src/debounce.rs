use std::future::IntoFuture;
use std::time::Duration;

use crate::clock::Clock;
use crate::latest::Latest;
use crate::work::Work;

#[derive(Debug)]
pub struct Debounce {
    delay: Duration,
    latest: Latest,
}

impl Debounce {
    pub fn new(delay: Duration) -> Self {
        Self {
            delay,
            latest: Latest::default(),
        }
    }

    pub fn restart<F>(&mut self, work: &Work, clock: &Clock, future: F)
    where
        F: IntoFuture<Output = ()> + Send + 'static,
        F::IntoFuture: Send + 'static,
    {
        let delay = self.delay;
        let timer = clock.clone();
        self.latest.restart(work, async move {
            let _ = timer.timer(delay).await;
            future.into_future().await;
        });
    }

    pub fn cancel(&mut self) {
        self.latest.cancel();
    }
}
