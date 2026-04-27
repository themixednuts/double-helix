use anyhow::Context;

use super::super::Editor;

impl Editor {
    pub fn assistant_record(
        &self,
        thread: crate::assistant::thread::Id,
    ) -> Option<crate::assistant::history::Record> {
        self.assistant.record(thread)
    }

    pub fn assistant_layout_threads(
        &self,
        scope: &crate::assistant::thread::Scope,
    ) -> (
        Vec<crate::assistant::thread::Id>,
        Option<crate::assistant::thread::Id>,
    ) {
        let open = self
            .assistant
            .threads()
            .filter(|thread| thread.scope() == scope)
            .map(|thread| thread.id)
            .collect();
        let active = self.assistant.active_id().filter(|thread| {
            self.assistant
                .thread(*thread)
                .is_some_and(|state| state.scope() == scope)
        });
        (open, active)
    }

    pub fn persist_assistant_layout(&self) {
        let scope = crate::assistant::layout::current_scope();
        let (open, active) = self.assistant_layout_threads(&scope);
        self.runtime
            .work()
            .spawn(async move {
                let _ = crate::assistant::layout::save_layout(&scope, open, active).await;
            })
            .detach();
    }

    pub async fn flush_assistant_persistence(&self) -> Vec<anyhow::Error> {
        let Some(history) = self.assistant_history_backend() else {
            return Vec::new();
        };

        let mut errors = Vec::new();
        let records = self.assistant.history_records();
        let scope = crate::assistant::layout::current_scope();
        let (open, active) = self.assistant_layout_threads(&scope);

        for record in records {
            if let Err(err) = history.save(record).await {
                errors.push(err);
            }
        }
        if let Err(err) = crate::assistant::layout::save_layout(&scope, open, active).await {
            errors.push(err);
        }
        errors
    }

    pub fn save_assistant_thread(&mut self, thread: crate::assistant::thread::Id) {
        let (Some(history), Some(record)) = (
            self.assistant_services.history.clone(),
            self.assistant_record(thread),
        ) else {
            return;
        };

        self.assistant_persistence
            .saves
            .entry(thread)
            .or_insert_with(|| helix_runtime::Debounce::new(std::time::Duration::from_millis(300)))
            .restart(self.runtime.work(), self.runtime.clock(), async move {
                let _ = history.save(record).await;
            });
    }

    pub fn save_assistant_record_now(&mut self, record: crate::assistant::history::Record) {
        if let Some(debounce) = self.assistant_persistence.saves.get_mut(&record.id) {
            debounce.cancel();
        }
        if let Some(history) = self.assistant_services.history.clone() {
            self.runtime
                .work()
                .spawn(async move {
                    let _ = history.save(record).await;
                })
                .detach();
        }
    }

    pub fn delete_assistant_thread(&mut self, thread: crate::assistant::thread::Id) {
        if let Some(debounce) = self.assistant_persistence.saves.get_mut(&thread) {
            debounce.cancel();
        }
        if let Some(history) = self.assistant_services.history.clone() {
            self.runtime
                .work()
                .spawn(async move {
                    let _ = history.delete(thread).await;
                })
                .detach();
        }
    }

    pub fn debounce_assistant_layout<F>(&mut self, future: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.assistant_persistence.layout_save.restart(
            self.runtime.work(),
            self.runtime.clock(),
            future,
        );
    }

    pub fn close_active_assistant_thread(
        &mut self,
    ) -> anyhow::Result<Vec<crate::assistant::effect::Effect>> {
        let thread = self
            .assistant
            .active_id()
            .context("No active assistant thread")?;
        Ok(self.close_assistant_thread(thread))
    }
}
