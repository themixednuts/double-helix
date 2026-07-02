use crate::{PulseGate, PulseHandle, PulseReceiver, PulseRequest, TryRecvError};

#[derive(Clone, Debug, PartialEq, Eq)]
enum FramePulse {}

#[derive(Debug, PartialEq, Eq)]
pub struct FrameRequest(PulseRequest<FramePulse>);

#[derive(Debug)]
pub struct FrameGate(PulseGate<FramePulse>);

#[derive(Clone, Debug)]
pub struct FrameHandle(PulseHandle<FramePulse>);

#[derive(Debug)]
pub struct FrameReceiver(PulseReceiver<FramePulse>);

impl FrameGate {
    pub fn new(_bound: usize) -> Self {
        Self(PulseGate::new())
    }

    pub fn handle(&self) -> FrameHandle {
        FrameHandle(self.0.handle())
    }

    pub fn request_redraw(&self) {
        self.0.request();
    }

    pub fn take_receiver(&mut self) -> FrameReceiver {
        FrameReceiver(self.0.take_receiver())
    }
}

impl FrameHandle {
    pub fn request_redraw(&self) {
        self.0.request();
    }

    pub async fn request_redraw_async(&self) -> Result<(), crate::Closed<()>> {
        self.0.request_async().await
    }
}

impl FrameReceiver {
    pub async fn recv(&mut self) -> Option<FrameRequest> {
        self.0.recv().await.map(FrameRequest)
    }

    pub fn try_recv(&mut self) -> Result<FrameRequest, TryRecvError> {
        self.0.try_recv().map(FrameRequest)
    }
}

impl futures_util::Stream for FrameReceiver {
    type Item = FrameRequest;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match std::pin::Pin::new(&mut self.0).poll_next(cx) {
            std::task::Poll::Ready(Some(request)) => {
                std::task::Poll::Ready(Some(FrameRequest(request)))
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

    #[test]
    fn frame_gate_delivers_requests() {
        let rt = RuntimeTest::default();
        let mut gate = FrameGate::new(4);
        let mut rx = gate.take_receiver();

        gate.request_redraw();

        rt.block_on(async {
            assert!(rx.recv().await.is_some());
        });
    }

    #[test]
    fn frame_gate_coalesces_pending_requests() {
        let mut gate = FrameGate::new(4);
        let mut rx = gate.take_receiver();

        gate.request_redraw();
        gate.request_redraw();

        assert!(rx.try_recv().is_ok());
        assert!(matches!(rx.try_recv(), Err(crate::TryRecvError::Empty)));
    }

    #[test]
    fn frame_gate_accepts_new_request_after_consumption() {
        let mut gate = FrameGate::new(4);
        let mut rx = gate.take_receiver();

        gate.request_redraw();
        assert!(rx.try_recv().is_ok());

        gate.request_redraw();
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn frame_handle_async_request_delivers() {
        let rt = RuntimeTest::default();
        let mut gate = FrameGate::new(4);
        let handle = gate.handle();
        let mut rx = gate.take_receiver();

        rt.block_on(async {
            handle.request_redraw_async().await.unwrap();
            assert!(rx.recv().await.is_some());
        });
    }
}
