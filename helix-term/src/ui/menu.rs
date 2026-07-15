use crate::{
    compositor::{Component, Context, Event, EventResult, PostAction, RenderContext},
    ctrl, key, shift,
};
use tui::text::Spans;

use helix_view::{
    editor::SmartTabConfig,
    graphics::{Rect, Style},
    Editor,
};
use tui::ratatui::layout::Constraint;

pub use crate::widgets::{TableCell as Cell, TableRow as Row};

const LEFT_PADDING: usize = 1;

fn constrained_width(constraint: Constraint, length: u16) -> u16 {
    match constraint {
        Constraint::Percentage(percent) => {
            let width = f32::from(percent) / 100.0 * f32::from(length);
            width.min(f32::from(length)) as u16
        }
        Constraint::Ratio(numerator, denominator) => {
            let width = numerator as f32 / denominator.max(1) as f32 * f32::from(length);
            width.min(f32::from(length)) as u16
        }
        Constraint::Length(width) | Constraint::Fill(width) => length.min(width),
        Constraint::Max(width) => length.min(width),
        Constraint::Min(width) => length.max(width),
    }
}

pub trait Item: Sync + Send + 'static {
    /// Additional editor state that is used for label calculation.
    type Data: Sync + Send + 'static;

    fn format(&self, data: &Self::Data) -> Row<'_>;
}

pub type MenuCallback<T> = Box<dyn Fn(&mut Editor, Option<&T>, MenuEvent) + Send>;

pub struct Menu<T: Item> {
    options: Vec<T>,
    editor_data: T::Data,

    /// Selection cursor — owns the wrap-around arithmetic for
    /// `move_up` / `move_down` / `move_half_page_*`. Shared with the
    /// file explorer and any future list-shaped UI. The `selection()`
    /// is *always* a valid index into `matches` (or 0 on an empty
    /// list); the menu uses [`has_user_navigated`] to gate whether
    /// that selection is "real" (user has acted on this menu).
    ///
    /// [`has_user_navigated`]: Self::has_user_navigated
    nav: helix_view::list_nav::ListNav,
    /// True once the user has pressed Up / Down / PageUp / PageDown
    /// against this menu. Until then, [`Self::selection`] returns
    /// `None` and pressing Enter is a no-op — so the autocompletion
    /// popup doesn't auto-confirm its first entry on the next Enter
    /// the user types as a newline. Cleared by [`Self::reset_cursor`]
    /// / [`Self::clear`] / [`Self::ensure_cursor_in_bounds`]-on-empty.
    has_user_navigated: bool,

    /// (index, score)
    matches: Vec<(u32, u32)>,

    widths: Vec<Constraint>,

    callback_fn: MenuCallback<T>,

    scroll: usize,
    size: (u16, u16),
    viewport: (u16, u16),
    recalculate: bool,
    auto_close: bool,
}

struct MenuRenderModel {
    rows: Vec<Row<'static>>,
    total_rows: usize,
    scroll: usize,
    selected: Option<usize>,
    widths: Vec<Constraint>,
    styles: crate::ui::design::MenuStyles,
    accent_rail_color: Option<helix_view::theme::Color>,
    render_borders: bool,
}

impl<T: Item> Menu<T> {
    // TODO: it's like a slimmed down picker, share code? (picker = menu + prompt with different
    // rendering)
    pub fn new(
        options: Vec<T>,
        editor_data: <T as Item>::Data,
        callback_fn: impl Fn(&mut Editor, Option<&T>, MenuEvent) + Send + 'static,
    ) -> Self {
        let matches: Vec<(u32, u32)> = (0..options.len() as u32).map(|i| (i, 0)).collect();
        let mut nav = helix_view::list_nav::ListNav::new();
        nav.set_item_count(matches.len());
        Self {
            options,
            editor_data,
            matches,
            nav,
            has_user_navigated: false,
            widths: Vec::new(),
            callback_fn: Box::new(callback_fn),
            scroll: 0,
            size: (0, 0),
            viewport: (0, 0),
            recalculate: true,
            auto_close: false,
        }
    }

    pub fn reset_cursor(&mut self) {
        self.has_user_navigated = false;
        self.scroll = 0;
        self.recalculate = true;
    }

    pub fn update_options(&mut self) -> (&mut Vec<(u32, u32)>, &mut Vec<T>) {
        self.recalculate = true;
        (&mut self.matches, &mut self.options)
    }

    pub fn ensure_cursor_in_bounds(&mut self) {
        // Push the latest match count into nav so it clamps any
        // out-of-range selection automatically. Empty match list
        // clears the user-navigated flag — there's nothing to be
        // pointed at.
        self.nav.set_item_count(self.matches.len());
        self.scroll = 0;
        if self.matches.is_empty() {
            self.has_user_navigated = false;
        } else {
            self.recalculate = true;
        }
    }

    pub fn clear(&mut self) {
        self.matches.clear();
        self.nav.set_item_count(0);
        // reset cursor position
        self.has_user_navigated = false;
        self.scroll = 0;
    }

    pub fn move_up(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.nav.set_item_count(self.matches.len());
        // First navigation lands on the last item — that's the
        // "press Up on a fresh popup, see the last completion"
        // behavior `cursor: Option<usize>` used to encode.
        if !self.has_user_navigated {
            self.nav.to_last();
        } else {
            self.nav
                .move_by(-1, helix_view::list_nav::WrapBehavior::Wrap);
        }
        self.has_user_navigated = true;
        self.adjust_scroll();
    }

    pub fn move_half_page_up(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let half = (self.size.1 as usize / 2).max(1);
        self.nav.set_item_count(self.matches.len());
        if !self.has_user_navigated {
            // From a fresh popup, lands at `len - half` (modulo).
            // Express that as "selection is implicitly at 0; move
            // back by `half` with wrap" — same result, no special
            // case in arithmetic.
            self.nav.set_selection(0);
            self.nav
                .move_by(-(half as isize), helix_view::list_nav::WrapBehavior::Wrap);
        } else {
            self.nav
                .move_by(-(half as isize), helix_view::list_nav::WrapBehavior::Wrap);
        }
        self.has_user_navigated = true;
        self.adjust_scroll();
    }

    pub fn move_down(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.nav.set_item_count(self.matches.len());
        if !self.has_user_navigated {
            // First nav lands on the first item (selection stays at 0).
            self.nav.set_selection(0);
        } else {
            self.nav
                .move_by(1, helix_view::list_nav::WrapBehavior::Wrap);
        }
        self.has_user_navigated = true;
        self.adjust_scroll();
    }

    pub fn move_half_page_down(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let half = (self.size.1 as usize / 2).max(1);
        self.nav.set_item_count(self.matches.len());
        if !self.has_user_navigated {
            // From a fresh popup, lands at index `half` (wrap if
            // half >= len). Same arithmetic as ListNav, but starting
            // implicitly at 0.
            self.nav.set_selection(0);
            self.nav
                .move_by(half as isize, helix_view::list_nav::WrapBehavior::Wrap);
        } else {
            self.nav
                .move_by(half as isize, helix_view::list_nav::WrapBehavior::Wrap);
        }
        self.has_user_navigated = true;
        self.adjust_scroll();
    }

    pub fn auto_close(mut self, auto_close: bool) -> Self {
        self.auto_close = auto_close;
        self
    }

    fn recalculate_size(&mut self, viewport: (u16, u16)) {
        let n = self
            .options
            .first()
            .map(|option| option.format(&self.editor_data).cells.len())
            .unwrap_or_default();
        let max_lens = self.options.iter().fold(vec![0; n], |mut acc, option| {
            let row = option.format(&self.editor_data);
            // maintain max for each column
            for (acc, cell) in acc.iter_mut().zip(row.cells.iter()) {
                let width = cell.content.width();
                if width > *acc {
                    *acc = width;
                }
            }

            acc
        });

        let height = self.matches.len().min(10).min(viewport.1 as usize);
        // do all the matches fit on a single screen?
        let fits = self.matches.len() <= height;

        let mut len = max_lens.iter().sum::<usize>() + n;

        if !fits {
            len += 1; // +1: reserve some space for scrollbar
        }

        len += LEFT_PADDING;
        let width = len.min(viewport.0 as usize);

        self.widths = max_lens
            .into_iter()
            .map(|len| Constraint::Length(len as u16))
            .collect();

        self.size = (width as u16, height as u16);

        // adjust scroll offsets if size changed
        self.adjust_scroll();
        self.recalculate = false;
    }

    /// The effective cursor — `Some(index)` only if the user has
    /// navigated this menu and the index is in range. This is the
    /// single source of truth for the menu's "is anything selected
    /// right now?" question; `selection()`, `selection_mut()`,
    /// `cursor_index()`, and the renderer all funnel through it so
    /// the `has_user_navigated` gating stays consistent.
    fn effective_cursor(&self) -> Option<usize> {
        if !self.has_user_navigated || self.matches.is_empty() {
            return None;
        }
        let cursor = self.nav.selection();
        (cursor < self.matches.len()).then_some(cursor)
    }

    fn adjust_scroll(&mut self) {
        let win_height = self.size.1 as usize;
        self.scroll = helix_view::list_nav::ListViewport::new(
            self.matches.len(),
            self.effective_cursor(),
            win_height,
            self.scroll,
        )
        .scroll_to_selected();
    }

    pub fn selection(&self) -> Option<&T> {
        self.effective_cursor().and_then(|cursor| {
            self.matches
                .get(cursor)
                .map(|(index, _score)| &self.options[*index as usize])
        })
    }

    pub fn selection_mut(&mut self) -> Option<&mut T> {
        self.effective_cursor().and_then(|cursor| {
            self.matches
                .get(cursor)
                .map(|(index, _score)| &mut self.options[*index as usize])
        })
    }

    pub fn is_empty(&self) -> bool {
        self.matches.is_empty()
    }

    pub fn len(&self) -> usize {
        self.matches.len()
    }

    /// Iterate matched items in display order. Each element is a reference to
    /// the original option at the match index.
    pub fn matched_items(&self) -> impl Iterator<Item = &T> + '_ {
        self.matches
            .iter()
            .map(|(index, _score)| &self.options[*index as usize])
    }

    /// Current selection index (into matched items), if any.
    pub fn cursor_index(&self) -> Option<usize> {
        self.effective_cursor()
    }

    fn render_model(&self, area: Rect, cx: &RenderContext) -> MenuRenderModel {
        let theme = cx.theme();
        let styles = crate::ui::design::MenuStyles::from_theme(theme);
        let accent_rail_color = theme.try_get("ui.text.focus").and_then(|s| s.fg);
        let scroll = self.scroll;
        let rows = self
            .matches
            .iter()
            .skip(scroll)
            .take(area.height as usize)
            .map(|(index, _score)| {
                self.options[*index as usize]
                    .format(&self.editor_data)
                    .into_owned()
            })
            .collect();
        MenuRenderModel {
            rows,
            total_rows: self.matches.len(),
            scroll,
            selected: self.effective_cursor(),
            widths: self.widths.clone(),
            styles,
            accent_rail_color,
            render_borders: cx.menu_border(),
        }
    }
}

fn paint_menu(surface: &mut crate::render::CellSurface, area: Rect, model: &MenuRenderModel) {
    let style = model.styles.background;
    let selected = model.styles.selected;
    let ratatui_area = tui::ratatui::to_ratatui_rect(area);
    tui::ratatui::widgets::Widget::render(tui::ratatui::widgets::Clear, ratatui_area, surface);
    surface.set_style(ratatui_area, tui::ratatui::to_ratatui_style(style));

    let len = model.total_rows;
    let win_height = area.height as usize;
    if len == 0 || win_height == 0 {
        return;
    }

    let table_area = area.clip_left(LEFT_PADDING as u16).clip_right(1);

    for (visible_row, row) in model.rows.iter().enumerate() {
        let y = table_area.y + visible_row as u16;
        if y >= table_area.bottom() {
            break;
        }

        let option_index = model.scroll + visible_row;
        let is_selected = model
            .selected
            .map(|cursor| cursor == option_index)
            .unwrap_or(false);
        let row_area = Rect::new(area.x, y, area.width, 1);
        if is_selected {
            surface.set_style(
                tui::ratatui::to_ratatui_rect(row_area),
                tui::ratatui::to_ratatui_style(selected),
            );
        }

        let mut x = table_area.x;
        for (constraint, cell) in model.widths.iter().zip(row.cells.iter()) {
            if x >= table_area.right() {
                break;
            }
            let width = constrained_width(*constraint, table_area.width);
            let width = width.min(table_area.right().saturating_sub(x));
            render_menu_cell(
                surface,
                cell,
                Rect::new(x, y, width, 1),
                is_selected.then_some(selected),
            );
            x = x.saturating_add(width).saturating_add(1);
        }

        if is_selected && !model.render_borders {
            // Left edge: accent rail glyph if the theme has a focus colour,
            // otherwise just the selection background extending to the edge.
            if let Some(cell) = surface.cell_mut((area.left(), y)) {
                let mut cell_style = tui::ratatui::to_ratatui_style(selected);
                if let Some(fg) = model.accent_rail_color {
                    cell.set_symbol("▎");
                    cell_style = cell_style.fg(tui::ratatui::to_ratatui_color(fg));
                }
                cell.set_style(cell_style);
            };
            if let Some(cell) = surface.cell_mut((area.right().saturating_sub(1), y)) {
                cell.set_style(tui::ratatui::to_ratatui_style(selected));
            };
        }
    }

    let fits = len <= win_height;
    if !fits {
        let scroll_style = model.styles.scroll;
        let thumb_fg = scroll_style.fg.unwrap_or(helix_view::theme::Color::Reset);
        let mut sb = crate::widgets::Scrollbar::new(len, model.scroll, win_height)
            .symbol(if model.render_borders { "▌" } else { "▐" })
            .thumb_style(helix_view::graphics::Style::default().fg(thumb_fg));
        if !model.render_borders {
            let track_fg = scroll_style.bg.unwrap_or(helix_view::theme::Color::Reset);
            sb = sb.track("▐", helix_view::graphics::Style::default().fg(track_fg));
        }
        sb.render(
            Rect::new(area.right() - 1, area.top(), 1, area.height),
            surface,
        );
    }
}

impl<T: Item + PartialEq> Menu<T> {
    pub fn replace_option(&mut self, old_option: &impl PartialEq<T>, new_option: T) {
        for option in &mut self.options {
            if old_option == option {
                *option = new_option;
                break;
            }
        }
    }
}

use super::PromptEvent as MenuEvent;
impl<T: Item + 'static> Component for Menu<T> {
    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        let event = match event {
            Event::Key(event) => *event,
            _ => return EventResult::Ignored(None),
        };

        let close_fn = Some(PostAction::PopLayer {
            model_layer: None,
            remember_picker: false,
        });

        // Ignore tab key when supertab is turned on in order not to interfere
        // with it. (Is there a better way to do this?)
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
            // esc or ctrl-c aborts the completion and closes the menu
            key!(Esc) | ctrl!('c') => {
                (self.callback_fn)(cx.editor, self.selection(), MenuEvent::Abort);
                return EventResult::Consumed(close_fn);
            }
            // arrow up/ctrl-p/shift-tab prev completion choice (including updating the doc)
            shift!(Tab) | key!(Up) | ctrl!('p') => {
                self.move_up();
                (self.callback_fn)(cx.editor, self.selection(), MenuEvent::Update);
                return EventResult::Consumed(None);
            }
            key!(Tab) | key!(Down) | ctrl!('n') => {
                // arrow down/ctrl-n/tab advances completion choice (including updating the doc)
                self.move_down();
                (self.callback_fn)(cx.editor, self.selection(), MenuEvent::Update);
                return EventResult::Consumed(None);
            }
            key!(PageUp) | ctrl!('u') => {
                // page up moves back in the completion choice (including updating the doc)
                self.move_half_page_up();
                (self.callback_fn)(cx.editor, self.selection(), MenuEvent::Update);
                return EventResult::Consumed(None);
            }
            key!(PageDown) | ctrl!('d') => {
                // page down advances completion choice (including updating the doc)
                self.move_half_page_down();
                (self.callback_fn)(cx.editor, self.selection(), MenuEvent::Update);
                return EventResult::Consumed(None);
            }
            key!(Enter) => {
                if let Some(selection) = self.selection() {
                    (self.callback_fn)(cx.editor, Some(selection), MenuEvent::Validate);
                    return EventResult::Consumed(close_fn);
                } else {
                    return EventResult::Ignored(close_fn);
                }
            }
            // KeyEvent {
            //     code: KeyCode::Char(c),
            //     modifiers: KeyModifiers::NONE,
            // } => {
            //     self.insert_char(c);
            //     (self.callback_fn)(cx.editor, &self.line, MenuEvent::Update);
            // }

            // / -> edit_filter?
            //
            // enter confirms the match and closes the menu
            // typing filters the menu
            // if we run out of options the menu closes itself
            _ if self.auto_close => {
                (self.callback_fn)(cx.editor, self.selection(), MenuEvent::Abort);
                return EventResult::Consumed(close_fn);
            }
            _ => (),
        }
        // for some events, we want to process them but send ignore, specifically all input except
        // tab/enter/ctrl-k or whatever will confirm the selection/ ctrl-n/ctrl-p for scroll.
        // EventResult::Consumed(None)
        EventResult::Ignored(None)
    }

    fn required_size(&mut self, viewport: (u16, u16)) -> Option<(u16, u16)> {
        if viewport != self.viewport || self.recalculate {
            self.recalculate_size(viewport);
        }

        Some(self.size)
    }

    fn prepare_render(&mut self, area: Rect, cx: &RenderContext) -> crate::render::PreparedRender {
        let model = self.render_model(area, cx);
        crate::render::PreparedRender::deferred(move |cancellation| {
            let mut output = crate::render::RenderOutput::sparse(area);
            if !cancellation.is_cancelled() {
                paint_menu(output.surface_mut(), area, &model);
            }
            output
        })
    }
}

fn render_menu_cell(
    surface: &mut crate::render::CellSurface,
    cell: &Cell<'_>,
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
            let patched = Spans(
                spans
                    .0
                    .iter()
                    .map(|span| {
                        tui::text::Span::styled(span.content.clone(), span.style.patch(selected))
                    })
                    .collect(),
            );
            let line = tui::ratatui::to_ratatui_line(&patched);
            surface.set_line(area.x, area.y + row as u16, &line, area.width);
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
