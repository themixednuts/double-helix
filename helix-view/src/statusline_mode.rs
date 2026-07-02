//! Mode → display name + theme style for status-line-style chips.
//!
//! Three independent surfaces in the editor render a "mode chip"
//! (a small coloured label naming the current mode): the editor's
//! own statusline, the file explorer's per-panel footer, and the
//! assistant panel's header. Each one used to hard-code the
//! `Mode::Normal => "NORMAL"` / theme-key mapping locally, which
//! meant a user remapping mode names via `[editor.statusline.mode]`
//! got picked up by the editor statusline but not the file explorer.
//!
//! These helpers centralize the mapping so every surface reads the
//! user's [`ModeConfig`] for the *label* and the same theme keys for
//! the *style*. Each surface still owns its own rendering layer
//! (the assistant uses `Spans`, the file explorer uses
//! `surface.set_stringn` via `chip_strip`, the editor statusline
//! uses themed `Span`) — only the lookup is shared, not the paint.
//!
//! [`ModeConfig`]: crate::editor::ModeConfig

use crate::document::Mode;
use crate::editor::ModeConfig;
use crate::theme::{Style, Theme};

/// The user-configurable display name for `mode`. Mirrors the
/// `[editor.statusline.mode]` config — surface this where you'd
/// otherwise hard-code `"NORMAL"` / `"INSERT"` / `"SELECT"`.
pub fn mode_name(mode: Mode, config: &ModeConfig) -> &str {
    match mode {
        Mode::Normal => &config.normal,
        Mode::Insert => &config.insert,
        Mode::Select => &config.select,
    }
}

/// The theme scope key for `mode`'s background colour — the one
/// each surface patches onto its `ui.statusline` (or panel-base)
/// style to colour the chip. Surfaces should call
/// [`mode_style`] rather than reading the theme directly so the
/// fallback story stays uniform.
pub fn mode_theme_key(mode: Mode) -> &'static str {
    match mode {
        Mode::Normal => "ui.statusline.normal",
        Mode::Insert => "ui.statusline.insert",
        Mode::Select => "ui.statusline.select",
    }
}

/// The mode chip's foreground/background style, with `fallback`
/// applied when the theme doesn't define a scope for the mode.
/// Most callers want to patch this onto a base `ui.statusline`
/// style — that's the host's job (so the panel-base background
/// shows through anywhere the mode scope doesn't override).
pub fn mode_style(mode: Mode, theme: &Theme, fallback: Style) -> Style {
    theme.try_get(mode_theme_key(mode)).unwrap_or(fallback)
}

/// `" {name} "` — the conventional space-padded mode label.
/// Allocates a new `String` because the padded form isn't a slice
/// of any pre-existing string. Cheap (3–7 bytes) and matches what
/// each surface already does inline.
pub fn padded_mode_name(mode: Mode, config: &ModeConfig) -> String {
    format!(" {} ", mode_name(mode, config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_name_returns_configured_label() {
        let config = ModeConfig {
            normal: String::from("NRM"),
            insert: String::from("INS"),
            select: String::from("SEL"),
        };
        assert_eq!(mode_name(Mode::Normal, &config), "NRM");
        assert_eq!(mode_name(Mode::Insert, &config), "INS");
        assert_eq!(mode_name(Mode::Select, &config), "SEL");
    }

    #[test]
    fn mode_theme_keys_match_documented_scopes() {
        // Pin the scope strings — they're part of the public theme
        // contract. Changing one in code without bumping themes is
        // a user-facing regression.
        assert_eq!(mode_theme_key(Mode::Normal), "ui.statusline.normal");
        assert_eq!(mode_theme_key(Mode::Insert), "ui.statusline.insert");
        assert_eq!(mode_theme_key(Mode::Select), "ui.statusline.select");
    }

    #[test]
    fn padded_mode_name_wraps_in_single_spaces() {
        let config = ModeConfig {
            normal: String::from("X"),
            insert: String::new(),
            select: String::from("YYYY"),
        };
        assert_eq!(padded_mode_name(Mode::Normal, &config), " X ");
        assert_eq!(padded_mode_name(Mode::Insert, &config), "  ");
        assert_eq!(padded_mode_name(Mode::Select, &config), " YYYY ");
    }
}
