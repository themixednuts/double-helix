use helix_runtime::Runtime;
use helix_view::handlers::PkgEvent;

#[derive(Debug)]
pub(super) struct PkgHandler {
    work: helix_runtime::Work,
    ingress: crate::runtime::RuntimeIngress,
}

impl PkgHandler {
    fn new(work: helix_runtime::Work, ingress: crate::runtime::RuntimeIngress) -> Self {
        Self { work, ingress }
    }

    fn event(&mut self, event: PkgEvent) {
        crate::runtime::spawn_pkg_operation(
            operation_for_event(event),
            self.work.clone(),
            self.ingress.clone(),
        );
    }

    pub fn spawn(
        runtime: Runtime,
        ingress: crate::runtime::RuntimeIngress,
    ) -> helix_runtime::Sender<PkgEvent> {
        let (tx, mut rx) = helix_runtime::channel(128);
        let work = runtime.work().clone();
        work.clone()
            .spawn(async move {
                let mut handler = PkgHandler::new(work, ingress);
                while let Some(event) = rx.recv().await {
                    handler.event(event);
                }
            })
            .detach();
        tx
    }
}

fn operation_for_event(event: PkgEvent) -> crate::runtime::PkgOperation {
    match event {
        PkgEvent::AutoInstall { name } => crate::runtime::PkgOperation::Install(vec![name]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_install_event_maps_to_standard_install_operation() {
        assert_eq!(
            operation_for_event(PkgEvent::AutoInstall {
                name: "rust-analyzer".to_owned(),
            }),
            crate::runtime::PkgOperation::Install(vec!["rust-analyzer".to_owned()])
        );
    }
}
