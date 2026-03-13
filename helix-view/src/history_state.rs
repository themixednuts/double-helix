use std::collections::{HashMap, VecDeque};

use crate::{Document, DocumentId, ViewId};
use helix_core::{Selection, Transaction};

const JUMP_LIST_CAPACITY: usize = 30;

pub type Jump = (DocumentId, Selection);

#[derive(Debug, Clone)]
pub struct JumpList {
    jumps: VecDeque<Jump>,
    current: usize,
}

impl JumpList {
    pub fn new(initial: Jump) -> Self {
        let mut jumps = VecDeque::with_capacity(JUMP_LIST_CAPACITY);
        jumps.push_back(initial);
        Self { jumps, current: 0 }
    }

    fn push_impl(&mut self, jump: Jump) -> usize {
        let mut num_removed_from_front = 0;
        self.jumps.truncate(self.current);
        if self.jumps.back() != Some(&jump) {
            while self.jumps.len() >= JUMP_LIST_CAPACITY {
                self.jumps.pop_front();
                num_removed_from_front += 1;
            }

            self.jumps.push_back(jump);
            self.current = self.jumps.len();
        }
        num_removed_from_front
    }

    pub fn push(&mut self, jump: Jump) {
        self.push_impl(jump);
    }

    pub(crate) fn forward(&mut self, count: usize) -> Option<&Jump> {
        if self.current + count < self.jumps.len() {
            self.current += count;
            self.jumps.get(self.current)
        } else {
            None
        }
    }

    pub(crate) fn backward(
        &mut self,
        view_id: ViewId,
        doc: &mut Document,
        count: usize,
    ) -> Option<&Jump> {
        if let Some(mut current) = self.current.checked_sub(count) {
            if self.current == self.jumps.len() {
                let jump = (doc.id(), doc.selection(view_id).clone());
                let num_removed = self.push_impl(jump);
                current = current.saturating_sub(num_removed);
            }
            self.current = current;

            let (doc_id, selection) = self.jumps.get(self.current)?;
            if doc.id() == *doc_id && doc.selection(view_id) == selection {
                self.current = self.current.checked_sub(1)?;
            }
            self.jumps.get(self.current)
        } else {
            None
        }
    }

    pub fn remove(&mut self, doc_id: &DocumentId) {
        let old_len = self.jumps.len();
        let old_current = self.current;
        let removed_before_current = self
            .jumps
            .iter()
            .take(self.current.min(old_len))
            .filter(|(other_id, _)| other_id == doc_id)
            .count();
        self.jumps.retain(|(other_id, _)| other_id != doc_id);

        if self.jumps.is_empty() {
            self.current = 0;
            return;
        }

        self.current = if old_current == old_len {
            self.jumps.len()
        } else {
            old_current
                .saturating_sub(removed_before_current)
                .min(self.jumps.len())
        };
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Jump> {
        self.jumps.iter()
    }

    pub fn apply(&mut self, transaction: &Transaction, doc: &Document) {
        let text = doc.text().slice(..);

        for (doc_id, selection) in &mut self.jumps {
            if doc.id() == *doc_id {
                *selection = selection
                    .clone()
                    .map(transaction.changes())
                    .ensure_invariants(text);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ViewHistoryState {
    pub jumps: JumpList,
    doc_revisions: HashMap<DocumentId, usize>,
}

impl ViewHistoryState {
    pub fn new(initial_doc: DocumentId) -> Self {
        Self {
            jumps: JumpList::new((initial_doc, Selection::point(0))),
            doc_revisions: HashMap::new(),
        }
    }

    pub fn remove_document(&mut self, doc_id: &DocumentId) {
        self.jumps.remove(doc_id);
        self.doc_revisions.remove(doc_id);
    }

    pub fn apply(&mut self, transaction: &Transaction, doc: &mut Document) {
        self.jumps.apply(transaction, doc);
        self.doc_revisions
            .insert(doc.id(), doc.get_current_revision());
    }

    pub fn sync_changes(&mut self, doc: &mut Document) {
        if let Some(transaction) = self.changes_to_sync(doc) {
            self.apply(&transaction, doc);
        }
    }

    pub fn changes_to_sync(&mut self, doc: &mut Document) -> Option<Transaction> {
        let latest_revision = doc.get_current_revision();
        let current_revision = *self
            .doc_revisions
            .entry(doc.id())
            .or_insert(latest_revision);

        if current_revision == latest_revision {
            return None;
        }

        doc.with_history_mut(|history| history.changes_since(current_revision))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroUsize;

    fn doc(id: u64) -> DocumentId {
        crate::id::Id::<crate::DocumentKind, NonZeroUsize>::new(
            NonZeroUsize::new(id as usize).expect("non-zero document id"),
        )
    }

    #[test]
    fn remove_document_repairs_jump_cursor() {
        let keep_a = doc(1);
        let remove_a = doc(2);
        let keep_b = doc(3);
        let remove_b = doc(4);
        let mut jumps = JumpList {
            jumps: VecDeque::from([
                (keep_a, Selection::point(0)),
                (remove_a, Selection::point(1)),
                (keep_b, Selection::point(2)),
                (remove_b, Selection::point(3)),
            ]),
            current: 4,
        };

        jumps.remove(&remove_a);
        jumps.remove(&remove_b);

        assert_eq!(jumps.jumps.len(), 2);
        assert_eq!(jumps.current, jumps.jumps.len());
        assert_eq!(jumps.jumps[0].0, keep_a);
        assert_eq!(jumps.jumps[1].0, keep_b);
    }

    #[test]
    fn remove_document_clears_revision_tracking() {
        let keep = doc(11);
        let remove = doc(12);
        let mut history = ViewHistoryState::new(keep);
        history.doc_revisions.insert(keep, 1);
        history.doc_revisions.insert(remove, 2);

        history.remove_document(&remove);

        assert_eq!(history.doc_revisions.get(&keep), Some(&1));
        assert!(!history.doc_revisions.contains_key(&remove));
    }
}
