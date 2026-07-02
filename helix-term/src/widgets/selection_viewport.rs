use std::ops::Range;

/// Shared selection + scroll math for vertical lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionViewport {
    total: usize,
    selected: Option<usize>,
    visible: usize,
    scroll: usize,
}

impl SelectionViewport {
    pub const fn new(total: usize, selected: Option<usize>, visible: usize, scroll: usize) -> Self {
        Self {
            total,
            selected,
            visible,
            scroll,
        }
    }

    pub fn max_scroll(self) -> usize {
        self.total.saturating_sub(self.visible)
    }

    pub fn clamped_scroll(self) -> usize {
        self.scroll.min(self.max_scroll())
    }

    pub fn scroll_to_selected(self) -> usize {
        let Some(selected) = self.selected else {
            return self.clamped_scroll();
        };
        if self.visible == 0 || self.total == 0 {
            return 0;
        }

        let selected = selected.min(self.total.saturating_sub(1));
        let scroll = self.clamped_scroll();
        if selected < scroll {
            selected
        } else if selected >= scroll.saturating_add(self.visible) {
            selected.saturating_add(1).saturating_sub(self.visible)
        } else {
            scroll
        }
    }

    pub fn visible_range(self) -> Range<usize> {
        let start = self.clamped_scroll();
        start..start.saturating_add(self.visible).min(self.total)
    }

    pub fn selected_visible_range(self) -> Range<usize> {
        let start = self.scroll_to_selected();
        start..start.saturating_add(self.visible).min(self.total)
    }
}

#[cfg(test)]
mod tests {
    use super::SelectionViewport;

    #[test]
    fn scrolls_down_to_keep_selection_visible() {
        let viewport = SelectionViewport::new(20, Some(9), 5, 0);

        assert_eq!(viewport.scroll_to_selected(), 5);
        assert_eq!(viewport.selected_visible_range(), 5..10);
    }

    #[test]
    fn clamps_overscroll_to_last_page() {
        let viewport = SelectionViewport::new(8, None, 3, 99);

        assert_eq!(viewport.clamped_scroll(), 5);
        assert_eq!(viewport.visible_range(), 5..8);
    }
}
