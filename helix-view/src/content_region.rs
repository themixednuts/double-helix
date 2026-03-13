//!
//! [`ContentRegion`] is the read-only counterpart to [`crate::edit_region::EditRegion`]:
//! a composable viewport + scroll + focus primitive for component-owned content.

use crate::graphics::Rect;
use crate::traits::{Bounded, Focusable, Identified, Scrollable, Viewport};
use crate::view::ViewPosition;
use crate::viewport::BaseViewport;
use crate::{Editor, ViewId};

/// A generic scrollable content region that components can embed.
///
/// Owns a [`BaseViewport`] for identity/area/offset, stores arbitrary component
/// content, and tracks measured content height for scrolling. Scroll state is
/// top-based, but `follow_end` lets chat/log style views stay pinned to the end
/// as content grows.
#[derive(Debug, Clone)]
pub struct ContentRegion<C> {
    viewport: BaseViewport,
    content: C,
    scroll: usize,
    content_height: usize,
    focused: bool,
    follow_end: bool,
    initialized: bool,
}

impl<C: Default> Default for ContentRegion<C> {
    fn default() -> Self {
        Self {
            viewport: BaseViewport::new(ViewId::default()),
            content: C::default(),
            scroll: 0,
            content_height: 0,
            focused: false,
            follow_end: true,
            initialized: false,
        }
    }
}

impl<C> ContentRegion<C> {
    pub fn new(content: C) -> Self {
        Self {
            viewport: BaseViewport::new(ViewId::default()),
            content,
            scroll: 0,
            content_height: 0,
            focused: false,
            follow_end: true,
            initialized: false,
        }
    }

    pub fn ensure_init(&mut self, editor: &mut Editor) {
        if !self.initialized {
            self.viewport = BaseViewport::new(editor.allocate_view_id());
            self.initialized = true;
        }
    }

    pub fn view_id(&self) -> ViewId {
        self.viewport.id
    }

    pub fn content(&self) -> &C {
        &self.content
    }

    pub fn content_mut(&mut self) -> &mut C {
        &mut self.content
    }

    pub fn set_content(&mut self, content: C) {
        self.content = content;
    }

    pub fn content_height(&self) -> usize {
        self.content_height
    }

    pub fn set_content_height(&mut self, content_height: usize) {
        self.content_height = content_height;
        if self.follow_end {
            self.scroll = self.max_scroll();
        } else {
            self.scroll = self.scroll.min(self.max_scroll());
        }
    }

    pub fn scroll_to_end(&mut self) {
        self.follow_end = true;
        self.scroll = self.max_scroll();
    }

    pub fn is_following_end(&self) -> bool {
        self.follow_end
    }
}

impl<C> Identified for ContentRegion<C> {
    fn id(&self) -> ViewId {
        self.viewport.id
    }
}

impl<C> Bounded for ContentRegion<C> {
    fn area(&self) -> Rect {
        self.viewport.area
    }

    fn set_area(&mut self, area: Rect) {
        self.viewport.set_area(area);
        if self.follow_end {
            self.scroll = self.max_scroll();
        } else {
            self.scroll = self.scroll.min(self.max_scroll());
        }
    }
}

impl<C> Viewport for ContentRegion<C> {
    fn offset(&self) -> &ViewPosition {
        self.viewport.offset()
    }

    fn set_offset(&mut self, pos: ViewPosition) {
        self.viewport.set_offset(pos);
    }
}

impl<C> Focusable for ContentRegion<C> {
    fn is_focused(&self) -> bool {
        self.focused
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

impl<C> Scrollable for ContentRegion<C> {
    fn scroll(&self) -> usize {
        self.scroll
    }

    fn scroll_to(&mut self, offset: usize) {
        self.follow_end = offset >= self.max_scroll();
        self.scroll = offset.min(self.max_scroll());
    }

    fn content_height(&self) -> usize {
        self.content_height
    }
}

#[cfg(test)]
mod tests {
    use super::ContentRegion;
    use crate::graphics::Rect;
    use crate::traits::{Bounded, Scrollable};

    #[test]
    fn follow_end_tracks_growing_content() {
        let mut region = ContentRegion::<()>::default();
        region.set_area(Rect::new(0, 0, 10, 4));
        region.set_content_height(10);
        assert_eq!(region.scroll(), 6);

        region.set_content_height(12);
        assert_eq!(region.scroll(), 8);
    }

    #[test]
    fn manual_scroll_stops_following_end() {
        let mut region = ContentRegion::<()>::default();
        region.set_area(Rect::new(0, 0, 10, 4));
        region.set_content_height(10);
        region.scroll_to(2);
        assert!(!region.is_following_end());

        region.set_content_height(12);
        assert_eq!(region.scroll(), 2);
    }
}
