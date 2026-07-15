use std::{
    hash::{Hash, Hasher},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

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
use crate::ui::markdown::MarkdownRenderSource;
use crate::ui::Markdown;

pub struct Hover {
    active_index: usize,
    contents: Vec<(Option<Markdown>, Markdown)>,
    render_cache_id: crate::render::CacheId,
    content_revision: u64,
}

static NEXT_HOVER_RENDER_CACHE: AtomicU64 = AtomicU64::new(1);

impl Hover {
    pub const ID: &'static str = "hover";

    pub fn new(
        hovers: Vec<(String, lsp::Hover)>,
        config_loader: Arc<ArcSwap<syntax::Loader>>,
    ) -> Self {
        let n_hovers = hovers.len();
        let contents: Vec<(Option<Markdown>, Markdown)> = hovers
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

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for (header, body) in &contents {
            header
                .as_ref()
                .map(|header| &header.contents)
                .hash(&mut hasher);
            body.contents.hash(&mut hasher);
        }
        Self {
            active_index: usize::default(),
            contents,
            render_cache_id: crate::render::CacheId::hashed(&(
                "hover",
                NEXT_HOVER_RENDER_CACHE.fetch_add(1, Ordering::Relaxed),
            )),
            content_revision: hasher.finish(),
        }
    }

    fn has_header(&self) -> bool {
        self.contents.len() > 1
    }

    pub fn content_string(&self) -> String {
        self.contents
            .iter()
            .map(|(header, body)| {
                let header: String = header
                    .iter()
                    .map(|header| header.contents.as_ref())
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

struct HoverPaintSnapshot {
    area: Rect,
    header: Option<MarkdownRenderSource>,
    contents: MarkdownRenderSource,
    has_header: bool,
    theme: Arc<helix_view::Theme>,
    scroll: usize,
}

impl HoverPaintSnapshot {
    fn render(
        self,
        cancellation: &crate::render::RenderCancellation,
    ) -> crate::render::RenderOutput {
        use tui::ratatui::widgets::{Paragraph, Widget, Wrap};

        let mut output = crate::render::RenderOutput::sparse(self.area);
        let surface = output.surface_mut();
        let area = self.area.inner(Margin::all(1));
        if cancellation.is_cancelled() || area.width == 0 || area.height == 0 {
            return output;
        }

        if let Some(header) = self.header {
            let header = header.layout(area.width as usize, &self.theme);
            Paragraph::new(tui::ratatui::to_ratatui_text(&header)).render(
                tui::ratatui::to_ratatui_rect(area.with_height(HEADER_HEIGHT)),
                surface,
            );
            let divider_style = self
                .theme
                .try_get("ui.window")
                .or_else(|| self.theme.try_get("comment"))
                .unwrap_or_default();
            for x in area.left()..area.right() {
                if let Some(cell) = surface.cell_mut((x, area.top() + HEADER_HEIGHT)) {
                    cell.set_symbol("─");
                    cell.set_style(tui::ratatui::to_ratatui_style(divider_style));
                }
            }
        }

        if cancellation.is_cancelled() {
            return output;
        }
        let contents_area = area.clip_top(if self.has_header {
            HEADER_HEIGHT + SEPARATOR_HEIGHT
        } else {
            0
        });
        let contents = self
            .contents
            .layout(contents_area.width as usize, &self.theme);
        let (_, content_height) = crate::ui::text::required_size(&contents, contents_area.width);
        let needs_scrollbar = content_height > contents_area.height;
        let body_area = if needs_scrollbar {
            contents_area.clip_right(1)
        } else {
            contents_area
        };
        Paragraph::new(tui::ratatui::to_ratatui_text(&contents))
            .wrap(Wrap { trim: false })
            .scroll((self.scroll as u16, 0))
            .render(tui::ratatui::to_ratatui_rect(body_area), surface);

        if needs_scrollbar && contents_area.height > 0 && contents_area.width > 0 {
            let thumb_style = self
                .theme
                .try_get("ui.menu.scroll")
                .or_else(|| self.theme.try_get("ui.text.inactive"))
                .unwrap_or_default();
            crate::widgets::Scrollbar::new(
                content_height as usize,
                self.scroll,
                contents_area.height as usize,
            )
            .symbol("▌")
            .thumb_style(thumb_style)
            .render(
                Rect::new(
                    contents_area.right().saturating_sub(1),
                    contents_area.y,
                    1,
                    contents_area.height,
                ),
                surface,
            );
        }
        output
    }
}

impl Component for Hover {
    fn prepare_render(&mut self, area: Rect, cx: &RenderContext) -> crate::render::PreparedRender {
        let (header, contents) = &self.contents[self.active_index];
        let snapshot = HoverPaintSnapshot {
            area,
            header: header.as_ref().map(Markdown::render_source),
            contents: contents.render_source(),
            has_header: self.has_header(),
            theme: cx.theme_arc(),
            scroll: cx.scroll().unwrap_or_default(),
        };
        let tag = crate::render::CacheTag {
            id: self.render_cache_id,
            key: crate::render::CacheKey::hashed(&(
                self.content_revision,
                self.active_index,
                area.x,
                area.y,
                area.width,
                area.height,
                cx.theme_generation(),
                snapshot.scroll,
            )),
            area,
        };
        crate::render::PreparedRender::snapshot(tag, snapshot, |snapshot, cancellation| {
            snapshot.render(cancellation)
        })
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        let max_text_width = viewport.0.saturating_sub(PADDING_HORIZONTAL).clamp(10, 120);

        let has_header = self.has_header();
        let active_index = self.active_index;
        let (header, contents) = &self.contents[active_index];

        let header_width = header
            .as_ref()
            .map(|header| header.estimated_size(max_text_width, viewport.1).0)
            .unwrap_or_default();

        let (content_width, content_height) = contents.estimated_size(max_text_width, viewport.1);

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
