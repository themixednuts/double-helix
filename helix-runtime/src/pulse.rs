use std::marker::PhantomData;

use crate::{channel, Closed, Receiver, Sender, TryRecvError, TrySend};

#[derive(Debug, PartialEq, Eq)]
pub struct PulseRequest<K>(PhantomData<fn() -> K>);

#[derive(Debug)]
pub struct PulseGate<K> {
    tx: Sender<()>,
    rx: Option<PulseReceiver<K>>,
    _kind: PhantomData<fn() -> K>,
}

#[derive(Debug)]
pub struct PulseHandle<K> {
    tx: Sender<()>,
    _kind: PhantomData<fn() -> K>,
}

impl<K> Clone for PulseHandle<K> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            _kind: PhantomData,
        }
    }
}

#[derive(Debug)]
pub struct PulseReceiver<K> {
    rx: Receiver<()>,
    _kind: PhantomData<fn() -> K>,
}

impl<K> PulseGate<K> {
    pub fn new() -> Self {
        let (tx, rx) = channel(1);
        Self {
            tx,
            rx: Some(PulseReceiver {
                rx,
                _kind: PhantomData,
            }),
            _kind: PhantomData,
        }
    }

    pub fn handle(&self) -> PulseHandle<K> {
        PulseHandle {
            tx: self.tx.clone(),
            _kind: PhantomData,
        }
    }

    pub fn request(&self) {
        self.handle().request();
    }

    pub fn take_receiver(&mut self) -> PulseReceiver<K> {
        self.rx
            .take()
            .expect("pulse receiver can only be taken once")
    }
}

impl<K> Default for PulseGate<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K> PulseHandle<K> {
    pub fn request(&self) {
        match self.tx.try_send(()) {
            Ok(()) | Err(TrySend::Full(())) => {}
            Err(TrySend::Closed(())) => {}
        }
    }

    pub async fn request_async(&self) -> Result<(), crate::Closed<()>> {
        match self.tx.try_send(()) {
            Ok(()) | Err(TrySend::Full(())) => Ok(()),
            Err(TrySend::Closed(())) => Err(Closed(())),
        }
    }
}

impl<K> PulseReceiver<K> {
    pub async fn recv(&mut self) -> Option<PulseRequest<K>> {
        self.rx.recv().await?;
        Some(PulseRequest(PhantomData))
    }

    pub fn try_recv(&mut self) -> Result<PulseRequest<K>, TryRecvError> {
        self.rx.try_recv()?;
        Ok(PulseRequest(PhantomData))
    }
}

impl<K> futures_util::Stream for PulseReceiver<K> {
    type Item = PulseRequest<K>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match std::pin::Pin::new(&mut self.rx).poll_next(cx) {
            std::task::Poll::Ready(Some(())) => {
                std::task::Poll::Ready(Some(PulseRequest(PhantomData)))
            }
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::RuntimeTest;

    #[derive(Debug, PartialEq, Eq)]
    enum TestPulse {}

    #[test]
    fn pulse_gate_delivers_requests() {
        let rt = RuntimeTest::default();
        let mut gate = PulseGate::<TestPulse>::new();
        let mut rx = gate.take_receiver();

        gate.request();

        rt.block_on(async {
            assert_eq!(rx.recv().await, Some(PulseRequest(PhantomData)));
        });
    }

    #[test]
    fn pulse_gate_coalesces_pending_requests() {
        let mut gate = PulseGate::<TestPulse>::new();
        let mut rx = gate.take_receiver();

        gate.request();
        gate.request();

        assert_eq!(rx.try_recv(), Ok(PulseRequest(PhantomData)));
        assert!(matches!(rx.try_recv(), Err(crate::TryRecvError::Empty)));
    }

    #[test]
    fn pulse_gate_accepts_new_request_after_consumption() {
        let mut gate = PulseGate::<TestPulse>::new();
        let mut rx = gate.take_receiver();

        gate.request();
        assert_eq!(rx.try_recv(), Ok(PulseRequest(PhantomData)));

        gate.request();
        assert_eq!(rx.try_recv(), Ok(PulseRequest(PhantomData)));
    }

    #[test]
    fn pulse_gate_preserves_request_arriving_during_dequeue() {
        let mut gate = PulseGate::<TestPulse>::new();
        let handle = gate.handle();
        let mut rx = gate.take_receiver();

        gate.request();

        // Hold the receiver at the point where the channel slot has been
        // consumed but recv has not yet returned to its caller.
        assert_eq!(rx.rx.try_recv(), Ok(()));
        handle.request();

        assert_eq!(rx.try_recv(), Ok(PulseRequest(PhantomData)));
    }

    #[test]
    fn pulse_handle_async_request_delivers() {
        let rt = RuntimeTest::default();
        let mut gate = PulseGate::<TestPulse>::new();
        let handle = gate.handle();
        let mut rx = gate.take_receiver();

        rt.block_on(async {
            handle.request_async().await.unwrap();
            assert_eq!(rx.recv().await, Some(PulseRequest(PhantomData)));
        });
    }
}
