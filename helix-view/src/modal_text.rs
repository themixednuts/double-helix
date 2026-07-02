use helix_core::{
    movement::{Direction, Movement},
    surround::FindType,
    text_annotations::TextAnnotations,
    textobject::{self, TextObject},
    Range, Rope, RopeSlice,
};

/// Typed motions for component-owned text that wants editor-equivalent modal behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModalTextMotion {
    Char(isize),
    FindChar {
        ch: char,
        direction: Direction,
        inclusive: bool,
        count: usize,
    },
    LineStart,
    LineEnd,
    NextWordStart(usize),
    PrevWordStart(usize),
    NextWordEnd(usize),
    PrevWordEnd(usize),
}

/// Typed text objects for component-owned text that mirrors editor selection
/// behavior without requiring a tree-backed document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModalTextObject {
    InsideWord,
    AroundWord,
    InsideLongWord,
    AroundLongWord,
    InsideParagraph,
    AroundParagraph,
    InsideSurroundingPair(char),
    AroundSurroundingPair(char),
    InsidePreviousPair(char),
    InsideNextPair(char),
    InsideClosestPair,
    AroundClosestPair,
}

impl ModalTextObject {
    const fn word_kind(self) -> Option<TextObject> {
        match self {
            Self::InsideWord | Self::InsideLongWord => Some(TextObject::Inside),
            Self::AroundWord | Self::AroundLongWord => Some(TextObject::Around),
            Self::InsideParagraph
            | Self::AroundParagraph
            | Self::InsideSurroundingPair(_)
            | Self::AroundSurroundingPair(_)
            | Self::InsidePreviousPair(_)
            | Self::InsideNextPair(_)
            | Self::InsideClosestPair
            | Self::AroundClosestPair => None,
        }
    }

    const fn long(self) -> bool {
        matches!(self, Self::InsideLongWord | Self::AroundLongWord)
    }
}

/// A single Helix-style text selection over component-owned text.
///
/// This intentionally stores Helix `Range` anchor/head semantics instead of a
/// standalone cursor so UI components do not have to reimplement modal movement
/// and highlighting rules.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModalTextSelection {
    anchor: usize,
    head: usize,
}

impl ModalTextSelection {
    pub const fn new(anchor: usize, head: usize) -> Self {
        Self { anchor, head }
    }

    pub const fn point(char_idx: usize) -> Self {
        Self::new(char_idx, char_idx)
    }

    pub fn all(text: &str) -> Self {
        Self::new(0, text.chars().count())
    }

    pub const fn anchor(self) -> usize {
        self.anchor
    }

    pub const fn head(self) -> usize {
        self.head
    }

    pub const fn from_range(range: Range) -> Self {
        Self::new(range.anchor, range.head)
    }

    pub const fn to_range(self) -> Range {
        Range::new(self.anchor, self.head)
    }

    pub fn clamp(self, text: &str) -> Self {
        self.clamp_len(text.chars().count())
    }

    const fn clamp_len(self, len: usize) -> Self {
        Self::new(
            if self.anchor < len { self.anchor } else { len },
            if self.head < len { self.head } else { len },
        )
    }

    pub fn collapse_to_cursor(self, text: &str) -> Self {
        Self::point(self.cursor(text))
    }

    pub const fn flip(self) -> Self {
        Self::new(self.head, self.anchor)
    }

    pub fn cursor(self, text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }
        let rope = Rope::from(text);
        self.to_range().cursor(rope.slice(..))
    }

    pub fn span(self, text: &str) -> Option<std::ops::Range<usize>> {
        let selection = self.clamp(text);
        let start = selection.anchor.min(selection.head);
        let end = selection.anchor.max(selection.head);
        (start < end).then_some(start..end)
    }

    pub fn edit_span(self, text: &str) -> Option<std::ops::Range<usize>> {
        let len = text.chars().count();
        if len == 0 {
            return None;
        }
        self.span(text).or_else(|| {
            let cursor = self.cursor(text).min(len.saturating_sub(1));
            Some(cursor..cursor.saturating_add(1).min(len))
        })
    }

    pub fn is_all(self, text: &str) -> bool {
        self.edit_span(text)
            .is_some_and(|span| span.start == 0 && span.end == text.chars().count())
    }

    pub fn apply_motion(self, text: &str, motion: ModalTextMotion, movement: Movement) -> Self {
        let len = text.chars().count();
        if len == 0 {
            return Self::default();
        }

        let rope = Rope::from(text);
        let text = rope.slice(..);
        let range = self.clamp_len(len).to_range();
        let cursor = range.cursor(text);
        let extend = movement == Movement::Extend;
        let annotations = TextAnnotations::default();

        let range = match motion {
            ModalTextMotion::Char(delta) => {
                let target = cursor
                    .saturating_add_signed(delta)
                    .min(len.saturating_sub(1));
                range.put_cursor(text, target, extend)
            }
            ModalTextMotion::FindChar {
                ch,
                direction,
                inclusive,
                count,
            } => find_char_target(text, cursor, ch, direction, inclusive, count)
                .map_or(range, |target| range.put_cursor(text, target, extend)),
            ModalTextMotion::LineStart => range.put_cursor(text, 0, extend),
            ModalTextMotion::LineEnd => range.put_cursor(text, len.saturating_sub(1), extend),
            ModalTextMotion::NextWordStart(count) => {
                let moved =
                    helix_core::movement::move_next_word_start(text, &annotations, range, count);
                apply_word_movement(text, range, moved, movement)
            }
            ModalTextMotion::PrevWordStart(count) => {
                let moved =
                    helix_core::movement::move_prev_word_start(text, &annotations, range, count);
                apply_word_movement(text, range, moved, movement)
            }
            ModalTextMotion::NextWordEnd(count) => {
                let moved =
                    helix_core::movement::move_next_word_end(text, &annotations, range, count);
                apply_word_movement(text, range, moved, movement)
            }
            ModalTextMotion::PrevWordEnd(count) => {
                let moved =
                    helix_core::movement::move_prev_word_end(text, &annotations, range, count);
                apply_word_movement(text, range, moved, movement)
            }
        };
        Self::from_range(range).clamp_len(len)
    }

    pub fn select_text_object(self, text: &str, object: ModalTextObject, count: usize) -> Self {
        let len = text.chars().count();
        if len == 0 {
            return Self::default();
        }

        let rope = Rope::from(text);
        let text = rope.slice(..);
        let range = self.clamp_len(len).to_range();
        let range = match object {
            ModalTextObject::InsideWord
            | ModalTextObject::AroundWord
            | ModalTextObject::InsideLongWord
            | ModalTextObject::AroundLongWord => match object.word_kind() {
                Some(kind) => textobject::textobject_word(text, range, kind, count, object.long()),
                None => range,
            },
            ModalTextObject::InsideParagraph | ModalTextObject::AroundParagraph => {
                Range::new(0, len)
            }
            ModalTextObject::InsideSurroundingPair(ch) => textobject::textobject_pair_surround(
                None,
                text,
                range,
                TextObject::Inside,
                ch,
                FindType::Surround,
                count,
            ),
            ModalTextObject::AroundSurroundingPair(ch) => textobject::textobject_pair_surround(
                None,
                text,
                range,
                TextObject::Around,
                ch,
                FindType::Surround,
                count,
            ),
            ModalTextObject::InsidePreviousPair(ch) => textobject::textobject_pair_surround(
                None,
                text,
                range,
                TextObject::Inside,
                ch,
                FindType::Prev,
                count,
            ),
            ModalTextObject::InsideNextPair(ch) => textobject::textobject_pair_surround(
                None,
                text,
                range,
                TextObject::Inside,
                ch,
                FindType::Next,
                count,
            ),
            ModalTextObject::InsideClosestPair => textobject::textobject_pair_surround_closest(
                None,
                text,
                range,
                TextObject::Inside,
                count,
            ),
            ModalTextObject::AroundClosestPair => textobject::textobject_pair_surround_closest(
                None,
                text,
                range,
                TextObject::Around,
                count,
            ),
        };
        Self::from_range(range).clamp_len(len)
    }
}

fn find_char_target(
    text: RopeSlice,
    cursor: usize,
    ch: char,
    direction: Direction,
    inclusive: bool,
    count: usize,
) -> Option<usize> {
    let chars = text.chars().collect::<Vec<_>>();
    let len = chars.len();
    if len == 0 {
        return None;
    }

    let mut remaining = count.max(1);
    if !inclusive {
        match direction {
            Direction::Forward if cursor + 1 < len && chars[cursor + 1] == ch => {
                remaining = remaining.saturating_add(1);
            }
            Direction::Backward if cursor > 0 && chars[cursor - 1] == ch => {
                remaining = remaining.saturating_add(1);
            }
            Direction::Forward | Direction::Backward => {}
        }
    }

    let found = match direction {
        Direction::Forward => chars
            .iter()
            .enumerate()
            .skip(cursor.saturating_add(1))
            .find_map(|(idx, candidate)| {
                if *candidate != ch {
                    return None;
                }
                remaining = remaining.saturating_sub(1);
                (remaining == 0).then_some(idx)
            }),
        Direction::Backward => {
            chars
                .iter()
                .enumerate()
                .take(cursor)
                .rev()
                .find_map(|(idx, candidate)| {
                    if *candidate != ch {
                        return None;
                    }
                    remaining = remaining.saturating_sub(1);
                    (remaining == 0).then_some(idx)
                })
        }
    }?;

    Some(match (direction, inclusive) {
        (Direction::Forward, true) | (Direction::Backward, true) => found,
        (Direction::Forward, false) => found.saturating_sub(1),
        (Direction::Backward, false) => found.saturating_add(1).min(len.saturating_sub(1)),
    })
}

fn apply_word_movement(text: RopeSlice, base: Range, moved: Range, movement: Movement) -> Range {
    match movement {
        Movement::Move => moved,
        Movement::Extend => base.put_cursor(text, moved.cursor(text), true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_word_uses_helix_range_semantics() {
        let selection = ModalTextSelection::point(0).apply_motion(
            "alpha-beta.rs",
            ModalTextMotion::NextWordStart(1),
            Movement::Move,
        );

        assert_eq!(selection, ModalTextSelection::new(0, 5));
        assert_eq!(selection.cursor("alpha-beta.rs"), 4);
        assert_eq!(selection.span("alpha-beta.rs"), Some(0..5));
    }

    #[test]
    fn extend_word_preserves_anchor() {
        let selection = ModalTextSelection::point(0)
            .apply_motion(
                "alpha-beta.rs",
                ModalTextMotion::NextWordStart(1),
                Movement::Move,
            )
            .apply_motion(
                "alpha-beta.rs",
                ModalTextMotion::NextWordEnd(1),
                Movement::Extend,
            );

        assert_eq!(selection, ModalTextSelection::new(0, 6));
        assert_eq!(selection.cursor("alpha-beta.rs"), 5);
        assert_eq!(selection.span("alpha-beta.rs"), Some(0..6));
    }

    #[test]
    fn select_text_object_uses_core_word_semantics() {
        let selection = ModalTextSelection::point(2).select_text_object(
            "alpha-beta.rs",
            ModalTextObject::InsideWord,
            1,
        );

        assert_eq!(selection.span("alpha-beta.rs"), Some(0..5));

        let selection =
            selection.select_text_object("alpha-beta.rs", ModalTextObject::AroundWord, 1);
        assert_eq!(selection.span("alpha-beta.rs"), Some(0..5));
    }

    #[test]
    fn constructors_are_const() {
        const RANGE: Range = Range::new(2, 5);
        const SELECTION: ModalTextSelection = ModalTextSelection::from_range(RANGE);
        const ROUND_TRIP: Range = SELECTION.to_range();
        const FLIPPED: ModalTextSelection = SELECTION.flip();

        assert_eq!(RANGE, Range::new(2, 5));
        assert_eq!(SELECTION, ModalTextSelection::new(2, 5));
        assert_eq!(ROUND_TRIP, Range::new(2, 5));
        assert_eq!(FLIPPED, ModalTextSelection::new(5, 2));
    }

    #[test]
    fn edit_span_treats_point_as_cursor_character() {
        let selection = ModalTextSelection::point(2);

        assert_eq!(selection.span("alpha"), None);
        assert_eq!(selection.edit_span("alpha"), Some(2..3));
        assert!(!selection.is_all("alpha"));
    }

    #[test]
    fn all_selects_the_complete_text() {
        let selection = ModalTextSelection::all("alpha-beta.rs");

        assert_eq!(selection.span("alpha-beta.rs"), Some(0..13));
        assert_eq!(selection.edit_span("alpha-beta.rs"), Some(0..13));
        assert!(selection.is_all("alpha-beta.rs"));
    }

    #[test]
    fn find_char_motion_uses_editor_till_semantics() {
        let text = "alpha-beta-alpha.rs";

        let found = ModalTextSelection::point(0).apply_motion(
            text,
            ModalTextMotion::FindChar {
                ch: 'a',
                direction: Direction::Forward,
                inclusive: true,
                count: 1,
            },
            Movement::Move,
        );
        assert_eq!(found.cursor(text), 4);

        let till = ModalTextSelection::point(0).apply_motion(
            text,
            ModalTextMotion::FindChar {
                ch: 'a',
                direction: Direction::Forward,
                inclusive: false,
                count: 1,
            },
            Movement::Move,
        );
        assert_eq!(till.cursor(text), 3);

        let previous = ModalTextSelection::point(12).apply_motion(
            text,
            ModalTextMotion::FindChar {
                ch: 'a',
                direction: Direction::Backward,
                inclusive: false,
                count: 1,
            },
            Movement::Move,
        );
        assert_eq!(previous.cursor(text), 10);
    }

    #[test]
    fn text_objects_cover_single_label_paragraph_and_pairs() {
        let text = "alpha(beta).rs";

        let paragraph = ModalTextSelection::point(6).select_text_object(
            text,
            ModalTextObject::InsideParagraph,
            1,
        );
        assert_eq!(paragraph.span(text), Some(0..14));

        let inside_pair = ModalTextSelection::point(7).select_text_object(
            text,
            ModalTextObject::InsideSurroundingPair('('),
            1,
        );
        assert_eq!(inside_pair.span(text), Some(6..10));

        let around_pair = ModalTextSelection::point(7).select_text_object(
            text,
            ModalTextObject::AroundSurroundingPair('('),
            1,
        );
        assert_eq!(around_pair.span(text), Some(5..11));
    }
}
