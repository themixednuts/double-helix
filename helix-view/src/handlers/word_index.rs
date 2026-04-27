//! Indexing of words from open buffers.
//!
//! This provides an eventually consistent set of words used in any open buffers. This set is
//! later used for lexical completion.

use std::{borrow::Cow, collections::HashMap, iter, mem, sync::Arc, time::Duration};

use arc_swap::ArcSwap;
use helix_core::{
    chars::char_is_word, fuzzy::fuzzy_match, movement, text_annotations::TextAnnotations,
    ChangeSet, Range, Rope, RopeSlice,
};
use helix_runtime::{channel, send_blocking, Clock, Debounce, Runtime, Sender, Work};
use helix_stdx::rope::RopeSliceExt as _;

use crate::{bench::log_command_phase, DocumentId};

use super::Handlers;

#[derive(Debug)]
struct Change {
    old_text: Rope,
    text: Rope,
    changes: ChangeSet,
}

#[derive(Debug)]
enum Event {
    Insert(Rope),
    Update(DocumentId, Change),
    Delete(DocumentId, Rope),
    /// Clear the entire word index.
    /// This is used to clear memory when the feature is turned off.
    Clear,
    FlushDebounced,
}

#[derive(Debug)]
pub struct Handler {
    pub(super) index: WordIndex,
    /// A sender into the debounced update actor for index maintenance.
    hook: Sender<Event>,
    /// A sender to a tokio task which coordinates the indexing of documents.
    ///
    /// See [WordIndex::run]. A supervisor-like task is in charge of spawning tasks to update the
    /// index. This ensures that consecutive edits to a document trigger the correct order of
    /// insertions and deletions into the word set.
    coordinator: Sender<Event>,
}

impl Handler {
    /// Create a dummy handler for headless testing (no async tasks spawned).
    pub fn dummy() -> Self {
        let (hook_tx, _) = helix_runtime::channel(1);
        let (coord_tx, _) = channel(1);
        Self {
            index: WordIndex::default(),
            hook: hook_tx,
            coordinator: coord_tx,
        }
    }

    pub fn spawn(runtime: Runtime) -> Self {
        let index = WordIndex::default();
        let (tx, rx) = channel(128);
        runtime.work().spawn(index.clone().run(rx)).detach();
        Self {
            hook: Hook::spawn(Hook {
                changes: HashMap::default(),
                coordinator: tx.clone(),
                debounce: Debounce::new(DEBOUNCE),
                work: runtime.work().clone(),
                clock: runtime.clock().clone(),
                tx: None,
            }),
            index,
            coordinator: tx,
        }
    }
}

#[derive(Debug)]
struct Hook {
    changes: HashMap<DocumentId, Change>,
    coordinator: Sender<Event>,
    debounce: Debounce,
    work: Work,
    clock: Clock,
    tx: Option<Sender<Event>>,
}

const DEBOUNCE: Duration = Duration::from_secs(1);

impl Hook {
    fn spawn(mut hook: Self) -> Sender<Event> {
        let (tx, mut rx) = helix_runtime::channel(128);
        hook.tx = Some(tx.clone());
        let work = hook.work.clone();
        work.spawn(async move {
            while let Some(event) = rx.recv().await {
                hook.handle_event(event);
            }
            hook.debounce.cancel();
        })
        .detach();
        tx
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Insert(_) => unreachable!("inserts are sent to the worker directly"),
            Event::Update(doc, change) => {
                if let Some(pending_change) = self.changes.get_mut(&doc) {
                    // If there is already a change waiting for this document, merge the two
                    // changes together by composing the changesets and saving the new `text`.
                    let pending = mem::take(&mut pending_change.changes);
                    pending_change.changes = if pending.len_after() == change.changes.len() {
                        pending.compose(change.changes)
                    } else {
                        change.changes
                    };
                    pending_change.text = change.text;
                    self.restart_debounce();
                } else if !is_changeset_significant(&change.changes) {
                    // If the changeset is fairly large, debounce before updating the index.
                    self.changes.insert(doc, change);
                    self.restart_debounce();
                } else {
                    // Otherwise if the change is small, queue the update to the index immediately.
                    send_blocking(&self.coordinator, Event::Update(doc, change));
                }
            }
            Event::Delete(doc, text) => {
                // If there are pending changes that haven't been indexed since the last debounce,
                // forget them and delete the old text.
                if let Some(change) = self.changes.remove(&doc) {
                    send_blocking(&self.coordinator, Event::Delete(doc, change.old_text));
                } else {
                    send_blocking(&self.coordinator, Event::Delete(doc, text));
                }
            }
            Event::Clear => {
                self.flush();
                send_blocking(&self.coordinator, Event::Clear);
            }
            Event::FlushDebounced => self.flush(),
        }
    }

    fn restart_debounce(&mut self) {
        let tx = self
            .tx
            .as_ref()
            .expect("word index hook sender initialized")
            .clone();
        self.debounce.restart(&self.work, &self.clock, async move {
            let _ = tx.send(Event::FlushDebounced).await;
        });
    }

    fn flush(&mut self) {
        for (doc, change) in self.changes.drain() {
            send_blocking(&self.coordinator, Event::Update(doc, change));
        }
    }
}

/// Minimum number of grapheme clusters required to include a word in the index
const MIN_WORD_GRAPHEMES: usize = 3;
/// Maximum word length allowed (in chars)
const MAX_WORD_LEN: usize = 50;

type Word = kstring::KString;

#[derive(Debug, Default, Clone)]
struct WordIndexInner {
    /// Reference counted storage for words.
    ///
    /// Words are very likely to be reused many times. Instead of storing duplicates we keep a
    /// reference count of times a word is used. When the reference count drops to zero the word
    /// is removed from the index.
    words: HashMap<Word, u32>,
}

impl WordIndexInner {
    fn words(&self) -> impl Iterator<Item = &Word> {
        self.words.keys()
    }

    fn insert(&mut self, word: RopeSlice) {
        let word: Cow<str> = word.into();
        if let Some(rc) = self.words.get_mut(word.as_ref()) {
            *rc = rc.saturating_add(1);
        } else {
            let word = match word {
                Cow::Owned(s) => Word::from_string(s),
                Cow::Borrowed(s) => Word::from_ref(s),
            };
            self.words.insert(word, 1);
        }
    }

    fn remove(&mut self, word: RopeSlice) {
        let word: Cow<str> = word.into();
        match self.words.get_mut(word.as_ref()) {
            Some(1) => {
                self.words.remove(word.as_ref());
            }
            Some(n) => *n -= 1,
            None => (),
        }
    }

    fn clear(&mut self) {
        std::mem::take(&mut self.words);
    }
}

#[derive(Debug, Clone)]
pub struct WordIndex {
    inner: Arc<ArcSwap<WordIndexInner>>,
}

impl Default for WordIndex {
    fn default() -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(WordIndexInner::default())),
        }
    }
}

impl WordIndex {
    /// Lock-free read — never blocks.
    pub fn matches(&self, pattern: &str) -> Vec<String> {
        let inner = self.inner.load();
        let mut matches = fuzzy_match(pattern, inner.words(), false);
        matches.sort_unstable_by_key(|(_, score)| *score);
        matches
            .into_iter()
            .map(|(word, _)| word.to_string())
            .collect()
    }

    /// Coordinate the indexing of documents.
    ///
    /// The worker owns a local mutable copy of the index and publishes immutable
    /// snapshots via ArcSwap after each batch — readers never block.
    async fn run(self, mut events: helix_runtime::Receiver<Event>) {
        let shared = self.inner;
        let mut local = WordIndexInner::default();
        while let Some(event) = events.recv().await {
            let shared = shared.clone();
            local = tokio::task::spawn_blocking(move || {
                match event {
                    Event::Insert(text) => {
                        for word in words(text.slice(..)) {
                            local.insert(word);
                        }
                    }
                    Event::Update(
                        _doc,
                        Change {
                            old_text,
                            text,
                            changes,
                            ..
                        },
                    ) => {
                        for (old_window, new_window) in
                            changed_windows(old_text.slice(..), text.slice(..), &changes)
                        {
                            for word in words(new_window) {
                                local.insert(word);
                            }
                            for word in words(old_window) {
                                local.remove(word);
                            }
                        }
                    }
                    Event::Delete(_doc, text) => {
                        for word in words(text.slice(..)) {
                            local.remove(word);
                        }
                    }
                    Event::Clear => {
                        local.clear();
                    }
                    Event::FlushDebounced => unreachable!("flush stays in hook actor"),
                }
                shared.store(Arc::new(local.clone()));
                local
            })
            .await
            .unwrap();
        }
    }
}

fn words(text: RopeSlice) -> impl Iterator<Item = RopeSlice> {
    let mut cursor = Range::point(0);
    if text
        .get_char(cursor.anchor)
        .is_some_and(|ch| !ch.is_whitespace())
    {
        let cursor_word_end =
            movement::move_next_word_end(text, &TextAnnotations::default(), cursor, 1);
        if cursor_word_end.anchor == 0 {
            cursor = cursor_word_end;
        }
    }

    iter::from_fn(move || {
        while cursor.head <= text.len_chars() {
            let mut word = None;
            if text
                .slice(..cursor.head)
                .graphemes_rev()
                .take(MIN_WORD_GRAPHEMES)
                .take_while(|g| g.chars().all(char_is_word))
                .count()
                == MIN_WORD_GRAPHEMES
            {
                cursor.anchor += text
                    .chars_at(cursor.anchor)
                    .take_while(|&c| !char_is_word(c))
                    .count();
                let slice = cursor.slice(text);
                if slice.len_chars() <= MAX_WORD_LEN {
                    word = Some(slice);
                }
            }
            let head = cursor.head;
            cursor = movement::move_next_word_end(text, &TextAnnotations::default(), cursor, 1);
            if cursor.head == head {
                cursor.head = usize::MAX;
            }
            if word.is_some() {
                return word;
            }
        }
        None
    })
}

/// Finds areas of the old and new texts around each operation in `changes`.
///
/// The window is larger than the changed area and can encompass multiple insert/delete operations
/// if they are grouped closely together.
///
/// The ranges of the old and new text should usually be of different sizes. For example a
/// deletion of "foo" surrounded by large retain sections would give a longer window into the
/// `old_text` and shorter window of `new_text`. Vice-versa for an insertion. A full replacement
/// of a word though would give two slices of the same size.
fn changed_windows<'a>(
    old_text: RopeSlice<'a>,
    new_text: RopeSlice<'a>,
    changes: &'a ChangeSet,
) -> impl Iterator<Item = (RopeSlice<'a>, RopeSlice<'a>)> {
    use helix_core::Operation::*;

    let mut operations = changes.changes().iter().peekable();
    let mut old_pos = 0;
    let mut new_pos = 0;
    iter::from_fn(move || loop {
        let operation = operations.next()?;
        let old_start = old_pos;
        let new_start = new_pos;
        let len = operation.len_chars();
        match operation {
            Retain(_) => {
                old_pos += len;
                new_pos += len;
                continue;
            }
            Insert(_) => new_pos += len,
            Delete(_) => old_pos += len,
        }

        // Scan ahead until a `Retain` is found which would end a window.
        while let Some(o) = operations.next_if(|op| !matches!(op, Retain(n) if *n > MAX_WORD_LEN)) {
            let len = o.len_chars();
            match o {
                Retain(_) => {
                    old_pos += len;
                    new_pos += len;
                }
                Delete(_) => old_pos += len,
                Insert(_) => new_pos += len,
            }
        }

        let old_window = old_start.saturating_sub(MAX_WORD_LEN)
            ..(old_pos + MAX_WORD_LEN).min(old_text.len_chars());
        let new_window = new_start.saturating_sub(MAX_WORD_LEN)
            ..(new_pos + MAX_WORD_LEN).min(new_text.len_chars());

        return Some((old_text.slice(old_window), new_text.slice(new_window)));
    })
}

/// Estimates whether a changeset is significant or small.
fn is_changeset_significant(changes: &ChangeSet) -> bool {
    use helix_core::Operation::*;

    let mut diff = 0;
    for operation in changes.changes() {
        match operation {
            Retain(_) => continue,
            Delete(_) | Insert(_) => diff += operation.len_chars(),
        }
    }

    // This is arbitrary and could be tuned further:
    diff > 1_000
}

pub(crate) fn attach(editor: &crate::Editor, handlers: &Handlers) {
    let coordinator = handlers.word_index.coordinator.clone();
    editor.lifecycle().on_document_open(move |event| {
        let doc = doc!(event.editor, &event.doc);
        if doc.word_completion_enabled() {
            send_blocking(&coordinator, Event::Insert(doc.text().clone()));
        }
        Ok(())
    });

    let tx = handlers.word_index.hook.clone();
    editor.lifecycle().on_document_change(move |event| {
        let hook_start = std::time::Instant::now();
        if !event.ghost_transaction && event.doc.word_completion_enabled() {
            helix_runtime::send_blocking(
                &tx,
                Event::Update(
                    event.doc.id(),
                    Change {
                        old_text: event.old_text.clone(),
                        text: event.doc.text().clone(),
                        changes: event.changes.clone(),
                    },
                ),
            );
        }
        let hook_dur = hook_start.elapsed();
        log_command_phase("document_did_change_hook", "word_index", hook_dur, || {
            format!(
                "doc_id={:?} ghost={} enabled={} lines={} bytes={} change_ops={}",
                event.doc.id(),
                event.ghost_transaction,
                event.doc.word_completion_enabled(),
                event.doc.text().len_lines(),
                event.doc.text().len_bytes(),
                event.changes.len()
            )
        });
        Ok(())
    });

    let tx = handlers.word_index.hook.clone();
    editor.lifecycle().on_document_close(move |event| {
        if event.doc.word_completion_enabled() {
            helix_runtime::send_blocking(
                &tx,
                Event::Delete(event.doc.id(), event.doc.text().clone()),
            );
        }
        Ok(())
    });

    let coordinator = handlers.word_index.coordinator.clone();
    editor.lifecycle().on_config_change(move |event| {
        // The feature has been turned off. Clear the index and reclaim any used memory.
        if event.old.word_completion.enable && !event.new.word_completion.enable {
            send_blocking(&coordinator, Event::Clear);
        }

        // The feature has been turned on. Index open documents.
        if !event.old.word_completion.enable && event.new.word_completion.enable {
            for doc in event.editor.documents() {
                if doc.word_completion_enabled() {
                    send_blocking(&coordinator, Event::Insert(doc.text().clone()));
                }
            }
        }

        Ok(())
    });
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use helix_core::diff::compare_ropes;

    fn add_document(inner: &mut WordIndexInner, text: &Rope) {
        for word in words(text.slice(..)) {
            inner.insert(word);
        }
    }

    fn update_document(
        inner: &mut WordIndexInner,
        old_text: &Rope,
        text: &Rope,
        changes: &ChangeSet,
    ) {
        for (old_window, new_window) in changed_windows(old_text.slice(..), text.slice(..), changes)
        {
            for word in words(new_window) {
                inner.insert(word);
            }
            for word in words(old_window) {
                inner.remove(word);
            }
        }
    }

    fn collect_words(inner: &WordIndexInner) -> HashSet<String> {
        inner.words().map(|w| w.to_string()).collect()
    }

    #[track_caller]
    fn assert_words<I: ToString, T: IntoIterator<Item = I>>(text: &str, expected: T) {
        let text = Rope::from_str(text);
        let mut inner = WordIndexInner::default();
        add_document(&mut inner, &text);
        let actual = collect_words(&inner);
        let expected: HashSet<_> = expected.into_iter().map(|i| i.to_string()).collect();
        assert_eq!(expected, actual);
    }

    #[test]
    fn parse() {
        assert_words("one two three", ["one", "two", "three"]);
        assert_words("a foo c", ["foo"]);
    }

    #[track_caller]
    fn assert_diff<S, R, I>(before: &str, after: &str, expect_removed: R, expect_inserted: I)
    where
        S: ToString,
        R: IntoIterator<Item = S>,
        I: IntoIterator<Item = S>,
    {
        let before = Rope::from_str(before);
        let after = Rope::from_str(after);
        let diff = compare_ropes(&before, &after);
        let expect_removed: HashSet<_> =
            expect_removed.into_iter().map(|i| i.to_string()).collect();
        let expect_inserted: HashSet<_> =
            expect_inserted.into_iter().map(|i| i.to_string()).collect();

        let mut inner = WordIndexInner::default();
        add_document(&mut inner, &before);
        let words_before = collect_words(&inner);
        update_document(&mut inner, &before, &after, diff.changes());
        let words_after = collect_words(&inner);

        let actual_removed = words_before.difference(&words_after).cloned().collect();
        let actual_inserted = words_after.difference(&words_before).cloned().collect();

        eprintln!("\"{before}\" {words_before:?} => \"{after}\" {words_after:?}");
        assert_eq!(
            expect_removed, actual_removed,
            "expected {expect_removed:?} to be removed, instead {actual_removed:?} was"
        );
        assert_eq!(
            expect_inserted, actual_inserted,
            "expected {expect_inserted:?} to be inserted, instead {actual_inserted:?} was"
        );
    }

    #[test]
    fn diff() {
        assert_diff("one two three", "one five three", ["two"], ["five"]);
        assert_diff("one two three", "one to three", ["two"], []);
        assert_diff("one two three", "one three", ["two"], []);
        assert_diff("one two three", "one t{o three", ["two"], []);
        assert_diff("one foo three", "one fooo three", ["foo"], ["fooo"]);
    }
}
