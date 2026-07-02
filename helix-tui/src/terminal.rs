//! Terminal capability configuration owned by Helix backends.
//!
//! Frame buffers, diffing, and draw lifecycle are provided by Ratatui. This module contains only
//! the Helix-specific terminal modes that Ratatui's backend trait does not model.

use helix_view::{
    editor::{Config as EditorConfig, KittyKeyboardProtocolConfig},
    graphics::Rect,
};

/// Terminal configuration applied by Helix terminal backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub enable_mouse_capture: bool,
    pub force_enable_extended_underlines: bool,
    pub kitty_keyboard_protocol: KittyKeyboardProtocolConfig,
}

impl From<&EditorConfig> for Config {
    fn from(config: &EditorConfig) -> Self {
        Self {
            enable_mouse_capture: config.mouse,
            force_enable_extended_underlines: config.undercurl,
            kitty_keyboard_protocol: config.kitty_keyboard_protocol,
        }
    }
}

/// Fallback terminal size used when a backend cannot report dimensions.
pub const DEFAULT_TERMINAL_SIZE: Rect = Rect {
    x: 0,
    y: 0,
    width: 80,
    height: 24,
};
