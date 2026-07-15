//! Message list widget for rendering multiple messages.

use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::graphics::{Rect, Style};
use std::sync::Arc;
use tui::text::Spans;

use super::{message, MessageAlign, MessageStyle};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageDetailsVisibility {
    #[default]
    Always,
    Selected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageAccessoryVisibility {
    #[default]
    Always,
    Selected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageAccessoryAlign {
    #[default]
    Left,
    Right,
}

#[derive(Debug, Clone)]
pub struct MessageAccessory {
    lines: Arc<[Spans<'static>]>,
    align: MessageAccessoryAlign,
    visibility: MessageAccessoryVisibility,
}

#[derive(Debug, Clone, Copy)]
pub enum MessageDecoration {
    Bar { symbol: &'static str, style: Style },
}

#[derive(Debug, Clone)]
pub struct Message {
    kind: MessageKind,
    details: Arc<[Spans<'static>]>,
    details_visibility: MessageDetailsVisibility,
    accessories: Vec<MessageAccessory>,
    selected_decoration: Option<MessageDecoration>,
}

#[derive(Debug, Clone)]
pub enum MessageKind {
    Bubble {
        label: Option<(String, Style)>,
        lines: Arc<[Spans<'static>]>,
        bubble_width: u16,
        align: MessageAlign,
        style: MessageStyle,
    },
    Plain(Arc<[Spans<'static>]>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageLayout {
    pub index: usize,
    pub top: usize,
    pub height: u16,
}

impl MessageLayout {
    pub fn bottom(&self) -> usize {
        self.top + self.height as usize
    }

    pub fn extent(&self) -> usize {
        self.bottom() + 1
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageListState {
    pub total_height: usize,
    pub visible_start: usize,
    pub visible_end: usize,
    pub items: Vec<MessageLayout>,
    pub selected_area: Option<Rect>,
    viewport_area: Rect,
    scroll_hit_area: Rect,
    scroll: usize,
    selected: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageCursor {
    selected: Option<usize>,
    scroll: usize,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MessagePaintOverrides<'a> {
    pub animated_prefix: Option<MessageAnimatedPrefix<'a>>,
    pub selected_accent: Option<MessageSelectedAccent>,
}

#[derive(Debug, Clone, Copy)]
pub struct MessageAnimatedPrefix<'a> {
    pub index: usize,
    pub text: &'a str,
}

#[derive(Debug, Clone, Copy)]
pub struct MessageSelectedAccent {
    pub style: Style,
    pub progress: f32,
}

impl Message {
    pub fn bubble(
        label: Option<(String, Style)>,
        lines: impl Into<Arc<[Spans<'static>]>>,
        bubble_width: u16,
        align: MessageAlign,
        style: MessageStyle,
    ) -> Self {
        Self {
            kind: MessageKind::Bubble {
                label,
                lines: lines.into(),
                bubble_width,
                align,
                style,
            },
            details: Arc::from([]),
            details_visibility: MessageDetailsVisibility::Always,
            accessories: Vec::new(),
            selected_decoration: None,
        }
    }

    pub fn plain(lines: impl Into<Arc<[Spans<'static>]>>) -> Self {
        Self {
            kind: MessageKind::Plain(lines.into()),
            details: Arc::from([]),
            details_visibility: MessageDetailsVisibility::Always,
            accessories: Vec::new(),
            selected_decoration: None,
        }
    }

    pub fn with_details(mut self, details: impl Into<Arc<[Spans<'static>]>>) -> Self {
        self.details = details.into();
        self.details_visibility = MessageDetailsVisibility::Always;
        self
    }

    pub fn with_selected_details(mut self, details: impl Into<Arc<[Spans<'static>]>>) -> Self {
        self.details = details.into();
        self.details_visibility = MessageDetailsVisibility::Selected;
        self
    }

    pub fn with_accessory(
        mut self,
        lines: impl Into<Arc<[Spans<'static>]>>,
        align: MessageAccessoryAlign,
    ) -> Self {
        self.accessories.push(MessageAccessory {
            lines: lines.into(),
            align,
            visibility: MessageAccessoryVisibility::Always,
        });
        self
    }

    pub fn with_selected_accessory(
        mut self,
        lines: impl Into<Arc<[Spans<'static>]>>,
        align: MessageAccessoryAlign,
    ) -> Self {
        self.accessories.push(MessageAccessory {
            lines: lines.into(),
            align,
            visibility: MessageAccessoryVisibility::Selected,
        });
        self
    }

    pub fn with_selected_decoration(mut self, decoration: MessageDecoration) -> Self {
        self.selected_decoration = Some(decoration);
        self
    }

    pub fn with_selected_bar(mut self, symbol: &'static str, style: Style) -> Self {
        self.selected_decoration = Some(MessageDecoration::Bar { symbol, style });
        self
    }

    pub fn details(&self, selected: bool) -> &[Spans<'static>] {
        if self.details.is_empty() {
            return &[];
        }

        match self.details_visibility {
            MessageDetailsVisibility::Always => &self.details,
            MessageDetailsVisibility::Selected if selected => &self.details,
            MessageDetailsVisibility::Selected => &[],
        }
    }

    pub fn accessories(&self, selected: bool) -> impl Iterator<Item = &MessageAccessory> {
        self.accessories
            .iter()
            .filter(move |accessory| match accessory.visibility {
                MessageAccessoryVisibility::Always => true,
                MessageAccessoryVisibility::Selected => selected,
            })
    }

    pub fn height(&self, selected: bool) -> u16 {
        let base = match &self.kind {
            MessageKind::Bubble { label, lines, .. } => {
                let label_height = u16::from(label.is_some());
                label_height + lines.len() as u16 + 2
            }
            MessageKind::Plain(lines) => lines.len() as u16,
        };

        let accessory_rows = self
            .accessories(selected)
            .map(|accessory| accessory.lines.len() as u16)
            .sum::<u16>();

        base + self.details(selected).len() as u16 + accessory_rows
    }

    pub fn selected_decoration(&self) -> Option<MessageDecoration> {
        self.selected_decoration
    }
}

impl MessageListState {
    /// Computes message geometry and viewport metadata without requiring a surface.
    pub fn layout(area: Rect, items: &[Message], scroll: usize, selected: Option<usize>) -> Self {
        let mut layout = Vec::with_capacity(items.len());
        let mut top = 0usize;
        for (index, item) in items.iter().enumerate() {
            let height = item.height(selected == Some(index));
            layout.push(MessageLayout { index, top, height });
            top += height as usize + 1;
        }

        let mut state = Self {
            total_height: top,
            visible_start: items.len(),
            visible_end: items.len(),
            items: layout,
            selected_area: None,
            viewport_area: Rect::default(),
            scroll_hit_area: Rect::default(),
            scroll,
            selected,
        };

        if area.height == 0 || area.width == 0 {
            return state;
        }
        state.viewport_area = area;
        state.scroll_hit_area = scroll_hit_area(area, state.total_height);

        let mut first_visible = None;
        let mut last_visible = None;

        for layout in &state.items {
            let block_top = area.y as isize + layout.top as isize - scroll as isize;
            let block_bottom = block_top + layout.height as isize;

            if block_top < area.bottom() as isize && block_bottom > area.y as isize {
                first_visible.get_or_insert(layout.index);
                last_visible = Some(layout.index);

                if selected == Some(layout.index) {
                    let visible_top = block_top.max(area.y as isize) as u16;
                    let visible_bottom = block_bottom.min(area.bottom() as isize) as u16;
                    state.selected_area = Some(Rect::new(
                        area.x,
                        visible_top,
                        area.width,
                        visible_bottom.saturating_sub(visible_top),
                    ));
                }
            }

            if block_top >= area.bottom() as isize {
                break;
            }
        }

        if let Some(start) = first_visible {
            state.visible_start = start;
            state.visible_end = last_visible.map_or(start, |end| end + 1);
        }

        state
    }

    /// Paints messages using this precomputed layout without mutating it.
    pub fn paint(&self, surface: &mut crate::render::CellSurface, items: &[Message]) {
        self.paint_with_overrides(surface, items, MessagePaintOverrides::default());
    }

    pub fn paint_dynamic(
        &self,
        surface: &mut crate::render::CellSurface,
        items: &[Message],
        animated_prefix: Option<(usize, &str)>,
        selected_accent: Option<(Style, f32)>,
    ) {
        self.paint_with_overrides(
            surface,
            items,
            MessagePaintOverrides {
                animated_prefix: animated_prefix
                    .map(|(index, text)| MessageAnimatedPrefix { index, text }),
                selected_accent: selected_accent
                    .map(|(style, progress)| MessageSelectedAccent { style, progress }),
            },
        );
    }

    /// Paints precomputed message geometry with lightweight frame-only overrides.
    fn paint_with_overrides(
        &self,
        surface: &mut crate::render::CellSurface,
        items: &[Message],
        overrides: MessagePaintOverrides<'_>,
    ) {
        let area = self.viewport_area;
        if area.height == 0 || area.width == 0 {
            return;
        }

        for layout in &self.items {
            let Some(item) = items.get(layout.index) else {
                continue;
            };
            let block_top = area.y as isize + layout.top as isize - self.scroll as isize;
            let block_bottom = block_top + layout.height as isize;

            if block_bottom <= area.y as isize {
                continue;
            }
            if block_top >= area.bottom() as isize {
                break;
            }

            let selected = self.selected == Some(layout.index);
            let selected_accent = selected.then_some(overrides.selected_accent).flatten();
            let animated_prefix = overrides
                .animated_prefix
                .filter(|prefix| prefix.index == layout.index)
                .map(|prefix| prefix.text);
            let selected_area = if selected { self.selected_area } else { None };
            let content_area = if selected_area.is_some() && item.selected_decoration().is_some() {
                area.clip_left(1)
            } else {
                area
            };

            match &item.kind {
                MessageKind::Bubble {
                    label,
                    lines,
                    bubble_width,
                    align,
                    style,
                } => {
                    let mut cur_y = block_top;

                    if let Some((text, label_style)) = label {
                        if cur_y >= content_area.y as isize
                            && cur_y < content_area.bottom() as isize
                        {
                            let x =
                                match align {
                                    MessageAlign::Right => {
                                        content_area.x
                                            + content_area.width.saturating_sub(
                                                UnicodeWidthStr::width(text.as_str()) as u16,
                                            )
                                    }
                                    MessageAlign::Left => content_area.x,
                                };
                            surface.set_stringn(
                                x,
                                cur_y as u16,
                                text,
                                content_area.width as usize,
                                tui::ratatui::to_ratatui_style(*label_style),
                            );
                        }
                        cur_y += 1;
                    }

                    if cur_y < content_area.bottom() as isize
                        && cur_y + lines.len() as isize + 2 > content_area.y as isize
                    {
                        let bubble_y = cur_y.max(content_area.y as isize) as u16;
                        let bubble_bottom =
                            ((cur_y + lines.len() as isize + 2) as u16).min(content_area.bottom());
                        let visible_height = bubble_bottom.saturating_sub(bubble_y);

                        if visible_height > 0 {
                            let skip_top = (content_area.y as isize - cur_y).max(0) as usize;
                            let mut effective_style = *style;
                            if let Some(accent) = selected_accent {
                                effective_style.accent = Some(accent.style);
                                effective_style.accent_progress = accent.progress;
                            }
                            message(
                                surface,
                                Rect::new(
                                    content_area.x,
                                    bubble_y,
                                    content_area.width,
                                    visible_height,
                                ),
                                lines,
                                *bubble_width,
                                *align,
                                effective_style,
                                skip_top,
                            );
                        }
                    }

                    let details = item.details(selected);
                    if !details.is_empty() {
                        render_text_lines(
                            surface,
                            content_area,
                            cur_y + lines.len() as isize + 2,
                            details,
                        );
                    }

                    let mut accessory_y = cur_y + lines.len() as isize + 2 + details.len() as isize;
                    for accessory in item.accessories(selected) {
                        render_accessory_lines(
                            surface,
                            content_area,
                            accessory_y,
                            &accessory.lines,
                            accessory.align,
                        );
                        accessory_y += accessory.lines.len() as isize;
                    }
                }
                MessageKind::Plain(lines) => {
                    render_text_lines_with_overrides(
                        surface,
                        content_area,
                        block_top,
                        lines,
                        animated_prefix,
                        selected_accent.map(|accent| accent.style),
                    );
                    let details = item.details(selected);
                    if !details.is_empty() {
                        render_text_lines(
                            surface,
                            content_area,
                            block_top + lines.len() as isize,
                            details,
                        );
                    }

                    let mut accessory_y = block_top + lines.len() as isize + details.len() as isize;
                    for accessory in item.accessories(selected) {
                        render_accessory_lines(
                            surface,
                            content_area,
                            accessory_y,
                            &accessory.lines,
                            accessory.align,
                        );
                        accessory_y += accessory.lines.len() as isize;
                    }
                }
            }

            if let (Some(selected_area), Some(decoration)) =
                (selected_area, item.selected_decoration())
            {
                render_selected_decoration(surface, selected_area, decoration);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn item(&self, index: usize) -> Option<&MessageLayout> {
        self.items.get(index)
    }

    pub fn prev_index(&self, selected: Option<usize>) -> Option<usize> {
        match selected {
            Some(index) if index > 0 => Some(index - 1),
            Some(index) if index < self.len() => Some(0),
            None if !self.is_empty() => Some(0),
            _ => None,
        }
    }

    pub fn next_index(&self, selected: Option<usize>) -> Option<usize> {
        match selected {
            Some(index) if index + 1 < self.len() => Some(index + 1),
            Some(index) if index < self.len() => Some(index),
            None if !self.is_empty() => Some(0),
            _ => None,
        }
    }

    pub fn item_at_offset(&self, offset: usize) -> Option<usize> {
        self.items
            .iter()
            .find(|item| offset < item.extent())
            .map(|item| item.index)
    }

    pub fn contains_viewport(&self, x: u16, y: u16) -> bool {
        rect_contains(self.viewport_area, x, y)
    }

    pub fn contains_scroll_target(&self, x: u16, y: u16) -> bool {
        rect_contains(self.scroll_hit_area, x, y)
    }

    pub fn scroll_to_item(&self, index: usize, scroll: usize, viewport_height: usize) -> usize {
        let Some(item) = self.item(index) else {
            return scroll;
        };
        if viewport_height == 0 {
            return scroll;
        }

        let top = item.top;
        let bottom = item.bottom();
        let viewport_bottom = scroll + viewport_height;

        if top < scroll {
            top
        } else if bottom > viewport_bottom {
            bottom.saturating_sub(viewport_height)
        } else {
            scroll
        }
    }
}

fn rect_contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.left() && x < area.right() && y >= area.top() && y < area.bottom()
}

fn scroll_hit_area(area: Rect, total_height: usize) -> Rect {
    if total_height > area.height as usize {
        Rect::new(area.x, area.y, area.width.saturating_add(1), area.height)
    } else {
        area
    }
}

impl MessageCursor {
    pub fn new(selected: Option<usize>, scroll: usize) -> Self {
        Self { selected, scroll }
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    pub fn select(&mut self, index: Option<usize>) {
        self.selected = index;
    }

    pub fn clamp_selection(&mut self, state: &MessageListState) {
        self.selected = if state.is_empty() {
            None
        } else {
            match self.selected {
                Some(index) if index < state.len() => Some(index),
                Some(_) => Some(state.len() - 1),
                None => None,
            }
        };
    }

    pub fn move_prev(&mut self, state: &MessageListState, viewport_height: usize) -> Option<usize> {
        self.selected = state.prev_index(self.selected);
        self.sync(state, viewport_height);
        self.selected
    }

    pub fn move_next(&mut self, state: &MessageListState, viewport_height: usize) -> Option<usize> {
        self.selected = state.next_index(self.selected);
        self.sync(state, viewport_height);
        self.selected
    }

    pub fn move_prev_page(
        &mut self,
        state: &MessageListState,
        viewport_height: usize,
    ) -> Option<usize> {
        let page = viewport_height.max(1);
        self.scroll = self.scroll.saturating_sub(page);
        self.selected = state
            .item_at_offset(self.scroll)
            .or_else(|| state.prev_index(self.selected));
        self.sync(state, viewport_height);
        self.selected
    }

    pub fn move_next_page(
        &mut self,
        state: &MessageListState,
        viewport_height: usize,
    ) -> Option<usize> {
        let page = viewport_height.max(1);
        self.scroll = self.scroll.saturating_add(page);
        self.selected = state
            .item_at_offset(self.scroll)
            .or_else(|| state.next_index(self.selected));
        self.sync(state, viewport_height);
        self.selected
    }

    pub fn sync(&mut self, state: &MessageListState, viewport_height: usize) {
        self.clamp_selection(state);

        if let Some(index) = self.selected {
            self.scroll = state.scroll_to_item(index, self.scroll, viewport_height);
        } else {
            self.scroll = 0;
        }
    }
}

fn render_text_lines(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    y_offset: isize,
    lines: &[Spans<'_>],
) {
    render_text_lines_with_overrides(surface, area, y_offset, lines, None, None);
}

fn render_text_lines_with_overrides(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    y_offset: isize,
    lines: &[Spans<'_>],
    first_prefix: Option<&str>,
    style_override: Option<Style>,
) {
    for (row, line) in lines.iter().enumerate() {
        let line_y = y_offset + row as isize;
        if line_y < area.y as isize {
            continue;
        }
        if line_y >= area.bottom() as isize {
            break;
        }

        if first_prefix.is_none() && style_override.is_none() {
            surface.set_line(
                area.x,
                line_y as u16,
                &tui::ratatui::to_ratatui_line(line),
                area.width,
            );
            continue;
        }

        let rendered = tui::ratatui::text::Line::from(
            line.0
                .iter()
                .enumerate()
                .map(|(span_index, span)| {
                    let content = if row == 0 && span_index == 0 {
                        first_prefix
                            .map(std::borrow::Cow::Borrowed)
                            .unwrap_or_else(|| span.content.clone())
                    } else {
                        span.content.clone()
                    };
                    let style = style_override.unwrap_or(span.style);
                    tui::ratatui::text::Span::styled(content, tui::ratatui::to_ratatui_style(style))
                })
                .collect::<Vec<_>>(),
        );
        surface.set_line(area.x, line_y as u16, &rendered, area.width);
    }
}

fn render_accessory_lines(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    y_offset: isize,
    lines: &[Spans<'_>],
    align: MessageAccessoryAlign,
) {
    for (row, line) in lines.iter().enumerate() {
        let line_y = y_offset + row as isize;
        if line_y < area.y as isize {
            continue;
        }
        if line_y >= area.bottom() as isize {
            break;
        }

        let line_width = line.width().min(area.width as usize) as u16;
        let x = match align {
            MessageAccessoryAlign::Left => area.x,
            MessageAccessoryAlign::Right => area.right().saturating_sub(line_width),
        };

        surface.set_line(
            x,
            line_y as u16,
            &tui::ratatui::to_ratatui_line(line),
            line_width,
        );
    }
}

fn render_selected_decoration(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    decoration: MessageDecoration,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    match decoration {
        MessageDecoration::Bar { symbol, style } => {
            for y in area.top()..area.bottom() {
                surface.set_stringn(area.x, y, symbol, 1, tui::ratatui::to_ratatui_style(style));
            }
        }
    }
}

pub fn message_list(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    items: &[Message],
    scroll: usize,
    selected: Option<usize>,
) -> MessageListState {
    let state = MessageListState::layout(area, items, scroll, selected);
    state.paint(surface, items);
    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_view::graphics::Style;
    use tui::ratatui::{buffer::Buffer as Surface, layout::Rect as SurfaceRect};

    #[test]
    fn message_keeps_shared_line_storage() {
        let lines: Arc<[Spans<'static>]> = Arc::from([Spans::from("shared")]);
        let message = Message::plain(Arc::clone(&lines));

        let MessageKind::Plain(stored) = &message.kind else {
            panic!("plain message expected");
        };
        assert!(Arc::ptr_eq(&lines, stored));
    }

    #[test]
    fn message_list_tracks_item_layout() {
        let mut surface = Surface::empty(SurfaceRect::new(0, 0, 40, 8));
        let items = vec![
            Message::plain(vec![Spans::from("one")]),
            Message::bubble(
                Some((" User".into(), Style::default())),
                vec![Spans::from("two")],
                10,
                MessageAlign::Right,
                MessageStyle {
                    border: Style::default(),
                    corners: Default::default(),
                    accent: None,
                    accent_progress: 0.0,
                },
            ),
        ];

        let state = message_list(&mut surface, Rect::new(0, 0, 40, 8), &items, 0, Some(1));

        assert_eq!(state.total_height, 7);
        assert_eq!(state.items.len(), 2);
        assert_eq!(state.items[0].top, 0);
        assert_eq!(state.items[1].top, 2);
        assert_eq!(state.visible_start, 0);
        assert_eq!(state.visible_end, 2);
        assert_eq!(state.selected_area, Some(Rect::new(0, 2, 40, 4)));
    }

    #[test]
    fn layout_only_state_matches_message_list_wrapper() {
        let area = Rect::new(2, 1, 20, 4);
        let items = vec![
            Message::plain(vec![Spans::from("one")]),
            Message::plain(vec![Spans::from("two")])
                .with_selected_details(vec![Spans::from("details")]),
        ];
        let expected = MessageListState::layout(area, &items, 1, Some(1));
        let mut surface = Surface::empty(SurfaceRect::new(0, 0, 24, 6));

        let actual = message_list(&mut surface, area, &items, 1, Some(1));

        assert_eq!(actual, expected);
    }

    #[test]
    fn paint_uses_supplied_item_layout() {
        let area = Rect::new(0, 0, 10, 4);
        let items = vec![
            Message::plain(vec![Spans::from("one")]),
            Message::plain(vec![Spans::from("two")]),
        ];
        let mut state = MessageListState::layout(area, &items, 0, None);
        state.items[1].top = 3;
        let mut surface = Surface::empty(SurfaceRect::new(0, 0, 10, 4));

        state.paint(&mut surface, &items);

        assert_eq!(surface[(0, 2)].symbol(), " ");
        assert_eq!(surface[(0, 3)].symbol(), "t");
    }

    #[test]
    fn message_list_scroll_target_includes_trailing_gutter_when_overflowing() {
        let mut surface = Surface::empty(SurfaceRect::new(0, 0, 20, 8));
        let items = vec![Message::plain(vec![Spans::from("row")]); 6];

        let state = message_list(&mut surface, Rect::new(4, 2, 10, 3), &items, 0, None);

        assert!(state.contains_viewport(4, 2));
        assert!(state.contains_scroll_target(4, 2));
        assert!(!state.contains_viewport(14, 2));
        assert!(state.contains_scroll_target(14, 2));
        assert!(!state.contains_scroll_target(15, 2));
        assert!(!state.contains_scroll_target(14, 5));
    }

    #[test]
    fn message_list_scroll_target_excludes_trailing_gutter_without_overflow() {
        let mut surface = Surface::empty(SurfaceRect::new(0, 0, 20, 8));
        let items = vec![Message::plain(vec![Spans::from("row")])];

        let state = message_list(&mut surface, Rect::new(4, 2, 10, 3), &items, 0, None);

        assert!(state.contains_scroll_target(4, 2));
        assert!(!state.contains_scroll_target(14, 2));
    }

    #[test]
    fn message_list_navigation_primitives_work() {
        let mut surface = Surface::empty(SurfaceRect::new(0, 0, 40, 8));
        let items = vec![
            Message::plain(vec![Spans::from("one")]),
            Message::plain(vec![Spans::from("two")]),
            Message::plain(vec![Spans::from("three")]),
        ];

        let state = message_list(&mut surface, Rect::new(0, 0, 40, 8), &items, 0, Some(1));

        assert_eq!(state.prev_index(Some(2)), Some(1));
        assert_eq!(state.next_index(Some(1)), Some(2));
        assert_eq!(state.next_index(None), Some(0));
        assert_eq!(state.item_at_offset(0), Some(0));
        assert_eq!(state.item_at_offset(2), Some(1));
        assert_eq!(state.scroll_to_item(2, 0, 2), 3);
    }

    #[test]
    fn selected_details_expand_layout() {
        let mut surface = Surface::empty(SurfaceRect::new(0, 0, 40, 8));
        let items = vec![
            Message::plain(vec![Spans::from("one")])
                .with_selected_details(vec![Spans::from("hint")]),
            Message::plain(vec![Spans::from("two")]),
        ];

        let unselected = message_list(&mut surface, Rect::new(0, 0, 40, 8), &items, 0, None);
        let selected = message_list(&mut surface, Rect::new(0, 0, 40, 8), &items, 0, Some(0));

        assert_eq!(unselected.items[0].height, 1);
        assert_eq!(selected.items[0].height, 2);
        assert_eq!(selected.items[1].top, 3);
    }

    #[test]
    fn cursor_moves_and_keeps_selection_visible() {
        let mut surface = Surface::empty(SurfaceRect::new(0, 0, 40, 3));
        let items = vec![
            Message::plain(vec![Spans::from("one")]),
            Message::plain(vec![Spans::from("two")]),
            Message::plain(vec![Spans::from("three")]),
        ];
        let state = message_list(&mut surface, Rect::new(0, 0, 40, 3), &items, 0, None);
        let mut cursor = MessageCursor::default();

        assert_eq!(cursor.move_next(&state, 2), Some(0));
        assert_eq!(cursor.move_next(&state, 2), Some(1));
        assert_eq!(cursor.move_next(&state, 2), Some(2));
        assert_eq!(cursor.scroll(), 3);
        assert_eq!(cursor.move_prev(&state, 2), Some(1));
    }

    #[test]
    fn cursor_moves_by_page() {
        let mut surface = Surface::empty(SurfaceRect::new(0, 0, 40, 6));
        let items = vec![
            Message::plain(vec![Spans::from("one")]),
            Message::plain(vec![Spans::from("two")]),
            Message::plain(vec![Spans::from("three")]),
            Message::plain(vec![Spans::from("four")]),
            Message::plain(vec![Spans::from("five")]),
        ];
        let state = message_list(&mut surface, Rect::new(0, 0, 40, 6), &items, 0, Some(0));
        let mut cursor = MessageCursor::new(Some(0), 0);

        assert_eq!(cursor.move_next_page(&state, 2), Some(1));
        assert_eq!(cursor.scroll(), 2);
        assert_eq!(cursor.move_next_page(&state, 2), Some(2));
        assert_eq!(cursor.move_prev_page(&state, 2), Some(1));
    }

    #[test]
    fn selected_bar_renders_for_selected_message() {
        let mut surface = Surface::empty(SurfaceRect::new(0, 0, 20, 4));
        let items =
            vec![Message::plain(vec![Spans::from("one")]).with_selected_bar("|", Style::default())];

        let state = message_list(&mut surface, Rect::new(0, 0, 20, 4), &items, 0, Some(0));

        assert_eq!(state.selected_area, Some(Rect::new(0, 0, 20, 1)));
        assert_eq!(surface[(0, 0)].symbol(), "|");
        assert_eq!(surface[(1, 0)].symbol(), "o");
    }

    #[test]
    fn selected_accessory_expands_layout() {
        let mut surface = Surface::empty(SurfaceRect::new(0, 0, 20, 4));
        let items = vec![Message::plain(vec![Spans::from("one")])
            .with_selected_accessory(vec![Spans::from("hint")], MessageAccessoryAlign::Right)];

        let unselected = message_list(&mut surface, Rect::new(0, 0, 20, 4), &items, 0, None);
        let selected = message_list(&mut surface, Rect::new(0, 0, 20, 4), &items, 0, Some(0));

        assert_eq!(unselected.items[0].height, 1);
        assert_eq!(selected.items[0].height, 2);
    }

    #[test]
    fn right_aligned_label_uses_display_width() {
        let mut surface = Surface::empty(SurfaceRect::new(0, 0, 10, 4));
        let items = vec![Message::bubble(
            Some(("é".into(), Style::default())),
            vec![Spans::from("one")],
            6,
            MessageAlign::Right,
            MessageStyle {
                border: Style::default(),
                corners: Default::default(),
                accent: None,
                accent_progress: 0.0,
            },
        )];

        message_list(&mut surface, Rect::new(0, 0, 10, 4), &items, 0, None);

        assert_eq!(surface[(9, 0)].symbol(), "é");
    }

    #[test]
    fn right_aligned_accessory_uses_display_width() {
        let mut surface = Surface::empty(SurfaceRect::new(0, 0, 10, 3));
        let items = vec![Message::plain(vec![Spans::from("body")])
            .with_accessory(vec![Spans::from("é界")], MessageAccessoryAlign::Right)];

        message_list(&mut surface, Rect::new(0, 0, 10, 3), &items, 0, None);

        assert_eq!(surface[(7, 1)].symbol(), "é");
        assert_eq!(surface[(8, 1)].symbol(), "界");
    }
}
