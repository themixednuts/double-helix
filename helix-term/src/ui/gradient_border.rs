use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::{
    editor::{GradientBorderConfig, GradientDirection},
    graphics::{Color, Rect, Style},
    theme::{Modifier, Theme},
};
use std::time::Instant;

type Rgb = (u8, u8, u8);

/// A utility for rendering gradient borders around UI components
#[derive(Clone)]
pub struct GradientBorder {
    config: GradientBorderConfig,
    started_at: Instant,
    // Cached parsed colors to avoid repeated hex parsing
    start_rgb: Rgb,
    end_rgb: Rgb,
    middle_rgb: Option<Rgb>,
}

impl GradientBorder {
    pub fn new(config: GradientBorderConfig) -> Self {
        let (start_rgb, end_rgb, middle_rgb) = Self::compute_cached_colors(&config);
        Self {
            config,
            started_at: Instant::now(),
            start_rgb,
            end_rgb,
            middle_rgb,
        }
    }

    pub fn is_animated(&self) -> bool {
        self.config.enable && self.config.animation_speed > 0
    }

    pub fn matches_config(&self, config: &GradientBorderConfig) -> bool {
        self.config == *config
    }

    /// Disable gradient animation (set speed to 0)
    pub fn disable_animation(&mut self) {
        self.config.animation_speed = 0;
    }

    /// Parse hex color string to RGB
    fn parse_hex_color(hex: &str) -> Option<Rgb> {
        if hex.len() != 7 || !hex.starts_with('#') {
            return None;
        }

        let r = u8::from_str_radix(&hex[1..3], 16).ok()?;
        let g = u8::from_str_radix(&hex[3..5], 16).ok()?;
        let b = u8::from_str_radix(&hex[5..7], 16).ok()?;

        Some((r, g, b))
    }

    /// Compute cached RGB values from config (with sensible fallbacks)
    fn compute_cached_colors(config: &GradientBorderConfig) -> (Rgb, Rgb, Option<Rgb>) {
        let start_rgb = Self::parse_hex_color(&config.start_color).unwrap_or((138, 43, 226));
        let end_rgb = Self::parse_hex_color(&config.end_color).unwrap_or((0, 191, 255));
        let middle_rgb = if config.middle_color.is_empty() {
            None
        } else {
            Self::parse_hex_color(&config.middle_color)
        };
        (start_rgb, end_rgb, middle_rgb)
    }

    /// Interpolate between two colors
    fn interpolate_color(start: Rgb, end: Rgb, ratio: f32) -> Color {
        let ratio = ratio.clamp(0.0, 1.0);
        let r = (start.0 as f32 + (end.0 as f32 - start.0 as f32) * ratio) as u8;
        let g = (start.1 as f32 + (end.1 as f32 - start.1 as f32) * ratio) as u8;
        let b = (start.2 as f32 + (end.2 as f32 - start.2 as f32) * ratio) as u8;
        Color::Rgb(r, g, b)
    }

    /// Interpolate between three colors for middle color support
    fn interpolate_three_colors(start: Rgb, middle: Rgb, end: Rgb, ratio: f32) -> Color {
        let ratio = ratio.clamp(0.0, 1.0);
        if ratio < 0.5 {
            Self::interpolate_color(start, middle, ratio * 2.0)
        } else {
            Self::interpolate_color(middle, end, (ratio - 0.5) * 2.0)
        }
    }

    /// Calculate gradient color at a specific position
    fn animation_offset_at(&self, now: Instant) -> f32 {
        if self.config.animation_speed == 0 {
            return 0.0;
        }

        // Preserve the old speed scale at a nominal 60 redraws/second while
        // making phase a function of elapsed time instead of frame count.
        (now.saturating_duration_since(self.started_at).as_secs_f32()
            * self.config.animation_speed as f32
            * 0.6)
            .fract()
    }

    fn get_gradient_color(&self, x: u16, y: u16, area: Rect, animation_offset: f32) -> Color {
        let start_color = self.start_rgb;
        let end_color = self.end_rgb;

        let ratio = match self.config.direction {
            GradientDirection::Horizontal => {
                let base_ratio = (x - area.x) as f32 / area.width.max(1) as f32;
                (base_ratio + animation_offset) % 1.0
            }
            GradientDirection::Vertical => {
                let base_ratio = (y - area.y) as f32 / area.height.max(1) as f32;
                (base_ratio + animation_offset) % 1.0
            }
            GradientDirection::Diagonal => {
                let base_ratio =
                    ((x - area.x) + (y - area.y)) as f32 / (area.width + area.height).max(1) as f32;
                (base_ratio + animation_offset) % 1.0
            }
            GradientDirection::Radial => {
                let center_x = area.x + area.width / 2;
                let center_y = area.y + area.height / 2;
                let distance = ((x as f32 - center_x as f32).powi(2)
                    + (y as f32 - center_y as f32).powi(2))
                .sqrt();
                let max_distance = (area.width.max(area.height) / 2) as f32;
                let base_ratio = (distance / max_distance.max(1.0)).min(1.0);
                (base_ratio + animation_offset) % 1.0
            }
        };

        // Check if we have a middle color for 3-color gradients
        if let Some(middle_color) = self.middle_rgb {
            return Self::interpolate_three_colors(start_color, middle_color, end_color, ratio);
        }

        Self::interpolate_color(start_color, end_color, ratio)
    }

    /// Get the appropriate border characters based on thickness and rounded corners setting
    fn get_border_chars(thickness: u8, rounded: bool) -> Vec<&'static str> {
        match (thickness, rounded) {
            // Thickness 1 - thin borders
            (1, false) => vec!["─", "│", "┌", "┐", "└", "┘"], // thin square
            (1, true) => vec!["─", "│", "╭", "╮", "╰", "╯"],  // thin rounded

            // Thickness 2 - thick borders
            (2, false) => vec!["━", "┃", "┏", "┓", "┗", "┛"], // thick square
            (2, true) => vec!["━", "┃", "┏", "┓", "┗", "┛"],  // thick (no rounded equivalent)

            // Thickness 3 - double borders
            (3, false) => vec!["═", "║", "╔", "╗", "╚", "╝"], // double square
            (3, true) => vec!["═", "║", "╔", "╗", "╚", "╝"],  // double (no rounded equivalent)

            // Thickness 4 - block characters
            (4, _) => vec!["▄", "█", "█", "█", "█", "█"], // block (rounded doesn't apply)

            // Thickness 5 - full block characters
            (5, _) => vec!["▀", "█", "█", "█", "█", "█"], // full block (rounded doesn't apply)

            // Fallback to thin
            _ => vec!["─", "│", "┌", "┐", "└", "┘"],
        }
    }

    /// Render the gradient border without requiring a theme reference.
    /// (The theme parameter on [`render`] is unused; this avoids the borrow.)
    pub fn render_no_theme(
        &self,
        area: Rect,
        surface: &mut crate::render::CellSurface,
        rounded: bool,
    ) {
        self.render_inner(area, surface, rounded);
    }

    /// Render the gradient border around the given area
    pub fn render(
        &self,
        area: Rect,
        surface: &mut crate::render::CellSurface,
        _theme: &Theme,
        rounded: bool,
    ) {
        self.render_inner(area, surface, rounded);
    }

    fn render_inner(&self, area: Rect, surface: &mut crate::render::CellSurface, rounded: bool) {
        self.render_inner_at(
            area,
            surface,
            rounded,
            self.animation_offset_at(Instant::now()),
        );
    }

    fn render_inner_at(
        &self,
        area: Rect,
        surface: &mut crate::render::CellSurface,
        rounded: bool,
        animation_offset: f32,
    ) {
        if !self.config.enable || area.width < 2 || area.height < 2 {
            return;
        }

        let border_chars = Self::get_border_chars(self.config.thickness, rounded);
        let [horizontal, vertical, top_left, top_right, bottom_left, bottom_right] = [
            border_chars[0],
            border_chars[1],
            border_chars[2],
            border_chars[3],
            border_chars[4],
            border_chars[5],
        ];

        // Render top border
        for x in area.left()..area.right() {
            let color = self.get_gradient_color(x, area.top(), area, animation_offset);
            let style = Style::default().fg(color);
            let symbol = if x == area.left() {
                top_left
            } else if x == area.right() - 1 {
                top_right
            } else {
                horizontal
            };

            {
                if let Some(cell) = surface.cell_mut((x, area.top())) {
                    cell.set_symbol(symbol);
                    cell.set_style(tui::ratatui::to_ratatui_style(style));
                }
            };
        }

        // Render bottom border
        let bottom_y = area.bottom() - 1;
        for x in area.left()..area.right() {
            let color = self.get_gradient_color(x, bottom_y, area, animation_offset);
            let style = Style::default().fg(color);
            let symbol = if x == area.left() {
                bottom_left
            } else if x == area.right() - 1 {
                bottom_right
            } else {
                horizontal
            };

            {
                if let Some(cell) = surface.cell_mut((x, bottom_y)) {
                    cell.set_symbol(symbol);
                    cell.set_style(tui::ratatui::to_ratatui_style(style));
                }
            };
        }

        // Render left and right borders (skip corners)
        for y in (area.top() + 1)..(area.bottom() - 1) {
            // Left border
            let color = self.get_gradient_color(area.left(), y, area, animation_offset);
            let style = Style::default().fg(color);
            {
                if let Some(cell) = surface.cell_mut((area.left(), y)) {
                    cell.set_symbol(vertical);
                    cell.set_style(tui::ratatui::to_ratatui_style(style));
                }
            };

            // Right border
            let right_x = area.right() - 1;
            let color = self.get_gradient_color(right_x, y, area, animation_offset);
            let style = Style::default().fg(color);
            {
                if let Some(cell) = surface.cell_mut((right_x, y)) {
                    cell.set_symbol(vertical);
                    cell.set_style(tui::ratatui::to_ratatui_style(style));
                }
            };
        }
    }

    /// Render gradient border with title (for pickers with titles)
    pub fn render_with_title(
        &self,
        area: Rect,
        surface: &mut crate::render::CellSurface,
        _theme: &Theme,
        title: Option<&str>,
        rounded: bool,
    ) {
        let animation_offset = self.animation_offset_at(Instant::now());
        self.render_inner_at(area, surface, rounded, animation_offset);

        // If there's a title, render it centered in the top border
        if let Some(title) = title {
            let title_width = UnicodeWidthStr::width(title) as u16;
            if !title.is_empty() && area.width > title_width + 4 {
                // Center the title
                let title_start = area.x + (area.width.saturating_sub(title_width)) / 2;
                let title_color =
                    self.get_gradient_color(title_start, area.y, area, animation_offset);
                let title_style = Style::default()
                    .fg(title_color)
                    .add_modifier(Modifier::BOLD);

                // Clear the area for the title and render it
                for (i, ch) in title.chars().enumerate() {
                    let mut buf = [0; 4];
                    {
                        if let Some(cell) = surface.cell_mut((title_start + i as u16, area.top())) {
                            cell.set_symbol(ch.encode_utf8(&mut buf));
                            cell.set_style(tui::ratatui::to_ratatui_style(title_style));
                        }
                    };
                }
            }
        }
    }

    /// Create a gradient border with default theme-based colors
    pub fn from_theme(_theme: &Theme, config: &GradientBorderConfig) -> Self {
        let mut border_config = config.clone();

        // Use theme colors as fallbacks if hex colors are invalid
        if Self::parse_hex_color(&border_config.start_color).is_none() {
            border_config.start_color = "#8A2BE2".to_string();
        }
        if Self::parse_hex_color(&border_config.end_color).is_none() {
            border_config.end_color = "#00BFFF".to_string();
        }

        Self::new(border_config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn animation_phase_depends_on_elapsed_time_not_render_count() {
        let config = GradientBorderConfig {
            enable: true,
            animation_speed: 2,
            ..GradientBorderConfig::default()
        };
        let border = GradientBorder::new(config);
        let sample_time = border.started_at + Duration::from_millis(250);

        let first = border.animation_offset_at(sample_time);
        let repeated = border.animation_offset_at(sample_time);

        assert!((first - 0.3).abs() < 0.000_1);
        assert_eq!(first, repeated);
    }

    #[test]
    fn disabled_animation_has_no_phase_or_redraw_driver() {
        let border = GradientBorder::new(GradientBorderConfig::default());

        assert!(!border.is_animated());
        assert_eq!(
            border.animation_offset_at(border.started_at + Duration::from_secs(10)),
            0.0
        );
    }
}
