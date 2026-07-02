use std::borrow::Cow;

use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::graphics::{Rect, Style};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab<'a> {
    pub label: Cow<'a, str>,
    pub badge: Option<Cow<'a, str>>,
}

impl<'a> Tab<'a> {
    pub fn new(label: impl Into<Cow<'a, str>>) -> Self {
        Self {
            label: label.into(),
            badge: None,
        }
    }

    pub fn badge(mut self, badge: impl Into<Cow<'a, str>>) -> Self {
        self.badge = Some(badge.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TabsStyle {
    pub background: Style,
    pub active: Style,
    pub inactive: Style,
    pub hover: Style,
    pub badge: Style,
    pub overflow: Style,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TabsState {
    pub scroll: u16,
    pub visible_start: usize,
    pub visible_end: usize,
    pub left_overflow: bool,
    pub right_overflow: bool,
}

pub fn tabs(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    tabs: &[Tab<'_>],
    active: usize,
    scroll: u16,
    style: TabsStyle,
) -> TabsState {
    let state = tabs_layout(tabs, active, area.width, scroll);
    if area.width == 0 || area.height == 0 {
        return state;
    }
    surface.set_style(
        tui::ratatui::to_ratatui_rect(area),
        tui::ratatui::to_ratatui_style(style.background),
    );
    let mut logical_x = 0u16;
    for (index, tab) in tabs.iter().enumerate() {
        let width = tab_width(tab);
        let end = logical_x.saturating_add(width);
        if end > state.scroll && logical_x < state.scroll.saturating_add(area.width) {
            let x = area.x + logical_x.saturating_sub(state.scroll);
            let available = area.right().saturating_sub(x);
            let tab_style = if index == active {
                style.active
            } else {
                style.inactive
            };
            let text = tab_text(tab);
            surface.set_stringn(
                x,
                area.y,
                &text,
                available as usize,
                tui::ratatui::to_ratatui_style(tab_style),
            );
            if let Some(badge) = &tab.badge {
                let badge_width = badge.width() as u16;
                if badge_width < available {
                    let badge_x = x + width.saturating_sub(badge_width + 1);
                    if badge_x < area.right() {
                        surface.set_stringn(
                            badge_x,
                            area.y,
                            badge,
                            area.right().saturating_sub(badge_x) as usize,
                            tui::ratatui::to_ratatui_style(style.badge),
                        );
                    }
                }
            }
        }
        logical_x = end;
    }
    if state.left_overflow {
        surface.set_stringn(
            area.x,
            area.y,
            "‹",
            1,
            tui::ratatui::to_ratatui_style(style.overflow),
        );
    }
    if state.right_overflow {
        surface.set_stringn(
            area.right().saturating_sub(1),
            area.y,
            "›",
            1,
            tui::ratatui::to_ratatui_style(style.overflow),
        );
    }
    state
}

pub fn tabs_layout(tabs: &[Tab<'_>], active: usize, width: u16, scroll: u16) -> TabsState {
    let mut positions = Vec::with_capacity(tabs.len());
    let mut total = 0u16;
    for tab in tabs {
        positions.push(total);
        total = total.saturating_add(tab_width(tab));
    }
    let active = active.min(tabs.len().saturating_sub(1));
    let active_start = positions.get(active).copied().unwrap_or(0);
    let active_end = active_start.saturating_add(tabs.get(active).map(tab_width).unwrap_or(0));
    let mut scroll = scroll.min(total.saturating_sub(width));
    if active_start < scroll {
        scroll = active_start;
    } else if active_end > scroll.saturating_add(width) {
        scroll = active_end.saturating_sub(width);
    }

    let visible_start = tabs
        .iter()
        .enumerate()
        .position(|(index, tab)| positions[index].saturating_add(tab_width(tab)) > scroll)
        .unwrap_or(0);
    let visible_end = tabs
        .iter()
        .enumerate()
        .skip(visible_start)
        .take_while(|(index, _)| positions[*index] < scroll.saturating_add(width))
        .map(|(index, _)| index + 1)
        .last()
        .unwrap_or(visible_start);

    TabsState {
        scroll,
        visible_start,
        visible_end,
        left_overflow: scroll > 0,
        right_overflow: total.saturating_sub(scroll) > width,
    }
}

fn tab_text(tab: &Tab<'_>) -> String {
    match &tab.badge {
        Some(badge) => format!(" {} {} ", tab.label, badge),
        None => format!(" {} ", tab.label),
    }
}

fn tab_width(tab: &Tab<'_>) -> u16 {
    tab_text(tab).width().try_into().unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_layout_scrolls_active_tab_into_view() {
        let tabs = [
            Tab::new("one"),
            Tab::new("two"),
            Tab::new("three"),
            Tab::new("four"),
        ];
        let state = tabs_layout(&tabs, 3, 12, 0);
        assert!(state.scroll > 0);
        assert!(state.left_overflow);
        assert!(!state.right_overflow);
    }
}
