use std::{
    hash::{Hash, Hasher},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use arc_swap::ArcSwap;
use helix_core::syntax::{self, OverlayHighlights};
use helix_view::graphics::{Margin, Rect};
use helix_view::input::Event;

use helix_lsp::lsp::{self, SignatureInformation};
use helix_view::document::Mode;
use helix_view::handlers::lsp::{SignatureHelpEvent, SignatureHelpInvoked, SignatureHelpRequestId};
use helix_view::Editor;

use crate::compositor::{Component, Compositor, Context, EventResult, RenderContext};

use crate::alt;
use crate::commands::Open;
use crate::ui;
use crate::ui::markdown::MarkdownRenderSource;

use crate::ui::Popup;

pub struct Signature {
    pub signature: Arc<str>,
    pub signature_doc: Option<Arc<str>>,
    /// Part of signature text
    pub active_param_range: Option<(usize, usize)>,
}

pub struct SignatureHelp {
    language: Arc<str>,
    config_loader: Arc<ArcSwap<syntax::Loader>>,
    active_signature: usize,
    lsp_signature: Option<usize>,
    signatures: Vec<Signature>,
    render_cache_id: crate::render::CacheId,
    content_revision: u64,
}

static NEXT_SIGNATURE_RENDER_CACHE: AtomicU64 = AtomicU64::new(1);

impl SignatureHelp {
    pub const ID: &'static str = "signature-help";

    pub fn new(
        language: String,
        config_loader: Arc<ArcSwap<syntax::Loader>>,
        active_signature: usize,
        lsp_signature: Option<usize>,
        signatures: Vec<Signature>,
    ) -> Self {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        language.hash(&mut hasher);
        for signature in &signatures {
            signature.signature.hash(&mut hasher);
            signature.signature_doc.hash(&mut hasher);
            signature.active_param_range.hash(&mut hasher);
        }
        Self {
            language: Arc::from(language),
            config_loader,
            active_signature,
            lsp_signature,
            signatures,
            render_cache_id: crate::render::CacheId::hashed(&(
                "signature-help",
                NEXT_SIGNATURE_RENDER_CACHE.fetch_add(1, Ordering::Relaxed),
            )),
            content_revision: hasher.finish(),
        }
    }

    pub fn active_signature(&self) -> usize {
        self.active_signature
    }

    pub fn lsp_signature(&self) -> Option<usize> {
        self.lsp_signature
    }

    pub fn visible_popup(compositor: &mut Compositor) -> Option<&mut Popup<Self>> {
        compositor.find_id::<Popup<Self>>(Self::ID)
    }

    fn signature_index(&self) -> String {
        format!("({}/{})", self.active_signature + 1, self.signatures.len())
    }
}

struct SignaturePaintSnapshot {
    area: Rect,
    signature: Arc<str>,
    signature_doc: Option<Arc<str>>,
    active_param_range: Option<(usize, usize)>,
    language: Arc<str>,
    loader: Arc<syntax::Loader>,
    theme: Arc<helix_view::Theme>,
    signature_index: Option<String>,
    scroll: u16,
    popup_border: bool,
}

impl SignaturePaintSnapshot {
    fn render(
        self,
        cancellation: &crate::render::RenderCancellation,
    ) -> crate::render::RenderOutput {
        use tui::ratatui::{
            layout::Alignment,
            widgets::{Paragraph, Widget, Wrap},
        };

        let mut output = crate::render::RenderOutput::sparse(self.area);
        let surface = output.surface_mut();
        let area = self.area.inner(Margin::all(1));
        if cancellation.is_cancelled() || area.width == 0 || area.height == 0 {
            return output;
        }

        let active_param_span = self.active_param_range.map(|(start, end)| {
            let highlight = self.theme.find_highlight_exact("ui.selection").unwrap();
            OverlayHighlights::single(highlight, start..end)
        });
        let signature_text = crate::ui::markdown::highlighted_code_block(
            &self.signature,
            &self.language,
            Some(&self.theme),
            &self.loader,
            active_param_span,
        );

        if let Some(index) = self.signature_index {
            Paragraph::new(index).alignment(Alignment::Right).render(
                tui::ratatui::to_ratatui_rect(area.with_height(1).clip_right(1)),
                surface,
            );
        }
        let signature_height = crate::ui::text::required_size(&signature_text, area.width).1;
        let signature_area = area
            .with_height(signature_height.min(area.height))
            .intersection(tui::ratatui::to_helix_rect(*surface.area()));
        Paragraph::new(tui::ratatui::to_ratatui_text(&signature_text))
            .wrap(Wrap { trim: false })
            .scroll((self.scroll, 0))
            .render(tui::ratatui::to_ratatui_rect(signature_area), surface);

        let Some(documentation) = self.signature_doc else {
            return output;
        };
        if cancellation.is_cancelled() {
            return output;
        }
        let divider_style = self
            .theme
            .try_get("ui.window")
            .or_else(|| self.theme.try_get("comment"))
            .unwrap_or_default();
        for x in signature_area.left()..signature_area.right() {
            if let Some(cell) = surface.cell_mut((x, signature_area.bottom())) {
                cell.set_symbol("─");
                cell.set_style(tui::ratatui::to_ratatui_style(divider_style));
            }
        }

        let doc_area = area
            .clip_top(signature_area.height + 2)
            .clip_bottom(u16::from(self.popup_border));
        let documentation = MarkdownRenderSource::new(documentation, self.loader)
            .layout(doc_area.width as usize, &self.theme);
        Paragraph::new(tui::ratatui::to_ratatui_text(&documentation))
            .wrap(Wrap { trim: false })
            .scroll((self.scroll, 0))
            .render(tui::ratatui::to_ratatui_rect(doc_area), surface);
        output
    }
}

impl Component for SignatureHelp {
    fn handle_event(&mut self, event: &Event, _cx: &mut Context) -> EventResult {
        let Event::Key(event) = event else {
            return EventResult::Ignored(None);
        };

        if self.signatures.len() <= 1 {
            return EventResult::Ignored(None);
        }

        match event {
            alt!('p') => {
                self.active_signature = self
                    .active_signature
                    .checked_sub(1)
                    .unwrap_or(self.signatures.len() - 1);
                EventResult::Consumed(None)
            }
            alt!('n') => {
                self.active_signature = (self.active_signature + 1) % self.signatures.len();
                EventResult::Consumed(None)
            }
            _ => EventResult::Ignored(None),
        }
    }

    fn prepare_render(&mut self, area: Rect, cx: &RenderContext) -> crate::render::PreparedRender {
        let signature = self
            .signatures
            .get(self.active_signature)
            .unwrap_or_else(|| &self.signatures[0]);
        let snapshot = SignaturePaintSnapshot {
            area,
            signature: Arc::clone(&signature.signature),
            signature_doc: signature.signature_doc.as_ref().map(Arc::clone),
            active_param_range: signature.active_param_range,
            language: Arc::clone(&self.language),
            loader: self.config_loader.load_full(),
            theme: cx.theme_arc(),
            signature_index: (self.signatures.len() > 1).then(|| self.signature_index()),
            scroll: cx.scroll().unwrap_or_default() as u16,
            popup_border: cx.popup_border(),
        };
        let tag = crate::render::CacheTag {
            id: self.render_cache_id,
            key: crate::render::CacheKey::hashed(&(
                self.content_revision,
                self.active_signature,
                area.x,
                area.y,
                area.width,
                area.height,
                cx.theme_generation(),
                snapshot.scroll,
                snapshot.popup_border,
            )),
            area,
        };
        crate::render::PreparedRender::snapshot(tag, snapshot, |snapshot, cancellation| {
            snapshot.render(cancellation)
        })
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        const PADDING: u16 = 2;
        const SEPARATOR_HEIGHT: u16 = 1;

        let signature = self
            .signatures
            .get(self.active_signature)
            .unwrap_or_else(|| &self.signatures[0]);

        let max_text_width = viewport.0.saturating_sub(PADDING).clamp(10, 120);

        let (sig_width, sig_height) =
            estimated_wrapped_size(&signature.signature, max_text_width, viewport.1);

        let (width, height) = match &signature.signature_doc {
            Some(doc) => {
                let (doc_width, doc_height) =
                    estimated_wrapped_size(doc, max_text_width, viewport.1);
                (
                    sig_width.max(doc_width),
                    sig_height + SEPARATOR_HEIGHT + doc_height,
                )
            }
            None => (sig_width, sig_height),
        };

        let sig_index_width = if self.signatures.len() > 1 {
            self.signature_index().len() + 1
        } else {
            0
        };

        Some((width + PADDING + sig_index_width as u16, height + PADDING))
    }
}

fn estimated_wrapped_size(text: &str, max_width: u16, max_height: u16) -> (u16, u16) {
    let max_width = max_width.max(1) as usize;
    let mut width = 0u16;
    let mut height = 0u16;
    for line in text.lines() {
        let line_width = helix_core::unicode::width::UnicodeWidthStr::width(line);
        width = width.max(line_width.min(max_width) as u16);
        height = height
            .saturating_add(line_width.max(1).div_ceil(max_width) as u16)
            .min(max_height);
        if height == max_height {
            break;
        }
    }
    (width, height)
}

fn active_param_range(
    signature: &SignatureInformation,
    response_active_parameter: Option<u32>,
) -> Option<(usize, usize)> {
    let param_idx = signature
        .active_parameter
        .or(response_active_parameter)
        .unwrap_or(0) as usize;
    let param = signature.parameters.as_ref()?.get(param_idx)?;
    match &param.label {
        lsp::ParameterLabel::Simple(string) => {
            let start = signature.label.find(string.as_str())?;
            Some((start, start + string.len()))
        }
        lsp::ParameterLabel::LabelOffsets([start, end]) => {
            use helix_core::str_utils::char_to_byte_idx;
            let from = char_to_byte_idx(&signature.label, *start as usize);
            let to = char_to_byte_idx(&signature.label, *end as usize);
            Some((from, to))
        }
    }
}

/// Apply LSP signature help on the main thread (invoked from typed runtime ingress / [`crate::runtime::UiCommand::Lsp`]).
pub(crate) fn show_signature(
    editor: &mut Editor,
    compositor: &mut Compositor,
    invoked: SignatureHelpInvoked,
    request: SignatureHelpRequestId,
    response: Option<lsp::SignatureHelp>,
) {
    let complete = |editor: &Editor, open| {
        editor
            .signature_help_sender()
            .send(SignatureHelpEvent::RequestComplete { request, open });
    };
    let config = &editor.config();

    if !(config.lsp.auto_signature_help
        || SignatureHelp::visible_popup(compositor).is_some()
        || invoked == SignatureHelpInvoked::Manual)
    {
        complete(editor, false);
        return;
    }

    if invoked == SignatureHelpInvoked::Automatic && editor.mode != Mode::Insert {
        complete(editor, false);
        return;
    }

    let response = match response {
        Some(s) if !s.signatures.is_empty() => s,
        _ => {
            complete(editor, false);
            compositor.remove(SignatureHelp::ID);
            return;
        }
    };

    let (_, doc) = focused_ref!(editor);
    let language = doc.language_name().unwrap_or("");

    let signatures: Vec<Signature> = response
        .signatures
        .into_iter()
        .map(|s| {
            let active_param_range = active_param_range(&s, response.active_parameter);

            let signature_doc = if config.lsp.display_signature_help_docs {
                s.documentation.map(|doc| match doc {
                    lsp::Documentation::String(s) => s,
                    lsp::Documentation::MarkupContent(markup) => markup.value,
                })
            } else {
                None
            };

            Signature {
                signature: Arc::from(s.label),
                signature_doc: signature_doc.map(Arc::from),
                active_param_range,
            }
        })
        .collect();

    let old_popup = compositor.find_id::<Popup<SignatureHelp>>(SignatureHelp::ID);
    let lsp_signature = response.active_signature.map(|s| s as usize);

    let active_signature = old_popup
        .as_ref()
        .map(|popup| {
            let old_lsp_sig = popup.contents().lsp_signature();
            let old_sig = popup
                .contents()
                .active_signature()
                .min(signatures.len() - 1);

            if old_lsp_sig != lsp_signature {
                lsp_signature.unwrap_or(old_sig)
            } else {
                old_sig
            }
        })
        .unwrap_or(lsp_signature.unwrap_or_default());

    let contents = SignatureHelp::new(
        language.to_string(),
        Arc::clone(&editor.syn_loader),
        active_signature,
        lsp_signature,
        signatures,
    );

    let position_bias =
        Open::from_signature_help_position(&editor.config().lsp.signature_help_position);

    let mut popup = Popup::new(SignatureHelp::ID, contents)
        .position(old_popup.and_then(|p| p.get_position()))
        .position_bias(position_bias)
        .ignore_escape_key(true);

    let size = compositor.size();
    if compositor
        .find::<ui::EditorView>()
        .unwrap()
        .completion
        .as_mut()
        .map(|completion| completion.area(size, editor))
        .filter(|area| area.intersects(popup.area(size, editor)))
        .is_some()
    {
        complete(editor, false);
        return;
    }

    compositor.replace_or_push(SignatureHelp::ID, popup);
    complete(editor, true);
}
