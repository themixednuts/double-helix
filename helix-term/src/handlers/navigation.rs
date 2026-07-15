use helix_runtime::{channel, Runtime, Sender};
use helix_view::handlers::NavigationRequest;

pub fn spawn(
    runtime: Runtime,
    ingress: crate::runtime::RuntimeIngress,
) -> Sender<NavigationRequest> {
    let (tx, mut rx) = channel(64);
    runtime
        .work()
        .spawn(async move {
            while let Some(request) = rx.recv().await {
                if ingress
                    .send_ui(crate::runtime::UiCommand::Document(
                        crate::runtime::DocumentCommand::OpenRequested { request },
                    ))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    tx
}
