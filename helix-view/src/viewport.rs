//! Lightweight viewport for components.
//!
//! [`BaseViewport`] implements the [`Viewport`] trait (and its prerequisites
//! [`Identified`] + [`Bounded`]) without any of the optional feature traits
//! (gutters, jumps, diagnostics). Components embed a `BaseViewport` when they
//! need a positioned, scrollable region with its own `ViewId`.

use crate::graphics::Rect;
use crate::traits::{Bounded, Identified, Viewport};
use crate::view::ViewPosition;
use crate::ViewId;

/// Bare viewport: identity + area + scroll offset. No gutters, jumps, or
/// diagnostics. Used by components for their content regions.
#[derive(Debug, Clone)]
pub struct BaseViewport {
    pub id: ViewId,
    pub area: Rect,
    pub offset: ViewPosition,
}

impl BaseViewport {
    pub fn new(id: ViewId) -> Self {
        Self {
            id,
            area: Rect::default(),
            offset: ViewPosition::default(),
        }
    }
}

impl Identified for BaseViewport {
    fn id(&self) -> ViewId {
        self.id
    }
}

impl Bounded for BaseViewport {
    fn area(&self) -> Rect {
        self.area
    }

    fn set_area(&mut self, area: Rect) {
        self.area = area;
    }
}

impl Viewport for BaseViewport {
    fn offset(&self) -> &ViewPosition {
        &self.offset
    }

    fn set_offset(&mut self, pos: ViewPosition) {
        self.offset = pos;
    }
}
