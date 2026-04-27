use std::{future::Future, sync::Arc};

use helix_plugin::PluginManager;
use helix_runtime::{Sender as IngressSender, Work};
use helix_view::Editor;

use super::{ingress::RuntimeEvent, ExitTaskResult, ExitTaskSet, RuntimeTaskEvent};

pub fn schedule_exit_task(
    exit_tasks: &mut ExitTaskSet,
    work: &Work,
    future: impl Future<Output = anyhow::Result<RuntimeTaskEvent>> + Send + 'static,
) {
    exit_tasks.push(work.spawn(future));
}

pub fn apply_exit_task(
    editor: &mut Editor,
    ingress: IngressSender<RuntimeEvent>,
    plugin_manager: Arc<PluginManager>,
    result: ExitTaskResult,
) -> anyhow::Result<()> {
    crate::effect::apply_exit_task_result(editor, ingress, plugin_manager, result)
}

pub fn drain_exit_tasks_blocking(
    editor: &mut Editor,
    exit_tasks: &mut ExitTaskSet,
    ingress: IngressSender<RuntimeEvent>,
    plugin_manager: Arc<PluginManager>,
) -> anyhow::Result<()> {
    log::debug!("waiting on pending exit-bound task work...");
    let results =
        tokio::task::block_in_place(|| helix_lsp::block_on(std::mem::take(exit_tasks).drain()));
    for result in results {
        apply_exit_task(editor, ingress.clone(), plugin_manager.clone(), result)?;
    }
    Ok(())
}

pub async fn drain_exit_tasks_collect(
    editor: &mut Editor,
    exit_tasks: &mut ExitTaskSet,
    ingress: IngressSender<RuntimeEvent>,
    plugin_manager: Arc<PluginManager>,
) -> Vec<anyhow::Error> {
    let mut errs = Vec::new();
    log::debug!("waiting on pending exit-bound task work...");
    for result in std::mem::take(exit_tasks).drain().await {
        if let Err(err) = apply_exit_task(editor, ingress.clone(), plugin_manager.clone(), result) {
            log::error!("Error finishing async UI work: {}", err);
            errs.push(err);
        }
    }
    errs
}
