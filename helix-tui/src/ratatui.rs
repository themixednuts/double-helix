//! Ratatui integration for the Helix terminal backends.
//!
//! This module is the Ratatui integration layer for Helix-owned terminal backends. It deliberately
//! keeps terminal ownership in Helix while exposing Ratatui's frame/widget model to UI code.

use std::{io, marker::PhantomData};

use helix_view::graphics as helix_graphics;

pub use ::ratatui::{
    backend, buffer, layout, style, text, widgets, CompletedFrame, Frame, Terminal,
};

use crate::backend::Backend as HelixBackend;

mod private {
    pub trait Sealed {}
}

/// Session state before the terminal backend has been claimed for TUI rendering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Unclaimed;

/// Session state after the backend has been claimed for TUI rendering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Claimed;

impl private::Sealed for Unclaimed {}
impl private::Sealed for Claimed {}

/// Type-state marker for [`TerminalSession`].
pub trait SessionState: private::Sealed {}

impl SessionState for Unclaimed {}
impl SessionState for Claimed {}

/// A Ratatui terminal session backed by a Helix terminal backend.
///
/// The type-state parameter makes lifecycle transitions explicit: rendering methods are only
/// available after [`TerminalSession::claim`] succeeds, and [`TerminalSession::restore`] consumes
/// the claimed session.
#[derive(Debug)]
pub struct TerminalSession<B, State = Unclaimed>
where
    B: backend::Backend,
{
    terminal: Terminal<B>,
    _state: PhantomData<State>,
}

impl<B> TerminalSession<B, Unclaimed>
where
    B: HelixBackend + backend::Backend<Error = io::Error>,
{
    /// Create a Ratatui terminal session around a Helix backend.
    pub fn new(backend: B) -> io::Result<Self> {
        Ok(Self {
            terminal: Terminal::new(backend)?,
            _state: PhantomData,
        })
    }

    /// Claim the terminal for TUI rendering.
    pub fn claim(mut self) -> io::Result<TerminalSession<B, Claimed>> {
        HelixBackend::claim(self.terminal.backend_mut())?;
        Ok(TerminalSession {
            terminal: self.terminal,
            _state: PhantomData,
        })
    }
}

impl<B> TerminalSession<B, Claimed>
where
    B: HelixBackend + backend::Backend<Error = io::Error>,
{
    /// Draw one Ratatui frame.
    pub fn draw<F>(&mut self, render: F) -> io::Result<CompletedFrame<'_>>
    where
        F: FnOnce(&mut Frame),
    {
        self.terminal.draw(render)
    }

    /// Draw one Ratatui frame with a fallible render callback.
    pub fn try_draw<F, E>(&mut self, render: F) -> io::Result<CompletedFrame<'_>>
    where
        F: FnOnce(&mut Frame) -> Result<(), E>,
        E: Into<io::Error>,
    {
        self.terminal.try_draw(render)
    }

    /// Reconfigure Helix-owned terminal modes without changing Ratatui buffers.
    pub fn reconfigure(&mut self, config: crate::terminal::Config) -> io::Result<()> {
        HelixBackend::reconfigure(self.terminal.backend_mut(), config)
    }

    /// Restore the terminal to normal mode.
    pub fn restore(mut self) -> io::Result<TerminalSession<B, Unclaimed>> {
        HelixBackend::restore(self.terminal.backend_mut())?;
        Ok(TerminalSession {
            terminal: self.terminal,
            _state: PhantomData,
        })
    }
}

impl<B, State> TerminalSession<B, State>
where
    B: backend::Backend,
    State: SessionState,
{
    /// Borrow the underlying Ratatui terminal.
    pub fn terminal(&self) -> &Terminal<B> {
        &self.terminal
    }

    /// Mutably borrow the underlying Ratatui terminal.
    pub fn terminal_mut(&mut self) -> &mut Terminal<B> {
        &mut self.terminal
    }

    /// Borrow the underlying backend.
    pub fn backend(&self) -> &B {
        self.terminal.backend()
    }

    /// Mutably borrow the underlying backend.
    pub fn backend_mut(&mut self) -> &mut B {
        self.terminal.backend_mut()
    }

    /// Unwrap the underlying Ratatui terminal.
    pub fn into_inner(self) -> Terminal<B> {
        self.terminal
    }
}

/// Convert a Helix rectangle into a Ratatui rectangle.
pub const fn to_ratatui_rect(area: helix_graphics::Rect) -> layout::Rect {
    layout::Rect::new(area.x, area.y, area.width, area.height)
}

/// Convert a Ratatui rectangle into a Helix rectangle.
pub const fn to_helix_rect(area: layout::Rect) -> helix_graphics::Rect {
    helix_graphics::Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height,
    }
}

/// Convert a Helix color into a Ratatui color.
pub const fn to_ratatui_color(color: helix_graphics::Color) -> style::Color {
    match color {
        helix_graphics::Color::Reset => style::Color::Reset,
        helix_graphics::Color::Black => style::Color::Black,
        helix_graphics::Color::Red => style::Color::Red,
        helix_graphics::Color::Green => style::Color::Green,
        helix_graphics::Color::Yellow => style::Color::Yellow,
        helix_graphics::Color::Blue => style::Color::Blue,
        helix_graphics::Color::Magenta => style::Color::Magenta,
        helix_graphics::Color::Cyan => style::Color::Cyan,
        helix_graphics::Color::Gray => style::Color::DarkGray,
        helix_graphics::Color::LightRed => style::Color::LightRed,
        helix_graphics::Color::LightGreen => style::Color::LightGreen,
        helix_graphics::Color::LightYellow => style::Color::LightYellow,
        helix_graphics::Color::LightBlue => style::Color::LightBlue,
        helix_graphics::Color::LightMagenta => style::Color::LightMagenta,
        helix_graphics::Color::LightCyan => style::Color::LightCyan,
        helix_graphics::Color::LightGray => style::Color::Gray,
        helix_graphics::Color::White => style::Color::White,
        helix_graphics::Color::Rgb(r, g, b) => style::Color::Rgb(r, g, b),
        helix_graphics::Color::Indexed(index) => style::Color::Indexed(index),
    }
}

/// Convert a Ratatui color into a Helix color.
pub const fn to_helix_color(color: style::Color) -> helix_graphics::Color {
    match color {
        style::Color::Reset => helix_graphics::Color::Reset,
        style::Color::Black => helix_graphics::Color::Black,
        style::Color::Red => helix_graphics::Color::Red,
        style::Color::Green => helix_graphics::Color::Green,
        style::Color::Yellow => helix_graphics::Color::Yellow,
        style::Color::Blue => helix_graphics::Color::Blue,
        style::Color::Magenta => helix_graphics::Color::Magenta,
        style::Color::Cyan => helix_graphics::Color::Cyan,
        style::Color::Gray => helix_graphics::Color::LightGray,
        style::Color::DarkGray => helix_graphics::Color::Gray,
        style::Color::LightRed => helix_graphics::Color::LightRed,
        style::Color::LightGreen => helix_graphics::Color::LightGreen,
        style::Color::LightYellow => helix_graphics::Color::LightYellow,
        style::Color::LightBlue => helix_graphics::Color::LightBlue,
        style::Color::LightMagenta => helix_graphics::Color::LightMagenta,
        style::Color::LightCyan => helix_graphics::Color::LightCyan,
        style::Color::White => helix_graphics::Color::White,
        style::Color::Rgb(r, g, b) => helix_graphics::Color::Rgb(r, g, b),
        style::Color::Indexed(index) => helix_graphics::Color::Indexed(index),
    }
}

/// Convert Ratatui text modifiers into Helix text modifiers.
pub const fn to_helix_modifier(modifier: style::Modifier) -> helix_graphics::Modifier {
    let mut out = helix_graphics::Modifier::empty();
    if modifier.contains(style::Modifier::BOLD) {
        out = out.union(helix_graphics::Modifier::BOLD);
    }
    if modifier.contains(style::Modifier::DIM) {
        out = out.union(helix_graphics::Modifier::DIM);
    }
    if modifier.contains(style::Modifier::ITALIC) {
        out = out.union(helix_graphics::Modifier::ITALIC);
    }
    if modifier.contains(style::Modifier::SLOW_BLINK) {
        out = out.union(helix_graphics::Modifier::SLOW_BLINK);
    }
    if modifier.contains(style::Modifier::RAPID_BLINK) {
        out = out.union(helix_graphics::Modifier::RAPID_BLINK);
    }
    if modifier.contains(style::Modifier::REVERSED) {
        out = out.union(helix_graphics::Modifier::REVERSED);
    }
    if modifier.contains(style::Modifier::HIDDEN) {
        out = out.union(helix_graphics::Modifier::HIDDEN);
    }
    if modifier.contains(style::Modifier::CROSSED_OUT) {
        out = out.union(helix_graphics::Modifier::CROSSED_OUT);
    }
    out
}

/// Convert a Ratatui style patch into a Helix style patch.
pub fn to_helix_style(style: style::Style) -> helix_graphics::Style {
    let mut out = helix_graphics::Style::default();
    if let Some(fg) = style.fg {
        out = out.fg(to_helix_color(fg));
    }
    if let Some(bg) = style.bg {
        out = out.bg(to_helix_color(bg));
    }
    if let Some(underline_color) = style.underline_color {
        out = out.underline_color(to_helix_color(underline_color));
    }
    if style.add_modifier.contains(style::Modifier::UNDERLINED) {
        out = out.underline_style(helix_graphics::UnderlineStyle::Line);
    }
    if style.sub_modifier.contains(style::Modifier::UNDERLINED) {
        out = out.underline_style(helix_graphics::UnderlineStyle::Reset);
    }
    out.add_modifier = to_helix_modifier(style.add_modifier);
    out.sub_modifier = to_helix_modifier(style.sub_modifier);
    out
}

/// Convert Helix text modifiers into Ratatui modifiers.
pub const fn to_ratatui_modifier(modifier: helix_graphics::Modifier) -> style::Modifier {
    let mut out = style::Modifier::empty();
    if modifier.contains(helix_graphics::Modifier::BOLD) {
        out = out.union(style::Modifier::BOLD);
    }
    if modifier.contains(helix_graphics::Modifier::DIM) {
        out = out.union(style::Modifier::DIM);
    }
    if modifier.contains(helix_graphics::Modifier::ITALIC) {
        out = out.union(style::Modifier::ITALIC);
    }
    if modifier.contains(helix_graphics::Modifier::SLOW_BLINK) {
        out = out.union(style::Modifier::SLOW_BLINK);
    }
    if modifier.contains(helix_graphics::Modifier::RAPID_BLINK) {
        out = out.union(style::Modifier::RAPID_BLINK);
    }
    if modifier.contains(helix_graphics::Modifier::REVERSED) {
        out = out.union(style::Modifier::REVERSED);
    }
    if modifier.contains(helix_graphics::Modifier::HIDDEN) {
        out = out.union(style::Modifier::HIDDEN);
    }
    if modifier.contains(helix_graphics::Modifier::CROSSED_OUT) {
        out = out.union(style::Modifier::CROSSED_OUT);
    }
    out
}

/// Convert a Helix style patch into a Ratatui style patch.
///
/// Ratatui currently models underline shape as a boolean modifier, so Helix underline variants
/// other than reset are represented as `UNDERLINED`. The Helix terminal backend still owns the
/// richer terminal escape support for native Helix cells.
pub fn to_ratatui_style(style: helix_graphics::Style) -> style::Style {
    let mut out = style::Style::default();
    if let Some(fg) = style.fg {
        out = out.fg(to_ratatui_color(fg));
    }
    if let Some(bg) = style.bg {
        out = out.bg(to_ratatui_color(bg));
    }
    if let Some(underline_color) = style.underline_color {
        out = out.underline_color(to_ratatui_color(underline_color));
    }
    match style.underline_style {
        Some(helix_graphics::UnderlineStyle::Reset) => {
            out = out.remove_modifier(style::Modifier::UNDERLINED);
        }
        Some(_) => {
            out = out.add_modifier(style::Modifier::UNDERLINED);
        }
        None => {}
    }
    out.add_modifier(to_ratatui_modifier(style.add_modifier))
        .remove_modifier(to_ratatui_modifier(style.sub_modifier))
}

/// Convert a Helix text span into a Ratatui text span.
pub fn to_ratatui_span<'a>(span: &crate::text::Span<'a>) -> text::Span<'a> {
    text::Span::styled(span.content.clone(), to_ratatui_style(span.style))
}

/// Convert a Helix single-line span collection into a Ratatui line.
pub fn to_ratatui_line<'a>(spans: &crate::text::Spans<'a>) -> text::Line<'a> {
    text::Line::from(spans.0.iter().map(to_ratatui_span).collect::<Vec<_>>())
}

/// Convert Helix multiline text into Ratatui multiline text.
pub fn to_ratatui_text<'a>(value: &crate::text::Text<'a>) -> text::Text<'a> {
    text::Text::from(value.lines.iter().map(to_ratatui_line).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::Config;
    use ::ratatui::buffer::Cell;
    use helix_view::graphics::{
        Color as HelixColor, CursorKind, Modifier as HelixModifier, Rect, Style as HelixStyle,
        UnderlineStyle,
    };

    #[derive(Debug)]
    struct SessionBackend {
        claimed: bool,
        drawn: usize,
    }

    impl SessionBackend {
        fn new() -> Self {
            Self {
                claimed: false,
                drawn: 0,
            }
        }
    }

    impl HelixBackend for SessionBackend {
        fn claim(&mut self) -> io::Result<()> {
            self.claimed = true;
            Ok(())
        }

        fn reconfigure(&mut self, _config: Config) -> io::Result<()> {
            Ok(())
        }

        fn restore(&mut self) -> io::Result<()> {
            self.claimed = false;
            Ok(())
        }

        fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            self.drawn += content.count();
            Ok(())
        }

        fn hide_cursor(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn show_cursor(&mut self, _kind: CursorKind) -> io::Result<()> {
            Ok(())
        }

        fn set_cursor(&mut self, _x: u16, _y: u16) -> io::Result<()> {
            Ok(())
        }

        fn clear(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn size(&self) -> io::Result<Rect> {
            Ok(Rect::new(0, 0, 10, 2))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn supports_true_color(&self) -> bool {
            true
        }

        fn get_theme_mode(&self) -> Option<helix_view::theme::Mode> {
            None
        }
    }

    impl backend::Backend for SessionBackend {
        type Error = io::Error;

        fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            self.drawn += content.count();
            Ok(())
        }

        fn hide_cursor(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }

        fn show_cursor(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }

        fn get_cursor_position(&mut self) -> Result<layout::Position, Self::Error> {
            Ok(layout::Position::new(0, 0))
        }

        fn set_cursor_position<P>(&mut self, _position: P) -> Result<(), Self::Error>
        where
            P: Into<layout::Position>,
        {
            Ok(())
        }

        fn clear(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }

        fn clear_region(&mut self, _clear_type: backend::ClearType) -> Result<(), Self::Error> {
            Ok(())
        }

        fn size(&self) -> Result<layout::Size, Self::Error> {
            Ok(layout::Size::new(10, 2))
        }

        fn window_size(&mut self) -> Result<backend::WindowSize, Self::Error> {
            Ok(backend::WindowSize {
                columns_rows: layout::Size::new(10, 2),
                pixels: layout::Size::new(0, 0),
            })
        }

        fn flush(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }

        fn scroll_region_up(
            &mut self,
            _region: std::ops::Range<u16>,
            _line_count: u16,
        ) -> Result<(), Self::Error> {
            Ok(())
        }

        fn scroll_region_down(
            &mut self,
            _region: std::ops::Range<u16>,
            _line_count: u16,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[test]
    fn terminal_session_enforces_claimed_render_state() {
        let session = TerminalSession::new(SessionBackend::new()).unwrap();
        assert!(!session.backend().claimed);

        let mut session = session.claim().unwrap();
        assert!(session.backend().claimed);

        session
            .draw(|frame| {
                frame.buffer_mut()[(0, 0)].set_symbol("x");
            })
            .unwrap();
        assert!(session.backend().drawn > 0);

        let session = session.restore().unwrap();
        assert!(!session.backend().claimed);
    }

    #[test]
    fn converts_helix_style_to_ratatui_style_patch() {
        let style = HelixStyle::default()
            .fg(HelixColor::LightBlue)
            .bg(HelixColor::Rgb(1, 2, 3))
            .underline_color(HelixColor::Indexed(4))
            .underline_style(UnderlineStyle::Curl)
            .add_modifier(HelixModifier::BOLD | HelixModifier::ITALIC);

        let style = to_ratatui_style(style);

        assert_eq!(style.fg, Some(style::Color::LightBlue));
        assert_eq!(style.bg, Some(style::Color::Rgb(1, 2, 3)));
        assert_eq!(style.underline_color, Some(style::Color::Indexed(4)));
        assert!(style.add_modifier.contains(style::Modifier::UNDERLINED));
        assert!(style.add_modifier.contains(style::Modifier::BOLD));
        assert!(style.add_modifier.contains(style::Modifier::ITALIC));
    }

    #[test]
    fn converts_ratatui_style_to_helix_style_patch() {
        let style = style::Style::default()
            .fg(style::Color::LightBlue)
            .bg(style::Color::Rgb(1, 2, 3))
            .underline_color(style::Color::Indexed(4))
            .add_modifier(style::Modifier::BOLD | style::Modifier::UNDERLINED)
            .remove_modifier(style::Modifier::CROSSED_OUT);

        let style = to_helix_style(style);

        assert_eq!(style.fg, Some(HelixColor::LightBlue));
        assert_eq!(style.bg, Some(HelixColor::Rgb(1, 2, 3)));
        assert_eq!(style.underline_color, Some(HelixColor::Indexed(4)));
        assert_eq!(style.underline_style, Some(UnderlineStyle::Line));
        assert!(style.add_modifier.contains(HelixModifier::BOLD));
        assert!(style.sub_modifier.contains(HelixModifier::CROSSED_OUT));
    }

    #[test]
    fn converts_helix_text_to_ratatui_text() {
        let source = crate::text::Text::from(vec![crate::text::Spans::from(vec![
            crate::text::Span::styled("a", HelixStyle::default().fg(HelixColor::LightBlue)),
            crate::text::Span::raw("b"),
        ])]);

        let converted = to_ratatui_text(&source);

        assert_eq!(converted.lines.len(), 1);
        assert_eq!(converted.lines[0].spans[0].content.as_ref(), "a");
        assert_eq!(
            converted.lines[0].spans[0].style.fg,
            Some(style::Color::LightBlue)
        );
        assert_eq!(converted.lines[0].spans[1].content.as_ref(), "b");
    }
}
