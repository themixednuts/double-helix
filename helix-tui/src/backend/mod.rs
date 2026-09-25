//! Provides interface for controlling the terminal

use std::io;

use crate::terminal::Config;
use ratatui::buffer::Cell;

use helix_view::graphics::{CursorKind, Rect};

mod cell;

#[cfg(all(feature = "termina", not(windows)))]
mod termina;
#[cfg(all(feature = "termina", not(windows)))]
pub use self::termina::TerminaBackend;

#[cfg(all(feature = "termina", windows))]
mod crossterm;
#[cfg(all(feature = "termina", windows))]
pub use self::crossterm::CrosstermBackend;

mod test;
pub use self::test::TestBackend;

/// Representation of a terminal backend.
pub trait Backend {
    /// Claims the terminal for TUI use.
    fn claim(&mut self) -> Result<(), io::Error>;
    /// Update terminal configuration.
    fn reconfigure(&mut self, config: Config) -> Result<(), io::Error>;
    /// Restores the terminal to a normal state, undoes `claim`
    fn restore(&mut self) -> Result<(), io::Error>;
    /// Draws styled text to the terminal
    fn draw<'a, I>(&mut self, content: I) -> Result<(), io::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>;
    /// Hides the cursor
    fn hide_cursor(&mut self) -> Result<(), io::Error>;
    /// Sets the cursor to the given shape
    fn show_cursor(&mut self, kind: CursorKind) -> Result<(), io::Error>;
    /// Sets the cursor to the given position
    fn set_cursor(&mut self, x: u16, y: u16) -> Result<(), io::Error>;
    /// Clears the terminal
    fn clear(&mut self) -> Result<(), io::Error>;
    /// Opens a synchronized-output frame, where the terminal supports it, so
    /// everything written until [`Backend::end_sync`] presents at once.
    fn start_sync(&mut self) -> Result<(), io::Error> {
        Ok(())
    }
    /// Closes the frame opened by [`Backend::start_sync`].
    fn end_sync(&mut self) -> Result<(), io::Error> {
        Ok(())
    }
    /// Gets the size of the terminal in cells
    fn size(&self) -> Result<Rect, io::Error>;
    /// Flushes the terminal buffer
    fn flush(&mut self) -> Result<(), io::Error>;
    fn supports_true_color(&self) -> bool;
    fn get_theme_mode(&self) -> Option<helix_view::theme::Mode>;
    /// Sets the terminal's own background color (OSC 11) so the area outside the editor and
    /// the terminal's padding match the theme; `None` restores the terminal's default.
    fn set_background_color(
        &mut self,
        _color: Option<helix_view::theme::Color>,
    ) -> Result<(), io::Error> {
        Ok(())
    }
}

/// The OSC 11 sequence that sets the terminal background to `color`, or the OSC 111 sequence
/// that restores the terminal's default when there's no RGB color to set.
pub(crate) fn osc_background(color: Option<helix_view::theme::Color>) -> String {
    match color {
        Some(helix_view::theme::Color::Rgb(r, g, b)) => {
            format!("\x1b]11;rgb:{r:02x}/{g:02x}/{b:02x}\x1b\\")
        }
        _ => OSC_RESET_BACKGROUND.to_owned(),
    }
}

/// OSC 111: restore the terminal's default background.
pub(crate) const OSC_RESET_BACKGROUND: &str = "\x1b]111\x1b\\";
