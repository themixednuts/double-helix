//! Message list widget for rendering multiple messages.

use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::graphics::{Rect, Style};
use tui::buffer::Buffer as Surface;
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
pub struct MessageAccessory<'a> {
    lines: Vec<Spans<'a>>,
    align: MessageAccessoryAlign,
    visibility: MessageAccessoryVisibility,
}

#[derive(Debug, Clone, Copy)]
pub enum MessageDecoration {
    Bar { symbol: &'static str, style: Style },
}

#[derive(Debug, Clone)]
pub struct Message<'a> {
    kind: MessageKind<'a>,
    details: Vec<Spans<'a>>,
    details_visibility: MessageDetailsVisibility,
    accessories: Vec<MessageAccessory<'a>>,
    selected_decoration: Option<MessageDecoration>,
}

#[derive(Debug, Clone)]
pub enum MessageKind<'a> {
    Bubble {
        label: Option<(String, Style)>,
        lines: Vec<Spans<'a>>,
        bubble_width: u16,
        align: MessageAlign,
        style: MessageStyle,
    },
    Plain(Vec<Spans<'a>>),
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

#[derive(Debug, Clone, Default)]
pub struct MessageListState {
    pub total_height: usize,
    pub visible_start: usize,
    pub visible_end: usize,
    pub items: Vec<MessageLayout>,
    pub selected_area: Option<Rect>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageCursor {
    selected: Option<usize>,
    scroll: usize,
}

impl<'a> Message<'a> {
    pub fn bubble(
        label: Option<(String, Style)>,
        lines: Vec<Spans<'a>>,
        bubble_width: u16,
        align: MessageAlign,
        style: MessageStyle,
    ) -> Self {
        Self {
            kind: MessageKind::Bubble {
                label,
                lines,
                bubble_width,
                align,
                style,
            },
            details: Vec::new(),
            details_visibility: MessageDetailsVisibility::Always,
            accessories: Vec::new(),
            selected_decoration: None,
        }
    }

    pub fn plain(lines: Vec<Spans<'a>>) -> Self {
        Self {
            kind: MessageKind::Plain(lines),
            details: Vec::new(),
            details_visibility: MessageDetailsVisibility::Always,
            accessories: Vec::new(),
            selected_decoration: None,
        }
    }

    pub fn with_details(mut self, details: Vec<Spans<'a>>) -> Self {
        self.details = details;
        self.details_visibility = MessageDetailsVisibility::Always;
        self
    }

    pub fn with_selected_details(mut self, details: Vec<Spans<'a>>) -> Self {
        self.details = details;
        self.details_visibility = MessageDetailsVisibility::Selected;
        self
    }

    pub fn with_accessory(mut self, lines: Vec<Spans<'a>>, align: MessageAccessoryAlign) -> Self {
        self.accessories.push(MessageAccessory {
            lines,
            align,
            visibility: MessageAccessoryVisibility::Always,
        });
        self
    }

    pub fn with_selected_accessory(
        mut self,
        lines: Vec<Spans<'a>>,
        align: MessageAccessoryAlign,
    ) -> Self {
        self.accessories.push(MessageAccessory {
            lines,
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

    pub fn details(&self, selected: bool) -> &[Spans<'a>] {
        if self.details.is_empty() {
            return &[];
        }

        match self.details_visibility {
            MessageDetailsVisibility::Always => &self.details,
            MessageDetailsVisibility::Selected if selected => &self.details,
            MessageDetailsVisibility::Selected => &[],
        }
    }

    pub fn accessories(&self, selected: bool) -> impl Iterator<Item = &MessageAccessory<'a>> {
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
    pub fn len(&self) -> usize {
        self.items.len()
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

fn render_text_lines(surface: &mut Surface, area: Rect, y_offset: isize, lines: &[Spans<'_>]) {
    for (row, line) in lines.iter().enumerate() {
        let line_y = y_offset + row as isize;
        if line_y < area.y as isize {
            continue;
        }
        if line_y >= area.bottom() as isize {
            break;
        }

        let mut x = area.x;
        for span in &line.0 {
            let remaining = area.right().saturating_sub(x) as usize;
            if remaining == 0 {
                break;
            }
            let width = span.content.len().min(remaining);
            surface.set_stringn(x, line_y as u16, span.content.as_ref(), width, span.style);
            x += width as u16;
        }
    }
}

fn spans_width(line: &Spans<'_>) -> usize {
    line.0.iter().map(|span| span.content.len()).sum()
}

fn render_accessory_lines(
    surface: &mut Surface,
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

        let line_width = spans_width(line).min(area.width as usize) as u16;
        let mut x = match align {
            MessageAccessoryAlign::Left => area.x,
            MessageAccessoryAlign::Right => area.right().saturating_sub(line_width),
        };

        for span in &line.0 {
            let remaining = area.right().saturating_sub(x) as usize;
            if remaining == 0 {
                break;
            }
            let width = span.content.len().min(remaining);
            surface.set_stringn(x, line_y as u16, span.content.as_ref(), width, span.style);
            x += width as u16;
        }
    }
}

fn render_selected_decoration(surface: &mut Surface, area: Rect, decoration: MessageDecoration) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    match decoration {
        MessageDecoration::Bar { symbol, style } => {
            for y in area.top()..area.bottom() {
                surface.set_stringn(area.x, y, symbol, 1, style);
            }
        }
    }
}

pub fn message_list(
    surface: &mut Surface,
    area: Rect,
    items: &[Message<'_>],
    scroll: usize,
    selected: Option<usize>,
) -> MessageListState {
    let mut layout = Vec::with_capacity(items.len());
    let mut top = 0usize;
    for (index, item) in items.iter().enumerate() {
        let height = item.height(selected == Some(index));
        layout.push(MessageLayout { index, top, height });
        top += height as usize + 1;
    }

    let mut state = MessageListState {
        total_height: top,
        visible_start: items.len(),
        visible_end: items.len(),
        items: layout,
        selected_area: None,
    };

    if area.height == 0 || area.width == 0 {
        return state;
    }

    let mut y_offset = area.y as isize - scroll as isize;
    let mut first_visible = None;
    let mut last_visible = None;

    for (item, layout) in items.iter().zip(state.items.iter()) {
        let block_total = layout.height as usize + 1;
        let block_top = y_offset;
        let block_bottom = block_top + layout.height as isize;
        let mut selected_area = None;

        if block_top < area.bottom() as isize && block_bottom > area.y as isize {
            first_visible.get_or_insert(layout.index);
            last_visible = Some(layout.index);

            if selected == Some(layout.index) {
                let visible_top = block_top.max(area.y as isize) as u16;
                let visible_bottom = block_bottom.min(area.bottom() as isize) as u16;
                let area = Rect::new(
                    area.x,
                    visible_top,
                    area.width,
                    visible_bottom.saturating_sub(visible_top),
                );
                selected_area = Some(area);
                state.selected_area = Some(area);
            }
        }

        if y_offset + block_total as isize <= area.y as isize {
            y_offset += block_total as isize;
            continue;
        }

        if y_offset >= area.bottom() as isize {
            break;
        }

        match &item.kind {
            MessageKind::Bubble {
                label,
                lines,
                bubble_width,
                align,
                style,
            } => {
                let mut cur_y = y_offset;

                if let Some((text, label_style)) = label {
                    if cur_y >= area.y as isize && cur_y < area.bottom() as isize {
                        let x = match align {
                            MessageAlign::Right => area.x
                                + area
                                    .width
                                    .saturating_sub(UnicodeWidthStr::width(text.as_str()) as u16),
                            MessageAlign::Left => area.x,
                        };
                        surface.set_stringn(
                            x,
                            cur_y as u16,
                            text,
                            area.width as usize,
                            *label_style,
                        );
                    }
                    cur_y += 1;
                }

                if cur_y < area.bottom() as isize
                    && cur_y + lines.len() as isize + 2 > area.y as isize
                {
                    let bubble_y = cur_y.max(area.y as isize) as u16;
                    let bubble_bottom =
                        ((cur_y + lines.len() as isize + 2) as u16).min(area.bottom());
                    let visible_height = bubble_bottom.saturating_sub(bubble_y);

                    if visible_height > 0 {
                        let skip_top = (area.y as isize - cur_y).max(0) as usize;
                        message(
                            surface,
                            Rect::new(area.x, bubble_y, area.width, visible_height),
                            lines,
                            *bubble_width,
                            *align,
                            *style,
                            skip_top,
                        );
                    }
                }

                let details = item.details(selected == Some(layout.index));
                if !details.is_empty() {
                    render_text_lines(surface, area, cur_y + lines.len() as isize + 2, details);
                }

                let mut accessory_y = cur_y + lines.len() as isize + 2 + details.len() as isize;
                for accessory in item.accessories(selected == Some(layout.index)) {
                    render_accessory_lines(
                        surface,
                        area,
                        accessory_y,
                        &accessory.lines,
                        accessory.align,
                    );
                    accessory_y += accessory.lines.len() as isize;
                }
            }
            MessageKind::Plain(lines) => {
                render_text_lines(surface, area, y_offset, lines);
                let details = item.details(selected == Some(layout.index));
                if !details.is_empty() {
                    render_text_lines(surface, area, y_offset + lines.len() as isize, details);
                }

                let mut accessory_y = y_offset + lines.len() as isize + details.len() as isize;
                for accessory in item.accessories(selected == Some(layout.index)) {
                    render_accessory_lines(
                        surface,
                        area,
                        accessory_y,
                        &accessory.lines,
                        accessory.align,
                    );
                    accessory_y += accessory.lines.len() as isize;
                }
            }
        }

        if let (Some(selected_area), Some(decoration)) = (selected_area, item.selected_decoration())
        {
            render_selected_decoration(surface, selected_area, decoration);
        }

        y_offset += block_total as isize;
    }

    if let Some(start) = first_visible {
        state.visible_start = start;
        state.visible_end = last_visible.map_or(start, |end| end + 1);
    }

    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use helix_view::graphics::Style;

    #[test]
    fn message_list_tracks_item_layout() {
        let mut surface = Surface::empty(Rect::new(0, 0, 40, 8));
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
    fn message_list_navigation_primitives_work() {
        let mut surface = Surface::empty(Rect::new(0, 0, 40, 8));
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
        let mut surface = Surface::empty(Rect::new(0, 0, 40, 8));
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
        let mut surface = Surface::empty(Rect::new(0, 0, 40, 3));
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
        let mut surface = Surface::empty(Rect::new(0, 0, 40, 6));
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
        let mut surface = Surface::empty(Rect::new(0, 0, 20, 4));
        let items =
            vec![Message::plain(vec![Spans::from("one")]).with_selected_bar("|", Style::default())];

        let state = message_list(&mut surface, Rect::new(0, 0, 20, 4), &items, 0, Some(0));

        assert_eq!(state.selected_area, Some(Rect::new(0, 0, 20, 1)));
        assert_eq!(surface[(0, 0)].symbol.as_ref(), "|");
    }

    #[test]
    fn selected_accessory_expands_layout() {
        let mut surface = Surface::empty(Rect::new(0, 0, 20, 4));
        let items = vec![Message::plain(vec![Spans::from("one")])
            .with_selected_accessory(vec![Spans::from("hint")], MessageAccessoryAlign::Right)];

        let unselected = message_list(&mut surface, Rect::new(0, 0, 20, 4), &items, 0, None);
        let selected = message_list(&mut surface, Rect::new(0, 0, 20, 4), &items, 0, Some(0));

        assert_eq!(unselected.items[0].height, 1);
        assert_eq!(selected.items[0].height, 2);
    }

    #[test]
    fn right_aligned_label_uses_display_width() {
        let mut surface = Surface::empty(Rect::new(0, 0, 10, 4));
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

        assert_eq!(surface[(9, 0)].symbol.as_ref(), "é");
    }
}
