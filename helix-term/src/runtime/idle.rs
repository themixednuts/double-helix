use helix_runtime::{PulseGate, PulseHandle, PulseReceiver, PulseRequest, TryRecvError};

#[derive(Clone, Debug, PartialEq, Eq)]
enum IdleResetPulse {}

#[derive(Debug, PartialEq, Eq)]
pub struct IdleResetRequest(PulseRequest<IdleResetPulse>);

#[derive(Debug)]
pub struct IdleResetGate(PulseGate<IdleResetPulse>);

#[derive(Clone, Debug)]
pub struct IdleResetHandle(PulseHandle<IdleResetPulse>);

#[derive(Debug)]
pub struct IdleResetReceiver(PulseReceiver<IdleResetPulse>);

impl IdleResetGate {
    pub fn new() -> Self {
        Self(PulseGate::new())
    }

    pub fn handle(&self) -> IdleResetHandle {
        IdleResetHandle(self.0.handle())
    }

    pub fn take_receiver(&mut self) -> IdleResetReceiver {
        IdleResetReceiver(self.0.take_receiver())
    }
}

impl Default for IdleResetGate {
    fn default() -> Self {
        Self::new()
    }
}

impl IdleResetHandle {
    pub fn request_reset(&self) {
        self.0.request();
    }
}

impl IdleResetReceiver {
    pub async fn recv(&mut self) -> Option<IdleResetRequest> {
        self.0.recv().await.map(IdleResetRequest)
    }

    pub fn try_recv(&mut self) -> Result<IdleResetRequest, TryRecvError> {
        self.0.try_recv().map(IdleResetRequest)
    }
}

impl futures_util::Stream for IdleResetReceiver {
    type Item = IdleResetRequest;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match std::pin::Pin::new(&mut self.0).poll_next(cx) {
            std::task::Poll::Ready(Some(request)) => {
                std::task::Poll::Ready(Some(IdleResetRequest(request)))
            }
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}
