use crate::compositor::{Component, Context, Event, EventResult, PostAction, RenderContext};
use crate::{alt, ctrl, key, shift};
use arc_swap::ArcSwap;
use helix_core::syntax;
use helix_view::document::Mode;
use helix_view::input::KeyEvent;
use helix_view::keyboard::KeyCode;
use std::sync::Arc;
use std::{borrow::Cow, ops::RangeFrom};
use tui::text::Span;

pub(crate) mod completion;
mod documentation;
pub use completion::CompletionRequest;
use documentation::DocumentationService;

use helix_core::{
    unicode::segmentation::{GraphemeCursor, UnicodeSegmentation},
    unicode::width::UnicodeWidthStr,
    Position,
};
use helix_view::{
    editor::CmdlineStyle,
    graphics::{CursorKind, Rect},
    Editor,
};

type PromptCharHandler = Box<dyn Fn(&mut Prompt, char, &Context) + Send>;

pub type Completion = (RangeFrom<usize>, Span<'static>);
type CallbackFn = Box<dyn FnMut(&mut Context, &str, PromptEvent) + Send>;
pub type DocFn = Arc<dyn for<'a> Fn(&'a str) -> Option<Cow<'a, str>> + Send + Sync>;

pub trait CompletionProvider: Send {
    fn capture(&mut self, editor: &Editor, input: Arc<str>) -> CompletionRequest;
}

pub struct Prompt {
    completion_id: completion::PromptId,
    completion_generation: u64,
    completion_session: completion::CompletionSession,
    completion_pipeline: completion::CompletionPipeline,
    pending_completion_request: Option<completion::CompletionRequest>,
    prompt: Cow<'static, str>,
    line: String,
    cursor: usize,
    // Fields used for Component callbacks and rendering:
    line_area: Rect,
    anchor: usize,
    truncate_start: bool,
    truncate_end: bool,
    /// Last cursor screen position computed during render (used by cursor()).
    last_cursor_pos: (u16, u16),
    // ---
    completion: Vec<Completion>,
    completion_max_width: u16,
    selection: Option<usize>,
    history_register: Option<char>,
    history_pos: Option<usize>,
    completion_provider: Box<dyn CompletionProvider>,
    callback_fn: CallbackFn,
    pub doc_fn: DocFn,
    documentation_service: Option<DocumentationService>,
    next_char_handler: Option<PromptCharHandler>,
    language: Option<(&'static str, Arc<ArcSwap<syntax::Loader>>)>,
    /// Model layer ID, set when this prompt is pushed to the layer stack.
    model_layer_id: Option<helix_view::model::LayerId>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PromptEvent {
    /// The prompt input has been updated.
    Update,
    /// Validate and finalize the change.
    Validate,
    /// Abort the change, reverting to the initial state.
    Abort,
}

pub enum CompletionDirection {
    Forward,
    Backward,
}

#[derive(Debug, Clone, Copy)]
pub enum Movement {
    BackwardChar(usize),
    BackwardWord(usize),
    ForwardChar(usize),
    ForwardWord(usize),
    StartOfLine,
    EndOfLine,
    None,
}

fn is_word_sep(c: char) -> bool {
    c == std::path::MAIN_SEPARATOR || c.is_whitespace()
}

impl Prompt {
    pub fn new(
        prompt: Cow<'static, str>,
        history_register: Option<char>,
        completion_provider: impl CompletionProvider + 'static,
        callback_fn: impl FnMut(&mut Context, &str, PromptEvent) + Send + 'static,
    ) -> Self {
        Self {
            completion_id: completion::PromptId::next(),
            completion_generation: 0,
            completion_session: completion::CompletionSession::default(),
            completion_pipeline: completion::CompletionPipeline::default(),
            pending_completion_request: None,
            prompt,
            line: String::new(),
            cursor: 0,
            line_area: Rect::default(),
            anchor: 0,
            truncate_start: false,
            truncate_end: false,
            last_cursor_pos: (0, 0),
            completion: Vec::new(),
            completion_max_width: 30,
            selection: None,
            history_register,
            history_pos: None,
            completion_provider: Box::new(completion_provider),
            callback_fn: Box::new(callback_fn),
            doc_fn: Arc::new(|_| None),
            documentation_service: None,
            next_char_handler: None,
            language: None,
            model_layer_id: None,
        }
    }

    /// Gets the byte index in the input representing the current cursor location.
    #[inline]
    pub(crate) fn position(&self) -> usize {
        self.cursor
    }

    pub fn with_line(mut self, line: String, editor: &Editor) -> Self {
        self.set_line(line, editor);
        self
    }

    pub fn set_line(&mut self, line: String, editor: &Editor) {
        let cursor = line.len();
        self.line = line;
        self.cursor = cursor;
        self.recalculate_completion(editor);
    }

    pub fn with_language(
        mut self,
        language: &'static str,
        loader: Arc<ArcSwap<syntax::Loader>>,
    ) -> Self {
        self.language = Some((language, loader));
        self
    }

    pub fn line(&self) -> &String {
        &self.line
    }

    pub fn with_history_register(&mut self, history_register: Option<char>) -> &mut Self {
        self.history_register = history_register;
        self
    }

    pub(crate) fn history_register(&self) -> Option<char> {
        self.history_register
    }

    pub(crate) fn first_history_completion<'a>(
        &'a self,
        editor: &'a Editor,
    ) -> Option<Cow<'a, str>> {
        self.history_register
            .and_then(|reg| editor.registers.first(reg, editor))
    }

    pub fn recalculate_completion(&mut self, editor: &Editor) {
        self.advance_completion_generation();
        self.evaluate_completion(editor);
    }

    fn evaluate_completion(&mut self, editor: &Editor) {
        self.exit_selection();
        self.set_completions(Vec::new());
        self.pending_completion_request = Some(
            self.completion_provider
                .capture(editor, Arc::from(self.line.as_str())),
        );
    }

    fn set_completions(&mut self, mut completion: Vec<Completion>) {
        completion.retain(|(range, _)| {
            range.start <= self.line.len() && self.line.is_char_boundary(range.start)
        });
        self.completion = completion;
        self.completion_max_width = self
            .completion
            .iter()
            .map(|(_, completion)| UnicodeWidthStr::width(completion.content.as_ref()) as u16)
            .max()
            .unwrap_or(BASE_WIDTH)
            .max(BASE_WIDTH);
        if self
            .selection
            .is_some_and(|selection| selection >= self.completion.len())
        {
            self.selection = None;
        }
    }

    fn advance_completion_generation(&mut self) {
        self.completion_generation = self.completion_generation.wrapping_add(1);
        if self.completion_generation == 0 {
            self.completion_generation = 1;
        }
    }

    fn invalidate_completion_work(&mut self) {
        self.advance_completion_generation();
        self.pending_completion_request = None;
        self.completion_pipeline.cancel();
    }

    fn documentation(
        &mut self,
        work: helix_runtime::Work,
        block: helix_runtime::Block,
        redraw: helix_runtime::FrameHandle,
    ) -> Option<Arc<str>> {
        let service = self
            .documentation_service
            .get_or_insert_with(|| DocumentationService::spawn(work, block, redraw));
        service.resolve(&self.line, &self.doc_fn)
    }

    pub(crate) fn dispatch_completion_work(
        &mut self,
        runtime: &helix_runtime::Runtime,
        ingress: crate::runtime::RuntimeIngress,
    ) {
        let Some(request) = self.pending_completion_request.take() else {
            return;
        };
        self.completion_pipeline.submit(
            self.completion_id,
            self.completion_generation,
            Arc::from(self.line.as_str()),
            request,
            self.completion_session.clone(),
            runtime.work().clone(),
            runtime.block().clone(),
            ingress,
        );
    }

    pub(crate) fn apply_completion_result(
        &mut self,
        result: crate::runtime::ui::PromptCompletionResult,
    ) -> bool {
        let result = result.0;
        if result.prompt_id != self.completion_id
            || result.generation != self.completion_generation
            || result.query.as_ref() != self.line
        {
            return false;
        }

        self.set_completions(result.completions);
        true
    }

    pub(crate) fn completion_id(&self) -> completion::PromptId {
        self.completion_id
    }

    #[cfg(test)]
    fn set_completion_loader(&mut self, loader: completion::CompletionLoader) {
        self.completion_pipeline.set_loader(loader);
    }

    /// Compute the cursor position after applying movement
    /// Taken from: <https://github.com/wez/wezterm/blob/e0b62d07ca9bf8ce69a61e30a3c20e7abc48ce7e/termwiz/src/lineedit/mod.rs#L516-L611>
    fn eval_movement(&self, movement: Movement) -> usize {
        match movement {
            Movement::BackwardChar(rep) => {
                let mut position = self.cursor;
                for _ in 0..rep {
                    let mut cursor = GraphemeCursor::new(position, self.line.len(), false);
                    if let Ok(Some(pos)) = cursor.prev_boundary(&self.line, 0) {
                        position = pos;
                    } else {
                        break;
                    }
                }
                position
            }
            Movement::BackwardWord(rep) => {
                let char_indices: Vec<(usize, char)> = self.line.char_indices().collect();
                if char_indices.is_empty() {
                    return self.cursor;
                }
                let mut char_position = char_indices
                    .iter()
                    .position(|(idx, _)| *idx == self.cursor)
                    .unwrap_or(char_indices.len() - 1);

                for _ in 0..rep {
                    if char_position == 0 {
                        break;
                    }

                    let mut found = None;
                    for prev in (0..char_position - 1).rev() {
                        if is_word_sep(char_indices[prev].1) {
                            found = Some(prev + 1);
                            break;
                        }
                    }

                    char_position = found.unwrap_or(0);
                }
                char_indices[char_position].0
            }
            Movement::ForwardWord(rep) => {
                let char_indices: Vec<(usize, char)> = self.line.char_indices().collect();
                if char_indices.is_empty() {
                    return self.cursor;
                }
                let mut char_position = char_indices
                    .iter()
                    .position(|(idx, _)| *idx == self.cursor)
                    .unwrap_or(char_indices.len());

                for _ in 0..rep {
                    // Skip any non-whitespace characters
                    while char_position < char_indices.len()
                        && !is_word_sep(char_indices[char_position].1)
                    {
                        char_position += 1;
                    }

                    // Skip any whitespace characters
                    while char_position < char_indices.len()
                        && is_word_sep(char_indices[char_position].1)
                    {
                        char_position += 1;
                    }

                    // We are now on the start of the next word
                }
                char_indices
                    .get(char_position)
                    .map(|(i, _)| *i)
                    .unwrap_or_else(|| self.line.len())
            }
            Movement::ForwardChar(rep) => {
                let mut position = self.cursor;
                for _ in 0..rep {
                    let mut cursor = GraphemeCursor::new(position, self.line.len(), false);
                    if let Ok(Some(pos)) = cursor.next_boundary(&self.line, 0) {
                        position = pos;
                    } else {
                        break;
                    }
                }
                position
            }
            Movement::StartOfLine => 0,
            Movement::EndOfLine => self.line.len(),
            Movement::None => self.cursor,
        }
    }

    pub fn insert_char(&mut self, c: char, cx: &Context) {
        if let Some(handler) = &self.next_char_handler.take() {
            handler(self, c, cx);

            self.next_char_handler = None;
            return;
        }

        self.line.insert(self.cursor, c);
        let mut cursor = GraphemeCursor::new(self.cursor, self.line.len(), false);
        if let Ok(Some(pos)) = cursor.next_boundary(&self.line, 0) {
            self.cursor = pos;
        }
        self.recalculate_completion(cx.editor);
    }

    pub fn insert_str(&mut self, s: &str, editor: &Editor) {
        self.line.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.recalculate_completion(editor);
    }

    pub fn move_cursor(&mut self, movement: Movement) {
        let pos = self.eval_movement(movement);
        self.cursor = pos
    }

    pub fn move_start(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.line.len();
    }

    pub fn delete_char_backwards(&mut self, editor: &Editor) {
        let pos = self.eval_movement(Movement::BackwardChar(1));
        self.line.replace_range(pos..self.cursor, "");
        self.cursor = pos;

        self.recalculate_completion(editor);
    }

    pub fn delete_char_forwards(&mut self, editor: &Editor) {
        let pos = self.eval_movement(Movement::ForwardChar(1));
        self.line.replace_range(self.cursor..pos, "");

        self.recalculate_completion(editor);
    }

    pub fn delete_word_backwards(&mut self, editor: &Editor) {
        let pos = self.eval_movement(Movement::BackwardWord(1));
        self.line.replace_range(pos..self.cursor, "");
        self.cursor = pos;

        self.recalculate_completion(editor);
    }

    pub fn delete_word_forwards(&mut self, editor: &Editor) {
        let pos = self.eval_movement(Movement::ForwardWord(1));
        self.line.replace_range(self.cursor..pos, "");

        self.recalculate_completion(editor);
    }

    pub fn kill_to_start_of_line(&mut self, editor: &Editor) {
        let pos = self.eval_movement(Movement::StartOfLine);
        self.line.replace_range(pos..self.cursor, "");
        self.cursor = pos;

        self.recalculate_completion(editor);
    }

    pub fn kill_to_end_of_line(&mut self, editor: &Editor) {
        let pos = self.eval_movement(Movement::EndOfLine);
        self.line.replace_range(self.cursor..pos, "");

        self.recalculate_completion(editor);
    }

    pub fn clear(&mut self, editor: &Editor) {
        self.line.clear();
        self.cursor = 0;
        self.recalculate_completion(editor);
    }

    pub fn change_history(
        &mut self,
        cx: &mut Context,
        register: char,
        direction: CompletionDirection,
    ) {
        (self.callback_fn)(cx, &self.line, PromptEvent::Abort);
        let mut values = match cx.editor.registers.read(register, cx.editor) {
            Some(values) if values.len() > 0 => values.rev(),
            _ => return,
        };

        let end = values.len().saturating_sub(1);

        let index = match direction {
            CompletionDirection::Forward => self.history_pos.map_or(0, |i| i + 1),
            CompletionDirection::Backward => self
                .history_pos
                .unwrap_or_else(|| values.len())
                .saturating_sub(1),
        }
        .min(end);

        self.line = values.nth(index).unwrap().to_string();
        // Appease the borrow checker.
        drop(values);

        self.history_pos = Some(index);

        self.move_end();
        (self.callback_fn)(cx, &self.line, PromptEvent::Update);
        self.recalculate_completion(cx.editor);
    }

    pub fn change_completion_selection(&mut self, direction: CompletionDirection) {
        if self.completion.is_empty() {
            return;
        }

        let index = match direction {
            CompletionDirection::Forward => self.selection.map_or(0, |i| i + 1),
            CompletionDirection::Backward => {
                self.selection.unwrap_or(0) + self.completion.len() - 1
            }
        } % self.completion.len();

        self.selection = Some(index);

        let (range, item) = &self.completion[index];
        let range = range.clone();
        let content = item.content.clone();
        if range.start > self.line.len() || !self.line.is_char_boundary(range.start) {
            self.selection = None;
            self.invalidate_completion_work();
            return;
        }

        self.line.replace_range(range, &content);

        self.move_end();
        self.invalidate_completion_work();
    }

    pub fn exit_selection(&mut self) {
        self.selection = None;
    }

    /// Get the current completions
    pub fn completions(&self) -> &Vec<Completion> {
        &self.completion
    }

    /// Get the current selection
    pub fn selection(&self) -> Option<usize> {
        self.selection
    }

    /// Get the language configuration
    pub fn language(
        &self,
    ) -> &Option<(
        &'static str,
        std::sync::Arc<arc_swap::ArcSwap<helix_core::syntax::Loader>>,
    )> {
        &self.language
    }

    /// Get the prompt text
    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    /// Get the current anchor position for horizontal scrolling
    pub fn anchor(&self) -> usize {
        self.anchor
    }

    /// Check if text is truncated at the start
    pub fn truncate_start(&self) -> bool {
        self.truncate_start
    }

    /// Check if text is truncated at the end
    pub fn truncate_end(&self) -> bool {
        self.truncate_end
    }

    /// Update the anchor position for horizontal scrolling based on cursor and available width.
    /// This should be called before rendering when the popup needs to handle its own scrolling.
    pub fn update_scroll_anchor(&mut self, line_width: usize) {
        if self.line.width() < line_width {
            self.anchor = 0;
        } else if self.cursor <= self.anchor {
            // Ensure the grapheme under the cursor is in view.
            self.anchor = self.line[..self.cursor]
                .grapheme_indices(true)
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or_default();
        } else if self.line[self.anchor..self.cursor].width() > line_width {
            // Set the anchor to the last grapheme cluster before the width is exceeded.
            let mut width = 0;
            self.anchor = self.line[..self.cursor]
                .grapheme_indices(true)
                .rev()
                .find_map(|(idx, g)| {
                    width += g.width();
                    if width > line_width {
                        Some(idx + g.len())
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
        }

        self.truncate_start = self.anchor > 0;
        self.truncate_end = self.line[self.anchor..].width() > line_width;

        // If we keep inserting characters just before the end ellipsis, move the anchor
        // so that those new characters are displayed.
        if self.truncate_end && self.line[self.anchor..self.cursor].width() >= line_width {
            // Move the anchor forward by one non-zero-width grapheme.
            self.anchor += self.line[self.anchor..]
                .grapheme_indices(true)
                .find_map(|(idx, g)| {
                    if g.width() > 0 {
                        Some(idx + g.len())
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
        }
    }

    /// Sync prompt state to the shared `Model` layer. Called during render so
    /// any frontend can read the prompt's current state without accessing the
    /// `Prompt` struct directly.
    fn sync_to_model(&mut self, editor: &mut Editor) {
        use helix_view::model::{Placement, PromptModel};

        // Lazily push a layer on first sync.
        let layer_id = match self.model_layer_id {
            Some(id) => id,
            None => {
                let id = editor
                    .model
                    .push_layer(Box::new(PromptModel::default()), Placement::Fullscreen);
                self.model_layer_id = Some(id);
                id
            }
        };

        let doc_text = self
            .documentation(
                editor.work(),
                editor.runtime().block().clone(),
                editor.redraw_handle(),
            )
            .map(|text| text.to_string());

        let Some(model) = editor.model.layer_model_mut::<PromptModel>(layer_id) else {
            return;
        };

        model.prompt_text = self.prompt.clone();
        model.input.clone_from(&self.line);
        model.cursor = self.cursor;
        model.completions = self
            .completion
            .iter()
            .map(|(_, span)| span.content.to_string().into_boxed_str())
            .collect();
        model.selected_completion = self.selection;
        model.doc = doc_text;
    }
}

const BASE_WIDTH: u16 = 30;

struct PromptCompletionSnapshot {
    content: Arc<str>,
    style: helix_view::graphics::Style,
    selected: bool,
}

struct PromptRenderSnapshot {
    area: Rect,
    completion_area: Rect,
    completion_columns: u16,
    completion_column_width: u16,
    completions: Arc<[PromptCompletionSnapshot]>,
    documentation: Option<(Rect, Arc<str>)>,
    label: Arc<str>,
    line_area: Rect,
    line: Arc<str>,
    text_input: Option<crate::widgets::TextInputState>,
    language: Option<(&'static str, Arc<syntax::Loader>)>,
    suggestion: Option<Arc<str>>,
    theme: Arc<helix_view::Theme>,
    rounded_corners: bool,
}

impl PromptRenderSnapshot {
    fn paint(
        self,
        surface: &mut crate::render::CellSurface,
        cancellation: &crate::render::RenderCancellation,
    ) {
        if cancellation.is_cancelled() {
            return;
        }
        self.paint_completions(surface);
        if cancellation.is_cancelled() {
            return;
        }
        self.paint_documentation(surface);
        if cancellation.is_cancelled() {
            return;
        }
        self.paint_line(surface);
    }

    fn paint_completions(&self, surface: &mut crate::render::CellSurface) {
        if self.completions.is_empty() || self.completion_area.height == 0 {
            return;
        }
        let background = self.theme.get("ui.menu");
        let area = tui::ratatui::to_ratatui_rect(self.completion_area);
        tui::ratatui::widgets::Widget::render(tui::ratatui::widgets::Clear, area, surface);
        surface.set_style(area, tui::ratatui::to_ratatui_style(background));
        let selected = self.theme.get("ui.menu.selected");
        let mut row = 0u16;
        let mut column = 0u16;
        for completion in self.completions.iter() {
            let style = if completion.selected {
                selected
            } else {
                background.patch(completion.style)
            };
            surface.set_stringn(
                self.completion_area
                    .x
                    .saturating_add(column.saturating_mul(1 + self.completion_column_width)),
                self.completion_area.y.saturating_add(row),
                &completion.content,
                self.completion_column_width.saturating_sub(1) as usize,
                tui::ratatui::to_ratatui_style(style),
            );
            row = row.saturating_add(1);
            if row >= self.completion_area.height {
                row = 0;
                column = column.saturating_add(1);
                if column >= self.completion_columns {
                    break;
                }
            }
        }
    }

    fn paint_documentation(&self, surface: &mut crate::render::CellSurface) {
        let Some((area, documentation)) = &self.documentation else {
            return;
        };
        let panel = crate::widgets::Panel::framed(
            crate::widgets::PanelStyle::plain(self.theme.get("ui.help")),
            self.rounded_corners,
        );
        let inner = panel.render(surface, *area);
        let text_area = Rect::new(
            inner.x.saturating_add(1),
            inner.y,
            inner.width.saturating_sub(2),
            inner.height,
        );
        let text = tui::text::Text::from(documentation.as_ref());
        crate::ui::text::paint_text(surface, text_area, &text);
    }

    fn paint_line(&self, surface: &mut crate::render::CellSurface) {
        let background = self.theme.get("ui.background");
        let line_area = self.area.clip_top(self.area.height.saturating_sub(1));
        let ratatui_area = tui::ratatui::to_ratatui_rect(line_area);
        tui::ratatui::widgets::Widget::render(tui::ratatui::widgets::Clear, ratatui_area, surface);
        surface.set_style(ratatui_area, tui::ratatui::to_ratatui_style(background));
        let text_style = self.theme.get("ui.text");
        let label_style = self.theme.try_get("ui.text.focus").unwrap_or(text_style);
        surface.set_string(
            line_area.x,
            line_area.y,
            &self.label,
            tui::ratatui::to_ratatui_style(label_style),
        );

        if self.line.is_empty() {
            if let Some(suggestion) = &self.suggestion {
                surface.set_string(
                    self.line_area.x,
                    self.line_area.y,
                    suggestion,
                    tui::ratatui::to_ratatui_style(self.theme.get("ui.text.inactive")),
                );
            }
        } else if let Some((language, loader)) = &self.language {
            let text: tui::text::Text<'static> = crate::ui::markdown::highlighted_code_block(
                &self.line,
                language,
                Some(&self.theme),
                loader,
                None,
            )
            .into();
            crate::ui::text::paint_text(surface, self.line_area, &text);
        } else if let Some(state) = &self.text_input {
            crate::widgets::paint_text_input(
                surface,
                self.line_area,
                &self.line,
                state,
                text_style,
                text_style,
            );
        }
    }
}

fn estimated_prompt_document_height(text: &str, width: u16, max_height: u16) -> u16 {
    let width = width.max(1) as usize;
    let mut height = 0u16;
    for line in text.lines() {
        let line_width = UnicodeWidthStr::width(line);
        height = height
            .saturating_add(line_width.max(1).div_ceil(width) as u16)
            .min(max_height);
        if height == max_height {
            break;
        }
    }
    height
}

impl Prompt {
    fn prepare_render_snapshot(&mut self, area: Rect, cx: &RenderContext) -> PromptRenderSnapshot {
        let max_width = self.completion_max_width;
        let columns = std::cmp::max(1, area.width / max_width);
        let column_width = area.width.saturating_sub(columns) / columns;
        let height = (self.completion.len() as u16)
            .div_ceil(columns)
            .min(10)
            .min(area.height.saturating_sub(1));
        let completion_area = Rect::new(
            area.x,
            area.y
                .saturating_add(area.height.saturating_sub(height).saturating_sub(1)),
            area.width,
            height,
        );
        let visible_items = height as usize * columns as usize;
        let offset = self
            .selection
            .map(|selection| selection / visible_items.max(1) * visible_items.max(1))
            .unwrap_or_default();
        let completions = self
            .completion
            .iter()
            .enumerate()
            .skip(offset)
            .take(visible_items)
            .map(|(index, (_, completion))| PromptCompletionSnapshot {
                content: Arc::from(completion.content.as_ref()),
                style: completion.style,
                selected: self.selection == Some(index),
            })
            .collect::<Vec<_>>();

        let documentation = self
            .documentation(cx.work(), cx.block(), cx.redraw.clone())
            .map(|documentation| {
                let max_width = (BASE_WIDTH * 3).min(area.width);
                let max_height = completion_area.y.saturating_sub(area.y);
                let height =
                    estimated_prompt_document_height(&documentation, max_width, max_height);
                let panel_height = height.saturating_add(2).min(max_height);
                let doc_area = area.intersection(Rect::new(
                    completion_area.x,
                    completion_area.y.saturating_sub(panel_height),
                    max_width,
                    panel_height,
                ));
                (doc_area, documentation)
            });

        let label: Arc<str> = Arc::from(if cx.config().cmdline.style == CmdlineStyle::Bottom {
            match self.prompt.as_ref() {
                "Cmdline" => ":",
                "Search" => "/",
                prompt => prompt,
            }
        } else {
            self.prompt.as_ref()
        });
        let line_row = area.height.saturating_sub(1);
        let label_width = UnicodeWidthStr::width(label.as_ref()) as u16;
        self.line_area = area.clip_left(label_width).clip_top(line_row).clip_right(2);

        let line: Arc<str> = Arc::from(self.line.as_str());
        let mut text_input = None;
        let language = self
            .language
            .as_ref()
            .map(|(language, loader)| (*language, loader.load_full()));
        let suggestion = if self.line.is_empty() {
            self.anchor = 0;
            self.truncate_start = false;
            self.truncate_end = false;
            self.last_cursor_pos = (self.line_area.x, self.line_area.y);
            cx.first_register_value(self.history_register)
                .map(|value| Arc::from(value.as_ref()))
        } else if language.is_some() {
            let cursor_column = self.line[..self.cursor.min(self.line.len())].width() as u16;
            self.anchor = 0;
            self.truncate_start = false;
            self.truncate_end = false;
            self.last_cursor_pos = (
                self.line_area.x.saturating_add(cursor_column),
                self.line_area.y,
            );
            None
        } else {
            let state =
                helix_view::layout::text_input_layout(self.line_area, &self.line, self.cursor);
            self.anchor = state.anchor;
            self.truncate_start = state.truncated_start;
            self.truncate_end = state.truncated_end;
            self.last_cursor_pos = (state.cursor_x, state.cursor_y);
            text_input = Some(state);
            None
        };

        PromptRenderSnapshot {
            area,
            completion_area,
            completion_columns: columns,
            completion_column_width: column_width,
            completions: Arc::from(completions),
            documentation,
            label,
            line_area: self.line_area,
            line,
            text_input,
            language,
            suggestion,
            theme: cx.theme_arc(),
            rounded_corners: cx.config().rounded_corners,
        }
    }
}

impl Component for Prompt {
    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        let event = match event {
            Event::Paste(data) => {
                self.insert_str(data, cx.editor);
                (self.callback_fn)(cx, &self.line, PromptEvent::Update);
                self.dispatch_completion_work(cx.editor.runtime(), cx.ingress.clone());
                return EventResult::Consumed(None);
            }
            Event::Key(event) => *event,
            Event::Resize(..) => return EventResult::Consumed(None),
            _ => return EventResult::Ignored(None),
        };

        let ui_layer_id = self.model_layer_id;
        let close_fn = EventResult::Consumed(Some(PostAction::PopLayer {
            model_layer: ui_layer_id,
            remember_picker: false,
        }));

        match event {
            ctrl!('c') | key!(Esc) => {
                self.invalidate_completion_work();
                (self.callback_fn)(cx, &self.line, PromptEvent::Abort);
                return close_fn;
            }
            alt!('b') | ctrl!(Left) => self.move_cursor(Movement::BackwardWord(1)),
            alt!('f') | ctrl!(Right) => self.move_cursor(Movement::ForwardWord(1)),
            ctrl!('b') | key!(Left) => self.move_cursor(Movement::BackwardChar(1)),
            ctrl!('f') | key!(Right) => self.move_cursor(Movement::ForwardChar(1)),
            ctrl!('e') | key!(End) => self.move_end(),
            ctrl!('a') | key!(Home) => self.move_start(),
            ctrl!('w') | alt!(Backspace) | ctrl!(Backspace) => {
                self.delete_word_backwards(cx.editor);
                (self.callback_fn)(cx, &self.line, PromptEvent::Update);
            }
            alt!('d') | alt!(Delete) | ctrl!(Delete) => {
                self.delete_word_forwards(cx.editor);
                (self.callback_fn)(cx, &self.line, PromptEvent::Update);
            }
            ctrl!('k') => {
                self.kill_to_end_of_line(cx.editor);
                (self.callback_fn)(cx, &self.line, PromptEvent::Update);
            }
            ctrl!('u') => {
                self.kill_to_start_of_line(cx.editor);
                (self.callback_fn)(cx, &self.line, PromptEvent::Update);
            }
            ctrl!('h') | key!(Backspace) | shift!(Backspace) => {
                self.delete_char_backwards(cx.editor);
                (self.callback_fn)(cx, &self.line, PromptEvent::Update);
            }
            ctrl!('d') | key!(Delete) => {
                self.delete_char_forwards(cx.editor);
                (self.callback_fn)(cx, &self.line, PromptEvent::Update);
            }
            ctrl!('s') => {
                let (view_id, doc) = focused!(cx.editor);
                let text = doc.text().slice(..);

                use helix_core::textobject;
                let range = textobject::textobject_word(
                    text,
                    doc.selection(view_id).primary(),
                    textobject::TextObject::Inside,
                    1,
                    false,
                );
                let line = text.slice(range.from()..range.to()).to_string();
                if !line.is_empty() {
                    self.insert_str(line.as_str(), cx.editor);
                    (self.callback_fn)(cx, &self.line, PromptEvent::Update);
                }
            }
            key!(Enter) => {
                if self.selection.is_some() && self.line.ends_with(std::path::MAIN_SEPARATOR) {
                    self.recalculate_completion(cx.editor);
                } else {
                    let last_item = self
                        .first_history_completion(cx.editor)
                        .map(|entry| entry.to_string())
                        .unwrap_or_else(|| String::from(""));

                    // handle executing with last command in history if nothing entered
                    let input = if self.line.is_empty() {
                        &last_item
                    } else {
                        if last_item != self.line {
                            // store in history
                            if let Some(register) = self.history_register {
                                if let Err(err) =
                                    cx.editor.registers.push(register, self.line.clone())
                                {
                                    cx.editor.set_error(err.to_string());
                                }
                            };
                        }

                        &self.line
                    };

                    (self.callback_fn)(cx, input, PromptEvent::Validate);
                    self.invalidate_completion_work();

                    return close_fn;
                }
            }
            ctrl!('p') | key!(Up) => {
                if let Some(register) = self.history_register {
                    self.change_history(cx, register, CompletionDirection::Backward);
                }
            }
            ctrl!('n') | key!(Down) => {
                if let Some(register) = self.history_register {
                    self.change_history(cx, register, CompletionDirection::Forward);
                }
            }
            key!(Tab) => {
                self.change_completion_selection(CompletionDirection::Forward);
                // if single completion candidate is a directory list content in completion
                if self.completion.len() == 1 && self.line.ends_with(std::path::MAIN_SEPARATOR) {
                    self.recalculate_completion(cx.editor);
                }
                (self.callback_fn)(cx, &self.line, PromptEvent::Update)
            }
            shift!(Tab) => {
                self.change_completion_selection(CompletionDirection::Backward);
                (self.callback_fn)(cx, &self.line, PromptEvent::Update)
            }
            ctrl!('q') => self.exit_selection(),
            ctrl!('r') => {
                let completion = cx
                    .editor
                    .registers
                    .iter_preview()
                    .map(|(ch, preview)| (0.., format!("{} {}", ch, &preview).into()))
                    .collect();
                self.set_completions(completion);
                self.invalidate_completion_work();
                self.next_char_handler = Some(Box::new(|prompt, c, context| {
                    prompt.insert_str(
                        &context
                            .editor
                            .registers
                            .first(c, context.editor)
                            .unwrap_or_default(),
                        context.editor,
                    );
                }));
                (self.callback_fn)(cx, &self.line, PromptEvent::Update);
                return EventResult::Consumed(None);
            }
            // any char event that's not mapped to any other combo
            KeyEvent {
                code: KeyCode::Char(c),
                modifiers: _,
            } => {
                self.insert_char(c, cx);
                (self.callback_fn)(cx, &self.line, PromptEvent::Update);
            }
            _ => (),
        };

        self.dispatch_completion_work(cx.editor.runtime(), cx.ingress.clone());
        EventResult::Consumed(None)
    }

    fn sync(&mut self, _viewport: Rect, editor: &mut Editor) {
        self.sync_to_model(editor);
    }

    fn prepare_render(&mut self, area: Rect, cx: &RenderContext) -> crate::render::PreparedRender {
        let snapshot = self.prepare_render_snapshot(area, cx);
        crate::render::PreparedRender::deferred(move |cancellation| {
            let mut output = crate::render::RenderOutput::sparse(area);
            snapshot.paint(output.surface_mut(), cancellation);
            output
        })
    }

    fn cursor(&self, _area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        let (cx, cy) = self.last_cursor_pos;
        (
            Some(Position::new(cy as usize, cx as usize)),
            editor.config().cursor_shape.from_mode(Mode::Insert),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    };
    use std::time::Duration;

    use arc_swap::ArcSwap;
    use helix_view::{
        editor::Config,
        graphics::Rect,
        handlers::Handlers,
        input::{KeyEvent, KeyModifiers},
        keyboard::KeyCode,
        theme,
    };

    use super::*;
    use crate::runtime::{ui::PromptCommand, RuntimeDelivery, UiCommand};

    fn test_editor(runtime: helix_runtime::Runtime) -> Editor {
        let theme_loader = Arc::new(theme::Loader::new(&[]));
        let syntax_loader = Arc::new(ArcSwap::from_pointee(helix_core::syntax::Loader::default()));
        let config = Arc::new(ArcSwap::from_pointee(Config::default()));
        Editor::new(
            Rect::new(0, 0, 120, 40),
            theme_loader,
            syntax_loader,
            config,
            runtime,
            Handlers::dummy(),
        )
    }

    #[derive(Default)]
    struct DeferredTestCompleter {
        forbidden_thread: Option<std::thread::ThreadId>,
    }

    impl DeferredTestCompleter {
        fn off_thread_from(thread: std::thread::ThreadId) -> Self {
            Self {
                forbidden_thread: Some(thread),
            }
        }
    }

    impl CompletionProvider for DeferredTestCompleter {
        fn capture(&mut self, _editor: &Editor, input: Arc<str>) -> completion::CompletionRequest {
            let forbidden_thread = self.forbidden_thread;
            completion::CompletionRequest::new(move || {
                if let Some(forbidden_thread) = forbidden_thread {
                    assert_ne!(
                        std::thread::current().id(),
                        forbidden_thread,
                        "completion evaluator ran on the input thread"
                    );
                }
                completion::test_values(&input)
                    .map(|values| {
                        values
                            .iter()
                            .map(|value| (0.., value.clone().into()))
                            .collect()
                    })
                    .unwrap_or_default()
            })
        }
    }

    async fn next_completion_result(
        receiver: &mut crate::runtime::RuntimeIngressReceiver,
    ) -> crate::runtime::ui::PromptCompletionResult {
        loop {
            match receiver.recv().await {
                Some(RuntimeDelivery::Ui(UiCommand::Prompt(PromptCommand::CompletionReady(
                    result,
                )))) => return result,
                Some(_) => continue,
                None => panic!("runtime ingress closed before prompt completion"),
            }
        }
    }

    #[test]
    fn input_returns_before_blocked_worker_and_newest_query_wins() {
        let tokio = tokio::runtime::Runtime::new().expect("test runtime");
        let _runtime_guard = tokio.enter();
        let mut editor = test_editor(helix_runtime::Runtime::new(tokio.handle().clone()));
        let (ingress, mut receiver) =
            crate::runtime::RuntimeIngress::channel(editor.runtime().clone());
        let (plugin_events, _plugin_events_rx) = helix_runtime::channel(16);
        let idle_reset = crate::runtime::IdleResetGate::new().handle();
        let mut exit_tasks = crate::runtime::ExitTaskSet::default();
        let notifier = crate::handlers::local::Notifier {
            redraw: editor.redraw_handle(),
            plugin_events: plugin_events.into(),
        };

        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let worker_finished = Arc::new(AtomicBool::new(false));
        let input_thread = std::thread::current().id();
        let loader_gate = gate.clone();
        let loader_started = started_tx.clone();
        let loader_finished = worker_finished.clone();
        let loader: completion::CompletionLoader = Arc::new(move |key, cancellation| {
            assert_ne!(
                std::thread::current().id(),
                input_thread,
                "completion loader ran on the input thread"
            );
            let completion::CompletionWorkKey::Test(key) = key else {
                panic!("unexpected completion work: {key:?}");
            };
            if key == "a" {
                if let Some(started) = loader_started
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                {
                    let _ = started.send(());
                }
                let (mutex, signal) = &*loader_gate;
                let open = mutex
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let _ = signal
                    .wait_timeout_while(open, Duration::from_secs(2), |open| !*open)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            loader_finished.store(true, Ordering::Release);
            if cancellation.is_cancelled() {
                return None;
            }
            Some(completion::CompletionWorkOutput::Test {
                key: key.clone(),
                values: Arc::from([format!("{key}-result")]),
            })
        });

        let mut prompt = Prompt::new(
            "Cmdline".into(),
            None,
            DeferredTestCompleter::off_thread_from(input_thread),
            |_, _, _| {},
        );
        prompt.set_completion_loader(loader);

        let exit_task_work = editor.work();
        let mut context = Context::new(
            &mut editor,
            &mut exit_tasks,
            exit_task_work,
            notifier,
            ingress.clone(),
            idle_reset,
            crate::plugin_registry::PluginRuntime::default(),
        );
        let _ = prompt.handle_event(
            &Event::Key(KeyEvent {
                code: KeyCode::Char('a'),
                modifiers: KeyModifiers::NONE,
            }),
            &mut context,
        );
        assert_eq!(prompt.line(), "a");
        assert!(prompt.completions().is_empty());

        tokio.block_on(async {
            started_rx.await.expect("first worker started");
        });
        assert!(
            !worker_finished.load(Ordering::Acquire),
            "input handler did not return before the blocked worker finished"
        );
        let _ = prompt.handle_event(
            &Event::Key(KeyEvent {
                code: KeyCode::Char('b'),
                modifiers: KeyModifiers::NONE,
            }),
            &mut context,
        );
        {
            let (mutex, signal) = &*gate;
            *mutex
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
            signal.notify_all();
        }
        drop(context);

        let result = tokio.block_on(next_completion_result(&mut receiver));
        assert_eq!(result.0.query.as_ref(), "ab");
        assert!(prompt.apply_completion_result(result));
        assert_eq!(prompt.completions().len(), 1);
        assert_eq!(prompt.completions()[0].1.content, "ab-result");
        assert!(
            receiver.try_recv().is_err(),
            "stale result was also emitted"
        );
    }

    #[test]
    fn stale_and_replaced_prompt_results_are_ignored() {
        let tokio = tokio::runtime::Runtime::new().expect("test runtime");
        let _runtime_guard = tokio.enter();
        let editor = test_editor(helix_runtime::Runtime::new(tokio.handle().clone()));
        let (_ingress, _receiver) =
            crate::runtime::RuntimeIngress::channel(editor.runtime().clone());
        let mut prompt = Prompt::new(
            "Cmdline".into(),
            None,
            DeferredTestCompleter::default(),
            |_, _, _| {},
        );
        prompt.set_line("old".into(), &editor);
        let stale =
            crate::runtime::ui::PromptCompletionResult(completion::PromptCompletionPayload {
                prompt_id: prompt.completion_id,
                generation: prompt.completion_generation,
                query: Arc::from("old"),
                completions: vec![(0.., "stale".into())],
            });
        let closed_id = prompt.completion_id;
        prompt.set_line("new".into(), &editor);
        assert!(!prompt.apply_completion_result(stale));
        assert!(prompt.completions().is_empty());
        drop(prompt);

        let mut replacement = Prompt::new(
            "Cmdline".into(),
            None,
            DeferredTestCompleter::default(),
            |_, _, _| {},
        );
        let late =
            crate::runtime::ui::PromptCompletionResult(completion::PromptCompletionPayload {
                prompt_id: closed_id,
                generation: 1,
                query: Arc::from(""),
                completions: Vec::new(),
            });
        assert!(!replacement.apply_completion_result(late));
        assert!(replacement.completions().is_empty());
    }

    #[test]
    fn async_replacement_keeps_selection_in_bounds() {
        let tokio = tokio::runtime::Runtime::new().expect("test runtime");
        let _runtime_guard = tokio.enter();
        let editor = test_editor(helix_runtime::Runtime::new(tokio.handle().clone()));
        let (_ingress, _receiver) =
            crate::runtime::RuntimeIngress::channel(editor.runtime().clone());
        let mut prompt = Prompt::new(
            "Cmdline".into(),
            None,
            DeferredTestCompleter::default(),
            |_, _, _| {},
        );
        prompt.set_line("query".into(), &editor);
        let result =
            crate::runtime::ui::PromptCompletionResult(completion::PromptCompletionPayload {
                prompt_id: prompt.completion_id,
                generation: prompt.completion_generation,
                query: Arc::from("query"),
                completions: vec![(0.., "first".into()), (0.., "second".into())],
            });
        assert!(prompt.apply_completion_result(result));
        prompt.change_completion_selection(CompletionDirection::Forward);
        assert_eq!(prompt.selection(), Some(0));

        let result =
            crate::runtime::ui::PromptCompletionResult(completion::PromptCompletionPayload {
                prompt_id: prompt.completion_id,
                generation: prompt.completion_generation,
                query: Arc::from(prompt.line.as_str()),
                completions: vec![(0.., "only".into())],
            });
        assert!(prompt.apply_completion_result(result));
        assert!(
            prompt
                .selection()
                .is_none_or(|selection| selection < prompt.completions().len()),
            "selection escaped the replacement completion list"
        );
    }
}
