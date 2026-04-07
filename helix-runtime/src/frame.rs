use crate::{channel, send_blocking, Receiver, Sender};

#[derive(Debug)]
pub struct FrameGate {
    tx: Sender<()>,
    rx: Receiver<()>,
}

#[derive(Clone, Debug)]
pub struct FrameHandle {
    tx: Sender<()>,
}

impl FrameGate {
    pub fn new(bound: usize) -> Self {
        let (tx, rx) = channel(bound);
        Self { tx, rx }
    }

    pub fn handle(&self) -> FrameHandle {
        FrameHandle {
            tx: self.tx.clone(),
        }
    }

    pub fn request_redraw(&self) {
        self.handle().request_redraw();
    }

    pub fn take_receiver(&mut self) -> Receiver<()> {
        std::mem::replace(&mut self.rx, channel(1).1)
    }
}

impl FrameHandle {
    pub fn request_redraw(&self) {
        send_blocking(&self.tx, ());
    }

    pub async fn request_redraw_async(&self) -> Result<(), crate::Closed<()>> {
        self.tx.send(()).await
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
            assert_eq!(rx.recv().await, Some(()));
        });
    }

    #[test]
    fn frame_handle_async_request_delivers() {
        let rt = RuntimeTest::default();
        let mut gate = FrameGate::new(4);
        let handle = gate.handle();
        let mut rx = gate.take_receiver();

        rt.block_on(async {
            handle.request_redraw_async().await.unwrap();
            assert_eq!(rx.recv().await, Some(()));
        });
    }
}
