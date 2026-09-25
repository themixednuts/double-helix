use std::{future::Future, pin::Pin};

use helix_runtime::Work;
use helix_view::Editor;
use std::sync::{Mutex, PoisonError};

use super::{ExitTaskResult, ExitTaskSet, RuntimeTaskEvent};

type ExitFuture = Pin<Box<dyn Future<Output = anyhow::Result<RuntimeTaskEvent>> + Send>>;

/// Exit-bound work queued while an exit task's result was being applied, where the task set
/// itself isn't reachable. Moved into the set right after, so a chain of steps (save with
/// `code-actions-on-save`) keeps quitting waiting until its last step.
static CHAINED_EXIT_TASKS: Mutex<Vec<ExitFuture>> = Mutex::new(Vec::new());

pub fn schedule_exit_task(
    exit_tasks: &mut ExitTaskSet,
    work: &Work,
    future: impl Future<Output = anyhow::Result<RuntimeTaskEvent>> + Send + 'static,
) {
    exit_tasks.push(work.spawn(future));
}

/// Continue exit-bound work from code that is applying an exit task's result. The future is
/// scheduled as soon as that result has been applied, and quitting waits for it.
pub fn chain_exit_task(
    future: impl Future<Output = anyhow::Result<RuntimeTaskEvent>> + Send + 'static,
) {
    CHAINED_EXIT_TASKS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Box::pin(future));
}

/// Move chained work into `exit_tasks`. Returns whether there was any.
pub fn schedule_chained_exit_tasks(exit_tasks: &mut ExitTaskSet, work: &Work) -> bool {
    let chained = std::mem::take(
        &mut *CHAINED_EXIT_TASKS
            .lock()
            .unwrap_or_else(PoisonError::into_inner),
    );
    let scheduled = !chained.is_empty();
    for future in chained {
        schedule_exit_task(exit_tasks, work, future);
    }
    scheduled
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
    let work = editor.runtime().work().clone();
    loop {
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
        if !schedule_chained_exit_tasks(exit_tasks, &work) {
            return Ok(());
        }
    }
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
    let work = editor.runtime().work().clone();
    loop {
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
        if !schedule_chained_exit_tasks(exit_tasks, &work) {
            return errs;
        }
    }
}
