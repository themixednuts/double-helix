use std::borrow::Cow;

use helix_view::{graphics::Rect, Editor};

use crate::compositor::{Component, Context, Event, EventResult, RenderContext};

use super::{menu::Item, Menu, PromptEvent, Text};

pub struct Select<T: Item> {
    message: Text,
    options: Menu<T>,
}

impl<T: Item> Select<T> {
    pub fn new<M, I, F>(message: M, options: I, data: T::Data, callback: F) -> Self
    where
        M: Into<Cow<'static, str>>,
        I: IntoIterator<Item = T>,
        F: Fn(&mut Editor, &T, PromptEvent) + Send + 'static,
    {
        let message = tui::text::Text::from(message.into()).into();
        let options: Vec<_> = options.into_iter().collect();
        assert!(!options.is_empty());
        let mut menu = Menu::new(options, data, move |editor, option, event| {
            // Options are non-empty (asserted above) and an option is selected by default,
            // so `option` must be Some here.
            let option = &option.unwrap();
            callback(editor, option, event)
        })
        .auto_close(true);
        // Select the first option by default.
        menu.move_down();

        Self {
            message,
            options: menu,
        }
    }

    fn render_surface<FM, FO>(
        &mut self,
        area: Rect,
        surface: &mut crate::render::CellSurface,
        cx: &RenderContext,
        render_message: FM,
        render_options: FO,
    ) where
        FM: FnOnce(&mut Text, Rect, &mut crate::render::CellSurface, &RenderContext),
        FO: FnOnce(&mut Menu<T>, Rect, &mut crate::render::CellSurface, &RenderContext),
    {
        let max_width = 80.min(((area.width as u32) * 80u32 / 100) as u16);
        let (message_width, message_height) =
            super::text::required_size(&self.message.contents, max_width);
        let (_, menu_height) = self
            .options
            .required_size((max_width, area.height))
            .unwrap();
        let width = message_width + 4;
        let height = message_height + 2 + menu_height;
        let area = Rect {
            x: (area.width / 2).saturating_sub(width / 2),
            y: (area.height / 2).saturating_sub(height / 2),
            width,
            height,
        };

        let background = cx.theme().get("ui.background");
        let text = cx.theme().get("ui.text");
        let message_style = background.patch(text);
        let message_box = area.with_height(message_height + 2);
        let message_inner = crate::widgets::Panel::framed(
            crate::widgets::PanelStyle::plain(message_style),
            cx.config().rounded_corners,
        )
        .render(surface, message_box);

        let message_area = Rect::new(
            message_inner.x.saturating_add(1),
            message_inner.y,
            message_inner.width.saturating_sub(2),
            message_inner.height,
        );
        render_message(&mut self.message, message_area, surface, cx);

        let menu_area = area.clip_top(message_height + 2);
        render_options(&mut self.options, menu_area, surface, cx);
    }
}

impl<T: Item> Component for Select<T> {
    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        self.options.handle_event(event, cx)
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        let (message_width, message_height) = self.message.required_size(viewport).unwrap();
        let (menu_width, menu_height) = self.options.required_size(viewport).unwrap();
        Some((
            menu_width.max(message_width + 2),
            message_height + menu_height + 2,
        ))
    }

    fn render(&mut self, area: Rect, surface: &mut crate::render::CellSurface, cx: &RenderContext) {
        self.render_surface(
            area,
            surface,
            cx,
            |message, area, surface, cx| message.render(area, surface, cx),
            |options, area, surface, cx| options.render(area, surface, cx),
        );
    }
}
