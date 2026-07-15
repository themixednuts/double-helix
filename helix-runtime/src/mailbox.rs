#[derive(Debug, thiserror::Error)]
#[error("channel closed")]
pub struct Closed<T>(pub T);

#[derive(Debug, thiserror::Error)]
pub enum TrySend<T> {
    #[error("channel full")]
    Full(T),
    #[error("channel closed")]
    Closed(T),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TryRecvError {
    #[error("channel empty")]
    Empty,
    #[error("channel closed")]
    Closed,
}

pub fn channel<T>(bound: usize) -> (Sender<T>, Receiver<T>) {
    let (tx, rx) = tokio::sync::mpsc::channel(bound);
    (Sender { inner: tx }, Receiver { inner: rx })
}

pub struct Sender<T> {
    inner: tokio::sync::mpsc::Sender<T>,
}

pub struct Receiver<T> {
    inner: tokio::sync::mpsc::Receiver<T>,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> std::fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sender").finish()
    }
}

impl<T> std::fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Receiver").finish()
    }
}

impl<T> Sender<T> {
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    pub async fn send(&self, value: T) -> Result<(), Closed<T>> {
        self.inner.send(value).await.map_err(|err| Closed(err.0))
    }

    pub fn try_send(&self, value: T) -> Result<(), TrySend<T>> {
        self.inner.try_send(value).map_err(|err| match err {
            tokio::sync::mpsc::error::TrySendError::Full(value) => TrySend::Full(value),
            tokio::sync::mpsc::error::TrySendError::Closed(value) => TrySend::Closed(value),
        })
    }
}

impl<T> Receiver<T> {
    pub async fn recv(&mut self) -> Option<T> {
        self.inner.recv().await
    }

    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        self.inner.try_recv().map_err(|err| match err {
            tokio::sync::mpsc::error::TryRecvError::Empty => TryRecvError::Empty,
            tokio::sync::mpsc::error::TryRecvError::Disconnected => TryRecvError::Closed,
        })
    }
}

impl<T> futures_util::Stream for Receiver<T> {
    type Item = T;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.inner.poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::RuntimeTest;

    #[test]
    fn try_send_distinguishes_full_and_closed() {
        let rt = RuntimeTest::default();
        let (tx, mut rx) = channel(1);
        tx.try_send(1).unwrap();
        assert!(!tx.is_closed());
        assert!(matches!(tx.try_send(2), Err(TrySend::Full(2))));
        rt.block_on(async {
            assert_eq!(rx.recv().await, Some(1));
        });
        drop(rx);
        assert!(tx.is_closed());
        assert!(matches!(tx.try_send(3), Err(TrySend::Closed(3))));
    }
}
