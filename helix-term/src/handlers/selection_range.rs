use helix_runtime::Runtime;
use helix_view::handlers::lsp::SelectionRangeResponse;

pub(super) fn spawn(
    runtime: Runtime,
    ingress: crate::runtime::RuntimeIngress,
) -> helix_runtime::Sender<SelectionRangeResponse> {
    let (tx, mut rx) = helix_runtime::channel(64);
    runtime
        .work()
        .spawn(async move {
            while let Some(response) = rx.recv().await {
                let _ = ingress
                    .send_task(crate::runtime::RuntimeTaskEvent::ApplyLspSelectionRange(
                        response,
                    ))
                    .await;
            }
        })
        .detach();
    tx
}
