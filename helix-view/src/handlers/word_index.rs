//! Indexing of words from open buffers.
//!
//! This provides an eventually consistent set of words used in any open buffers. This set is
//! later used for lexical completion.

use std::{borrow::Cow, collections::HashMap, iter, mem, sync::Arc, time::Duration};

use arc_swap::ArcSwap;
use helix_core::{
    chars::char_is_word, diff::compare_ropes, fuzzy::fuzzy_match, movement,
    text_annotations::TextAnnotations, ChangeSet, Range, Rope, RopeSlice,
};
use helix_runtime::{Clock, PulseGate, PulseHandle, PulseReceiver, Runtime};
use helix_stdx::rope::RopeSliceExt as _;
use parking_lot::Mutex;

use crate::{bench::log_command_phase, DocumentId};

use super::Handlers;

#[derive(Debug)]
struct Change {
    old_text: Rope,
    text: Rope,
    changes: ChangeSet,
}

#[derive(Debug)]
enum DocumentIntent {
    Replace(Rope),
    Update(Change),
    Delete,
}

#[derive(Debug, Default)]
struct ReadyWordIndex {
    clear: bool,
    documents: HashMap<DocumentId, DocumentIntent>,
}

impl ReadyWordIndex {
    fn is_empty(&self) -> bool {
        !self.clear && self.documents.is_empty()
    }
}

#[derive(Debug, Default)]
struct PendingWordIndex {
    ready: ReadyWordIndex,
    debounced: HashMap<DocumentId, Change>,
    debounce_generation: u64,
}

#[derive(Clone, Debug)]
enum ReadyPulse {}

#[derive(Clone, Debug)]
enum DebouncePulse {}

/// A state-based inbox sender. There is at most one ready and one debounced
/// intent per document, regardless of producer rate.
#[derive(Clone, Debug)]
struct WordIndexSender {
    pending: Option<Arc<Mutex<PendingWordIndex>>>,
    ready_wake: Option<PulseHandle<ReadyPulse>>,
    debounce_wake: Option<PulseHandle<DebouncePulse>>,
}

#[derive(Debug)]
struct WordIndexInbox {
    pending: Arc<Mutex<PendingWordIndex>>,
    wake: PulseReceiver<ReadyPulse>,
}

#[derive(Debug)]
struct WordIndexDebounceInbox {
    pending: Arc<Mutex<PendingWordIndex>>,
    wake: PulseReceiver<DebouncePulse>,
    ready_wake: PulseHandle<ReadyPulse>,
}

fn word_index_channel() -> (WordIndexSender, WordIndexInbox, WordIndexDebounceInbox) {
    let pending = Arc::new(Mutex::new(PendingWordIndex::default()));
    let mut ready_gate = PulseGate::new();
    let mut debounce_gate = PulseGate::new();
    let ready_wake = ready_gate.handle();

    (
        WordIndexSender {
            pending: Some(pending.clone()),
            ready_wake: Some(ready_wake.clone()),
            debounce_wake: Some(debounce_gate.handle()),
        },
        WordIndexInbox {
            pending: pending.clone(),
            wake: ready_gate.take_receiver(),
        },
        WordIndexDebounceInbox {
            pending,
            wake: debounce_gate.take_receiver(),
            ready_wake,
        },
    )
}

impl WordIndexSender {
    fn closed() -> Self {
        Self {
            pending: None,
            ready_wake: None,
            debounce_wake: None,
        }
    }

    fn insert(&self, doc: DocumentId, text: Rope) {
        let Some(pending) = &self.pending else {
            return;
        };
        let mut pending = pending.lock();
        pending.debounced.remove(&doc);
        pending
            .ready
            .documents
            .insert(doc, DocumentIntent::Replace(text));
        drop(pending);
        self.wake_ready();
    }

    fn update(&self, doc: DocumentId, change: Change) {
        let Some(pending) = &self.pending else {
            return;
        };
        let mut pending = pending.lock();
        let debounce =
            pending.debounced.contains_key(&doc) || !is_changeset_significant(&change.changes);
        if debounce {
            pending.debounce_generation = pending.debounce_generation.wrapping_add(1);
            pending.debounced.insert(doc, change);
        } else {
            pending.debounced.remove(&doc);
            pending
                .ready
                .documents
                .insert(doc, DocumentIntent::Update(change));
        }
        drop(pending);

        if debounce {
            if let Some(wake) = &self.debounce_wake {
                wake.request();
            }
        } else {
            self.wake_ready();
        }
    }

    fn delete(&self, doc: DocumentId) {
        let Some(pending) = &self.pending else {
            return;
        };
        let mut pending = pending.lock();
        pending.debounced.remove(&doc);
        pending.ready.documents.insert(doc, DocumentIntent::Delete);
        drop(pending);
        self.wake_ready();
    }

    fn clear(&self) {
        let Some(pending) = &self.pending else {
            return;
        };
        let mut pending = pending.lock();
        pending.debounced.clear();
        pending.ready.clear = true;
        pending.ready.documents.clear();
        drop(pending);
        self.wake_ready();
    }

    fn wake_ready(&self) {
        if let Some(wake) = &self.ready_wake {
            wake.request();
        }
    }
}

impl WordIndexInbox {
    fn take_ready(&self) -> ReadyWordIndex {
        mem::take(&mut self.pending.lock().ready)
    }
}

impl WordIndexDebounceInbox {
    fn generation(&self) -> Option<u64> {
        let pending = self.pending.lock();
        (!pending.debounced.is_empty()).then_some(pending.debounce_generation)
    }

    fn flush_if_current(&self, generation: u64) -> bool {
        let mut pending = self.pending.lock();
        if pending.debounce_generation != generation || pending.debounced.is_empty() {
            return false;
        }

        let changes = mem::take(&mut pending.debounced);
        pending.ready.documents.extend(
            changes
                .into_iter()
                .map(|(doc, change)| (doc, DocumentIntent::Update(change))),
        );
        drop(pending);
        self.ready_wake.request();
        true
    }
}

#[derive(Debug)]
pub struct Handler {
    pub(super) index: WordIndex,
    events: WordIndexSender,
}

impl Handler {
    /// Create a dummy handler for headless testing (no async tasks spawned).
    pub fn dummy() -> Self {
        Self {
            index: WordIndex::default(),
            events: WordIndexSender::closed(),
        }
    }

    pub fn spawn(runtime: Runtime) -> Self {
        let index = WordIndex::default();
        let (events, inbox, debounce_inbox) = word_index_channel();
        runtime.work().spawn(index.clone().run(inbox)).detach();
        runtime
            .work()
            .spawn(run_debounce(debounce_inbox, runtime.clock().clone()))
            .detach();
        Self { index, events }
    }
}

const DEBOUNCE: Duration = Duration::from_secs(1);

async fn run_debounce(mut inbox: WordIndexDebounceInbox, clock: Clock) {
    while inbox.wake.recv().await.is_some() {
        while let Some(generation) = inbox.generation() {
            let mut timer = clock.timer(DEBOUNCE);
            tokio::select! {
                pulse = inbox.wake.recv() => {
                    if pulse.is_none() {
                        return;
                    }
                }
                _ = &mut timer => {
                    if inbox.flush_if_current(generation) {
                        break;
                    }
                }
            }
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

    /// The worker owns mutable document state and publishes immutable index
    /// snapshots after each coalesced batch, so readers never block.
    async fn run(self, mut inbox: WordIndexInbox) {
        let shared = self.inner;
        let mut local = WordIndexInner::default();
        let mut documents = HashMap::default();
        while inbox.wake.recv().await.is_some() {
            loop {
                let batch = inbox.take_ready();
                if batch.is_empty() {
                    break;
                }

                let shared = shared.clone();
                (local, documents) = tokio::task::spawn_blocking(move || {
                    apply_batch(&mut local, &mut documents, batch);
                    shared.store(Arc::new(local.clone()));
                    (local, documents)
                })
                .await
                .unwrap();
            }
        }
    }
}

fn apply_batch(
    index: &mut WordIndexInner,
    documents: &mut HashMap<DocumentId, Rope>,
    batch: ReadyWordIndex,
) {
    if batch.clear {
        index.clear();
        documents.clear();
    }

    for (doc, intent) in batch.documents {
        match intent {
            DocumentIntent::Delete => {
                if let Some(text) = documents.remove(&doc) {
                    remove_document(index, &text);
                }
            }
            DocumentIntent::Replace(text) => {
                upsert_document(index, documents, doc, text, None);
            }
            DocumentIntent::Update(change) => {
                let Change {
                    old_text,
                    text,
                    changes,
                } = change;
                upsert_document(index, documents, doc, text, Some((old_text, changes)));
            }
        }
    }
}

fn upsert_document(
    index: &mut WordIndexInner,
    documents: &mut HashMap<DocumentId, Rope>,
    doc: DocumentId,
    text: Rope,
    delta: Option<(Rope, ChangeSet)>,
) {
    if let Some(previous) = documents.get(&doc) {
        if previous != &text {
            if let Some((old_text, changes)) = delta.filter(|(old_text, changes)| {
                previous == old_text
                    && changes.len() == previous.len_chars()
                    && changes.len_after() == text.len_chars()
            }) {
                update_document(index, &old_text, &text, &changes);
            } else {
                let transaction = compare_ropes(previous, &text);
                update_document(index, previous, &text, transaction.changes());
            }
        }
    } else {
        add_document(index, &text);
    }
    documents.insert(doc, text);
}

fn add_document(index: &mut WordIndexInner, text: &Rope) {
    for word in words(text.slice(..)) {
        index.insert(word);
    }
}

fn remove_document(index: &mut WordIndexInner, text: &Rope) {
    for word in words(text.slice(..)) {
        index.remove(word);
    }
}

fn update_document(index: &mut WordIndexInner, old_text: &Rope, text: &Rope, changes: &ChangeSet) {
    for (old_window, new_window) in changed_windows(old_text.slice(..), text.slice(..), changes) {
        for word in words(new_window) {
            index.insert(word);
        }
        for word in words(old_window) {
            index.remove(word);
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
    let events = handlers.word_index.events.clone();
    editor.lifecycle().on_document_open(move |event| {
        let doc = doc!(event.editor, &event.doc);
        if doc.word_completion_enabled() {
            events.insert(doc.id(), doc.text().clone());
        }
        Ok(())
    });

    let events = handlers.word_index.events.clone();
    editor.lifecycle().on_document_change(move |event| {
        let hook_start = std::time::Instant::now();
        if !event.ghost_transaction && event.doc.word_completion_enabled() {
            events.update(
                event.doc.id(),
                Change {
                    old_text: event.old_text.clone(),
                    text: event.doc.text().clone(),
                    changes: event.changes.clone(),
                },
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

    let events = handlers.word_index.events.clone();
    editor.lifecycle().on_document_close(move |event| {
        if event.doc.word_completion_enabled() {
            events.delete(event.doc.id());
        }
        Ok(())
    });

    let events = handlers.word_index.events.clone();
    editor.lifecycle().on_config_change(move |event| {
        // The feature has been turned off. Clear the index and reclaim any used memory.
        if event.old.word_completion.enable && !event.new.word_completion.enable {
            events.clear();
        }

        // The feature has been turned on. Index open documents.
        if !event.old.word_completion.enable && event.new.word_completion.enable {
            for doc in event.editor.documents() {
                if doc.word_completion_enabled() {
                    events.insert(doc.id(), doc.text().clone());
                }
            }
        }

        Ok(())
    });
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, num::NonZeroUsize};

    use super::*;

    fn doc_id(value: usize) -> DocumentId {
        DocumentId::new(NonZeroUsize::new(value).unwrap())
    }

    fn change(before: &str, after: &str) -> Change {
        let old_text = Rope::from_str(before);
        let text = Rope::from_str(after);
        let changes = compare_ropes(&old_text, &text).changes().clone();
        Change {
            old_text,
            text,
            changes,
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

    #[test]
    fn ready_inbox_coalesces_saturation_and_uses_one_wakeup() {
        let (sender, mut inbox, _debounce) = word_index_channel();
        let doc = doc_id(1);
        let stale = Rope::from_str("stale words");

        for _ in 0..10_000 {
            sender.insert(doc, stale.clone());
        }
        sender.insert(doc, Rope::from_str("latest words"));

        assert!(inbox.wake.try_recv().is_ok());
        assert!(matches!(
            inbox.wake.try_recv(),
            Err(helix_runtime::TryRecvError::Empty)
        ));
        let batch = inbox.take_ready();
        assert_eq!(batch.documents.len(), 1);
        assert!(matches!(
            batch.documents.get(&doc),
            Some(DocumentIntent::Replace(text)) if text == &Rope::from_str("latest words")
        ));
    }

    #[test]
    fn debounce_keeps_latest_state_and_rejects_stale_timer() {
        let (sender, ready, mut debounce) = word_index_channel();
        let doc = doc_id(1);

        sender.update(doc, change("alpha word", "beta word"));
        let stale_generation = debounce.generation().unwrap();
        sender.update(doc, change("beta word", "gamma word"));

        assert!(debounce.wake.try_recv().is_ok());
        assert!(matches!(
            debounce.wake.try_recv(),
            Err(helix_runtime::TryRecvError::Empty)
        ));
        assert!(!debounce.flush_if_current(stale_generation));
        assert!(ready.take_ready().is_empty());

        let current_generation = debounce.generation().unwrap();
        assert!(debounce.flush_if_current(current_generation));
        let batch = ready.take_ready();
        assert_eq!(batch.documents.len(), 1);
        assert!(matches!(
            batch.documents.get(&doc),
            Some(DocumentIntent::Update(Change { text, .. }))
                if text == &Rope::from_str("gamma word")
        ));
    }

    #[test]
    fn deletes_and_clears_remain_semantically_lossless() {
        let (sender, inbox, _debounce) = word_index_channel();
        let first = doc_id(1);
        let second = doc_id(2);
        let mut index = WordIndexInner::default();
        let mut documents = HashMap::default();

        sender.insert(first, Rope::from_str("alpha word"));
        apply_batch(&mut index, &mut documents, inbox.take_ready());
        assert_words_in_index(&index, ["alpha", "word"]);

        sender.delete(first);
        apply_batch(&mut index, &mut documents, inbox.take_ready());
        assert!(index.words().next().is_none());

        sender.insert(first, Rope::from_str("stale value"));
        sender.clear();
        sender.insert(second, Rope::from_str("fresh value"));
        apply_batch(&mut index, &mut documents, inbox.take_ready());
        assert_words_in_index(&index, ["fresh", "value"]);
        assert_eq!(documents.len(), 1);
        assert!(documents.contains_key(&second));
    }

    #[test]
    fn skipped_coalesced_delta_diffs_against_indexed_text() {
        let doc = doc_id(1);
        let mut index = WordIndexInner::default();
        let mut documents = HashMap::default();
        let mut initial = ReadyWordIndex::default();
        initial
            .documents
            .insert(doc, DocumentIntent::Replace(Rope::from_str("alpha word")));
        apply_batch(&mut index, &mut documents, initial);

        let mut latest = ReadyWordIndex::default();
        latest.documents.insert(
            doc,
            DocumentIntent::Update(change("beta word", "gamma word")),
        );
        apply_batch(&mut index, &mut documents, latest);

        assert_words_in_index(&index, ["gamma", "word"]);
    }

    #[track_caller]
    fn assert_words_in_index<const N: usize>(index: &WordIndexInner, expected: [&str; N]) {
        let actual = collect_words(index);
        let expected = expected.into_iter().map(str::to_owned).collect();
        assert_eq!(actual, expected);
    }
}
