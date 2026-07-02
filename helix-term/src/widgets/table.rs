//! Lightweight table row/cell data shared by menu-like renderers.

use helix_view::graphics::Style;
use tui::text::Text;

/// Table-like cell data used by menu and picker renderers.
///
/// This is intentionally a lightweight application model, not a terminal
/// buffer or widget backend. Rendering happens directly into Ratatui buffers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TableCell<'a> {
    pub content: Text<'a>,
}

impl TableCell<'_> {
    pub fn style(mut self, style: Style) -> Self {
        self.set_style(style);
        self
    }

    pub fn set_style(&mut self, style: Style) {
        self.content.patch_style(style);
    }
}

impl<'a, T> From<T> for TableCell<'a>
where
    T: Into<Text<'a>>,
{
    fn from(content: T) -> Self {
        Self {
            content: content.into(),
        }
    }
}

/// Row data used by menu and picker renderers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TableRow<'a> {
    pub cells: Vec<TableCell<'a>>,
}

impl<'a> TableRow<'a> {
    pub fn new<T>(cells: T) -> Self
    where
        T: IntoIterator,
        T::Item: Into<TableCell<'a>>,
    {
        Self {
            cells: cells.into_iter().map(Into::into).collect(),
        }
    }

    pub fn style(mut self, style: Style) -> Self {
        for cell in &mut self.cells {
            cell.set_style(style);
        }
        self
    }
}

impl<'a, T> From<T> for TableRow<'a>
where
    T: Into<TableCell<'a>>,
{
    fn from(cell: T) -> Self {
        TableRow::new([cell.into()])
    }
}
