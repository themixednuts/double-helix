use crate::register::Registers;
use helix_core::unicode::width::UnicodeWidthStr;
use std::{borrow::Cow, fmt::Write};

#[derive(Debug, Clone)]
/// Info box used in editor. Rendering logic will be in other crate.
pub struct Info {
    /// Title shown at top.
    pub title: Cow<'static, str>,
    /// Text body, should contain newlines.
    pub text: String,
    /// Body width.
    pub width: u16,
    /// Body height.
    pub height: u16,
    /// First visible body row when the body is taller than the popup.
    pub scroll: usize,
}

impl Info {
    pub fn new<T, K, V>(title: T, body: &[(K, V)]) -> Self
    where
        T: Into<Cow<'static, str>>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let title = title.into();
        if body.is_empty() {
            return Self {
                height: 1,
                width: title.len() as u16,
                text: "".to_string(),
                title,
                scroll: 0,
            };
        }

        let item_width = body
            .iter()
            .map(|(item, _)| item.as_ref().width())
            .max()
            .unwrap();
        let mut text = String::new();

        for (item, desc) in body {
            let _ = writeln!(
                text,
                "{:width$}  {}",
                item.as_ref(),
                desc.as_ref(),
                width = item_width
            );
        }

        Self {
            title,
            width: text.lines().map(|l| l.width()).max().unwrap() as u16,
            height: body.len() as u16,
            text,
            scroll: 0,
        }
    }

    /// Compute the screen area this info box would occupy in the given viewport.
    pub fn screen_area(&self, viewport: crate::graphics::Rect) -> crate::graphics::Rect {
        let reserved_bottom = if viewport.height > 2 { 2 } else { 0 };
        let available_height = viewport.height.saturating_sub(reserved_bottom);
        let width = self.width.saturating_add(4).min(viewport.width); // +2 border, +2 margin
        let height = self.height.saturating_add(2).min(available_height); // +2 border
        crate::graphics::Rect::new(
            viewport.x + viewport.width.saturating_sub(width),
            viewport.y
                + viewport
                    .height
                    .saturating_sub(reserved_bottom.saturating_add(height)),
            width,
            height,
        )
    }

    pub fn max_scroll(&self, visible_body_height: u16) -> usize {
        (self.height as usize).saturating_sub(visible_body_height as usize)
    }

    pub fn visible_scroll(&self, visible_body_height: u16) -> usize {
        self.scroll.min(self.max_scroll(visible_body_height))
    }

    pub fn scroll_to(&mut self, offset: usize, visible_body_height: u16) {
        self.scroll = offset.min(self.max_scroll(visible_body_height));
    }

    pub fn scroll_by(&mut self, delta: isize, visible_body_height: u16) {
        let next = if delta.is_negative() {
            self.scroll.saturating_sub(delta.unsigned_abs())
        } else {
            self.scroll.saturating_add(delta as usize)
        };
        self.scroll_to(next, visible_body_height);
    }

    pub fn from_registers(title: impl Into<Cow<'static, str>>, registers: &Registers) -> Self {
        let body: Vec<_> = registers
            .iter_preview()
            .map(|(ch, preview)| (ch.to_string(), preview))
            .collect();

        let mut infobox = Self::new(title, &body);
        infobox.width = 30; // copied content could be very long
        infobox
    }
}

#[cfg(test)]
mod tests {
    use super::Info;
    use crate::graphics::Rect;

    #[test]
    fn screen_area_keeps_statusline_margin_when_clamped() {
        let rows: Vec<_> = (0..20)
            .map(|index| (index.to_string(), "description".to_string()))
            .collect();
        let info = Info::new("Help", &rows);

        let area = info.screen_area(Rect::new(0, 0, 80, 10));

        assert_eq!(area.y, 0);
        assert_eq!(area.height, 8);
        assert_eq!(area.bottom(), 8);
    }

    #[test]
    fn scroll_is_clamped_to_visible_body() {
        let rows: Vec<_> = (0..20)
            .map(|index| (index.to_string(), "description".to_string()))
            .collect();
        let mut info = Info::new("Help", &rows);

        info.scroll_to(99, 6);

        assert_eq!(info.scroll, 14);
        assert_eq!(info.visible_scroll(6), 14);
    }
}
