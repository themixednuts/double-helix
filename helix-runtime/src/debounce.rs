use std::future::IntoFuture;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

use crate::clock::Clock;
use crate::latest::Latest;
use crate::mailbox::{Sender, TrySend};
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
        let timer = clock.clone();
        let deadline = clock.deadline_after(self.delay);
        self.latest.restart(work, async move {
            if timer.timer_at(deadline).await.is_err() {
                return;
            }
            future.into_future().await;
        });
    }

    pub fn cancel(&mut self) {
        self.latest.cancel();
    }
}

#[derive(Debug)]
pub struct DebouncedSender<T> {
    delay: Duration,
    work: Work,
    clock: Clock,
    tx: Sender<T>,
    generation: Arc<AtomicU64>,
}

impl<T> Clone for DebouncedSender<T> {
    fn clone(&self) -> Self {
        Self {
            delay: self.delay,
            work: self.work.clone(),
            clock: self.clock.clone(),
            tx: self.tx.clone(),
            generation: self.generation.clone(),
        }
    }
}

impl<T> DebouncedSender<T>
where
    T: Send + 'static,
{
    pub fn new(delay: Duration, work: Work, clock: Clock, tx: Sender<T>) -> Self {
        Self {
            delay,
            work,
            clock,
            tx,
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn send(&self, value: T) {
        self.send_after(value, self.delay);
    }

    pub fn send_now(&self, value: T) {
        self.send_after(value, Duration::ZERO);
    }

    pub fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    pub fn send_after(&self, value: T, delay: Duration) {
        self.schedule_after(delay, move || Some(value), true);
    }

    pub fn send_after_with<F>(&self, delay: Duration, build: F)
    where
        F: FnOnce() -> Option<T> + Send + 'static,
    {
        self.schedule_after(delay, build, false);
    }

    fn schedule_after<F>(&self, delay: Duration, build: F, cancel_after_build: bool)
    where
        F: FnOnce() -> Option<T> + Send + 'static,
    {
        let ticket = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let generation = self.generation.clone();
        let clock = self.clock.clone();
        let tx = self.tx.clone();
        let deadline = (!delay.is_zero()).then(|| clock.deadline_after(delay));

        if delay.is_zero() {
            if generation.load(Ordering::Acquire) != ticket {
                return;
            }
            let Some(value) = build() else {
                return;
            };
            match tx.try_send(value) {
                Ok(()) | Err(TrySend::Closed(_)) => {}
                Err(TrySend::Full(value)) => {
                    self.spawn_delivery(ticket, generation, clock, tx, value, cancel_after_build);
                }
            }
        } else {
            let Some(deadline) = deadline else {
                return;
            };
            self.work
                .clone()
                .spawn(async move {
                    if clock.timer_at(deadline).await.is_err()
                        || generation.load(Ordering::Acquire) != ticket
                    {
                        return;
                    }

                    let Some(value) = build() else {
                        return;
                    };

                    Self::deliver(ticket, generation, clock, tx, value, cancel_after_build).await;
                })
                .detach();
        }
    }

    fn spawn_delivery(
        &self,
        ticket: u64,
        generation: Arc<AtomicU64>,
        clock: Clock,
        tx: Sender<T>,
        value: T,
        cancel_after_build: bool,
    ) {
        self.work
            .clone()
            .spawn(async move {
                Self::deliver(ticket, generation, clock, tx, value, cancel_after_build).await;
            })
            .detach();
    }

    async fn deliver(
        ticket: u64,
        generation: Arc<AtomicU64>,
        clock: Clock,
        tx: Sender<T>,
        mut value: T,
        cancel_after_build: bool,
    ) {
        loop {
            if cancel_after_build && generation.load(Ordering::Acquire) != ticket {
                return;
            }

            match tx.try_send(value) {
                Ok(()) | Err(TrySend::Closed(_)) => return,
                Err(TrySend::Full(next_value)) => {
                    value = next_value;
                    if clock.timer(Duration::from_millis(1)).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod debounced_sender_tests {
    use super::*;
    use crate::test::RuntimeTest;

    #[test]
    fn sends_only_latest_value_after_delay() {
        let rt = RuntimeTest::new_paused();
        let runtime = rt.runtime();
        let (tx, mut rx) = crate::channel(8);
        let sender = DebouncedSender::new(
            Duration::from_millis(50),
            runtime.work().clone(),
            runtime.clock().clone(),
            tx,
        );

        sender.send(1);
        rt.advance(Duration::from_millis(49));
        assert!(matches!(rx.try_recv(), Err(crate::TryRecvError::Empty)));

        sender.send(2);
        rt.advance(Duration::from_millis(50));
        assert_eq!(rx.try_recv(), Ok(2));
        assert!(matches!(rx.try_recv(), Err(crate::TryRecvError::Empty)));
    }

    #[test]
    fn send_now_delivers_without_timer_advance() {
        let rt = RuntimeTest::new_paused();
        let runtime = rt.runtime();
        let (tx, mut rx) = crate::channel(8);
        let sender = DebouncedSender::new(
            Duration::from_secs(1),
            runtime.work().clone(),
            runtime.clock().clone(),
            tx,
        );

        sender.send_now(1);
        assert_eq!(rx.try_recv(), Ok(1));
    }

    #[test]
    fn cancel_prevents_pending_delivery() {
        let rt = RuntimeTest::new_paused();
        let runtime = rt.runtime();
        let (tx, mut rx) = crate::channel(8);
        let sender = DebouncedSender::new(
            Duration::from_millis(50),
            runtime.work().clone(),
            runtime.clock().clone(),
            tx,
        );

        sender.send(1);
        sender.cancel();
        rt.advance(Duration::from_millis(50));
        assert!(matches!(rx.try_recv(), Err(crate::TryRecvError::Empty)));
    }

    #[test]
    fn retries_keep_latest_value_under_backpressure() {
        let rt = RuntimeTest::new_paused();
        let runtime = rt.runtime();
        let (tx, mut rx) = crate::channel(1);
        tx.try_send(0).unwrap();
        let sender = DebouncedSender::new(
            Duration::ZERO,
            runtime.work().clone(),
            runtime.clock().clone(),
            tx,
        );

        sender.send_now(1);
        sender.send_now(2);
        assert_eq!(rx.try_recv(), Ok(0));

        rt.advance(Duration::from_millis(1));
        assert_eq!(rx.try_recv(), Ok(2));
        assert!(matches!(rx.try_recv(), Err(crate::TryRecvError::Empty)));
    }

    #[test]
    fn builds_only_latest_delayed_value() {
        let rt = RuntimeTest::new_paused();
        let runtime = rt.runtime();
        let (tx, mut rx) = crate::channel(8);
        let sender = DebouncedSender::new(
            Duration::from_millis(50),
            runtime.work().clone(),
            runtime.clock().clone(),
            tx,
        );

        sender.send_after_with(Duration::from_millis(50), || Some(1));
        sender.send_after_with(Duration::from_millis(50), || Some(2));
        rt.advance(Duration::from_millis(50));

        assert_eq!(rx.try_recv(), Ok(2));
        assert!(matches!(rx.try_recv(), Err(crate::TryRecvError::Empty)));
    }

    #[test]
    fn built_payload_retries_even_after_newer_generation() {
        let rt = RuntimeTest::new_paused();
        let runtime = rt.runtime();
        let (tx, mut rx) = crate::channel(1);
        tx.try_send(0).unwrap();
        let sender = DebouncedSender::new(
            Duration::ZERO,
            runtime.work().clone(),
            runtime.clock().clone(),
            tx,
        );

        sender.send_after_with(Duration::ZERO, || Some(1));
        sender.send_after_with(Duration::ZERO, || Some(2));
        assert_eq!(rx.try_recv(), Ok(0));

        rt.advance(Duration::from_millis(1));
        assert_eq!(rx.try_recv(), Ok(1));
        rt.advance(Duration::from_millis(1));
        assert_eq!(rx.try_recv(), Ok(2));
        assert!(matches!(rx.try_recv(), Err(crate::TryRecvError::Empty)));
    }
}
