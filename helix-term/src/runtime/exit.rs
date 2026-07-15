use std::future::Future;

use helix_runtime::Work;
use helix_view::Editor;

use super::{ExitTaskResult, ExitTaskSet, RuntimeTaskEvent};

pub fn schedule_exit_task(
    exit_tasks: &mut ExitTaskSet,
    work: &Work,
    future: impl Future<Output = anyhow::Result<RuntimeTaskEvent>> + Send + 'static,
) {
    exit_tasks.push(work.spawn(future));
}

pub fn apply_exit_task(
    editor: &mut Editor,
    ingress: crate::runtime::RuntimeIngress,
    foreground: crate::runtime::ForegroundEvents,
    plugin_runtime: crate::plugin_registry::PluginRuntime,
    result: ExitTaskResult,
) -> anyhow::Result<()> {
    crate::effect::apply_exit_task_result(editor, ingress, foreground, plugin_runtime, result)
}

pub fn drain_exit_tasks_blocking(
    editor: &mut Editor,
    exit_tasks: &mut ExitTaskSet,
    ingress: crate::runtime::RuntimeIngress,
    foreground: crate::runtime::ForegroundEvents,
    plugin_runtime: crate::plugin_registry::PluginRuntime,
) -> anyhow::Result<()> {
    log::debug!("waiting on pending exit-bound task work...");
    let results =
        tokio::task::block_in_place(|| helix_lsp::block_on(std::mem::take(exit_tasks).drain()));
    for result in results {
        apply_exit_task(
            editor,
            ingress.clone(),
            foreground.clone(),
            plugin_runtime.clone(),
            result,
        )?;
    }
    Ok(())
}

pub async fn drain_exit_tasks_collect(
    editor: &mut Editor,
    exit_tasks: &mut ExitTaskSet,
    ingress: crate::runtime::RuntimeIngress,
    foreground: crate::runtime::ForegroundEvents,
    plugin_runtime: crate::plugin_registry::PluginRuntime,
) -> Vec<anyhow::Error> {
    let mut errs = Vec::new();
    log::debug!("waiting on pending exit-bound task work...");
    for result in std::mem::take(exit_tasks).drain().await {
        if let Err(err) = apply_exit_task(
            editor,
            ingress.clone(),
            foreground.clone(),
            plugin_runtime.clone(),
            result,
        ) {
            log::error!("Error finishing async UI work: {}", err);
            errs.push(err);
        }
    }
    errs
}
