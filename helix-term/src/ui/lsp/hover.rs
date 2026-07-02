use std::sync::Arc;

use arc_swap::ArcSwap;
use helix_core::{syntax, Rope};
use helix_lsp::lsp;
use helix_view::editor::Action;
use helix_view::graphics::{Margin, Rect};
use helix_view::input::Event;
use helix_view::{Document, Editor};

use crate::compositor::{Component, Compositor, Context, EventResult, RenderContext};
use crate::runtime::ui::command::LspHoverDisplay;
use crate::ui::Popup;

use crate::alt;
use crate::ui::Markdown;

pub struct Hover {
    active_index: usize,
    contents: Vec<(Option<Markdown>, Markdown)>,
}

impl Hover {
    pub const ID: &'static str = "hover";

    pub fn new(
        hovers: Vec<(String, lsp::Hover)>,
        config_loader: Arc<ArcSwap<syntax::Loader>>,
    ) -> Self {
        let n_hovers = hovers.len();
        let contents = hovers
            .into_iter()
            .enumerate()
            .map(|(idx, (server_name, hover))| {
                let header = (n_hovers > 1)
                    .then(|| format!("**[{}/{}] {}**\n", idx + 1, n_hovers, server_name))
                    .map(|h| Markdown::new(h, Arc::clone(&config_loader)));

                let body = Markdown::new(
                    hover_contents_to_string(hover.contents),
                    Arc::clone(&config_loader),
                );

                (header, body)
            })
            .collect();

        Self {
            active_index: usize::default(),
            contents,
        }
    }

    fn has_header(&self) -> bool {
        self.contents.len() > 1
    }

    fn content_markdown(&self) -> &(Option<Markdown>, Markdown) {
        &self.contents[self.active_index]
    }

    pub fn content_string(&self) -> String {
        self.contents
            .iter()
            .map(|(header, body)| {
                let header: String = header
                    .iter()
                    .map(|header| header.contents.clone())
                    .collect();

                format!("{}{}", header, body.contents)
            })
            .collect::<Vec<String>>()
            .join("\n\n---\n\n")
            + "\n"
    }

    fn set_index(&mut self, index: usize) {
        assert!((0..self.contents.len()).contains(&index));
        self.active_index = index;
    }
}

const PADDING_HORIZONTAL: u16 = 2;
const PADDING_TOP: u16 = 1;
const PADDING_BOTTOM: u16 = 1;
const HEADER_HEIGHT: u16 = 1;
const SEPARATOR_HEIGHT: u16 = 1;

impl Component for Hover {
    fn render(&mut self, area: Rect, surface: &mut crate::render::CellSurface, cx: &RenderContext) {
        use tui::ratatui::widgets::{Paragraph, Widget, Wrap};

        let margin = Margin::all(1);
        let area = area.inner(margin);

        let theme = cx.theme();
        let has_header = self.has_header();
        let active_index = self.active_index;
        let (header, contents) = &mut self.contents[active_index];

        if let Some(header) = header {
            let header = header.layout(area.width as usize, Some(theme));
            let header = tui::ratatui::to_ratatui_text(&header);
            let header = Paragraph::new(header);
            header.render(
                tui::ratatui::to_ratatui_rect(area.with_height(HEADER_HEIGHT)),
                surface,
            );

            // Theme the divider between the LSP-name header (e.g.
            // "[1/3] rust-analyzer") and the hover body. `Style::default()`
            // was uncolored, making the line look like a stray character
            // when the popup background was tinted. `comment` (or
            // `ui.window`) gives a subtle visual separator that matches
            // the rest of the chrome.
            let divider_style = theme
                .try_get("ui.window")
                .or_else(|| theme.try_get("comment"))
                .unwrap_or_default();
            for x in area.left()..area.right() {
                if let Some(cell) = surface.cell_mut((x, area.top() + HEADER_HEIGHT)) {
                    cell.set_symbol("─");
                    cell.set_style(tui::ratatui::to_ratatui_style(divider_style));
                }
            }
        }

        let contents_area = area.clip_top(if has_header {
            HEADER_HEIGHT + SEPARATOR_HEIGHT
        } else {
            0
        });
        let contents_parsed = contents.layout(contents_area.width as usize, Some(theme));
        // Re-measure the content height so we can decide whether to
        // draw a scrollbar. The `Paragraph` widget itself doesn't
        // expose this — we use the same `required_size` helper the
        // hover's `required_size()` uses to lay out the popup. The
        // popup's actual area may be smaller than the requested
        // size (clamped to viewport), so a scrollbar appears when
        // the content was clipped.
        let (_, content_height) =
            crate::ui::text::required_size(&contents_parsed, contents_area.width);
        let scroll_pos = cx.scroll().unwrap_or_default();
        let needs_scrollbar = content_height > contents_area.height;
        // Reserve the rightmost column for the scrollbar when needed
        // so content doesn't render under it.
        let body_area = if needs_scrollbar {
            contents_area.clip_right(1)
        } else {
            contents_area
        };

        let contents_text = tui::ratatui::to_ratatui_text(&contents_parsed);
        let contents_para = Paragraph::new(contents_text)
            .wrap(Wrap { trim: false })
            .scroll((scroll_pos as u16, 0));
        contents_para.render(tui::ratatui::to_ratatui_rect(body_area), surface);

        // Render the scrollbar. Uses `ui.menu.scroll` (the same scope
        // the picker uses) so the visual language stays consistent.
        // Falls back through `ui.text.inactive` → default style so
        // even an unconfigured theme yields something visible.
        if needs_scrollbar && contents_area.height > 0 {
            let thumb_style = theme
                .try_get("ui.menu.scroll")
                .or_else(|| theme.try_get("ui.text.inactive"))
                .unwrap_or_default();
            let scrollbar_x = contents_area.right().saturating_sub(1);
            let scrollbar_area = Rect::new(scrollbar_x, contents_area.y, 1, contents_area.height);
            crate::widgets::Scrollbar::new(
                content_height as usize,
                scroll_pos,
                contents_area.height as usize,
            )
            .symbol("▌")
            .thumb_style(thumb_style)
            .render(scrollbar_area, surface);
        }
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        let max_text_width = viewport.0.saturating_sub(PADDING_HORIZONTAL).clamp(10, 120);

        let has_header = self.has_header();
        let active_index = self.active_index;
        let (header, contents) = &mut self.contents[active_index];

        let header_width = header
            .as_mut()
            .map(|header| {
                let header = header.layout(max_text_width as usize, None);
                let (width, _height) = crate::ui::text::required_size(&header, max_text_width);
                width
            })
            .unwrap_or_default();

        let contents = contents.layout(max_text_width as usize, None);
        let (content_width, content_height) =
            crate::ui::text::required_size(&contents, max_text_width);

        let width = PADDING_HORIZONTAL + header_width.max(content_width);
        let height = if has_header {
            PADDING_TOP + HEADER_HEIGHT + SEPARATOR_HEIGHT + content_height + PADDING_BOTTOM
        } else {
            PADDING_TOP + content_height + PADDING_BOTTOM
        };

        Some((width, height))
    }

    fn handle_event(&mut self, event: &Event, _ctx: &mut Context) -> EventResult {
        let Event::Key(event) = event else {
            return EventResult::Ignored(None);
        };

        match event {
            alt!('p') => {
                let index = self
                    .active_index
                    .checked_sub(1)
                    .unwrap_or(self.contents.len() - 1);
                self.set_index(index);
                EventResult::Consumed(None)
            }
            alt!('n') => {
                self.set_index((self.active_index + 1) % self.contents.len());
                EventResult::Consumed(None)
            }
            _ => EventResult::Ignored(None),
        }
    }
}

/// Apply [`crate::runtime::ui::command::LspCommand::Hover`] on the main thread.
pub fn show_hover(
    editor: &mut Editor,
    compositor: &mut Compositor,
    hovers: Vec<(String, lsp::Hover)>,
    display: LspHoverDisplay,
) {
    if hovers.is_empty() {
        editor.set_status("No hover results available.");
        return;
    }

    let hover = Hover::new(hovers, editor.syn_loader.clone());

    match display {
        LspHoverDisplay::Popup => {
            let popup = Popup::new(Hover::ID, hover).auto_close(true);
            compositor.replace_or_push(Hover::ID, popup);
        }
        LspHoverDisplay::FileBuffer => {
            editor.new_file_from_document(
                Action::Replace,
                Document::from(
                    Rope::from(hover.content_string()),
                    None,
                    Arc::clone(&editor.config),
                    Arc::clone(&editor.syn_loader),
                ),
            );
            let (_, hover_doc) = focused!(editor);

            let _ = hover_doc.set_language_by_language_id("markdown", &editor.syn_loader.load());
        }
    }
}

fn hover_contents_to_string(contents: lsp::HoverContents) -> String {
    fn marked_string_to_markdown(contents: lsp::MarkedString) -> String {
        match contents {
            lsp::MarkedString::String(contents) => contents,
            lsp::MarkedString::LanguageString(string) => {
                if string.language == "markdown" {
                    string.value
                } else {
                    format!("```{}\n{}\n```", string.language, string.value)
                }
            }
        }
    }
    match contents {
        lsp::HoverContents::Scalar(contents) => marked_string_to_markdown(contents),
        lsp::HoverContents::Array(contents) => contents
            .into_iter()
            .map(marked_string_to_markdown)
            .collect::<Vec<_>>()
            .join("\n\n"),
        lsp::HoverContents::Markup(contents) => contents.value,
    }
}
