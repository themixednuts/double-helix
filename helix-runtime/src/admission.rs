use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::marker::PhantomData;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::{PulseGate, PulseHandle, PulseReceiver, TryRecvError};

#[derive(Debug, PartialEq, Eq)]
pub enum LatestAdmission<V> {
    Inserted,
    Replaced(V),
    Folded,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LatestAdmissionError<K, V> {
    #[error("latest-value channel is full")]
    Full(K, V),
    #[error("latest-value channel is closed")]
    Closed(K, V),
}

#[derive(Debug)]
struct LatestState<K, V> {
    capacity: usize,
    values: HashMap<K, V>,
    order: VecDeque<K>,
    receiver_open: bool,
}

#[derive(Debug)]
pub struct LatestByKeySender<K, V, Kind = ()> {
    state: Arc<Mutex<LatestState<K, V>>>,
    ready: PulseHandle<Kind>,
    space: Arc<tokio::sync::Notify>,
}

#[derive(Debug)]
pub struct LatestByKeyReceiver<K, V, Kind = ()> {
    state: Arc<Mutex<LatestState<K, V>>>,
    ready: PulseReceiver<Kind>,
    space: Arc<tokio::sync::Notify>,
}

pub fn latest_by_key<K, V>(capacity: usize) -> (LatestByKeySender<K, V>, LatestByKeyReceiver<K, V>)
where
    K: Clone + Eq + Hash,
{
    latest_by_key_for(capacity)
}

pub fn latest_by_key_for<K, V, Kind>(
    capacity: usize,
) -> (
    LatestByKeySender<K, V, Kind>,
    LatestByKeyReceiver<K, V, Kind>,
)
where
    K: Clone + Eq + Hash,
{
    assert!(
        capacity > 0,
        "latest-value channel capacity must be positive"
    );

    let mut gate = PulseGate::<Kind>::new();
    let space = Arc::new(tokio::sync::Notify::new());
    let state = Arc::new(Mutex::new(LatestState {
        capacity,
        values: HashMap::with_capacity(capacity),
        order: VecDeque::with_capacity(capacity),
        receiver_open: true,
    }));

    (
        LatestByKeySender {
            state: state.clone(),
            ready: gate.handle(),
            space: space.clone(),
        },
        LatestByKeyReceiver {
            state,
            ready: gate.take_receiver(),
            space,
        },
    )
}

impl<K, V, Kind> Clone for LatestByKeySender<K, V, Kind> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            ready: self.ready.clone(),
            space: self.space.clone(),
        }
    }
}

impl<K, V, Kind> LatestByKeySender<K, V, Kind>
where
    K: Clone + Eq + Hash,
{
    pub fn try_send(
        &self,
        key: K,
        value: V,
    ) -> Result<LatestAdmission<V>, LatestAdmissionError<K, V>> {
        let outcome = {
            let mut state = self.state.lock();
            if !state.receiver_open {
                return Err(LatestAdmissionError::Closed(key, value));
            }

            if let Some(current) = state.values.get_mut(&key) {
                LatestAdmission::Replaced(std::mem::replace(current, value))
            } else {
                if state.values.len() == state.capacity {
                    return Err(LatestAdmissionError::Full(key, value));
                }
                state.order.push_back(key.clone());
                state.values.insert(key, value);
                LatestAdmission::Inserted
            }
        };
        self.ready.request();
        Ok(outcome)
    }

    pub async fn send(
        &self,
        mut key: K,
        mut value: V,
    ) -> Result<LatestAdmission<V>, LatestAdmissionError<K, V>> {
        loop {
            let space = self.space.notified();
            match self.try_send(key, value) {
                Ok(outcome) => return Ok(outcome),
                Err(LatestAdmissionError::Closed(key, value)) => {
                    return Err(LatestAdmissionError::Closed(key, value));
                }
                Err(LatestAdmissionError::Full(next_key, next_value)) => {
                    key = next_key;
                    value = next_value;
                    space.await;
                }
            }
        }
    }

    pub fn try_fold(
        &self,
        key: K,
        value: V,
        fold: impl FnOnce(&mut V, V),
    ) -> Result<LatestAdmission<V>, LatestAdmissionError<K, V>> {
        let outcome = {
            let mut state = self.state.lock();
            if !state.receiver_open {
                return Err(LatestAdmissionError::Closed(key, value));
            }

            if let Some(current) = state.values.get_mut(&key) {
                fold(current, value);
                LatestAdmission::Folded
            } else {
                if state.values.len() == state.capacity {
                    return Err(LatestAdmissionError::Full(key, value));
                }
                state.order.push_back(key.clone());
                state.values.insert(key, value);
                LatestAdmission::Inserted
            }
        };
        self.ready.request();
        Ok(outcome)
    }

    pub fn pending_len(&self) -> usize {
        self.state.lock().values.len()
    }

    pub fn capacity(&self) -> usize {
        self.state.lock().capacity
    }
}

impl<K, V, Kind> LatestByKeyReceiver<K, V, Kind>
where
    K: Eq + Hash,
{
    pub async fn recv(&mut self) -> Option<(K, V)> {
        loop {
            if let Some(value) = self.pop() {
                return Some(value);
            }
            self.ready.recv().await?;
        }
    }

    pub fn try_recv(&mut self) -> Result<(K, V), TryRecvError> {
        loop {
            if let Some(value) = self.pop() {
                return Ok(value);
            }
            self.ready.try_recv()?;
        }
    }

    fn pop(&mut self) -> Option<(K, V)> {
        let mut state = self.state.lock();
        while let Some(key) = state.order.pop_front() {
            if let Some(value) = state.values.remove(&key) {
                drop(state);
                self.space.notify_one();
                return Some((key, value));
            }
        }
        None
    }
}

impl<K, V, Kind> Drop for LatestByKeyReceiver<K, V, Kind> {
    fn drop(&mut self) {
        let mut state = self.state.lock();
        state.receiver_open = false;
        state.values.clear();
        state.order.clear();
        drop(state);
        self.space.notify_waiters();
    }
}

#[derive(Debug)]
struct RingState<T> {
    capacity: usize,
    values: VecDeque<T>,
    receiver_open: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("ring channel is closed")]
pub struct RingClosed<T>(pub T);

#[derive(Debug)]
pub struct RingSender<T, Kind = ()> {
    state: Arc<Mutex<RingState<T>>>,
    ready: PulseHandle<Kind>,
}

#[derive(Debug)]
pub struct RingReceiver<T, Kind = ()> {
    state: Arc<Mutex<RingState<T>>>,
    ready: PulseReceiver<Kind>,
    _kind: PhantomData<fn() -> Kind>,
}

pub fn ring<T>(capacity: usize) -> (RingSender<T>, RingReceiver<T>) {
    ring_for(capacity)
}

pub fn ring_for<T, Kind>(capacity: usize) -> (RingSender<T, Kind>, RingReceiver<T, Kind>) {
    assert!(capacity > 0, "ring channel capacity must be positive");

    let mut gate = PulseGate::<Kind>::new();
    let state = Arc::new(Mutex::new(RingState {
        capacity,
        values: VecDeque::with_capacity(capacity),
        receiver_open: true,
    }));

    (
        RingSender {
            state: state.clone(),
            ready: gate.handle(),
        },
        RingReceiver {
            state,
            ready: gate.take_receiver(),
            _kind: PhantomData,
        },
    )
}

impl<T, Kind> Clone for RingSender<T, Kind> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            ready: self.ready.clone(),
        }
    }
}

impl<T, Kind> RingSender<T, Kind> {
    pub fn push(&self, value: T) -> Result<Option<T>, RingClosed<T>> {
        let evicted = {
            let mut state = self.state.lock();
            if !state.receiver_open {
                return Err(RingClosed(value));
            }
            let evicted = (state.values.len() == state.capacity)
                .then(|| state.values.pop_front())
                .flatten();
            state.values.push_back(value);
            evicted
        };
        self.ready.request();
        Ok(evicted)
    }

    pub fn pending_len(&self) -> usize {
        self.state.lock().values.len()
    }

    pub fn capacity(&self) -> usize {
        self.state.lock().capacity
    }
}

impl<T, Kind> RingReceiver<T, Kind> {
    pub async fn recv(&mut self) -> Option<T> {
        loop {
            if let Some(value) = self.state.lock().values.pop_front() {
                return Some(value);
            }
            self.ready.recv().await?;
        }
    }

    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        loop {
            if let Some(value) = self.state.lock().values.pop_front() {
                return Ok(value);
            }
            self.ready.try_recv()?;
        }
    }
}

impl<T, Kind> Drop for RingReceiver<T, Kind> {
    fn drop(&mut self) {
        let mut state = self.state.lock();
        state.receiver_open = false;
        state.values.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::RuntimeTest;

    #[test]
    fn latest_replaces_a_pending_key_without_growing() {
        let (tx, mut rx) = latest_by_key(2);
        assert_eq!(tx.try_send("doc", 1), Ok(LatestAdmission::Inserted));
        assert_eq!(tx.try_send("doc", 2), Ok(LatestAdmission::Replaced(1)));
        assert_eq!(tx.pending_len(), 1);
        assert_eq!(rx.try_recv(), Ok(("doc", 2)));
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn latest_rejects_a_new_key_at_capacity() {
        let (tx, _rx) = latest_by_key(1);
        tx.try_send("first", 1).unwrap();
        assert_eq!(
            tx.try_send("second", 2),
            Err(LatestAdmissionError::Full("second", 2))
        );
    }

    #[test]
    fn fold_reduces_pending_updates_in_place() {
        let (tx, mut rx) = latest_by_key(1);
        tx.try_send("progress", vec!["begin"]).unwrap();
        assert_eq!(
            tx.try_fold("progress", vec!["report", "end"], |current, update| {
                current.extend(update)
            }),
            Ok(LatestAdmission::Folded)
        );
        assert_eq!(
            rx.try_recv(),
            Ok(("progress", vec!["begin", "report", "end"]))
        );
    }

    #[test]
    fn latest_wakes_after_receiver_observes_empty() {
        let rt = RuntimeTest::default();
        let (tx, mut rx) = latest_by_key(1);
        rt.block_on(async move {
            tokio::spawn(async move {
                tokio::task::yield_now().await;
                tx.try_send(1, 2).unwrap();
            });
            assert_eq!(rx.recv().await, Some((1, 2)));
        });
    }

    #[test]
    fn latest_reports_closed_and_returns_the_value() {
        let (tx, rx) = latest_by_key(1);
        drop(rx);
        assert_eq!(
            tx.try_send("doc", 1),
            Err(LatestAdmissionError::Closed("doc", 1))
        );
    }

    #[test]
    fn latest_async_send_waits_for_a_distinct_key_slot() {
        let rt = RuntimeTest::default();
        let (tx, mut rx) = latest_by_key(1);
        tx.try_send("first", 1).unwrap();

        rt.block_on(async move {
            let producer = tokio::spawn(async move { tx.send("second", 2).await });
            tokio::task::yield_now().await;
            assert!(!producer.is_finished());
            assert_eq!(rx.recv().await, Some(("first", 1)));
            assert_eq!(producer.await.unwrap(), Ok(LatestAdmission::Inserted));
            assert_eq!(rx.recv().await, Some(("second", 2)));
        });
    }

    #[test]
    fn ring_evicts_oldest_and_reports_it() {
        let (tx, mut rx) = ring(2);
        assert_eq!(tx.push(1), Ok(None));
        assert_eq!(tx.push(2), Ok(None));
        assert_eq!(tx.push(3), Ok(Some(1)));
        assert_eq!(rx.try_recv(), Ok(2));
        assert_eq!(rx.try_recv(), Ok(3));
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn ring_reports_closed_and_returns_the_value() {
        let (tx, rx) = ring(1);
        drop(rx);
        assert_eq!(tx.push(1), Err(RingClosed(1)));
    }
}
