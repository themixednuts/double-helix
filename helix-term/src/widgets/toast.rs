use std::borrow::Cow;

use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::graphics::{Rect, Style};

use crate::ui::animation::Animation;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ToastId(pub usize);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast<'a> {
    pub id: ToastId,
    pub message: Cow<'a, str>,
    pub severity: ToastSeverity,
}

impl<'a> Toast<'a> {
    pub fn new(id: usize, message: impl Into<Cow<'a, str>>, severity: ToastSeverity) -> Self {
        Self {
            id: ToastId(id),
            message: message.into(),
            severity,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastSeverity {
    Error,
    Warning,
    Info,
    Hint,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ToastStyle {
    pub background: Style,
    pub error: Style,
    pub warning: Style,
    pub info: Style,
    pub hint: Style,
    pub overflow: Style,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToastQueueState {
    pub visible_ids: Vec<ToastId>,
    pub overflow_count: usize,
}

#[derive(Debug, Default)]
pub struct ToastQueue {
    order: Vec<ToastId>,
}

impl ToastQueue {
    pub fn sync<'a>(&mut self, toasts: impl IntoIterator<Item = &'a Toast<'a>>) {
        let ids: Vec<_> = toasts.into_iter().map(|toast| toast.id).collect();
        self.order.retain(|id| ids.contains(id));
        for id in ids {
            if !self.order.contains(&id) {
                self.order.push(id);
            }
        }
    }

    pub fn dismiss_one(&mut self, id: ToastId) {
        self.order.retain(|queued| *queued != id);
    }

    pub fn dismiss_all(&mut self) {
        self.order.clear();
    }

    pub fn layout<'a>(&self, toasts: &'a [Toast<'a>], max_visible: usize) -> ToastQueueState {
        let mut visible_ids = Vec::new();
        for id in self.order.iter().rev() {
            if toasts.iter().any(|toast| toast.id == *id) {
                visible_ids.push(*id);
                if visible_ids.len() == max_visible {
                    break;
                }
            }
        }
        ToastQueueState {
            overflow_count: self.order.len().saturating_sub(visible_ids.len()),
            visible_ids,
        }
    }
}

pub fn toast_queue(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    queue: &ToastQueue,
    toasts: &[Toast<'_>],
    max_visible: usize,
    _animation: Option<&Animation>,
    style: ToastStyle,
) -> ToastQueueState {
    let state = queue.layout(toasts, max_visible);
    if area.width == 0 || area.height == 0 {
        return state;
    }

    let mut y = area.y;
    for id in &state.visible_ids {
        if y >= area.bottom() {
            break;
        }
        let Some(toast) = toasts.iter().find(|toast| toast.id == *id) else {
            continue;
        };
        let row = Rect::new(area.x, y, area.width, 1);
        surface.set_style(
            tui::ratatui::to_ratatui_rect(row),
            tui::ratatui::to_ratatui_style(style.background),
        );
        let toast_style = match toast.severity {
            ToastSeverity::Error => style.error,
            ToastSeverity::Warning => style.warning,
            ToastSeverity::Info => style.info,
            ToastSeverity::Hint => style.hint,
        };
        surface.set_stringn(
            row.x,
            row.y,
            &toast.message,
            row.width as usize,
            tui::ratatui::to_ratatui_style(toast_style),
        );
        y = y.saturating_add(1);
    }

    if state.overflow_count > 0 && y < area.bottom() {
        let text = format!("+{} more", state.overflow_count);
        let x = area
            .right()
            .saturating_sub(text.width().try_into().unwrap_or(u16::MAX));
        surface.set_stringn(
            x,
            y,
            &text,
            area.right().saturating_sub(x) as usize,
            tui::ratatui::to_ratatui_style(style.overflow),
        );
    }

    state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toast_queue_orders_newest_first_and_counts_overflow() {
        let toasts = [
            Toast::new(1, "one", ToastSeverity::Info),
            Toast::new(2, "two", ToastSeverity::Info),
            Toast::new(3, "three", ToastSeverity::Info),
        ];
        let mut queue = ToastQueue::default();
        queue.sync(toasts.iter());
        let state = queue.layout(&toasts, 2);
        assert_eq!(state.visible_ids, vec![ToastId(3), ToastId(2)]);
        assert_eq!(state.overflow_count, 1);
    }

    #[test]
    fn toast_queue_dismisses_one_and_all() {
        let toasts = [
            Toast::new(1, "one", ToastSeverity::Info),
            Toast::new(2, "two", ToastSeverity::Info),
        ];
        let mut queue = ToastQueue::default();
        queue.sync(toasts.iter());
        queue.dismiss_one(ToastId(2));
        assert_eq!(queue.layout(&toasts, 2).visible_ids, vec![ToastId(1)]);
        queue.dismiss_all();
        assert!(queue.layout(&toasts, 2).visible_ids.is_empty());
    }
}
