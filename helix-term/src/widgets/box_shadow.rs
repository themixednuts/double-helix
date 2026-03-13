//! Box shadow rendering for terminal UI.
//!
//! Analogous to CSS `box-shadow`: renders a shadow behind a rectangular area
//! by darkening the existing surface cells. Supports offset, spread, and
//! simulated blur (graduated darkening layers).
//!
//! # Terminal limitations
//!
//! True Gaussian blur isn't possible in a cell grid. Instead, `blur` adds
//! extra layers around the shadow, each progressively lighter. With
//! `blur = 0` you get a hard-edged shadow; `blur = 2` gives a 2-cell
//! falloff on each side.
//!
//! # Example
//!
//! ```ignore
//! // Simple drop shadow (2 right, 1 down, slight blur)
//! let shadow = BoxShadow::new()
//!     .offset(2, 1)
//!     .blur(1)
//!     .color(Color::Rgb(0, 0, 0))
//!     .opacity(0.5);
//! shadow.render(surface, content_area);
//! ```

use helix_view::graphics::{Color, Rect};
use tui::buffer::Buffer as Surface;

/// A box shadow definition, analogous to CSS `box-shadow`.
#[derive(Debug, Clone, Copy)]
pub struct BoxShadow {
    /// Horizontal offset (positive = right, negative = left).
    pub offset_x: i16,
    /// Vertical offset (positive = down, negative = up).
    pub offset_y: i16,
    /// Blur radius — number of graduated falloff cells on each edge.
    /// 0 = hard shadow, 1+ = soft edges.
    pub blur: u16,
    /// Spread radius — expand (positive) or shrink (negative) the shadow
    /// relative to the content area.
    pub spread: i16,
    /// Shadow color (typically black or near-black).
    pub color: Color,
    /// Opacity of the innermost (darkest) shadow layer, 0.0–1.0.
    pub opacity: f32,
    /// If true, shadow is drawn inside the content area (inset shadow).
    pub inset: bool,
}

impl Default for BoxShadow {
    fn default() -> Self {
        Self {
            offset_x: 1,
            offset_y: 1,
            blur: 0,
            spread: 0,
            color: Color::Rgb(0, 0, 0),
            opacity: 0.5,
            inset: false,
        }
    }
}

impl BoxShadow {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set horizontal and vertical offset.
    pub fn offset(mut self, x: i16, y: i16) -> Self {
        self.offset_x = x;
        self.offset_y = y;
        self
    }

    /// Set blur radius (falloff cells on each edge).
    pub fn blur(mut self, blur: u16) -> Self {
        self.blur = blur;
        self
    }

    /// Set spread radius (expand/shrink shadow area).
    pub fn spread(mut self, spread: i16) -> Self {
        self.spread = spread;
        self
    }

    /// Set shadow color.
    pub fn color(mut self, color: Color) -> Self {
        self.color = color;
        self
    }

    /// Set opacity (0.0 = invisible, 1.0 = fully opaque).
    pub fn opacity(mut self, opacity: f32) -> Self {
        self.opacity = opacity.clamp(0.0, 1.0);
        self
    }

    /// Set inset mode (shadow inside the content area).
    pub fn inset(mut self, inset: bool) -> Self {
        self.inset = inset;
        self
    }

    /// Render the shadow onto the surface relative to `content_area`.
    ///
    /// For outset shadows (default), darkens cells *behind* the content area.
    /// The content area itself is left untouched — the caller renders content
    /// on top after this call.
    ///
    /// For inset shadows, darkens cells *inside* the content area edges.
    pub fn render(&self, surface: &mut Surface, content_area: Rect) {
        if self.opacity <= 0.0 {
            return;
        }

        let bounds = *surface.area();

        if self.inset {
            self.render_inset(surface, content_area, bounds);
        } else {
            self.render_outset(surface, content_area, bounds);
        }
    }

    fn render_outset(&self, surface: &mut Surface, content_area: Rect, bounds: Rect) {
        // Compute the shadow rectangle: content_area + offset + spread.
        let shadow = self.shadow_rect(content_area);

        // With blur, we expand the shadow rect by `blur` on each side and
        // draw graduated layers from outside in.
        let outer = expand_rect(shadow, self.blur as i16);

        let (sr, sg, sb) = color_rgb(self.color);

        for y in outer.top()..outer.bottom() {
            for x in outer.left()..outer.right() {
                // Skip cells that fall inside the content area (content draws on top).
                if content_area.contains(x, y) {
                    continue;
                }
                // Skip cells outside the terminal.
                if !bounds.contains(x, y) {
                    continue;
                }

                // Distance from the inner shadow edge (0 = inside shadow, >0 = in blur zone).
                let dist = edge_distance(x, y, shadow);
                if dist > self.blur {
                    continue;
                }

                let alpha = self.alpha_at_distance(dist);
                blend_cell(surface, x, y, sr, sg, sb, alpha);
            }
        }
    }

    fn render_inset(&self, surface: &mut Surface, content_area: Rect, bounds: Rect) {
        // Inset shadow: darken inside edges of the content area.
        // The "shadow" comes from the edges inward, offset by offset_x/offset_y.
        let depth = (self.blur as i16 + self.spread).max(1) as u16;
        let (sr, sg, sb) = color_rgb(self.color);

        for y in content_area.top()..content_area.bottom() {
            for x in content_area.left()..content_area.right() {
                if !bounds.contains(x, y) {
                    continue;
                }

                // Distance from each edge, shifted by offset.
                let dist_left = (x as i16 - content_area.x as i16 - self.offset_x) as f32;
                let dist_right =
                    (content_area.right() as i16 - 1 - x as i16 + self.offset_x) as f32;
                let dist_top = (y as i16 - content_area.y as i16 - self.offset_y) as f32;
                let dist_bottom =
                    (content_area.bottom() as i16 - 1 - y as i16 + self.offset_y) as f32;

                let min_dist = dist_left.min(dist_right).min(dist_top).min(dist_bottom);

                if min_dist >= depth as f32 {
                    continue;
                }
                if min_dist < 0.0 {
                    // Fully in shadow.
                    blend_cell(surface, x, y, sr, sg, sb, self.opacity);
                    continue;
                }

                // Graduated falloff.
                let t = 1.0 - (min_dist / depth as f32);
                let alpha = self.opacity * t;
                blend_cell(surface, x, y, sr, sg, sb, alpha);
            }
        }
    }

    /// Compute the core shadow rectangle (before blur expansion).
    fn shadow_rect(&self, content_area: Rect) -> Rect {
        let x = content_area.x as i16 + self.offset_x - self.spread;
        let y = content_area.y as i16 + self.offset_y - self.spread;
        let w = content_area.width as i16 + self.spread * 2;
        let h = content_area.height as i16 + self.spread * 2;

        Rect {
            x: x.max(0) as u16,
            y: y.max(0) as u16,
            width: w.max(0) as u16,
            height: h.max(0) as u16,
        }
    }

    /// Compute alpha for a cell at `dist` cells from the inner shadow edge.
    fn alpha_at_distance(&self, dist: u16) -> f32 {
        if self.blur == 0 {
            return self.opacity;
        }
        // Linear falloff from full opacity at dist=0 to 0 at dist=blur.
        let t = 1.0 - (dist as f32 / self.blur as f32);
        self.opacity * t
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Expand a rect by `amount` on each side.
fn expand_rect(r: Rect, amount: i16) -> Rect {
    let x = (r.x as i16 - amount).max(0) as u16;
    let y = (r.y as i16 - amount).max(0) as u16;
    let right = r.x as i16 + r.width as i16 + amount;
    let bottom = r.y as i16 + r.height as i16 + amount;
    Rect {
        x,
        y,
        width: (right - x as i16).max(0) as u16,
        height: (bottom - y as i16).max(0) as u16,
    }
}

/// Minimum distance from point (px, py) to the nearest edge of `rect`.
/// Returns 0 if the point is inside the rect.
fn edge_distance(px: u16, py: u16, rect: Rect) -> u16 {
    let dx = if px < rect.x {
        rect.x - px
    } else if px >= rect.x + rect.width {
        px - (rect.x + rect.width) + 1
    } else {
        0
    };

    let dy = if py < rect.y {
        rect.y - py
    } else if py >= rect.y + rect.height {
        py - (rect.y + rect.height) + 1
    } else {
        0
    };

    // Chebyshev distance (max of dx, dy) — looks better in terminal cells
    // than Euclidean because cells are rectangular.
    dx.max(dy)
}

/// Extract RGB from a Color, falling back to black for non-RGB colors.
fn color_rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Black => (0, 0, 0),
        Color::White => (255, 255, 255),
        Color::Gray => (128, 128, 128),
        Color::LightGray => (192, 192, 192),
        Color::Red => (170, 0, 0),
        Color::Green => (0, 170, 0),
        Color::Blue => (0, 0, 170),
        Color::Yellow => (170, 170, 0),
        Color::Magenta => (170, 0, 170),
        Color::Cyan => (0, 170, 170),
        _ => (0, 0, 0),
    }
}

/// Blend the shadow color into an existing cell's background.
///
/// Uses alpha compositing: `result = shadow * alpha + existing * (1 - alpha)`.
/// Preserves the cell's symbol and foreground (just darkens the bg), and also
/// dims the foreground proportionally so text looks "behind" the shadow.
fn blend_cell(surface: &mut Surface, x: u16, y: u16, sr: u8, sg: u8, sb: u8, alpha: f32) {
    if alpha <= 0.0 {
        return;
    }
    let cell = &mut surface[(x, y)];

    // Blend background.
    let (br, bg_color, bb) = cell_bg_rgb(cell.bg);
    cell.bg = Color::Rgb(
        lerp_u8(br, sr, alpha),
        lerp_u8(bg_color, sg, alpha),
        lerp_u8(bb, sb, alpha),
    );

    // Dim foreground toward shadow color so text fades naturally.
    let (fr, fg_color, fb) = cell_fg_rgb(cell.fg);
    cell.fg = Color::Rgb(
        lerp_u8(fr, sr, alpha * 0.6),
        lerp_u8(fg_color, sg, alpha * 0.6),
        lerp_u8(fb, sb, alpha * 0.6),
    );
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    let result = a as f32 * (1.0 - t) + b as f32 * t;
    result.round() as u8
}

/// Extract RGB from a cell's background color, defaulting to a dark base.
fn cell_bg_rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Reset => (30, 30, 30), // assume dark terminal background
        _ => color_rgb(color),
    }
}

/// Extract RGB from a cell's foreground color, defaulting to light text.
fn cell_fg_rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Reset => (200, 200, 200), // assume light terminal foreground
        _ => color_rgb(color),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_shadow() {
        let s = BoxShadow::new();
        assert_eq!(s.offset_x, 1);
        assert_eq!(s.offset_y, 1);
        assert_eq!(s.blur, 0);
        assert_eq!(s.spread, 0);
        assert!(!s.inset);
    }

    #[test]
    fn builder_chain() {
        let s = BoxShadow::new()
            .offset(3, 2)
            .blur(2)
            .spread(1)
            .color(Color::Rgb(10, 10, 10))
            .opacity(0.8)
            .inset(true);

        assert_eq!(s.offset_x, 3);
        assert_eq!(s.offset_y, 2);
        assert_eq!(s.blur, 2);
        assert_eq!(s.spread, 1);
        assert_eq!(s.opacity, 0.8);
        assert!(s.inset);
    }

    #[test]
    fn opacity_clamped() {
        assert_eq!(BoxShadow::new().opacity(1.5).opacity, 1.0);
        assert_eq!(BoxShadow::new().opacity(-0.3).opacity, 0.0);
    }

    #[test]
    fn edge_distance_inside() {
        let r = Rect::new(5, 5, 10, 10);
        assert_eq!(edge_distance(7, 7, r), 0);
    }

    #[test]
    fn edge_distance_outside() {
        let r = Rect::new(5, 5, 10, 10);
        // 2 cells to the right of the rect
        assert_eq!(edge_distance(16, 7, r), 2);
        // 3 cells above
        assert_eq!(edge_distance(7, 2, r), 3);
    }

    #[test]
    fn lerp_endpoints() {
        assert_eq!(lerp_u8(0, 255, 0.0), 0);
        assert_eq!(lerp_u8(0, 255, 1.0), 255);
        assert_eq!(lerp_u8(100, 200, 0.5), 150);
    }

    #[test]
    fn shadow_rect_with_spread() {
        let s = BoxShadow::new().offset(2, 1).spread(1);
        let content = Rect::new(10, 10, 20, 10);
        let shadow = s.shadow_rect(content);
        // spread expands by 1 on each side, offset shifts
        assert_eq!(shadow.x, 11); // 10 + 2 - 1
        assert_eq!(shadow.y, 10); // 10 + 1 - 1
        assert_eq!(shadow.width, 22); // 20 + 2
        assert_eq!(shadow.height, 12); // 10 + 2
    }

    #[test]
    fn render_does_not_panic_on_empty() {
        let mut surface = Surface::empty(Rect::new(0, 0, 80, 24));
        let shadow = BoxShadow::new();
        shadow.render(&mut surface, Rect::new(5, 5, 10, 5));
    }

    #[test]
    fn render_inset_does_not_panic() {
        let mut surface = Surface::empty(Rect::new(0, 0, 80, 24));
        let shadow = BoxShadow::new().inset(true).blur(2);
        shadow.render(&mut surface, Rect::new(5, 5, 10, 5));
    }

    #[test]
    fn zero_opacity_is_noop() {
        let mut surface = Surface::empty(Rect::new(0, 0, 80, 24));
        let before = surface[(6, 6)].clone();
        BoxShadow::new()
            .opacity(0.0)
            .render(&mut surface, Rect::new(5, 5, 10, 5));
        assert_eq!(surface[(6, 6)], before);
    }
}
