use crate::DocumentId;

use super::{Action, Editor};

impl Editor {
    pub fn open_assistant_entry_doc(
        &mut self,
        thread: crate::assistant::thread::Id,
        entry: crate::assistant::thread::EntryId,
        action: Action,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::OpenEntryDoc {
            thread,
            entry,
            action,
        })
    }

    pub fn open_assistant_entry_scratch(
        &mut self,
        thread: crate::assistant::thread::Id,
        entry: crate::assistant::thread::EntryId,
        action: Action,
    ) -> Option<Vec<crate::assistant::effect::Effect>> {
        let details = self.assistant.panel(false).entry_markdown(entry)?;
        let opened = self.assistant.panel(false).opened_doc(entry);

        if let Some(doc_id) = opened {
            if self.switch_document_if_exists(doc_id, action) {
                return Some(Vec::new());
            }
            let effects = self.untrack_assistant_entry_doc(thread, entry);
            let doc_id = self.open_markdown_scratch(action, details);
            let mut next = self.track_assistant_entry_doc(thread, entry, doc_id);
            let mut all = effects;
            all.append(&mut next);
            return Some(all);
        }

        let doc_id = self.open_markdown_scratch(action, details);
        Some(self.track_assistant_entry_doc(thread, entry, doc_id))
    }

    pub fn open_selected_assistant_entry_scratch(
        &mut self,
        action: Action,
    ) -> Option<Vec<crate::assistant::effect::Effect>> {
        let entry = self.assistant.panel(false).selected_entry_id()?;
        let thread = self.assistant.panel(false).active_id()?;
        self.open_assistant_entry_scratch(thread, entry, action)
    }

    pub fn untrack_assistant_entry_doc(
        &mut self,
        thread: crate::assistant::thread::Id,
        entry: crate::assistant::thread::EntryId,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::UntrackEntryDoc { thread, entry })
    }

    pub fn track_assistant_entry_doc(
        &mut self,
        thread: crate::assistant::thread::Id,
        entry: crate::assistant::thread::EntryId,
        doc: DocumentId,
    ) -> Vec<crate::assistant::effect::Effect> {
        self.assistant_act(crate::assistant::Action::TrackEntryDoc { thread, entry, doc })
    }
}
