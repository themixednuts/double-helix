use crate::compositor::{Component, Context, Event, EventResult, RenderContext};
use crate::ui::gradient_border::GradientBorder;
use crate::ui::prompt::{Prompt, PromptEvent};
use crate::widgets::{draw_string_anchored, AnchoredText};
use helix_core::{unicode::width::UnicodeWidthStr, Position};
use helix_view::{
    editor::CmdlineStyle,
    graphics::{CursorKind, Rect},
    Editor,
};
use std::borrow::Cow;
use std::sync::Arc;

const GRADIENT_FRAME: helix_runtime::FrameSource =
    helix_runtime::FrameSource::new("cmdline.gradient-border");

pub struct CmdlinePopup {
    prompt: Prompt,
    style: CmdlineStyle,
    // Popup-specific properties
    popup_area: Rect,
    min_width: u16,
    max_width: u16,
    padding: u16,
    // Gradient border for cmdline popup
    gradient_border: Option<GradientBorder>,
}

struct CmdlineCompletionSnapshot {
    content: Arc<str>,
    style: helix_view::graphics::Style,
    selected: bool,
}

struct CmdlinePopupSnapshot {
    popup_area: Rect,
    inner_area: Rect,
    completion_area: Option<Rect>,
    completion_inner: Option<Rect>,
    theme: Arc<helix_view::Theme>,
    gradient_border: Option<GradientBorder>,
    rounded_corners: bool,
    title: Arc<str>,
    icon: Arc<str>,
    input_area: Rect,
    line: Arc<str>,
    anchor: usize,
    truncate_start: bool,
    truncate_end: bool,
    completions: Arc<[CmdlineCompletionSnapshot]>,
    completion_scroll: usize,
    completion_total: usize,
    picker_symbol: Arc<str>,
}

impl CmdlinePopupSnapshot {
    fn paint(
        mut self,
        surface: &mut crate::render::CellSurface,
        cancellation: &crate::render::RenderCancellation,
    ) {
        if cancellation.is_cancelled() {
            return;
        }
        let popup = tui::ratatui::to_ratatui_rect(self.popup_area);
        tui::ratatui::widgets::Widget::render(tui::ratatui::widgets::Clear, popup, surface);
        surface.set_style(
            popup,
            tui::ratatui::to_ratatui_style(self.theme.get("ui.popup")),
        );
        if let Some(gradient) = self.gradient_border.as_mut() {
            gradient.render_with_title(
                self.popup_area,
                surface,
                &self.theme,
                Some(&self.title),
                self.rounded_corners,
            );
        } else {
            let border = self.theme.get("ui.popup.border");
            crate::widgets::Panel::framed(
                crate::widgets::PanelStyle::new(self.theme.get("ui.popup"), border, border),
                self.rounded_corners,
            )
            .render(surface, self.popup_area);
            if !self.title.is_empty()
                && self.popup_area.width > UnicodeWidthStr::width(self.title.as_ref()) as u16 + 2
            {
                surface.set_stringn(
                    self.popup_area.x.saturating_add(1),
                    self.popup_area.y,
                    &self.title,
                    self.popup_area.width.saturating_sub(2) as usize,
                    tui::ratatui::to_ratatui_style(border),
                );
            }
        }
        if !self.icon.is_empty() {
            surface.set_string(
                self.inner_area.x,
                self.inner_area.y,
                &self.icon,
                tui::ratatui::to_ratatui_style(
                    self.theme
                        .get("ui.text.focus")
                        .add_modifier(helix_view::theme::Modifier::BOLD),
                ),
            );
        }
        let mut style_for_offset = |_| tui::ratatui::to_ratatui_style(self.theme.get("ui.text"));
        draw_string_anchored(
            surface,
            AnchoredText::new(
                self.input_area.x,
                self.input_area.y,
                &self.line[self.anchor..],
                self.input_area.width as usize,
            )
            .truncate_start(self.truncate_start)
            .truncate_end(self.truncate_end),
            &mut style_for_offset,
        );
        if cancellation.is_cancelled() {
            return;
        }
        self.paint_completions(surface);
    }

    fn paint_completions(&mut self, surface: &mut crate::render::CellSurface) {
        let (Some(area), Some(inner)) = (self.completion_area, self.completion_inner) else {
            return;
        };
        let ratatui_area = tui::ratatui::to_ratatui_rect(area);
        tui::ratatui::widgets::Widget::render(tui::ratatui::widgets::Clear, ratatui_area, surface);
        let background = self.theme.get("ui.menu");
        let selected = self.theme.get("ui.menu.selected");
        surface.set_style(ratatui_area, tui::ratatui::to_ratatui_style(background));
        if let Some(gradient) = self.gradient_border.as_mut() {
            gradient.render(area, surface, &self.theme, self.rounded_corners);
        } else {
            crate::widgets::Panel::framed(
                crate::widgets::PanelStyle::new(
                    self.theme.get("ui.popup"),
                    self.theme.get("ui.popup.border"),
                    self.theme.get("ui.popup.border"),
                ),
                self.rounded_corners,
            )
            .render(surface, area);
        }
        let symbol_width = UnicodeWidthStr::width(self.picker_symbol.as_ref());
        for (row, completion) in self.completions.iter().enumerate() {
            if row as u16 >= inner.height {
                break;
            }
            let y = inner.y.saturating_add(row as u16);
            let style = if completion.selected {
                surface.set_stringn(
                    inner.x,
                    y,
                    " ".repeat(inner.width as usize),
                    inner.width as usize,
                    tui::ratatui::to_ratatui_style(selected),
                );
                selected
            } else {
                background.patch(completion.style)
            };
            let prefix = if completion.selected {
                self.picker_symbol.as_ref()
            } else {
                ""
            };
            let text = if completion.selected {
                format!("{prefix}{}", completion.content)
            } else {
                format!("{}{}", " ".repeat(symbol_width), completion.content)
            };
            surface.set_stringn(
                inner.x,
                y,
                &text,
                inner.width as usize,
                tui::ratatui::to_ratatui_style(style),
            );
        }
        let inactive = self.theme.get("ui.text.inactive");
        if self.completion_total > self.completions.len() {
            if self.completion_scroll > 0 {
                surface.set_string(
                    inner.right().saturating_sub(1),
                    inner.y,
                    "↑",
                    tui::ratatui::to_ratatui_style(inactive),
                );
            }
            if self.completion_scroll + self.completions.len() < self.completion_total {
                surface.set_string(
                    inner.right().saturating_sub(1),
                    inner.bottom().saturating_sub(1),
                    "↓",
                    tui::ratatui::to_ratatui_style(inactive),
                );
            }
        }
    }
}

impl CmdlinePopup {
    pub fn new(
        prompt_text: Cow<'static, str>,
        history_register: Option<char>,
        completion_provider: impl crate::ui::prompt::CompletionProvider + 'static,
        callback_fn: impl FnMut(&mut Context, &str, PromptEvent) + Send + 'static,
        style: CmdlineStyle,
    ) -> Self {
        Self {
            prompt: Prompt::new(
                prompt_text,
                history_register,
                completion_provider,
                callback_fn,
            ),
            style,
            popup_area: Rect::default(),
            min_width: 40,
            max_width: 80,
            padding: 2,
            gradient_border: None,
        }
    }

    pub fn with_line(mut self, line: String, editor: &Editor) -> Self {
        self.prompt = self.prompt.with_line(line, editor);
        self
    }

    pub fn with_language(
        mut self,
        language: &'static str,
        loader: std::sync::Arc<arc_swap::ArcSwap<helix_core::syntax::Loader>>,
    ) -> Self {
        self.prompt = self.prompt.with_language(language, loader);
        self
    }

    pub(crate) fn apply_completion_result(
        &mut self,
        result: crate::runtime::ui::PromptCompletionResult,
    ) -> bool {
        self.prompt.apply_completion_result(result)
    }

    pub(crate) fn prepare_completion(
        &mut self,
        editor: &Editor,
        ingress: crate::runtime::RuntimeIngress,
    ) {
        self.prompt.recalculate_completion(editor);
        self.prompt
            .dispatch_completion_work(editor.runtime(), ingress);
    }

    /// Calculate optimal popup dimensions and position
    fn calculate_popup_area(&self, viewport: Rect) -> Rect {
        let content_width = self.prompt.line().width().max(self.min_width as usize);
        let width = (content_width as u16 + self.padding * 2)
            .min(self.max_width)
            .min(viewport.width.saturating_sub(4));

        let height = 3; // Base height for single line + borders

        let x = viewport.x + (viewport.width.saturating_sub(width)) / 2;
        let y = viewport.y + (viewport.height.saturating_sub(height)) / 3; // Position in upper third

        Rect::new(x, y, width, height)
    }

    /// Get command type icon based on the input
    fn get_command_icon<'a>(&self, config: &'a helix_view::editor::CmdlineIcons) -> &'a str {
        let line = self.prompt.line();
        if line.starts_with("search:") || line.starts_with("/") || line.starts_with("?") {
            &config.search
        } else if line.starts_with(":") {
            &config.command
        } else if line.starts_with('!') {
            &config.shell
        } else {
            // Check if this is a regex prompt by looking at the prompt text
            match self.prompt.prompt() {
                s if s.starts_with("search:") || s == "Search" => &config.search,
                "Cmdline" => &config.command,
                _ => &config.general,
            }
        }
    }

    fn prepare_popup_snapshot(
        &mut self,
        viewport: Rect,
        cx: &RenderContext,
    ) -> CmdlinePopupSnapshot {
        let popup_area = self.calculate_popup_area(viewport);
        self.popup_area = popup_area;
        let config = cx.config();
        let gradient_enabled = config.gradient_borders.enable;
        if gradient_enabled
            && self
                .gradient_border
                .as_ref()
                .is_none_or(|border| !border.matches_config(&config.gradient_borders))
        {
            self.gradient_border = Some(GradientBorder::from_theme(
                cx.theme(),
                &config.gradient_borders,
            ));
        } else if !gradient_enabled {
            self.gradient_border = None;
        }
        if self
            .gradient_border
            .as_ref()
            .is_some_and(GradientBorder::is_animated)
        {
            cx.request_frame_at(GRADIENT_FRAME, cx.clock().now());
        }
        let thickness = if gradient_enabled {
            config.gradient_borders.thickness as u16
        } else {
            1
        };
        let inner_area = Rect::new(
            popup_area.x.saturating_add(thickness),
            popup_area.y.saturating_add(thickness),
            popup_area.width.saturating_sub(thickness.saturating_mul(2)),
            popup_area
                .height
                .saturating_sub(thickness.saturating_mul(2)),
        );
        let icon: Arc<str> = Arc::from(if config.cmdline.show_icons {
            self.get_command_icon(&config.cmdline.icons)
        } else {
            ""
        });
        let input_area = Rect::new(
            inner_area
                .x
                .saturating_add(UnicodeWidthStr::width(icon.as_ref()) as u16),
            inner_area.y,
            inner_area
                .width
                .saturating_sub(UnicodeWidthStr::width(icon.as_ref()) as u16),
            1,
        );
        self.prompt.update_scroll_anchor(input_area.width as usize);

        const MAX_COMPLETIONS: usize = 10;
        let total = self.prompt.completions().len();
        let visible = total.min(MAX_COMPLETIONS);
        let completion_area = (visible > 0).then(|| {
            Rect::new(
                popup_area.x,
                popup_area
                    .y
                    .saturating_add(popup_area.height)
                    .saturating_add(1),
                popup_area.width,
                visible as u16 + 2,
            )
        });
        let completion_inner = completion_area.map(|area| {
            Rect::new(
                area.x.saturating_add(thickness),
                area.y.saturating_add(thickness),
                area.width.saturating_sub(thickness.saturating_mul(2)),
                area.height.saturating_sub(thickness.saturating_mul(2)),
            )
        });
        let selected = self.prompt.selection().unwrap_or(0);
        let scroll = selected.saturating_sub(MAX_COMPLETIONS.saturating_sub(1));
        let completions = self
            .prompt
            .completions()
            .iter()
            .enumerate()
            .skip(scroll)
            .take(MAX_COMPLETIONS)
            .map(|(index, (_, completion))| CmdlineCompletionSnapshot {
                content: Arc::from(completion.content.as_ref()),
                style: completion.style,
                selected: self.prompt.selection() == Some(index),
            })
            .collect::<Vec<_>>();

        CmdlinePopupSnapshot {
            popup_area,
            inner_area,
            completion_area,
            completion_inner,
            theme: cx.theme_arc(),
            gradient_border: gradient_enabled
                .then(|| self.gradient_border.clone())
                .flatten(),
            rounded_corners: config.rounded_corners,
            title: Arc::from(self.prompt.prompt()),
            icon,
            input_area,
            line: Arc::from(self.prompt.line().as_str()),
            anchor: self.prompt.anchor(),
            truncate_start: self.prompt.truncate_start(),
            truncate_end: self.prompt.truncate_end(),
            completions: Arc::from(completions),
            completion_scroll: scroll,
            completion_total: total,
            picker_symbol: Arc::from(config.picker_symbol.as_str()),
        }
    }
}

impl Component for CmdlinePopup {
    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        // Delegate event handling to the underlying prompt
        self.prompt.handle_event(event, cx)
    }

    fn prepare_render(&mut self, area: Rect, cx: &RenderContext) -> crate::render::PreparedRender {
        match self.style {
            CmdlineStyle::Popup => {
                let snapshot = self.prepare_popup_snapshot(area, cx);
                crate::render::PreparedRender::deferred(move |cancellation| {
                    let mut output = crate::render::RenderOutput::sparse(area);
                    snapshot.paint(output.surface_mut(), cancellation);
                    output
                })
            }
            CmdlineStyle::Bottom => self.prompt.prepare_render(area, cx),
        }
    }

    fn cursor(&self, area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        match self.style {
            CmdlineStyle::Popup => {
                // Calculate cursor position for popup style
                let config = editor.config();
                let icon = if config.cmdline.show_icons {
                    self.get_command_icon(&config.cmdline.icons)
                } else {
                    ""
                };
                let prefix_width = if icon.is_empty() { 0 } else { icon.width() };

                // Compute inner area similar to render: respect gradient border thickness if enabled
                let inner_area = if editor.config().gradient_borders.enable {
                    let t: u16 = editor.config().gradient_borders.thickness as u16;
                    Rect {
                        x: self.popup_area.x + t,
                        y: self.popup_area.y + t,
                        width: self.popup_area.width.saturating_sub(t * 2),
                        height: self.popup_area.height.saturating_sub(t * 2),
                    }
                } else {
                    Rect::new(
                        self.popup_area.x + 1,
                        self.popup_area.y + 1,
                        self.popup_area.width.saturating_sub(2),
                        self.popup_area.height.saturating_sub(2),
                    )
                };

                // Build the same input area used in render_popup
                let input_area = Rect::new(
                    inner_area.x + prefix_width as u16,
                    inner_area.y,
                    inner_area.width.saturating_sub(prefix_width as u16),
                    1,
                );

                // Compute cursor position accounting for horizontal scroll anchor
                let byte_pos = self.prompt.position();
                let anchor = self.prompt.anchor();
                let line = self.prompt.line();

                // Calculate cursor position relative to the visible portion (after anchor)
                // Also account for truncation indicator if text is scrolled
                let truncate_start = self.prompt.truncate_start();
                let visible_cursor_offset = if byte_pos >= anchor {
                    line[anchor..byte_pos].width()
                } else {
                    0
                };

                // Add 1 for the truncation indicator "…" if we're scrolled
                let indicator_offset = if truncate_start { 1 } else { 0 };
                let cursor_offset = (visible_cursor_offset + indicator_offset) as u16;
                let clamped_offset = cursor_offset.min(input_area.width.saturating_sub(1));
                let cursor_x = input_area.x as usize + clamped_offset as usize;
                let cursor_y = input_area.y as usize;

                (
                    Some(Position::new(cursor_y, cursor_x)),
                    editor
                        .config()
                        .cursor_shape
                        .from_mode(helix_view::document::Mode::Insert),
                )
            }
            CmdlineStyle::Bottom => {
                // Delegate to original prompt cursor calculation
                self.prompt.cursor(area, editor)
            }
        }
    }
}
