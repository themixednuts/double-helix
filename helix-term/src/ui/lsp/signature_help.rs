use std::sync::Arc;

use arc_swap::ArcSwap;
use helix_core::syntax::{self, OverlayHighlights};
use helix_view::graphics::{Margin, Rect};
use helix_view::input::Event;

use helix_lsp::lsp::{self, SignatureInformation};
use helix_runtime::send_blocking;
use helix_view::document::Mode;
use helix_view::handlers::lsp::{SignatureHelpEvent, SignatureHelpInvoked, SignatureHelpRequestId};
use helix_view::Editor;

use crate::compositor::{Component, Compositor, Context, EventResult, RenderContext};

use crate::alt;
use crate::commands::Open;
use crate::ui;
use crate::ui::Markdown;

use crate::ui::Popup;

pub struct Signature {
    pub signature: String,
    pub signature_doc: Option<String>,
    /// Part of signature text
    pub active_param_range: Option<(usize, usize)>,
}

pub struct SignatureHelp {
    language: String,
    config_loader: Arc<ArcSwap<syntax::Loader>>,
    active_signature: usize,
    lsp_signature: Option<usize>,
    signatures: Vec<Signature>,
}

impl SignatureHelp {
    pub const ID: &'static str = "signature-help";

    pub fn new(
        language: String,
        config_loader: Arc<ArcSwap<syntax::Loader>>,
        active_signature: usize,
        lsp_signature: Option<usize>,
        signatures: Vec<Signature>,
    ) -> Self {
        Self {
            language,
            config_loader,
            active_signature,
            lsp_signature,
            signatures,
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

    fn render(&mut self, area: Rect, surface: &mut crate::render::CellSurface, cx: &RenderContext) {
        use tui::ratatui::{
            layout::Alignment,
            widgets::{Paragraph, Widget, Wrap},
        };

        let margin = Margin::all(1);
        let area = area.inner(margin);

        let signature = self
            .signatures
            .get(self.active_signature)
            .unwrap_or_else(|| &self.signatures[0]);
        let theme = cx.theme();

        let active_param_span = signature.active_param_range.map(|(start, end)| {
            let highlight = theme.find_highlight_exact("ui.selection").unwrap();
            OverlayHighlights::single(highlight, start..end)
        });

        let signature = self
            .signatures
            .get(self.active_signature)
            .unwrap_or_else(|| &self.signatures[0]);

        let sig_text = crate::ui::markdown::highlighted_code_block(
            signature.signature.as_str(),
            &self.language,
            Some(theme),
            &self.config_loader.load(),
            active_param_span,
        );

        if self.signatures.len() > 1 {
            let signature_index = self.signature_index();
            let paragraph = Paragraph::new(signature_index).alignment(Alignment::Right);
            paragraph.render(
                tui::ratatui::to_ratatui_rect(area.with_height(1).clip_right(1)),
                surface,
            );
        }

        let sig_text_height = crate::ui::text::required_size(&sig_text, area.width).1;
        let sig_text_area = area.with_height(sig_text_height.min(area.height));
        let sig_text_area =
            sig_text_area.intersection(tui::ratatui::to_helix_rect(*surface.area()));
        let sig_text = tui::ratatui::to_ratatui_text(&sig_text);
        let sig_text_para = Paragraph::new(sig_text)
            .wrap(Wrap { trim: false })
            .scroll((cx.scroll().unwrap_or_default() as u16, 0));
        sig_text_para.render(tui::ratatui::to_ratatui_rect(sig_text_area), surface);

        if signature.signature_doc.is_none() {
            return;
        }

        // Theme the divider between the signature line and its doc
        // — matches the hover popup polish (same theme key fallback
        // chain). `Style::default()` was uncolored so the line read
        // as a stray character on tinted popup backgrounds.
        let divider_style = theme
            .try_get("ui.window")
            .or_else(|| theme.try_get("comment"))
            .unwrap_or_default();
        for x in sig_text_area.left()..sig_text_area.right() {
            if let Some(cell) = surface.cell_mut((x, sig_text_area.bottom())) {
                cell.set_symbol("─");
                cell.set_style(tui::ratatui::to_ratatui_style(divider_style));
            }
        }

        let sig_doc = match &signature.signature_doc {
            None => return,
            Some(doc) => Markdown::new(doc.clone(), Arc::clone(&self.config_loader)),
        };
        let sig_doc = sig_doc.parse(Some(theme));
        let sig_doc = tui::ratatui::to_ratatui_text(&sig_doc);
        let sig_doc_area = area
            .clip_top(sig_text_area.height + 2)
            .clip_bottom(u16::from(cx.popup_border()));
        let sig_doc_para = Paragraph::new(sig_doc)
            .wrap(Wrap { trim: false })
            .scroll((cx.scroll().unwrap_or_default() as u16, 0));
        sig_doc_para.render(tui::ratatui::to_ratatui_rect(sig_doc_area), surface);
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        const PADDING: u16 = 2;
        const SEPARATOR_HEIGHT: u16 = 1;

        let signature = self
            .signatures
            .get(self.active_signature)
            .unwrap_or_else(|| &self.signatures[0]);

        let max_text_width = viewport.0.saturating_sub(PADDING).clamp(10, 120);

        let signature_text = crate::ui::markdown::highlighted_code_block(
            signature.signature.as_str(),
            &self.language,
            None,
            &self.config_loader.load(),
            None,
        );
        let (sig_width, sig_height) =
            crate::ui::text::required_size(&signature_text, max_text_width);

        let (width, height) = match signature.signature_doc {
            Some(ref doc) => {
                let doc_md = Markdown::new(doc.clone(), Arc::clone(&self.config_loader));
                let doc_text = doc_md.parse(None);
                let (doc_width, doc_height) =
                    crate::ui::text::required_size(&doc_text, max_text_width);
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
    let config = &editor.config();

    if !(config.lsp.auto_signature_help
        || SignatureHelp::visible_popup(compositor).is_some()
        || invoked == SignatureHelpInvoked::Manual)
    {
        return;
    }

    if invoked == SignatureHelpInvoked::Automatic && editor.mode != Mode::Insert {
        return;
    }

    let response = match response {
        Some(s) if !s.signatures.is_empty() => s,
        _ => {
            send_blocking(
                editor.signature_help_sender(),
                SignatureHelpEvent::RequestComplete {
                    request,
                    open: false,
                },
            );
            compositor.remove(SignatureHelp::ID);
            return;
        }
    };
    send_blocking(
        editor.signature_help_sender(),
        SignatureHelpEvent::RequestComplete {
            request,
            open: true,
        },
    );

    let (_, doc) = focused_ref!(editor);
    let language = doc.language_name().unwrap_or("");

    if response.signatures.is_empty() {
        return;
    }

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
                signature: s.label,
                signature_doc,
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
        return;
    }

    compositor.replace_or_push(SignatureHelp::ID, popup);
}
