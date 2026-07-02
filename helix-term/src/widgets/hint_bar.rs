use std::borrow::Cow;

use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::graphics::{Rect, Style};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint<'a> {
    pub key: Cow<'a, str>,
    pub label: Cow<'a, str>,
    pub priority: u8,
}

impl<'a> Hint<'a> {
    pub fn new(key: impl Into<Cow<'a, str>>, label: impl Into<Cow<'a, str>>) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            priority: 128,
        }
    }

    pub const fn priority(mut self, priority: u8) -> Self {
        self.priority = priority;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HintBarStyle {
    pub background: Style,
    pub key: Style,
    pub label: Style,
    pub separator: Style,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintBarState {
    pub visible_indices: Vec<usize>,
    pub hidden_count: usize,
}

pub fn hint_bar(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    hints: &[Hint<'_>],
    style: HintBarStyle,
) -> HintBarState {
    let state = hint_bar_layout(hints, area.width);
    if area.width == 0 || area.height == 0 {
        return state;
    }

    surface.set_style(
        tui::ratatui::to_ratatui_rect(area),
        tui::ratatui::to_ratatui_style(style.background),
    );

    let mut x = area.x;
    for (slot, index) in state.visible_indices.iter().copied().enumerate() {
        if slot > 0 {
            surface.set_stringn(
                x,
                area.y,
                "  ",
                area.right().saturating_sub(x) as usize,
                tui::ratatui::to_ratatui_style(style.separator),
            );
            x = x.saturating_add(2);
        }
        let hint = &hints[index];
        surface.set_stringn(
            x,
            area.y,
            &hint.key,
            area.right().saturating_sub(x) as usize,
            tui::ratatui::to_ratatui_style(style.key),
        );
        x = x.saturating_add(hint.key.width() as u16);
        if x < area.right() {
            surface.set_stringn(
                x,
                area.y,
                " ",
                area.right().saturating_sub(x) as usize,
                tui::ratatui::to_ratatui_style(style.separator),
            );
            x = x.saturating_add(1);
        }
        surface.set_stringn(
            x,
            area.y,
            &hint.label,
            area.right().saturating_sub(x) as usize,
            tui::ratatui::to_ratatui_style(style.label),
        );
        x = x.saturating_add(hint.label.width() as u16);
    }

    if state.hidden_count > 0 && x < area.right() {
        let more = format!(" +{} more", state.hidden_count);
        surface.set_stringn(
            x,
            area.y,
            &more,
            area.right().saturating_sub(x) as usize,
            tui::ratatui::to_ratatui_style(style.separator),
        );
    }

    state
}

pub fn hint_bar_layout(hints: &[Hint<'_>], width: u16) -> HintBarState {
    let mut candidates: Vec<_> = hints.iter().enumerate().collect();
    candidates.sort_by_key(|(index, hint)| (std::cmp::Reverse(hint.priority), *index));

    let mut used = 0u16;
    let mut visible_indices = Vec::new();
    for (index, hint) in candidates {
        let hint_width =
            hint_width(hint).saturating_add(if visible_indices.is_empty() { 0 } else { 2 });
        if used.saturating_add(hint_width) <= width {
            used = used.saturating_add(hint_width);
            visible_indices.push(index);
        }
    }
    visible_indices.sort_unstable();

    HintBarState {
        hidden_count: hints.len().saturating_sub(visible_indices.len()),
        visible_indices,
    }
}

fn hint_width(hint: &Hint<'_>) -> u16 {
    hint.key
        .width()
        .saturating_add(1)
        .saturating_add(hint.label.width())
        .try_into()
        .unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hint_bar_elides_low_priority_hints_first() {
        let hints = [
            Hint::new("enter", "open").priority(200),
            Hint::new("ctrl-s", "split").priority(50),
            Hint::new("esc", "close").priority(180),
        ];
        let state = hint_bar_layout(&hints, 21);
        assert_eq!(state.visible_indices, vec![0, 2]);
        assert_eq!(state.hidden_count, 1);
    }
}
