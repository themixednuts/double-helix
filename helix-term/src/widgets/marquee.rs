use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use helix_core::unicode::width::UnicodeWidthChar;
use helix_view::graphics::{Rect, Style};

pub const DEFAULT_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_SCROLL_DURATION: Duration = Duration::from_secs(8);
pub const DEFAULT_HOLD_END: Duration = Duration::from_millis(1500);
pub const DEFAULT_HOLD_START: Duration = Duration::from_millis(1000);

#[derive(Debug)]
struct TextLayout {
    text: Arc<str>,
    byte_offsets: Box<[usize]>,
    cum_widths: Box<[usize]>,
    total_width: usize,
}

#[derive(Debug, Clone)]
pub struct MarqueeFrame {
    text: Arc<str>,
    byte_start: usize,
    pub next_redraw: Option<Instant>,
}

impl MarqueeFrame {
    pub fn paint(&self, area: Rect, surface: &mut crate::render::CellSurface, style: Style) {
        surface.set_stringn(
            area.x,
            area.y,
            &self.text[self.byte_start..],
            area.width as usize,
            tui::ratatui::to_ratatui_style(style),
        );
    }
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

    fn max_scroll_offset(&self, viewport_width: usize) -> usize {
        if self.total_width <= viewport_width {
            return 0;
        }
        let threshold = self.total_width - viewport_width;
        match self.cum_widths.binary_search(&threshold) {
            Ok(i) | Err(i) => i,
        }
    }
}

/// Scrolling single-line text for fixed-width areas.
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

    pub fn with_inactivity_timeout(mut self, d: Duration) -> Self {
        self.inactivity_timeout = d;
        self
    }

    pub fn with_scroll_duration(mut self, d: Duration) -> Self {
        self.scroll_duration = d;
        self
    }

    pub fn with_hold_times(mut self, end: Duration, start: Duration) -> Self {
        self.hold_end = end;
        self.hold_start = start;
        self
    }

    pub fn set_text(&mut self, text: Option<impl Into<Arc<str>>>) {
        let now = Instant::now();
        self.layout = text.map(Into::into).map(TextLayout::new);
        self.scroll_start = self.layout.as_ref().map(|_| now);
        self.last_activity = self.layout.as_ref().map(|_| now);
    }

    pub fn touch(&mut self) {
        if self.layout.is_some() {
            let now = Instant::now();
            self.last_activity = Some(now);
            if self.scroll_start.is_none() {
                self.scroll_start = Some(now);
            }
        }
    }

    pub fn has_text(&self) -> bool {
        self.layout.is_some()
    }

    pub fn render(
        &self,
        area: Rect,
        surface: &mut crate::render::CellSurface,
        style: Style,
    ) -> Option<Instant> {
        let frame = self.sample(area.width, Instant::now())?;
        frame.paint(area, surface, style);
        frame.next_redraw
    }

    pub fn sample(&self, viewport_width: u16, now: Instant) -> Option<MarqueeFrame> {
        let layout = self.layout.as_ref()?;
        let viewport = viewport_width as usize;
        if viewport == 0 {
            return None;
        }
        if layout.total_width <= viewport {
            return Some(MarqueeFrame {
                text: Arc::clone(&layout.text),
                byte_start: 0,
                next_redraw: None,
            });
        }

        let scroll_start = self.scroll_start?;
        let last_activity = self.last_activity?;
        if now.saturating_duration_since(last_activity) > self.inactivity_timeout {
            return Some(MarqueeFrame {
                text: Arc::clone(&layout.text),
                byte_start: 0,
                next_redraw: None,
            });
        }

        let max_offset = layout.max_scroll_offset(viewport);
        if max_offset == 0 {
            return Some(MarqueeFrame {
                text: Arc::clone(&layout.text),
                byte_start: 0,
                next_redraw: None,
            });
        }

        let cycle = self.scroll_duration + self.hold_end + self.hold_start;
        let cycle_secs = cycle.as_secs_f64();
        if cycle_secs <= 0.0 {
            return Some(MarqueeFrame {
                text: Arc::clone(&layout.text),
                byte_start: 0,
                next_redraw: None,
            });
        }

        let pos = now.saturating_duration_since(scroll_start).as_secs_f64() % cycle_secs;
        let scroll_secs = self.scroll_duration.as_secs_f64();
        let hold_end_secs = self.hold_end.as_secs_f64();
        let (char_offset, next_frame) = if pos < scroll_secs {
            let t = pos / scroll_secs;
            let offset = (t * max_offset as f64).round().min(max_offset as f64) as usize;
            let next_offset = (offset + 1).min(max_offset);
            if next_offset > offset {
                let next_t = next_offset as f64 / max_offset as f64;
                (
                    offset,
                    now + Duration::from_secs_f64((next_t - t) * scroll_secs),
                )
            } else {
                (offset, now + Duration::from_secs_f64(scroll_secs - pos))
            }
        } else if pos < scroll_secs + hold_end_secs {
            (
                max_offset,
                now + Duration::from_secs_f64((scroll_secs + hold_end_secs) - pos),
            )
        } else {
            (0, now + Duration::from_secs_f64(cycle_secs - pos))
        };

        let byte_start = layout
            .byte_offsets
            .get(char_offset)
            .copied()
            .unwrap_or(layout.text.len());
        Some(MarqueeFrame {
            text: Arc::clone(&layout.text),
            byte_start,
            next_redraw: Some(next_frame),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_scroll_offset_ascii() {
        let layout = TextLayout::new("hello world".into());
        assert_eq!(layout.max_scroll_offset(5), 6);
        assert_eq!(layout.max_scroll_offset(11), 0);
        assert_eq!(layout.max_scroll_offset(1), 10);
    }

    #[test]
    fn max_scroll_offset_wide() {
        let layout = TextLayout::new("あいう".into());
        assert_eq!(layout.total_width, 6);
        assert_eq!(layout.max_scroll_offset(4), 1);
        assert_eq!(layout.max_scroll_offset(2), 2);
    }

    #[test]
    fn render_returns_none_when_fits() {
        let mut marquee = Marquee::new();
        marquee.set_text(Some("hi"));
        let area = Rect::new(0, 0, 10, 1);
        let mut surface =
            crate::render::CellSurface::empty(tui::ratatui::layout::Rect::new(0, 0, 10, 1));
        assert!(marquee
            .render(area, &mut surface, Style::default())
            .is_none());
    }

    #[test]
    fn render_returns_some_when_scrolling() {
        let mut marquee = Marquee::new();
        marquee.set_text(Some("this is a long string that overflows"));
        let area = Rect::new(0, 0, 10, 1);
        let mut surface =
            crate::render::CellSurface::empty(tui::ratatui::layout::Rect::new(0, 0, 10, 1));
        assert!(marquee
            .render(area, &mut surface, Style::default())
            .is_some());
    }

    #[test]
    fn sampled_frame_is_owned_and_send() {
        fn assert_send<T: Send>() {}

        let mut marquee = Marquee::new();
        marquee.set_text(Some("owned frame"));
        let frame = marquee.sample(5, Instant::now()).expect("frame");

        assert_send::<MarqueeFrame>();
        assert!(Arc::strong_count(&frame.text) >= 2);
    }
}
