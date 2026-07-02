use crate::render::CellSurface as Buffer;
use helix_view::{
    graphics::{Rect, Style},
    theme::Theme,
};

use crate::ui::design::HelixUiDesign;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiStyleGuide {
    pub tick: u64,
    pub surface: Style,
    pub panel: Style,
    pub popup: Style,
    pub popup_border: Style,
    pub menu: Style,
    pub menu_selected: Style,
    pub popup_info: Style,
    pub border: Style,
    pub text: Style,
    pub directory: Style,
    pub text_info: Style,
    pub muted: Style,
    pub accent: Style,
    pub selected: Style,
    pub bufferline: Style,
    pub bufferline_active: Style,
    pub bufferline_inactive: Style,
    pub statusline: Style,
    pub statusline_normal: Style,
    pub statusline_insert: Style,
    pub statusline_separator: Style,
    pub gutter: Style,
    pub linenr: Style,
    pub linenr_selected: Style,
    pub cursorline: Style,
    pub menu_scroll: Style,
    pub success: Style,
    pub warning: Style,
    pub error: Style,
    pub info: Style,
}

impl UiStyleGuide {
    pub fn from_theme(theme: &Theme) -> Self {
        let text = theme.get("ui.text");
        let surface = text.patch(theme.get("ui.background"));
        let directory = theme.get("ui.text.directory");
        let helix_design = HelixUiDesign::from_theme(theme);
        let panel = text.patch(
            theme
                .try_get("ui.popup")
                .or_else(|| theme.try_get("ui.menu"))
                .or_else(|| theme.try_get("ui.background"))
                .unwrap_or_default(),
        );
        let selected = text.patch(
            theme
                .try_get("ui.menu.selected")
                .or_else(|| theme.try_get("ui.selection"))
                .or_else(|| theme.try_get("ui.text.focus"))
                .unwrap_or_default(),
        );
        let accent = theme
            .try_get("ui.text.focus")
            .or_else(|| theme.try_get("ui.cursor.primary"))
            .or_else(|| theme.try_get("ui.menu.selected"))
            .unwrap_or(text);
        let muted = theme
            .try_get("ui.text.inactive")
            .or_else(|| theme.try_get("comment"))
            .unwrap_or(text);
        let border = theme
            .try_get("ui.popup.border")
            .or_else(|| theme.try_get("ui.window"))
            .or_else(|| theme.try_get("ui.background.separator"))
            .unwrap_or(muted);

        Self {
            tick: 0,
            surface,
            panel,
            popup: helix_design.popup.background,
            popup_border: helix_design.popup.border,
            menu: helix_design.menu.background,
            menu_selected: helix_design.menu.selected,
            popup_info: helix_design.info_popup.popup,
            border,
            text,
            directory,
            text_info: helix_design.info_popup.text,
            muted,
            accent,
            selected,
            bufferline: helix_design.bufferline.background,
            bufferline_active: helix_design.bufferline.active,
            bufferline_inactive: helix_design.bufferline.inactive,
            statusline: helix_design.statusline.base,
            statusline_normal: helix_design.statusline.normal,
            statusline_insert: helix_design.statusline.insert,
            statusline_separator: helix_design.statusline.separator,
            gutter: helix_design.gutter.base,
            linenr: helix_design.gutter.line_number,
            linenr_selected: helix_design.gutter.selected_line_number,
            cursorline: helix_design.cursorline.primary,
            menu_scroll: helix_design.menu.scroll,
            success: theme
                .try_get("diff.plus")
                .unwrap_or_else(|| theme.get("hint")),
            warning: theme.get("warning"),
            error: theme.get("error"),
            info: theme
                .try_get("ui.text.info")
                .or_else(|| theme.try_get("info"))
                .unwrap_or(text),
        }
    }

    pub(super) fn with_tick(mut self, tick: u64) -> Self {
        self.tick = tick;
        self
    }

    pub(super) fn rat(self, style: Style) -> tui::ratatui::style::Style {
        tui::ratatui::to_ratatui_style(style)
    }
}

#[derive(Debug, Clone)]
pub(super) struct LoadedTheme {
    pub(super) name: String,
    pub(super) theme: Theme,
    pub(super) editor_config: helix_view::editor::Config,
    pub(super) styles: UiStyleGuide,
}

impl LoadedTheme {
    pub(super) fn new(theme: Theme, editor_config: helix_view::editor::Config) -> Self {
        Self {
            name: theme.name().to_string(),
            styles: UiStyleGuide::from_theme(&theme),
            theme,
            editor_config,
        }
    }

    pub(super) fn with_tick(&self, tick: u64) -> Self {
        Self {
            name: self.name.clone(),
            theme: self.theme.clone(),
            editor_config: self.editor_config.clone(),
            styles: self.styles.with_tick(tick),
        }
    }

    pub(super) fn context(&self) -> StoryContext<'_> {
        StoryContext {
            theme: &self.theme,
            editor_config: &self.editor_config,
            styles: self.styles,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct StoryContext<'a> {
    pub(super) theme: &'a Theme,
    pub(super) editor_config: &'a helix_view::editor::Config,
    pub(super) styles: UiStyleGuide,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct StoryArg {
    pub(super) name: &'static str,
    pub(super) value: &'static str,
    pub(super) description: &'static str,
}

impl StoryArg {
    pub(super) const fn new(
        name: &'static str,
        value: &'static str,
        description: &'static str,
    ) -> Self {
        Self {
            name,
            value,
            description,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StoryKind {
    Docs,
    Component,
    Pattern,
    Composite,
    Contract,
}

impl StoryKind {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Docs => "docs",
            Self::Component => "component",
            Self::Pattern => "pattern",
            Self::Composite => "composite",
            Self::Contract => "contract",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StoryCanvas {
    FullBleed,
    Padded { x: u16, y: u16 },
    Centered { width: u16, height: u16 },
}

impl StoryCanvas {
    fn apply(self, area: Rect) -> Rect {
        match self {
            Self::FullBleed => area,
            Self::Padded { x, y } => crate::widgets::inset(area, x, y),
            Self::Centered { width, height } => {
                super::centered_rect(area, width.min(area.width), height.min(area.height))
            }
        }
    }

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::FullBleed => "fullscreen",
            Self::Padded { .. } => "padded",
            Self::Centered { .. } => "centered",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum StoryRenderer {
    Styled(fn(&mut Buffer, Rect, UiStyleGuide)),
    Runtime(for<'a> fn(&mut Buffer, Rect, StoryContext<'a>)),
}

#[derive(Debug, Clone, Copy)]
pub struct Story {
    pub id: &'static str,
    pub title: &'static str,
    pub category: &'static str,
    pub component: &'static str,
    pub variant: &'static str,
    pub summary: &'static str,
    pub(super) kind: StoryKind,
    pub(super) args: &'static [StoryArg],
    pub(super) canvas: StoryCanvas,
    pub(super) render: StoryRenderer,
}

impl Story {
    pub(super) fn render(self, surface: &mut Buffer, area: Rect, context: StoryContext<'_>) {
        let canvas = self.canvas.apply(area);
        match self.render {
            StoryRenderer::Styled(render) => render(surface, canvas, context.styles),
            StoryRenderer::Runtime(render) => render(surface, canvas, context),
        }
    }
}
