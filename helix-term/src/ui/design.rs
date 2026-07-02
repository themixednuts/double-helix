use helix_view::{
    graphics::{Modifier, Rect, Style},
    info::Info,
    theme::Theme,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BufferlineStyles {
    pub background: Style,
    pub active: Style,
    pub inactive: Style,
}

impl BufferlineStyles {
    pub(crate) fn from_theme(theme: &Theme) -> Self {
        Self {
            background: theme
                .try_get("ui.bufferline.background")
                .unwrap_or_else(|| theme.get("ui.statusline")),
            active: theme
                .try_get("ui.bufferline.active")
                .unwrap_or_else(|| theme.get("ui.statusline.active")),
            inactive: theme
                .try_get("ui.bufferline")
                .unwrap_or_else(|| theme.get("ui.statusline.inactive")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GutterStyles {
    pub base: Style,
    pub selected: Style,
    pub virtual_line: Style,
    pub selected_virtual: Style,
    pub line_number: Style,
    pub selected_line_number: Style,
}

impl GutterStyles {
    pub(crate) fn from_theme(theme: &Theme) -> Self {
        Self {
            base: theme.get("ui.gutter"),
            selected: theme.get("ui.gutter.selected"),
            virtual_line: theme.get("ui.gutter.virtual"),
            selected_virtual: theme.get("ui.gutter.selected.virtual"),
            line_number: theme.get("ui.linenr"),
            selected_line_number: theme.get("ui.linenr.selected"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CursorLineStyles {
    pub primary: Style,
    pub secondary: Style,
    pub column_primary: Style,
    pub column_secondary: Style,
}

impl CursorLineStyles {
    pub(crate) fn from_theme(theme: &Theme) -> Self {
        let primary = theme.get("ui.cursorline.primary");
        let secondary = theme.get("ui.cursorline.secondary");
        Self {
            primary,
            secondary,
            column_primary: theme
                .try_get_exact("ui.cursorcolumn.primary")
                .or_else(|| theme.try_get_exact("ui.cursorcolumn"))
                .unwrap_or(primary),
            column_secondary: theme
                .try_get_exact("ui.cursorcolumn.secondary")
                .or_else(|| theme.try_get_exact("ui.cursorcolumn"))
                .unwrap_or(secondary),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StatuslineStyles {
    pub base: Style,
    pub inactive: Style,
    pub normal: Style,
    pub insert: Style,
    pub select: Style,
    pub separator: Style,
}

impl StatuslineStyles {
    pub(crate) fn from_theme(theme: &Theme) -> Self {
        Self {
            base: theme.get("ui.statusline"),
            inactive: theme.get("ui.statusline.inactive"),
            normal: theme.get("ui.statusline.normal"),
            insert: theme.get("ui.statusline.insert"),
            select: theme.get("ui.statusline.select"),
            separator: theme.get("ui.statusline.separator"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InfoPopupStyles {
    pub popup: Style,
    pub text: Style,
    pub scrollbar: Style,
}

impl InfoPopupStyles {
    pub(crate) fn from_theme(theme: &Theme) -> Self {
        Self {
            popup: theme.get("ui.popup.info"),
            text: theme.get("ui.text.info"),
            scrollbar: theme.get("ui.menu.scroll"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MenuStyles {
    pub background: Style,
    pub selected: Style,
    pub scroll: Style,
}

impl MenuStyles {
    pub(crate) fn from_theme(theme: &Theme) -> Self {
        Self {
            background: theme
                .try_get("ui.menu")
                .unwrap_or_else(|| theme.get("ui.text")),
            selected: theme.get("ui.menu.selected"),
            scroll: theme.get("ui.menu.scroll"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PopupStyles {
    pub background: Style,
    pub border: Style,
}

impl PopupStyles {
    pub(crate) fn from_theme(theme: &Theme) -> Self {
        Self {
            background: theme.get("ui.popup"),
            border: theme
                .try_get("ui.popup.border")
                .or_else(|| theme.try_get("ui.window"))
                .unwrap_or_else(|| theme.get("ui.popup")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileExplorerStatusStyles {
    pub added: Style,
    pub modified: Style,
    pub deleted: Style,
    pub renamed: Style,
    pub conflict: Style,
    pub diagnostic_hint: Style,
    pub diagnostic_info: Style,
    pub diagnostic_warning: Style,
    pub diagnostic_error: Style,
}

impl FileExplorerStatusStyles {
    pub(crate) fn from_theme(theme: &Theme) -> Self {
        Self {
            added: theme.get("diff.plus"),
            modified: theme.get("diff.delta"),
            deleted: theme.get("diff.minus"),
            renamed: theme.get("diff.delta.moved"),
            conflict: theme.get("diff.delta.conflict"),
            diagnostic_hint: theme.get("hint"),
            diagnostic_info: theme.get("info"),
            diagnostic_warning: theme.get("warning"),
            diagnostic_error: theme.get("error"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileExplorerStyles {
    pub background: Style,
    pub text: Style,
    pub inactive: Style,
    pub directory: Style,
    pub guide: Style,
    pub selection: Style,
    pub header: Style,
    pub border: Style,
    pub status: FileExplorerStatusStyles,
}

impl FileExplorerStyles {
    pub(crate) fn from_theme(theme: &Theme, focused: bool) -> Self {
        let background = theme.get("ui.background");
        let text = theme.get("ui.text");
        let inactive = theme.get("ui.text.inactive");
        let header = if focused {
            theme.get("ui.statusline")
        } else {
            theme.get("ui.statusline.inactive")
        };
        let border = if focused {
            theme.get("ui.window").add_modifier(Modifier::BOLD)
        } else {
            inactive
        };

        Self {
            background,
            text,
            inactive,
            directory: theme.get("ui.text.directory"),
            guide: theme.try_get("ui.virtual.indent-guide").unwrap_or(inactive),
            selection: theme
                .try_get("ui.selection.primary")
                .unwrap_or_else(|| theme.get("ui.selection")),
            header,
            border,
            status: FileExplorerStatusStyles::from_theme(theme),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HelixUiDesign {
    pub bufferline: BufferlineStyles,
    pub gutter: GutterStyles,
    pub cursorline: CursorLineStyles,
    pub statusline: StatuslineStyles,
    pub info_popup: InfoPopupStyles,
    pub menu: MenuStyles,
    pub popup: PopupStyles,
    pub file_explorer: FileExplorerStyles,
}

impl HelixUiDesign {
    pub(crate) fn from_theme(theme: &Theme) -> Self {
        Self {
            bufferline: BufferlineStyles::from_theme(theme),
            gutter: GutterStyles::from_theme(theme),
            cursorline: CursorLineStyles::from_theme(theme),
            statusline: StatuslineStyles::from_theme(theme),
            info_popup: InfoPopupStyles::from_theme(theme),
            menu: MenuStyles::from_theme(theme),
            popup: PopupStyles::from_theme(theme),
            file_explorer: FileExplorerStyles::from_theme(theme, true),
        }
    }
}

pub(crate) fn info_popup_area(viewport: Rect, info: &Info) -> Rect {
    let reserved_bottom = if viewport.height > 2 { 2 } else { 0 };
    let available_height = viewport.height.saturating_sub(reserved_bottom);
    let width = info.width.saturating_add(4).min(viewport.width);
    let height = info.height.saturating_add(2).min(available_height);
    Rect::new(
        viewport.x + viewport.width.saturating_sub(width),
        viewport.y
            + viewport
                .height
                .saturating_sub(reserved_bottom.saturating_add(height)),
        width,
        height,
    )
}

pub(crate) fn info_popup_body_height(area: Rect) -> u16 {
    area.height.saturating_sub(2)
}
