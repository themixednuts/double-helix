use std::{borrow::Cow, sync::Arc};

use helix_view::{
    editor::SmartTabConfig,
    graphics::{Rect, Style},
    Editor,
};
use tui::text::Spans;

use crate::{
    compositor::{Component, Context, Event, EventResult, PostAction, RenderContext},
    ctrl, key, shift,
};

use super::{menu::Item, PromptEvent, Text};

type SelectCallback<T> = Box<dyn Fn(&mut Editor, &T, PromptEvent) + Send>;

pub struct Select<T: Item> {
    message: Text,
    options: Arc<[T]>,
    data: Arc<T::Data>,
    callback: SelectCallback<T>,
    selection: usize,
    scroll: usize,
    menu_height: usize,
}

struct SelectRenderModel<T: Item> {
    message: Arc<tui::text::Text<'static>>,
    options: Arc<[T]>,
    data: Arc<T::Data>,
    selection: usize,
    scroll: usize,
    message_style: Style,
    menu_styles: crate::ui::design::MenuStyles,
    accent_rail_color: Option<helix_view::theme::Color>,
    rounded_corners: bool,
    render_menu_borders: bool,
}

impl<T: Item> Select<T> {
    pub fn new<M, I, F>(message: M, options: I, data: T::Data, callback: F) -> Self
    where
        M: Into<Cow<'static, str>>,
        I: IntoIterator<Item = T>,
        F: Fn(&mut Editor, &T, PromptEvent) + Send + 'static,
    {
        let message = tui::text::Text::from(message.into()).into();
        let options: Arc<[T]> = options.into_iter().collect::<Vec<_>>().into();
        assert!(!options.is_empty());

        Self {
            message,
            options,
            data: Arc::new(data),
            callback: Box::new(callback),
            selection: 0,
            scroll: 0,
            menu_height: 0,
        }
    }

    fn adjust_scroll(&mut self) {
        self.scroll = helix_view::list_nav::ListViewport::new(
            self.options.len(),
            Some(self.selection),
            self.menu_height,
            self.scroll,
        )
        .scroll_to_selected();
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.options.len();
        self.selection = (self.selection as isize + delta).rem_euclid(len as isize) as usize;
        self.adjust_scroll();
    }

    fn prepare_model(&mut self, area: Rect, cx: &RenderContext) -> SelectRenderModel<T> {
        self.menu_height = self.options.len().min(10).min(area.height as usize);
        self.adjust_scroll();

        let theme = cx.theme();
        SelectRenderModel {
            message: Arc::clone(&self.message.contents),
            options: Arc::clone(&self.options),
            data: Arc::clone(&self.data),
            selection: self.selection,
            scroll: self.scroll,
            message_style: theme.get("ui.background").patch(theme.get("ui.text")),
            menu_styles: crate::ui::design::MenuStyles::from_theme(theme),
            accent_rail_color: theme.try_get("ui.text.focus").and_then(|style| style.fg),
            rounded_corners: cx.config().rounded_corners,
            render_menu_borders: cx.menu_border(),
        }
    }

    fn menu_size(&self, viewport: (u16, u16)) -> (u16, u16) {
        let mut max_lens = Vec::new();
        for option in self.options.iter() {
            let row = option.format(&self.data);
            if max_lens.len() < row.cells.len() {
                max_lens.resize(row.cells.len(), 0usize);
            }
            for (width, cell) in max_lens.iter_mut().zip(row.cells.iter()) {
                *width = (*width).max(cell.content.width());
            }
        }

        let height = self.options.len().min(10).min(viewport.1 as usize);
        let mut width = max_lens.iter().sum::<usize>() + max_lens.len() + 1;
        if self.options.len() > height {
            width += 1;
        }
        (width.min(viewport.0 as usize) as u16, height as u16)
    }
}

impl<T: Item> Component for Select<T> {
    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        let event = match event {
            Event::Key(event) => *event,
            _ => return EventResult::Ignored(None),
        };
        let close = Some(PostAction::PopLayer {
            model_layer: None,
            remember_picker: false,
        });

        if (event == key!(Tab) || event == shift!(Tab))
            && cx.editor.config().auto_completion
            && matches!(
                cx.editor.config().smart_tab,
                Some(SmartTabConfig {
                    enable: true,
                    supersede_menu: true,
                })
            )
        {
            return EventResult::Ignored(None);
        }

        match event {
            key!(Esc) | ctrl!('c') => {
                (self.callback)(cx.editor, &self.options[self.selection], PromptEvent::Abort);
                EventResult::Consumed(close)
            }
            shift!(Tab) | key!(Up) | ctrl!('p') => {
                self.move_selection(-1);
                (self.callback)(
                    cx.editor,
                    &self.options[self.selection],
                    PromptEvent::Update,
                );
                EventResult::Consumed(None)
            }
            key!(Tab) | key!(Down) | ctrl!('n') => {
                self.move_selection(1);
                (self.callback)(
                    cx.editor,
                    &self.options[self.selection],
                    PromptEvent::Update,
                );
                EventResult::Consumed(None)
            }
            key!(PageUp) | ctrl!('u') => {
                self.move_selection(-((self.menu_height / 2).max(1) as isize));
                (self.callback)(
                    cx.editor,
                    &self.options[self.selection],
                    PromptEvent::Update,
                );
                EventResult::Consumed(None)
            }
            key!(PageDown) | ctrl!('d') => {
                self.move_selection((self.menu_height / 2).max(1) as isize);
                (self.callback)(
                    cx.editor,
                    &self.options[self.selection],
                    PromptEvent::Update,
                );
                EventResult::Consumed(None)
            }
            key!(Enter) => {
                (self.callback)(
                    cx.editor,
                    &self.options[self.selection],
                    PromptEvent::Validate,
                );
                EventResult::Consumed(close)
            }
            _ => {
                (self.callback)(cx.editor, &self.options[self.selection], PromptEvent::Abort);
                EventResult::Consumed(close)
            }
        }
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        let (message_width, message_height) = self.message.required_size(viewport).unwrap();
        let (menu_width, menu_height) = self.menu_size(viewport);
        self.menu_height = menu_height as usize;
        self.adjust_scroll();
        Some((
            menu_width.max(message_width + 2),
            message_height + menu_height + 2,
        ))
    }

    fn prepare_render(&mut self, area: Rect, cx: &RenderContext) -> crate::render::PreparedRender {
        let model = self.prepare_model(area, cx);
        crate::render::PreparedRender::deferred(move |cancellation| {
            let mut output = crate::render::RenderOutput::sparse(area);
            if !cancellation.is_cancelled() {
                paint_select(output.surface_mut(), area, &model);
            }
            output
        })
    }
}

fn paint_select<T: Item>(
    surface: &mut crate::render::CellSurface,
    viewport: Rect,
    model: &SelectRenderModel<T>,
) {
    let max_width = 80.min(((viewport.width as u32) * 80 / 100) as u16);
    let (message_width, message_height) = super::text::required_size(&model.message, max_width);
    let menu_height = model.options.len().min(10).min(viewport.height as usize) as u16;
    let width = message_width + 4;
    let height = message_height + 2 + menu_height;
    let area = Rect {
        x: (viewport.width / 2).saturating_sub(width / 2),
        y: (viewport.height / 2).saturating_sub(height / 2),
        width,
        height,
    };

    let message_box = area.with_height(message_height + 2);
    let message_inner = crate::widgets::Panel::framed(
        crate::widgets::PanelStyle::plain(model.message_style),
        model.rounded_corners,
    )
    .render(surface, message_box);
    let message_area = Rect::new(
        message_inner.x.saturating_add(1),
        message_inner.y,
        message_inner.width.saturating_sub(2),
        message_inner.height,
    );
    super::text::paint_text(surface, message_area, &model.message);

    let menu_area = area.clip_top(message_height + 2);
    paint_select_menu(surface, menu_area, model);
}

fn paint_select_menu<T: Item>(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    model: &SelectRenderModel<T>,
) {
    let ratatui_area = tui::ratatui::to_ratatui_rect(area);
    tui::ratatui::widgets::Widget::render(tui::ratatui::widgets::Clear, ratatui_area, surface);
    surface.set_style(
        ratatui_area,
        tui::ratatui::to_ratatui_style(model.menu_styles.background),
    );

    let mut widths = Vec::new();
    for option in model.options.iter() {
        let row = option.format(&model.data);
        if widths.len() < row.cells.len() {
            widths.resize(row.cells.len(), 0usize);
        }
        for (width, cell) in widths.iter_mut().zip(row.cells.iter()) {
            *width = (*width).max(cell.content.width());
        }
    }

    let len = model.options.len();
    let win_height = area.height as usize;
    if len == 0 || win_height == 0 {
        return;
    }
    let table_area = area.clip_left(1).clip_right(1);
    for (visible_row, option) in model
        .options
        .iter()
        .skip(model.scroll)
        .take(win_height)
        .enumerate()
    {
        let y = table_area.y + visible_row as u16;
        if y >= table_area.bottom() {
            break;
        }
        let is_selected = model.scroll + visible_row == model.selection;
        let row_area = Rect::new(area.x, y, area.width, 1);
        if is_selected {
            surface.set_style(
                tui::ratatui::to_ratatui_rect(row_area),
                tui::ratatui::to_ratatui_style(model.menu_styles.selected),
            );
        }

        let row = option.format(&model.data);
        let mut x = table_area.x;
        for (width, cell) in widths.iter().zip(row.cells.iter()) {
            if x >= table_area.right() {
                break;
            }
            let width = (*width as u16).min(table_area.right().saturating_sub(x));
            paint_select_cell(
                surface,
                cell,
                Rect::new(x, y, width, 1),
                is_selected.then_some(model.menu_styles.selected),
            );
            x = x.saturating_add(width).saturating_add(1);
        }

        if is_selected && !model.render_menu_borders {
            if let Some(cell) = surface.cell_mut((area.left(), y)) {
                let mut style = tui::ratatui::to_ratatui_style(model.menu_styles.selected);
                if let Some(fg) = model.accent_rail_color {
                    cell.set_symbol("▎");
                    style = style.fg(tui::ratatui::to_ratatui_color(fg));
                }
                cell.set_style(style);
            }
            if let Some(cell) = surface.cell_mut((area.right().saturating_sub(1), y)) {
                cell.set_style(tui::ratatui::to_ratatui_style(model.menu_styles.selected));
            }
        }
    }

    if len > win_height {
        let thumb_fg = model
            .menu_styles
            .scroll
            .fg
            .unwrap_or(helix_view::theme::Color::Reset);
        let mut scrollbar = crate::widgets::Scrollbar::new(len, model.scroll, win_height)
            .symbol(if model.render_menu_borders {
                "▌"
            } else {
                "▐"
            })
            .thumb_style(helix_view::graphics::Style::default().fg(thumb_fg));
        if !model.render_menu_borders {
            let track_fg = model
                .menu_styles
                .scroll
                .bg
                .unwrap_or(helix_view::theme::Color::Reset);
            scrollbar = scrollbar.track("▐", helix_view::graphics::Style::default().fg(track_fg));
        }
        scrollbar.render(
            Rect::new(area.right() - 1, area.top(), 1, area.height),
            surface,
        );
    }
}

fn paint_select_cell(
    surface: &mut crate::render::CellSurface,
    cell: &crate::widgets::TableCell<'_>,
    area: Rect,
    selected: Option<Style>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    for (row, spans) in cell.content.lines.iter().enumerate() {
        if row as u16 >= area.height {
            break;
        }
        if let Some(selected) = selected {
            let spans = Spans(
                spans
                    .0
                    .iter()
                    .map(|span| {
                        tui::text::Span::styled(span.content.clone(), span.style.patch(selected))
                    })
                    .collect(),
            );
            surface.set_line(
                area.x,
                area.y + row as u16,
                &tui::ratatui::to_ratatui_line(&spans),
                area.width,
            );
        } else {
            surface.set_line(
                area.x,
                area.y + row as u16,
                &tui::ratatui::to_ratatui_line(spans),
                area.width,
            );
        }
    }
}
