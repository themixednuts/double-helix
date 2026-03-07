//! Marquee: scrolling text for fixed-width areas.
//!
//! Use when text may exceed the display width: scrolls from start → hold at end →
//! reset → hold at start, and repeats. Stops scrolling after an inactivity timeout
//! since last focus or user interaction (call `touch()` when the user interacts
//! or when the container gains focus).

use helix_core::unicode::width::{UnicodeWidthChar, UnicodeWidthStr};
use helix_view::graphics::{Rect, Style};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tui::buffer::Buffer as Surface;

/// Default inactivity duration after which the marquee pauses (held at start).
pub const DEFAULT_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(30);
/// Default duration to scroll from start to end (full sweep).
pub const DEFAULT_SCROLL_DURATION: Duration = Duration::from_secs(8);
/// Default hold time at the end of the text before resetting.
pub const DEFAULT_HOLD_END: Duration = Duration::from_millis(1500);
/// Default hold time at the start after reset before scrolling again.
pub const DEFAULT_HOLD_START: Duration = Duration::from_millis(1000);

/// Marquee: optionally scrolling text in a fixed-width line. Call `touch()` on
/// focus or user interaction so scrolling continues; after the inactivity
/// timeout, the text is held at the start until the next `touch()`.
#[derive(Debug)]
pub struct Marquee {
    text: Option<Arc<str>>,
    scroll_start: Option<Instant>,
    last_activity: Option<Instant>,
    inactivity_timeout: Duration,
    scroll_duration: Duration,
    hold_end: Duration,
    hold_start: Duration,
}

impl Default for Marquee {
    fn default() -> Self {
        Self {
            text: None,
            scroll_start: None,
            last_activity: None,
            inactivity_timeout: DEFAULT_INACTIVITY_TIMEOUT,
            scroll_duration: DEFAULT_SCROLL_DURATION,
            hold_end: DEFAULT_HOLD_END,
            hold_start: DEFAULT_HOLD_START,
        }
    }
}

impl Marquee {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set inactivity timeout (after this with no `touch()`, scrolling pauses).
    pub fn with_inactivity_timeout(mut self, d: Duration) -> Self {
        self.inactivity_timeout = d;
        self
    }

    /// Set time for one full scroll (start → end).
    pub fn with_scroll_duration(mut self, d: Duration) -> Self {
        self.scroll_duration = d;
        self
    }

    /// Set hold time at end and at start.
    pub fn with_hold_times(mut self, end: Duration, start: Duration) -> Self {
        self.hold_end = end;
        self.hold_start = start;
        self
    }

    /// Set or clear the text. When set, scroll and activity timers are reset.
    pub fn set_text(&mut self, text: Option<impl Into<Arc<str>>>) {
        let now = Instant::now();
        self.text = text.map(Into::into);
        self.scroll_start = self.text.as_ref().map(|_| now);
        self.last_activity = self.text.as_ref().map(|_| now);
    }

    /// Call when the user interacts or when the container gains focus so that
    /// scrolling continues (and the inactivity timeout is reset).
    pub fn touch(&mut self) {
        if self.text.is_some() {
            self.last_activity = Some(Instant::now());
            if self.scroll_start.is_none() {
                self.scroll_start = Some(Instant::now());
            }
        }
    }

    /// Whether there is text to show (even if empty string).
    pub fn has_text(&self) -> bool {
        self.text.is_some()
    }

    /// Render the current (possibly scrolled) text into the given line area.
    /// Uses `Instant::now()` for timing. If the text fits in the width, it is
    /// drawn static; otherwise the marquee cycle runs (or is frozen after
    /// inactivity).
    pub fn render(&mut self, area: Rect, surface: &mut Surface, style: Style) {
        let Some(ref text) = self.text else {
            return;
        };
        let now = Instant::now();
        let width_cols = area.width as usize;
        if width_cols == 0 {
            return;
        }

        let total_width = text.width();
        if total_width <= width_cols {
            surface.set_stringn(
                area.x,
                area.y,
                text.as_ref(),
                width_cols.saturating_add(8),
                style,
            );
            return;
        }

        let scroll_start = match self.scroll_start {
            Some(s) => s,
            None => return,
        };
        let last_activity = match self.last_activity {
            Some(a) => a,
            None => return,
        };
        if now.saturating_duration_since(last_activity) > self.inactivity_timeout {
            let visible = slice_to_width(text, 0, width_cols);
            surface.set_stringn(
                area.x,
                area.y,
                &visible,
                width_cols.saturating_add(8),
                style,
            );
            return;
        }

        let max_offset = max_scroll_offset(text, width_cols);
        if max_offset == 0 {
            surface.set_stringn(
                area.x,
                area.y,
                text.as_ref(),
                width_cols.saturating_add(8),
                style,
            );
            return;
        }

        let cycle_duration = self.scroll_duration + self.hold_end + self.hold_start;
        let elapsed = now.saturating_duration_since(scroll_start);
        let pos_secs = elapsed.as_secs_f64();
        let cycle_secs = cycle_duration.as_secs_f64();
        let phase = if cycle_secs <= 0.0 {
            0.0
        } else {
            (pos_secs % cycle_secs) / cycle_secs
        };

        let scroll_phase_duration = self.scroll_duration.as_secs_f64() / cycle_secs;
        let hold_end_duration = self.hold_end.as_secs_f64() / cycle_secs;

        let char_offset = if phase < scroll_phase_duration {
            let t = phase / scroll_phase_duration;
            (t * max_offset as f64).round() as usize
        } else if phase < scroll_phase_duration + hold_end_duration {
            max_offset
        } else {
            0
        };

        let char_offset = char_offset.min(max_offset);
        let visible = slice_to_width(text, char_offset, width_cols);
        surface.set_stringn(
            area.x,
            area.y,
            &visible,
            width_cols.saturating_add(8),
            style,
        );
    }
}

/// Slice `s` starting at character index `start_char`, taking characters until
/// display width reaches `max_width` (or string ends). Returns a boxed slice for display.
fn slice_to_width(s: &str, start_char: usize, max_width: usize) -> Box<str> {
    let byte_start = s
        .char_indices()
        .nth(start_char)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    let tail = &s[byte_start..];
    let mut width = 0usize;
    let mut end_byte = byte_start;
    for (i, c) in tail.char_indices() {
        let w = c.width().unwrap_or(0);
        if width + w > max_width {
            break;
        }
        width += w;
        end_byte = byte_start + i + c.len_utf8();
    }
    s[byte_start..end_byte.min(s.len())]
        .to_string()
        .into_boxed_str()
}

/// Largest character index such that the substring from that index to the end
/// has display width <= max_width (so we can scroll to show the tail).
fn max_scroll_offset(s: &str, max_width: usize) -> usize {
    if s.width() <= max_width {
        return 0;
    }
    let chars: Box<[char]> = s.chars().collect();
    for i in 0..=chars.len() {
        let tail: String = chars[i..].iter().collect();
        if tail.width() <= max_width {
            return i;
        }
    }
    chars.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slice_to_width() {
        assert_eq!(&*slice_to_width("hello", 0, 5), "hello");
        assert_eq!(&*slice_to_width("hello", 0, 3), "hel");
        assert_eq!(&*slice_to_width("hello", 2, 3), "llo");
    }

    #[test]
    fn test_max_scroll_offset() {
        assert_eq!(max_scroll_offset("hi", 10), 0);
        assert_eq!(max_scroll_offset("hello world", 5), 6);
    }
}
