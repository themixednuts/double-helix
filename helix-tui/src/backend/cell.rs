use helix_view::graphics::{Color, Modifier, UnderlineStyle};

pub(crate) trait TerminalCell {
    fn symbol(&self) -> &str;
    fn fg(&self) -> Color;
    fn bg(&self) -> Color;
    fn underline_color(&self) -> Color;
    fn underline_style(&self) -> UnderlineStyle;
    fn modifier(&self) -> Modifier;
}

impl TerminalCell for ratatui::buffer::Cell {
    #[inline]
    fn symbol(&self) -> &str {
        self.symbol()
    }

    #[inline]
    fn fg(&self) -> Color {
        crate::ratatui::to_helix_color(self.fg)
    }

    #[inline]
    fn bg(&self) -> Color {
        crate::ratatui::to_helix_color(self.bg)
    }

    #[inline]
    fn underline_color(&self) -> Color {
        crate::ratatui::to_helix_color(self.underline_color)
    }

    #[inline]
    fn underline_style(&self) -> UnderlineStyle {
        if self.modifier.contains(ratatui::style::Modifier::UNDERLINED) {
            UnderlineStyle::Line
        } else {
            UnderlineStyle::Reset
        }
    }

    #[inline]
    fn modifier(&self) -> Modifier {
        crate::ratatui::to_helix_modifier(self.modifier)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratatui_cell_maps_supported_style_to_helix_style() {
        let mut cell = ratatui::buffer::Cell::new("x");
        cell.set_fg(ratatui::style::Color::LightBlue);
        cell.set_bg(ratatui::style::Color::Rgb(1, 2, 3));
        cell.underline_color = ratatui::style::Color::Indexed(4);
        cell.modifier = ratatui::style::Modifier::BOLD
            | ratatui::style::Modifier::ITALIC
            | ratatui::style::Modifier::UNDERLINED;

        assert_eq!(cell.symbol(), "x");
        assert_eq!(TerminalCell::fg(&cell), Color::LightBlue);
        assert_eq!(TerminalCell::bg(&cell), Color::Rgb(1, 2, 3));
        assert_eq!(TerminalCell::underline_color(&cell), Color::Indexed(4));
        assert_eq!(TerminalCell::underline_style(&cell), UnderlineStyle::Line);
        assert_eq!(
            TerminalCell::modifier(&cell),
            Modifier::BOLD | Modifier::ITALIC
        );
    }
}
