use helix_core::{
    history::{History, State},
    ChangeSet, LineEnding, Rope,
};
use std::fmt;
use std::sync::Mutex;

pub struct TextBuffer {
    text: Rope,
    line_ending: LineEnding,
    changes: ChangeSet,
    old_state: Option<State>,
    history: Mutex<History>,
}

impl fmt::Debug for TextBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TextBuffer")
            .field("text", &self.text)
            .field("line_ending", &self.line_ending)
            .field("changes", &self.changes)
            .field("old_state", &self.old_state)
            .finish_non_exhaustive()
    }
}

impl TextBuffer {
    pub fn new(text: Rope, line_ending: LineEnding) -> Self {
        let changes = ChangeSet::new(text.slice(..));
        Self {
            text,
            line_ending,
            changes,
            old_state: None,
            history: Mutex::new(History::default()),
        }
    }

    #[inline]
    pub fn text(&self) -> &Rope {
        &self.text
    }

    #[inline]
    pub(crate) fn text_mut(&mut self) -> &mut Rope {
        &mut self.text
    }

    #[inline]
    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    #[inline]
    pub fn set_line_ending(&mut self, line_ending: LineEnding) {
        self.line_ending = line_ending;
    }

    #[inline]
    pub fn changes(&self) -> &ChangeSet {
        &self.changes
    }

    #[inline]
    pub(crate) fn changes_mut(&mut self) -> &mut ChangeSet {
        &mut self.changes
    }

    #[inline]
    pub(crate) fn set_changes(&mut self, changes: ChangeSet) {
        self.changes = changes;
    }

    #[inline]
    pub(crate) fn set_old_state(&mut self, old_state: Option<State>) {
        self.old_state = old_state;
    }

    #[inline]
    pub(crate) fn take_old_state(&mut self) -> Option<State> {
        self.old_state.take()
    }

    pub fn with_history<R>(&self, f: impl FnOnce(&History) -> R) -> R {
        let guard = self.history.lock().unwrap();
        f(&guard)
    }

    pub fn with_history_mut<R>(&mut self, f: impl FnOnce(&mut History) -> R) -> R {
        let mut guard = self.history.lock().unwrap();
        f(&mut guard)
    }
}
