use std::{
    mem,
    sync::{Arc, Weak},
    time::Instant,
};

use helix_core::Transaction;

use crate::{document::SavePoint, ViewId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DocumentOpenState {
    #[default]
    Interactive,
    Preview,
}

impl DocumentOpenState {
    pub const fn is_preview(self) -> bool {
        matches!(self, Self::Preview)
    }
}

#[derive(Debug)]
pub struct DocumentSessionState {
    savepoints: Vec<Weak<SavePoint>>,
    version: i32,
    modified_since_accessed: bool,
    focused_at: Instant,
    open_state: DocumentOpenState,
}

impl Default for DocumentSessionState {
    fn default() -> Self {
        Self {
            savepoints: Vec::new(),
            version: 0,
            modified_since_accessed: false,
            focused_at: Instant::now(),
            open_state: DocumentOpenState::Interactive,
        }
    }
}

impl DocumentSessionState {
    pub fn version(&self) -> i32 {
        self.version
    }

    pub fn mark_text_changed(&mut self) {
        self.modified_since_accessed = true;
        self.version += 1;
    }

    pub fn focused_at(&self) -> Instant {
        self.focused_at
    }

    pub fn mark_as_focused(&mut self) {
        self.focused_at = Instant::now();
    }

    pub const fn open_state(&self) -> DocumentOpenState {
        self.open_state
    }

    pub fn set_open_state(&mut self, open_state: DocumentOpenState) {
        self.open_state = open_state;
    }

    pub fn take_modified_since_accessed(&mut self) -> bool {
        mem::take(&mut self.modified_since_accessed)
    }

    pub fn has_savepoints(&self) -> bool {
        !self.savepoints.is_empty()
    }

    pub fn matching_savepoint(
        &self,
        view_id: ViewId,
        revert: &Transaction,
    ) -> Option<Arc<SavePoint>> {
        self.savepoints
            .iter()
            .rev()
            .find_map(|savepoint| savepoint.upgrade())
            .and_then(|savepoint| {
                let transaction = savepoint.revert.lock();
                let matches = savepoint.view == view_id
                    && transaction.changes().is_empty()
                    && transaction.selection() == revert.selection();
                drop(transaction);
                matches.then_some(savepoint)
            })
    }

    pub fn track_savepoint(&mut self, savepoint: &Arc<SavePoint>) {
        self.savepoints.push(Arc::downgrade(savepoint));
    }

    pub fn update_savepoints(&mut self, revert: &Transaction) {
        self.savepoints
            .retain_mut(|save_point| match save_point.upgrade() {
                Some(savepoint) => {
                    let mut revert_to_savepoint = savepoint.revert.lock();
                    if revert.changes().len_after() != revert_to_savepoint.changes().len() {
                        return true;
                    }
                    *revert_to_savepoint =
                        revert.clone().compose(mem::take(&mut revert_to_savepoint));
                    true
                }
                None => false,
            });
    }

    pub fn remove_savepoint(&mut self, savepoint: &SavePoint) -> Weak<SavePoint> {
        let savepoint_idx = self
            .savepoints
            .iter()
            .position(|savepoint_ref| std::ptr::eq(savepoint_ref.as_ptr(), savepoint))
            .expect("Savepoint must belong to this document");

        self.savepoints.remove(savepoint_idx)
    }

    pub fn restore_savepoint_tracking(&mut self, savepoint_ref: Weak<SavePoint>) {
        self.savepoints.push(savepoint_ref);
    }
}
