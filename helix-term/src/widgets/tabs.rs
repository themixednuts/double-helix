use std::borrow::Cow;
use std::ops::Range;

use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::graphics::{Rect, Style};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabCell<'a> {
    pub text: Cow<'a, str>,
    pub style: Option<Style>,
}

impl<'a> TabCell<'a> {
    pub fn new(text: impl Into<Cow<'a, str>>) -> Self {
        Self {
            text: text.into(),
            style: None,
        }
    }

    pub fn styled(text: impl Into<Cow<'a, str>>, style: Style) -> Self {
        Self {
            text: text.into(),
            style: Some(style),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab<'a> {
    pub label: Cow<'a, str>,
    pub badge: Option<Cow<'a, str>>,
    pub style: Option<Style>,
    pub cells: Vec<TabCell<'a>>,
}

impl<'a> Tab<'a> {
    pub fn new(label: impl Into<Cow<'a, str>>) -> Self {
        Self {
            label: label.into(),
            badge: None,
            style: None,
            cells: Vec::new(),
        }
    }

    pub fn badge(mut self, badge: impl Into<Cow<'a, str>>) -> Self {
        self.badge = Some(badge.into());
        self
    }

    pub fn style(mut self, style: Style) -> Self {
        self.style = Some(style);
        self
    }

    pub fn cells(cells: impl IntoIterator<Item = TabCell<'a>>) -> Self {
        Self {
            label: Cow::Borrowed(""),
            badge: None,
            style: None,
            cells: cells.into_iter().collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TabsStyle {
    pub background: Style,
    pub active: Style,
    pub inactive: Style,
    pub hover: Style,
    pub badge: Style,
    pub separator: Style,
    pub overflow: Style,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TabsScrollPolicy {
    #[default]
    EnsureActiveVisible,
    CenterActive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabsOptions<'a> {
    pub active: usize,
    pub scroll: u16,
    pub separator: Cow<'a, str>,
    pub scroll_policy: TabsScrollPolicy,
}

impl<'a> TabsOptions<'a> {
    pub fn new(active: usize) -> Self {
        Self {
            active,
            scroll: 0,
            separator: Cow::Borrowed(""),
            scroll_policy: TabsScrollPolicy::EnsureActiveVisible,
        }
    }

    pub fn scroll(mut self, scroll: u16) -> Self {
        self.scroll = scroll;
        self
    }

    pub fn separator(mut self, separator: impl Into<Cow<'a, str>>) -> Self {
        self.separator = separator.into();
        self
    }

    pub fn scroll_policy(mut self, scroll_policy: TabsScrollPolicy) -> Self {
        self.scroll_policy = scroll_policy;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabRange {
    pub index: usize,
    pub logical: Range<u16>,
    pub visible: Range<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TabsState {
    pub scroll: u16,
    pub visible_start: usize,
    pub visible_end: usize,
    pub left_overflow: bool,
    pub right_overflow: bool,
    pub total_width: u16,
    pub visible_window: Range<u16>,
    pub tab_ranges: Vec<TabRange>,
}

impl TabsState {
    pub fn tab_at(&self, x: u16) -> Option<usize> {
        self.tab_ranges
            .iter()
            .find(|range| range.visible.contains(&x))
            .map(|range| range.index)
    }
}

pub fn tabs(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    tabs: &[Tab<'_>],
    active: usize,
    scroll: u16,
    style: TabsStyle,
) -> TabsState {
    let options = TabsOptions::new(active).scroll(scroll);
    tabs_with_options(surface, area, tabs, options, style)
}

pub fn tabs_with_options(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    tabs: &[Tab<'_>],
    options: TabsOptions<'_>,
    style: TabsStyle,
) -> TabsState {
    let state = tabs_layout_with_options(tabs, area.width, &options);
    if area.width == 0 || area.height == 0 {
        return state;
    }
    surface.set_style(
        tui::ratatui::to_ratatui_rect(area),
        tui::ratatui::to_ratatui_style(style.background),
    );
    let mut logical_x = 0u16;
    for (index, tab) in tabs.iter().enumerate() {
        if index > 0 {
            let separator_width = options.separator.width() as u16;
            let separator_x = logical_x.saturating_sub(state.scroll);
            if separator_x < area.width {
                surface.set_stringn(
                    area.x + separator_x,
                    area.y,
                    &options.separator,
                    area.width.saturating_sub(separator_x) as usize,
                    tui::ratatui::to_ratatui_style(style.separator),
                );
            }
            logical_x = logical_x.saturating_add(separator_width);
        }

        let width = tab_width(tab);
        let end = logical_x.saturating_add(width);
        if end > state.scroll && logical_x < state.scroll.saturating_add(area.width) {
            let x = area.x + logical_x.saturating_sub(state.scroll);
            let available = area.right().saturating_sub(x);
            let tab_style = tab.style.unwrap_or(if index == options.active {
                style.active
            } else {
                style.inactive
            });
            render_tab(
                surface,
                area,
                tab,
                logical_x,
                state.scroll,
                x,
                available,
                tab_style,
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
    let options = TabsOptions::new(active).scroll(scroll);
    tabs_layout_with_options(tabs, width, &options)
}

pub fn tabs_layout_with_options(
    tabs: &[Tab<'_>],
    width: u16,
    options: &TabsOptions<'_>,
) -> TabsState {
    let mut positions = Vec::with_capacity(tabs.len());
    let mut total = 0u16;
    let separator_width = options.separator.width() as u16;
    for (index, tab) in tabs.iter().enumerate() {
        if index > 0 {
            total = total.saturating_add(separator_width);
        }
        positions.push(total);
        total = total.saturating_add(tab_width(tab));
    }
    let active = options.active.min(tabs.len().saturating_sub(1));
    let active_start = positions.get(active).copied().unwrap_or(0);
    let active_end = active_start.saturating_add(tabs.get(active).map(tab_width).unwrap_or(0));
    let max_scroll = total.saturating_sub(width);
    let mut scroll = options.scroll.min(max_scroll);
    match options.scroll_policy {
        TabsScrollPolicy::EnsureActiveVisible => {
            if active_start < scroll {
                scroll = active_start;
            } else if active_end > scroll.saturating_add(width) {
                scroll = active_end.saturating_sub(width);
            }
        }
        TabsScrollPolicy::CenterActive => {
            scroll = if active_start >= width / 2 {
                active_start.saturating_sub(width / 2).min(max_scroll)
            } else {
                0
            };
        }
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
    let visible_window = scroll..scroll.saturating_add(width);
    let tab_ranges = tabs
        .iter()
        .enumerate()
        .filter_map(|(index, tab)| {
            let start = positions[index];
            let end = start.saturating_add(tab_width(tab));
            if end <= visible_window.start || start >= visible_window.end {
                return None;
            }
            let visible_start = start.saturating_sub(scroll);
            let visible_end = end
                .saturating_sub(scroll)
                .min(width)
                .max(visible_start.min(width));
            Some(TabRange {
                index,
                logical: start..end,
                visible: visible_start..visible_end,
            })
        })
        .collect();

    TabsState {
        scroll,
        visible_start,
        visible_end,
        left_overflow: scroll > 0,
        right_overflow: total.saturating_sub(scroll) > width,
        total_width: total,
        visible_window,
        tab_ranges,
    }
}

fn render_tab(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    tab: &Tab<'_>,
    logical_x: u16,
    scroll: u16,
    render_x: u16,
    available: u16,
    style: Style,
) {
    if tab.cells.is_empty() {
        let text = tab_text(tab);
        let visible_text = if logical_x < scroll {
            text.chars()
                .skip(scroll.saturating_sub(logical_x) as usize)
                .collect::<String>()
        } else {
            text
        };
        surface.set_stringn(
            render_x,
            area.y,
            &visible_text,
            available as usize,
            tui::ratatui::to_ratatui_style(style),
        );
        return;
    }

    let mut cell_x = logical_x;
    for cell in &tab.cells {
        let cell_width = cell.text.width() as u16;
        let cell_end = cell_x.saturating_add(cell_width);
        if cell_end > scroll && cell_x < scroll.saturating_add(area.width) {
            let x = area.x + cell_x.saturating_sub(scroll);
            let available = area.right().saturating_sub(x);
            let text = if cell_x < scroll {
                cell.text
                    .chars()
                    .skip(scroll.saturating_sub(cell_x) as usize)
                    .collect::<String>()
            } else {
                cell.text.to_string()
            };
            surface.set_stringn(
                x,
                area.y,
                &text,
                available as usize,
                tui::ratatui::to_ratatui_style(cell.style.unwrap_or(style)),
            );
        }
        cell_x = cell_end;
    }
}

fn tab_text(tab: &Tab<'_>) -> String {
    if !tab.cells.is_empty() {
        return tab
            .cells
            .iter()
            .map(|cell| cell.text.as_ref())
            .collect::<String>();
    }
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

    #[test]
    fn tabs_layout_reports_hit_ranges_with_centered_overflow() {
        let tabs = [
            Tab::new("one"),
            Tab::new("two"),
            Tab::new("three"),
            Tab::new("four"),
        ];
        let options = TabsOptions::new(2)
            .separator("|")
            .scroll_policy(TabsScrollPolicy::CenterActive);
        let state = tabs_layout_with_options(&tabs, 10, &options);

        assert_eq!(state.scroll, 7);
        assert!(state.left_overflow);
        assert!(state.right_overflow);
        assert_eq!(state.tab_at(0), Some(1));
        assert_eq!(state.tab_at(4), None);
        assert_eq!(state.tab_at(5), Some(2));
        assert_eq!(state.tab_at(9), Some(2));
    }
}
