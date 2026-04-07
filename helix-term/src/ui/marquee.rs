//! Marquee: scrolling text for fixed-width areas.
//!
//! Text that fits the viewport is rendered statically. Text that overflows scrolls
//! in a cycle: hold at start → scroll to end → hold at end → snap back → repeat.
//! Stops scrolling after an inactivity timeout; call `touch()` to resume.
//!
//! `render()` returns `Option<Instant>` — the next time the visual output changes.
//! The caller should schedule a re-render at that time (e.g., via `request_redraw`).

use helix_core::unicode::width::UnicodeWidthChar;
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

/// Precomputed text layout for zero-allocation rendering.
struct TextLayout {
    text: Arc<str>,
    /// Byte offset of each character, plus a sentinel at `text.len()`.
    /// Length = char_count + 1.
    byte_offsets: Box<[usize]>,
    /// Cumulative display width at the start of each char.
    /// `cum_widths[0] = 0`, `cum_widths[n] = total_width`.
    /// Length = char_count + 1.
    cum_widths: Box<[usize]>,
    total_width: usize,
}

impl TextLayout {
    fn new(text: Arc<str>) -> Self {
        let mut byte_offsets = Vec::new();
        let mut cum_widths = Vec::new();
        let mut cum = 0usize;

        for (byte_off, ch) in text.char_indices() {
            byte_offsets.push(byte_off);
            cum_widths.push(cum);
            cum += ch.width().unwrap_or(0);
        }
        byte_offsets.push(text.len());
        cum_widths.push(cum);

        Self {
            text,
            byte_offsets: byte_offsets.into_boxed_slice(),
            cum_widths: cum_widths.into_boxed_slice(),
            total_width: cum,
        }
    }

    /// Largest char offset where remaining text fits in `viewport_width`.
    /// Uses binary search on the cumulative width table.
    fn max_scroll_offset(&self, viewport_width: usize) -> usize {
        if self.total_width <= viewport_width {
            return 0;
        }
        let threshold = self.total_width - viewport_width;
        // Find first i where cum_widths[i] >= threshold.
        // Both Ok (exact) and Err (insertion point) give the right index.
        match self.cum_widths.binary_search(&threshold) {
            Ok(i) | Err(i) => i,
        }
    }

    /// Render text starting at `char_offset` into the surface area.
    /// Zero allocation — passes a `&str` slice directly to `set_stringn`.
    fn render_at_offset(
        &self,
        char_offset: usize,
        area: Rect,
        surface: &mut Surface,
        style: Style,
    ) {
        let byte_start = self
            .byte_offsets
            .get(char_offset)
            .copied()
            .unwrap_or(self.text.len());
        surface.set_stringn(
            area.x,
            area.y,
            &self.text[byte_start..],
            area.width as usize,
            style,
        );
    }
}

/// Scrolling text widget for fixed-width areas.
///
/// Call `touch()` on focus or user interaction so scrolling continues;
/// after the inactivity timeout, text holds at the start until the next `touch()`.
///
/// # Render loop
///
/// ```ignore
/// let next = marquee.render(area, surface, style);
/// if let Some(when) = next {
///     schedule_redraw_at(when);
/// }
/// ```
#[derive(Debug)]
pub struct Marquee {
    layout: Option<TextLayout>,
    scroll_start: Option<Instant>,
    last_activity: Option<Instant>,
    inactivity_timeout: Duration,
    scroll_duration: Duration,
    hold_end: Duration,
    hold_start: Duration,
}

impl std::fmt::Debug for TextLayout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextLayout")
            .field("total_width", &self.total_width)
            .field("char_count", &(self.byte_offsets.len().saturating_sub(1)))
            .finish()
    }
}

impl Default for Marquee {
    fn default() -> Self {
        Self {
            layout: None,
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

    /// Set or clear the text. Precomputes layout and resets timers.
    pub fn set_text(&mut self, text: Option<impl Into<Arc<str>>>) {
        let now = Instant::now();
        let text: Option<Arc<str>> = text.map(Into::into);
        self.layout = text.map(TextLayout::new);
        self.scroll_start = self.layout.as_ref().map(|_| now);
        self.last_activity = self.layout.as_ref().map(|_| now);
    }

    /// Reset the inactivity timeout. Call on user interaction or focus so
    /// scrolling continues (or resumes if paused).
    pub fn touch(&mut self) {
        if self.layout.is_some() {
            self.last_activity = Some(Instant::now());
            if self.scroll_start.is_none() {
                self.scroll_start = Some(Instant::now());
            }
        }
    }

    /// Whether there is text to show.
    pub fn has_text(&self) -> bool {
        self.layout.is_some()
    }

    /// Render the marquee into `area`.
    ///
    /// Returns `Some(instant)` if the marquee is animating and needs another
    /// render at that time. Returns `None` if static (text fits, inactive, or
    /// no text).
    pub fn render(&self, area: Rect, surface: &mut Surface, style: Style) -> Option<Instant> {
        let layout = self.layout.as_ref()?;
        let viewport = area.width as usize;
        if viewport == 0 {
            return None;
        }

        // Text fits — static render, no animation.
        if layout.total_width <= viewport {
            layout.render_at_offset(0, area, surface, style);
            return None;
        }

        let scroll_start = self.scroll_start?;
        let last_activity = self.last_activity?;
        let now = Instant::now();

        // Inactive — freeze at start.
        if now.saturating_duration_since(last_activity) > self.inactivity_timeout {
            layout.render_at_offset(0, area, surface, style);
            return None;
        }

        let max_offset = layout.max_scroll_offset(viewport);
        if max_offset == 0 {
            layout.render_at_offset(0, area, surface, style);
            return None;
        }

        // Cycle timing.
        let cycle = self.scroll_duration + self.hold_end + self.hold_start;
        let cycle_secs = cycle.as_secs_f64();
        if cycle_secs <= 0.0 {
            layout.render_at_offset(0, area, surface, style);
            return None;
        }

        let elapsed = now.saturating_duration_since(scroll_start);
        let pos = elapsed.as_secs_f64() % cycle_secs;
        let scroll_secs = self.scroll_duration.as_secs_f64();
        let hold_end_secs = self.hold_end.as_secs_f64();

        let (char_offset, next_frame) = if pos < scroll_secs {
            // Scrolling phase: interpolate offset.
            let t = pos / scroll_secs;
            let offset = (t * max_offset as f64).round() as usize;
            let offset = offset.min(max_offset);

            // Next frame: when the next character boundary scrolls in.
            let next_offset = (offset + 1).min(max_offset);
            if next_offset > offset {
                let next_t = next_offset as f64 / max_offset as f64;
                let dt = Duration::from_secs_f64((next_t - t) * scroll_secs);
                (offset, now + dt)
            } else {
                // At max offset — wait for hold_end transition.
                let remaining = scroll_secs - pos;
                (offset, now + Duration::from_secs_f64(remaining))
            }
        } else if pos < scroll_secs + hold_end_secs {
            // Hold at end.
            let remaining = (scroll_secs + hold_end_secs) - pos;
            (max_offset, now + Duration::from_secs_f64(remaining))
        } else {
            // Hold at start.
            let remaining = cycle_secs - pos;
            (0, now + Duration::from_secs_f64(remaining))
        };

        layout.render_at_offset(char_offset, area, surface, style);
        Some(next_frame)
    }
}

/// Schedule a `request_redraw` at the given instant (for marquee animation).
/// Spawns runtime work that sleeps, then queues a typed redraw event.
pub fn schedule_redraw_at(
    work: helix_runtime::Work,
    when: Instant,
    ingress: helix_runtime::Sender<crate::runtime::RuntimeEvent>,
) {
    work.spawn(async move {
        tokio::time::sleep_until(tokio::time::Instant::from_std(when)).await;
        crate::runtime::send_redraw_with(ingress).await;
    }).detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_text_fits() {
        let layout = TextLayout::new("hello".into());
        assert_eq!(layout.total_width, 5);
        assert_eq!(layout.max_scroll_offset(10), 0);
        assert_eq!(layout.max_scroll_offset(5), 0);
    }

    #[test]
    fn max_scroll_offset_ascii() {
        let layout = TextLayout::new("hello world".into());
        // 11 chars, viewport 5 → need to scroll 6 chars to see "world"
        assert_eq!(layout.max_scroll_offset(5), 6);
        assert_eq!(layout.max_scroll_offset(11), 0);
        assert_eq!(layout.max_scroll_offset(1), 10);
    }

    #[test]
    fn max_scroll_offset_wide() {
        // 3 wide chars, each width 2, total width 6
        let layout = TextLayout::new("あいう".into());
        assert_eq!(layout.total_width, 6);
        // viewport 4: need offset where remaining width ≤ 4
        // offset 1 → "いう" (width 4) ✓
        assert_eq!(layout.max_scroll_offset(4), 1);
        // viewport 2: offset 2 → "う" (width 2) ✓
        assert_eq!(layout.max_scroll_offset(2), 2);
    }

    #[test]
    fn render_returns_none_when_fits() {
        let mut marquee = Marquee::new();
        marquee.set_text(Some("hi"));
        let area = Rect::new(0, 0, 10, 1);
        let mut surface = Surface::empty(Rect::new(0, 0, 10, 1));
        let next = marquee.render(area, &mut surface, Style::default());
        assert!(next.is_none());
    }

    #[test]
    fn render_returns_some_when_scrolling() {
        let mut marquee = Marquee::new();
        marquee.set_text(Some("this is a long string that overflows"));
        let area = Rect::new(0, 0, 10, 1);
        let mut surface = Surface::empty(Rect::new(0, 0, 10, 1));
        let next = marquee.render(area, &mut surface, Style::default());
        assert!(next.is_some());
    }

    #[test]
    fn no_text_returns_none() {
        let marquee = Marquee::new();
        let area = Rect::new(0, 0, 10, 1);
        let mut surface = Surface::empty(Rect::new(0, 0, 10, 1));
        assert!(marquee
            .render(area, &mut surface, Style::default())
            .is_none());
    }
}
