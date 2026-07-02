//! Host-agnostic label-jump session — Helix's "press two letters to
//! jump anywhere visible" UI, lifted out of [`crate::editor::Editor`]
//! so any UI surface (file explorer, picker, future surfaces) can offer
//! the same `gw`-style navigation without reimplementing the algorithm.
//!
//! # The model
//!
//! Each "jump target" is identified by an opaque `u32` chosen by the
//! host — a row index for a tree, a `(line, char)` packed index for a
//! buffer, an item slot for a list. The session generates a label
//! (one or two characters from a configurable alphabet) for each target
//! and runs a tiny state machine over the user's next 1–2 keystrokes
//! to resolve which target was chosen.
//!
//! # What the host owns vs. what the session owns
//!
//! - **Session owns:** the alphabet, the label-generation algorithm
//!   (which reproduces the editor's "square pattern" so a tree with N
//!   targets uses the same label order an editor view with N targets
//!   would), the state machine across the user's keystrokes, and the
//!   reverse mapping from typed-chars to target index.
//! - **Host owns:** picking which targets are "visible" and assigning
//!   them target IDs, *rendering* the label glyphs over each target
//!   (because each surface knows where its targets live on screen),
//!   and acting on the resolved target ID.
//!
//! This split is intentional. The label-rendering UI for a tree
//! (overlaid on the first 1–2 chars of each row label) is fundamentally
//! different from an editor buffer's (virtual `Overlay` text on a
//! `Rope`), but the algorithm and state machine are identical — and
//! that's the part worth sharing.
//!
//! # Example
//!
//! ```ignore
//! // Host: start a session over its visible items.
//! let alphabet = vec!['a','b','c','d','e','f','g','h','i','j','k','l','m',
//!                     'n','o','p','q','r','s','t','u','v','w','x','y','z'];
//! let mut session = JumpSession::new(visible_count, alphabet);
//!
//! // Host: render each item with its label.
//! for (i, item) in visible_items.iter().enumerate() {
//!     let label = session.label_at(i).expect("target_id in range");
//!     draw_label_over(item, label);
//! }
//!
//! // Host: pump keys through the session until it resolves.
//! match session.feed_key(key) {
//!     JumpSignal::Pending => /* keep session alive */,
//!     JumpSignal::Selected(target_id) => /* jump to visible_items[target_id as usize] */,
//!     JumpSignal::Cancelled => /* dismiss labels, return to normal */,
//! }
//! ```

use crate::input::{KeyCode, KeyEvent, KeyModifiers};

/// Outcome of feeding a single key into a [`JumpSession`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JumpSignal {
    /// Need another keystroke. The session stays alive — keep
    /// dispatching keys into it.
    Pending,
    /// The user selected the target with id `target_id`. The session
    /// is exhausted; the host should clear it and act on the jump.
    /// `target_id` is the index in the host's `visible_targets` list
    /// that was passed to [`JumpSession::new`].
    Selected(u32),
    /// The user cancelled (typed Esc, or typed a key not in the
    /// alphabet). The session is exhausted; clear it and return to
    /// the host's normal key dispatch.
    Cancelled,
}

/// Internal state machine — tracks which "stage" of the two-character
/// label the user is currently entering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JumpState {
    /// No keys typed yet; waiting for the first label character.
    AwaitingFirst,
    /// First character matched alphabet index `first`; waiting for
    /// the second character.
    AwaitingSecond { first: usize },
    /// The session has resolved; further keys are ignored. Hosts
    /// should drop the session once they see `Selected` / `Cancelled`,
    /// so this state is mostly defensive — guarantees that a stale
    /// session is a harmless no-op rather than a panic risk.
    Done,
}

/// A label-jump session — pure state, no rendering. Hosts feed keys
/// in via [`Self::feed_key`] and read labels out via [`Self::label_at`]
/// for display.
///
/// See the module-level docs for the model and the host/session
/// responsibility split.
#[derive(Clone, Debug)]
pub struct JumpSession {
    /// Characters available for labels — first letter of each label
    /// is drawn from here, second letter likewise. Order matters:
    /// earlier letters appear on more targets (since labels are
    /// allocated in a square-spiral pattern).
    alphabet: Vec<char>,
    /// Number of targets the host announced. Labels are valid for
    /// indices `0..target_count`.
    target_count: u32,
    state: JumpState,
}

impl JumpSession {
    /// Start a new session over `target_count` host targets using
    /// `alphabet` for label characters. Empty alphabets or zero
    /// targets produce a session that immediately resolves to
    /// `Cancelled` on the first key — there's nothing to jump to.
    pub fn new(target_count: u32, alphabet: Vec<char>) -> Self {
        Self {
            alphabet,
            target_count,
            state: JumpState::AwaitingFirst,
        }
    }

    /// Maximum number of targets this session's alphabet can label.
    /// For an alphabet of N characters, that's `N * N`. Hosts can use
    /// this to truncate their target list and avoid handing the
    /// session more targets than it could possibly label.
    pub fn capacity(&self) -> u32 {
        (self.alphabet.len() as u32).saturating_mul(self.alphabet.len() as u32)
    }

    /// The 1- or 2-character label for the `target_id`-th target, or
    /// `None` if `target_id` exceeds either `target_count` or this
    /// session's labeling capacity. Returned as a `[char; 2]` plus a
    /// length so the host can render without allocating.
    ///
    /// # Algorithm
    ///
    /// Reproduces Helix's existing "square-spiral" label allocation
    /// from `commands::jump_to_label` so the file explorer's `gw`
    /// labels the first N targets identically to the editor's `gw`
    /// labelling the first N words. The pattern is:
    ///
    /// ```text
    ///    a  b  c  d
    /// a  0  1  4  9
    /// b  2  3  5 10
    /// c  6  7  8 11
    /// d 12 13 14 15
    /// ```
    ///
    /// where the column index drives the *first* label character and
    /// the row index drives the *second*. This gives single-letter
    /// "aa", "ba", "ab", … sequences that minimize finger travel for
    /// the common case of few visible targets.
    pub fn label_at(&self, target_id: u32) -> Option<JumpLabel> {
        if target_id >= self.target_count {
            return None;
        }
        let (first, second) = label_indices_for(target_id as usize)?;
        if first >= self.alphabet.len() || second >= self.alphabet.len() {
            return None;
        }
        Some(JumpLabel {
            first: self.alphabet[first],
            second: self.alphabet[second],
        })
    }

    /// True if the session has not yet resolved — the host should
    /// keep rendering labels and routing keys into the session.
    pub fn is_active(&self) -> bool {
        !matches!(self.state, JumpState::Done)
    }

    /// Feed one keystroke into the state machine.
    pub fn feed_key(&mut self, key: KeyEvent) -> JumpSignal {
        // Esc cancels at any stage — the universal bail-out.
        if matches!(key.code, KeyCode::Esc) && key.modifiers == KeyModifiers::NONE {
            self.state = JumpState::Done;
            return JumpSignal::Cancelled;
        }

        // Only plain (unmodified) printable chars count as label input.
        // Ctrl-A / Shift-Tab / function keys all cancel — anything that
        // wasn't going to be a label should drop the session and let
        // the host handle the unrelated input.
        let Some(ch) = key.char().filter(|_| key.modifiers == KeyModifiers::NONE) else {
            self.state = JumpState::Done;
            return JumpSignal::Cancelled;
        };

        let Some(idx) = self.alphabet.iter().position(|&c| c == ch) else {
            // Key not in the alphabet — bail.
            self.state = JumpState::Done;
            return JumpSignal::Cancelled;
        };

        match self.state {
            JumpState::AwaitingFirst => {
                self.state = JumpState::AwaitingSecond { first: idx };
                JumpSignal::Pending
            }
            JumpState::AwaitingSecond { first } => {
                self.state = JumpState::Done;
                let target_id = target_index_for(first, idx);
                if (target_id as u32) < self.target_count {
                    JumpSignal::Selected(target_id as u32)
                } else {
                    // The label was valid as a 2-char tuple but didn't
                    // map to one of our visible targets. Treat as a
                    // miss — same as the editor's behavior.
                    JumpSignal::Cancelled
                }
            }
            JumpState::Done => JumpSignal::Cancelled,
        }
    }
}

/// A 1- or 2-character label. We always emit two characters for
/// consistency with the editor's `goto_word`; if the host wants to
/// display only the first character for tiny target sets, it can do
/// so by ignoring `second`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JumpLabel {
    pub first: char,
    pub second: char,
}

impl JumpLabel {
    /// The full label as a 2-character string, materialized on demand.
    pub fn as_string(&self) -> String {
        let mut s = String::with_capacity(2);
        s.push(self.first);
        s.push(self.second);
        s
    }
}

/// Convert a target index into the (first, second) alphabet positions
/// that label it. Mirrors `commands::jump_to_label`'s forward
/// computation byte-for-byte so the order is stable across hosts.
fn label_indices_for(i: usize) -> Option<(usize, usize)> {
    let base = (i as f64).sqrt() as usize;
    let offset = i.checked_sub(base * base)?;
    let outer = if offset < base { base } else { offset - base };
    let inner = if offset < base { offset } else { base };
    Some((outer, inner))
}

/// Reverse of [`label_indices_for`]: given the two alphabet positions
/// the user typed, compute which target index they selected. Mirrors
/// `commands::jump_to_label`'s reverse computation exactly.
fn target_index_for(outer: usize, inner: usize) -> usize {
    if outer > inner {
        outer * outer + inner
    } else {
        inner * (inner + 1) + outer
    }
}

#[cfg(test)]
mod tests {
    //! These tests pin the contract every host inherits:
    //! - label assignment is deterministic and matches the editor's
    //!   square-spiral order (so the file explorer and the editor
    //!   label the i-th target identically),
    //! - the state machine resolves on exactly two valid characters,
    //!   cancels on Esc or any non-alphabet key, and leaves no stale
    //!   "waiting forever" state.
    use super::*;
    use crate::input::{KeyEvent, KeyModifiers};

    fn alphabet() -> Vec<char> {
        ('a'..='z').collect()
    }

    fn key(ch: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(ch),
            modifiers: KeyModifiers::NONE,
        }
    }

    fn esc() -> KeyEvent {
        KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn label_at_matches_editor_square_spiral_order() {
        let session = JumpSession::new(16, alphabet());
        // Reproduce the schematic from the module docs:
        //    a  b  c  d
        // a  0  1  4  9
        // b  2  3  5 10
        // c  6  7  8 11
        // d 12 13 14 15
        //
        // Cell at row R, column C maps from target i to (first=C, second=R)
        // → "first char is column letter, second char is row letter".
        let expect = |i: u32, first: char, second: char| {
            let label = JumpSession::new(16, alphabet())
                .label_at(i)
                .unwrap_or_else(|| panic!("label_at({i}) returned None"));
            assert_eq!(label.first, first, "target {i} first char");
            assert_eq!(label.second, second, "target {i} second char");
        };
        let _ = session; // sanity-suppress unused
                         // Diagonal targets land at (col, col).
        expect(0, 'a', 'a');
        expect(3, 'b', 'b');
        expect(8, 'c', 'c');
        expect(15, 'd', 'd');
        // Row 0 (second char = 'a') along the column axis.
        expect(1, 'b', 'a');
        expect(4, 'c', 'a');
        expect(9, 'd', 'a');
        // Column 0 (first char = 'a') along the row axis.
        expect(2, 'a', 'b');
        expect(6, 'a', 'c');
        expect(12, 'a', 'd');
    }

    #[test]
    fn label_at_returns_none_outside_target_range() {
        let session = JumpSession::new(3, alphabet());
        assert!(session.label_at(0).is_some());
        assert!(session.label_at(1).is_some());
        assert!(session.label_at(2).is_some());
        assert!(session.label_at(3).is_none(), "beyond target_count");
        assert!(session.label_at(100).is_none(), "way beyond target_count");
    }

    #[test]
    fn capacity_equals_alphabet_squared() {
        assert_eq!(JumpSession::new(0, alphabet()).capacity(), 26 * 26);
        assert_eq!(JumpSession::new(0, vec!['x', 'y']).capacity(), 4);
    }

    #[test]
    fn feed_key_resolves_on_two_valid_chars() {
        let mut session = JumpSession::new(16, alphabet());
        // Target 5 → label_indices = (2, 1) → ('c', 'b').
        assert_eq!(session.feed_key(key('c')), JumpSignal::Pending);
        assert_eq!(session.feed_key(key('b')), JumpSignal::Selected(5));
    }

    #[test]
    fn feed_key_cancels_on_esc() {
        let mut session = JumpSession::new(16, alphabet());
        assert_eq!(session.feed_key(esc()), JumpSignal::Cancelled);
        assert!(!session.is_active(), "session should be done after cancel");
    }

    #[test]
    fn feed_key_cancels_on_esc_after_first_char() {
        let mut session = JumpSession::new(16, alphabet());
        assert_eq!(session.feed_key(key('a')), JumpSignal::Pending);
        assert_eq!(session.feed_key(esc()), JumpSignal::Cancelled);
    }

    #[test]
    fn feed_key_cancels_on_non_alphabet_char() {
        let mut session = JumpSession::new(16, alphabet());
        // '!' is not in [a-z]; should bail rather than dangle.
        assert_eq!(session.feed_key(key('!')), JumpSignal::Cancelled);
    }

    #[test]
    fn feed_key_cancels_on_modified_key() {
        // Ctrl-A doesn't count as label input even though 'a' is in
        // the alphabet — the user is doing something else.
        let mut session = JumpSession::new(16, alphabet());
        let ctrl_a = KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::CONTROL,
        };
        assert_eq!(session.feed_key(ctrl_a), JumpSignal::Cancelled);
    }

    #[test]
    fn feed_key_cancels_when_label_resolves_outside_target_count() {
        // 3 targets means valid labels are 0,1,2 (aa, ba, ab). Typing
        // "bb" would resolve to target 3, which doesn't exist.
        let mut session = JumpSession::new(3, alphabet());
        assert_eq!(session.feed_key(key('b')), JumpSignal::Pending);
        assert_eq!(session.feed_key(key('b')), JumpSignal::Cancelled);
    }

    #[test]
    fn feed_key_after_done_keeps_returning_cancelled() {
        let mut session = JumpSession::new(16, alphabet());
        session.feed_key(key('a'));
        session.feed_key(key('a')); // Selected
        assert_eq!(session.feed_key(key('a')), JumpSignal::Cancelled);
        assert_eq!(session.feed_key(esc()), JumpSignal::Cancelled);
    }

    /// Round-trip property: every target gets a unique label, and
    /// feeding that label back resolves to the same target.
    #[test]
    fn every_target_in_first_64_round_trips_through_its_label() {
        // Stop at 64 = 8x8 to keep the test fast but still exercise
        // the full square-spiral algorithm (8 diagonals).
        for target in 0..64u32 {
            let label = JumpSession::new(64, alphabet())
                .label_at(target)
                .unwrap_or_else(|| panic!("label missing for target {target}"));
            let mut session = JumpSession::new(64, alphabet());
            let p1 = session.feed_key(key(label.first));
            let p2 = session.feed_key(key(label.second));
            assert_eq!(p1, JumpSignal::Pending);
            assert_eq!(
                p2,
                JumpSignal::Selected(target),
                "label {}{} should round-trip to target {target}, but selected {p2:?}",
                label.first,
                label.second,
            );
        }
    }

    /// Property: all 64 labels are distinct. Two targets must never
    /// share a label — that would make the reverse lookup ambiguous.
    #[test]
    fn labels_are_unique_for_first_64_targets() {
        let mut seen = std::collections::HashSet::new();
        for target in 0..64u32 {
            let label = JumpSession::new(64, alphabet()).label_at(target).unwrap();
            assert!(
                seen.insert((label.first, label.second)),
                "duplicate label {}{} at target {target}",
                label.first,
                label.second,
            );
        }
        assert_eq!(seen.len(), 64);
    }
}
