use crate::{
    commands::{self, OnKeyCallback, OnKeyCallbackKind},
    compositor::{Component, Context, Event, EventResult, RenderContext},
    events::{OnModeSwitch, PostCommand},
    handlers::completion::CompletionItem,
    key,
    keymap::Keymaps,
    render::{CacheStore, PreparedRender},
    ui::{
        document::{
            render_document, HighlighterInput, LinePos, RenderOutput, RenderSeed, TextRenderer,
        },
        statusline,
        text_decorations::{
            self, Decoration, DecorationManager, FoldDecoration, InlineDiagnostics,
            PluginDecoration,
        },
        Completion, NotificationPopup, ProgressSpinners,
    },
};

use helix_core::{
    diagnostic::NumberOrString,
    graphemes::{next_grapheme_boundary, prev_grapheme_boundary},
    movement::Direction,
    syntax::{self, OverlayHighlights},
    text_annotations::TextAnnotations,
    text_folding::RopeSliceFoldExt,
    unicode::width::UnicodeWidthStr,
    plain_visual_col_at_char_idx, visual_offset_from_block, Position, Range, Selection,
};
use helix_loader::VERSION_AND_GIT_HASH;
use helix_view::{
    // annotations::diagnostics::DiagnosticFilter,
    document::{Mode, SCRATCH_BUFFER_NAME},
    editor::{CompleteAction, CursorShapeConfig, InlineBlameConfig, InlineBlameShow},
    graphics::{Color, CursorKind, Modifier, Rect, Style},
    icons::ICONS,
    input::{KeyEvent, MouseButton, MouseEvent, MouseEventKind},
    keyboard::{KeyCode, KeyModifiers},
    Document,
    DocumentId,
    Editor,
    Theme,
    View,
    ViewId,
};
use std::{
    collections::HashMap,
    mem::take,
    ops,
    path::{PathBuf, MAIN_SEPARATOR, MAIN_SEPARATOR_STR},
    rc::Rc,
    sync::{Arc, LazyLock},
};

use tui::{
    buffer::Buffer as Surface,
    text::{Span, Spans},
};

use super::text_decorations::blame::InlineBlame;

use helix_view::engine::{KeymapQuery, ModalInputState};
use helix_view::model::FocusTarget;
use helix_view::view::{LineMap, SyntaxStyleCache, ViewContentState};

const MAX_SEED_LINE_MAP_GAP: usize = 4_096;

/// View render context grouping parameters for `render_view`.
pub(crate) struct ViewRenderContext<'a> {
    pub editor: &'a Editor,
    pub doc: &'a Document,
    pub view: &'a View,
    pub viewport: Rect,
    pub is_focused: bool,
    /// Cached syntax styles to replay instead of running tree-sitter.
    pub cached_syntax: Option<&'a SyntaxStyleCache>,
    /// Set of dirty visual rows. Only these rows will be re-rendered.
    /// `None` means all rows are dirty (full render).
    pub dirty_rows: Option<&'a std::collections::HashSet<u16>>,
    pub seed_line_map: Option<&'a LineMap>,
}

fn choose_render_seed(
    line_map: Option<&LineMap>,
    top_doc_line: usize,
    horizontal_offset: usize,
) -> Option<RenderSeed> {
    line_map.and_then(|line_map| {
        line_map.best_horizontal_checkpoint_within_gap(
            top_doc_line,
            horizontal_offset,
            MAX_SEED_LINE_MAP_GAP,
        )
    })
        .map(|checkpoint| RenderSeed {
            doc_line: top_doc_line,
            char_idx: checkpoint.char_idx,
            visual_col: checkpoint.visual_col,
        })
}

fn can_reuse_seed_line_map(
    previous: &ViewContentState,
    current: &ViewContentState,
) -> bool {
    previous.doc_id == current.doc_id
        && previous.doc_version == current.doc_version
        && previous.annotation_gen == current.annotation_gen
        && previous.config_gen == current.config_gen
        && previous.theme_name == current.theme_name
        && previous.area.width == current.area.width
}

fn cursor_position_from_line_map(doc: &Document, view: &View, line_map: &LineMap) -> Option<Position> {
    let text = doc.text().slice(..);
    let cursor = doc.selection(view.id).primary().cursor(text);
    let tab_width = doc.tab_width();

    line_map.lines.iter().find_map(|line| {
        if line.visible_char_start == usize::MAX || line.visible_char_last == usize::MAX {
            return None;
        }
        if cursor < line.visible_char_start || cursor > line.char_range_end {
            return None;
        }
        if line.doc_line != text.char_to_line(cursor.min(text.len_chars())) {
            return None;
        }

        let delta_chars = cursor.saturating_sub(line.visible_char_start);
        let delta_col = plain_visual_col_at_char_idx(
            text.slice(line.visible_char_start..cursor),
            delta_chars,
            tab_width,
        );

        let visual_col = line.visible_col_start + delta_col;
        if visual_col < doc.view_offset(view.id).horizontal_offset {
            return None;
        }

        Some(Position::new(
            line.visual_row as usize,
            visual_col - doc.view_offset(view.id).horizontal_offset,
        ))
    })
}

fn update_cursor_cache_from_line_map(editor: &Editor, doc: &Document, view: &View, line_map: &LineMap) {
    if editor.tree.focus != view.id {
        return;
    }
    editor
        .cursor_cache
        .set(cursor_position_from_line_map(doc, view, line_map));
}

/// Cached rendered cells for a single view.
struct ViewRenderCacheEntry {
    /// Content-level state (doc version, scroll, area, theme, config).
    /// When this matches, syntax styles and line map can be reused.
    content_state: ViewContentState,
    /// The rendered cells for the view's area.
    cells: tui::buffer::Buffer,
    /// Cached syntax styles — reusable when content state is unchanged.
    syntax_styles: SyntaxStyleCache,
    /// Visual-line → document position mapping, built during rendering.
    line_map: LineMap,
    /// Per-line overlay fingerprints for dirty-line detection.
    overlay_fingerprints: Arc<[u64]>,
}

/// Per-view render cache with two-tier invalidation:
///
/// 1. **Content hit** — content state matches → blit cached cells, compute dirty
///    lines from overlay fingerprints, re-render only dirty lines. When 0 lines
///    are dirty (nothing changed at all), this is a pure blit — zero extra work.
/// 2. **Content miss** — content changed → full re-render + record syntax styles + line map
#[derive(Default)]
struct ViewRenderCache {
    entries: HashMap<ViewId, ViewRenderCacheEntry>,
    #[cfg(debug_assertions)]
    hits: u64,
    #[cfg(debug_assertions)]
    misses: u64,
    #[cfg(debug_assertions)]
    dirty_lines: u64,
    #[cfg(debug_assertions)]
    clean_lines: u64,
    #[cfg(debug_assertions)]
    frames: u64,
}


pub struct EditorView {
    pub keymaps: Keymaps,
    /// The editing engine. Always `Some` during normal operation; temporarily
    /// `None` during `feed_key` to satisfy the borrow checker (engine and
    /// editor are disjoint, but the compiler can't prove it).
    engine: Option<Box<dyn helix_view::engine::EditingEngine>>,
    /// Shared command registry — borrowed by Context for MappableCommand::Engine execution.
    pub(crate) registry: std::sync::Arc<helix_modal::registry::CommandRegistry>,
    on_next_key: Option<(OnKeyCallback, OnKeyCallbackKind)>,
    pseudo_pending: Vec<KeyEvent>,
    pub(crate) completion: Option<Completion>,
    spinners: ProgressSpinners,
    bufferline_info: BufferLineInfo,
    /// Tracks if the terminal window is focused by reaction to terminal focus events
    terminal_focused: bool,
    bufferline_positions: Vec<u16>,
    /// Tracks if there are prompt layers active (updated by compositor)
    pub prompt_active: bool,
    notification_popup: NotificationPopup,
    /// Per-view render cache for skipping re-render of unchanged views.
    render_cache: ViewRenderCache,
    chrome_cache: CacheStore,
}

impl EditorView {
    fn content_area(view: &View) -> Rect {
        view.area.clip_bottom(1)
    }

    fn engine_input_state(&self) -> ModalInputState {
        self.engine
            .as_ref()
            .map_or_else(ModalInputState::default, |engine| engine.input_state())
    }

    fn set_engine_input_state(&mut self, state: ModalInputState) {
        if let Some(engine) = self.engine.as_mut() {
            engine.set_input_state(state);
        }
    }

    fn sync_context_from_engine(&self, cx: &mut commands::Context) {
        let state = self.engine_input_state();
        cx.count = state.count;
        cx.register = state.selected_register;
    }

    fn sync_engine_from_context(&mut self, cx: &mut commands::Context) {
        let state = ModalInputState {
            count: cx.count,
            selected_register: cx.register,
        };
        self.set_engine_input_state(state);
        cx.editor.focused_modal_input = state;
    }

    fn publish_focused_modal_input(&self, editor: &mut Editor) {
        editor.focused_modal_input = self.engine_input_state();
    }

    fn prepare_statusline(
        &self,
        editor: &Editor,
        doc: &Document,
        view: &View,
        is_focused: bool,
    ) -> PreparedRender {
        let statusline_area = view.area.clip_top(view.area.height.saturating_sub(1));
        let statusline_mode = editor.mode();
        let statusline_register = self.engine_input_state().selected_register;
        let statusline_model = statusline::StatuslineModel::collect(
            editor,
            doc,
            view,
            is_focused,
            statusline_mode,
            statusline_register,
            &self.spinners,
        );
        statusline::Statusline::prepare(statusline_model, statusline_area)
    }

    pub fn new(
        keymaps: Keymaps,
        engine: Box<dyn helix_view::engine::EditingEngine>,
        registry: std::sync::Arc<helix_modal::registry::CommandRegistry>,
    ) -> Self {
        Self {
            keymaps,
            engine: Some(engine),
            registry,
            on_next_key: None,
            pseudo_pending: Vec::new(),
            completion: None,
            spinners: ProgressSpinners::default(),
            bufferline_info: BufferLineInfo::default(),
            terminal_focused: true,
            bufferline_positions: Vec::new(),
            prompt_active: false,
            notification_popup: NotificationPopup::new(),
            render_cache: ViewRenderCache::default(),
            chrome_cache: CacheStore::default(),
        }
    }

    /// Blit cached cells onto the main surface.
    fn blit(src: &tui::buffer::Buffer, dst: &mut Surface) {
        let a = src.area;
        for y in a.top()..a.bottom() {
            for x in a.left()..a.right() {
                dst[(x, y)] = src[(x, y)].clone();
            }
        }
    }

    /// Copy a rectangular region from the surface into a standalone buffer.
    fn copy_region(src: &Surface, area: Rect) -> tui::buffer::Buffer {
        let mut buf = tui::buffer::Buffer::empty(area);
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                buf[(x, y)] = src[(x, y)].clone();
            }
        }
        buf
    }

    pub fn spinners_mut(&mut self) -> &mut ProgressSpinners {
        &mut self.spinners
    }

    pub fn render_welcome(theme: &Theme, view: &View, surface: &mut Surface, is_colorful: bool) {
        /// Logo for Helix
        const LOGO_STR: &str = "\
**             
*****        ::
 ******** :::::
     **::::::: 
   ::::::::***=
:::::::    ====
::::    =======
:---========   
 =======--     
===== -------- 
==        -----
             --";

        /// Size of the maximum line of the logo
        static LOGO_WIDTH: LazyLock<u16> = LazyLock::new(|| {
            LOGO_STR
                .lines()
                .max_by(|line, other| line.len().cmp(&other.len()))
                .unwrap_or("")
                .len() as u16
        });

        /// Use when true color is not supported
        static LOGO_NO_COLOR: LazyLock<Vec<Spans>> = LazyLock::new(|| {
            LOGO_STR
                .lines()
                .map(|line| Spans(vec![Span::raw(line)]))
                .collect()
        });

        /// The logo is colored using Helix's colors
        static LOGO_WITH_COLOR: LazyLock<Vec<Spans>> = LazyLock::new(|| {
            LOGO_STR
                .lines()
                .map(|line| {
                    line.chars()
                        .map(|ch| match ch {
                            '*' | ':' | '=' | '-' => Span::styled(
                                ch.to_string(),
                                Style::new().fg(match ch {
                                    // Dark purple
                                    '*' => Color::Rgb(112, 107, 200),
                                    // Dark blue
                                    ':' => Color::Rgb(132, 221, 234),
                                    // Bright purple
                                    '=' => Color::Rgb(153, 123, 200),
                                    // Bright blue
                                    '-' => Color::Rgb(85, 197, 228),
                                    _ => unreachable!(),
                                }),
                            ),
                            ' ' => Span::raw(" "),
                            _ => unreachable!("logo should only contain '*', ':', '=', '-' or ' '"),
                        })
                        .collect()
                })
                .collect()
        });

        /// How much space to put between the help text and the logo
        const LOGO_LEFT_PADDING: u16 = 6;

        // Shift the help text to the right by this amount, to add space
        // for the logo
        static HELP_X_LOGO_OFFSET: LazyLock<u16> =
            LazyLock::new(|| *LOGO_WIDTH / 2 + LOGO_LEFT_PADDING / 2);

        #[derive(PartialEq, PartialOrd, Eq, Ord)]
        enum AlignLine {
            Left,
            Center,
        }
        use AlignLine::*;

        let logo = if is_colorful {
            &LOGO_WITH_COLOR
        } else {
            &LOGO_NO_COLOR
        };

        let empty_line = || (Spans::from(""), Left);

        let raw_help_lines: [(Spans, AlignLine); 12] = [
            (
                vec![
                    Span::raw("helix "),
                    Span::styled(VERSION_AND_GIT_HASH, theme.get("comment")),
                ]
                .into(),
                Center,
            ),
            empty_line(),
            (
                Span::styled(
                    "A post-modern modal text editor",
                    theme.get("ui.text").add_modifier(Modifier::ITALIC),
                )
                .into(),
                Center,
            ),
            empty_line(),
            (
                vec![
                    Span::styled(":tutor", theme.get("markup.raw")),
                    Span::styled("<enter>", theme.get("comment")),
                    Span::raw("       learn helix"),
                ]
                .into(),
                Left,
            ),
            (
                vec![
                    Span::styled(":theme", theme.get("markup.raw")),
                    Span::styled("<space><tab>", theme.get("comment")),
                    Span::raw("  choose a theme"),
                ]
                .into(),
                Left,
            ),
            (
                vec![
                    Span::styled("<space>e", theme.get("markup.raw")),
                    Span::raw("            file explorer"),
                ]
                .into(),
                Left,
            ),
            (
                vec![
                    Span::styled("<space>?", theme.get("markup.raw")),
                    Span::raw("            see all commands"),
                ]
                .into(),
                Left,
            ),
            (
                vec![
                    Span::styled(":quit", theme.get("markup.raw")),
                    Span::styled("<enter>", theme.get("comment")),
                    Span::raw("        quit helix"),
                ]
                .into(),
                Left,
            ),
            empty_line(),
            (
                vec![
                    Span::styled("docs: ", theme.get("ui.text")),
                    Span::styled("docs.helix-editor.com", theme.get("markup.link.url")),
                ]
                .into(),
                Center,
            ),
            empty_line(),
        ];

        debug_assert!(
            raw_help_lines.len() >= LOGO_STR.lines().count(),
            "help lines get chained with lines of logo. if there are not \
             enough help lines, logo will be cut off. add `empty_line()`s if necessary"
        );

        let mut help_lines = Vec::with_capacity(raw_help_lines.len());
        let mut len_of_longest_left_align = 0;
        let mut len_of_longest_center_align = 0;

        for (spans, align) in raw_help_lines {
            let width = spans.width();
            match align {
                Left => len_of_longest_left_align = len_of_longest_left_align.max(width),
                Center => len_of_longest_center_align = len_of_longest_center_align.max(width),
            }
            help_lines.push((spans, align));
        }

        let len_of_longest_left_align = len_of_longest_left_align as u16;

        // the y-coordinate where we start drawing the welcome screen
        let start_drawing_at_y =
            view.area.y + (view.area.height / 2).saturating_sub(help_lines.len() as u16 / 2);

        // x-coordinate of the center of the viewport
        let x_view_center = view.area.x + view.area.width / 2;

        // the x-coordinate where we start drawing the `AlignLine::Left` lines
        // +2 to make the text look like more balanced relative to the center of the help
        let start_drawing_left_align_at_x =
            view.area.x + (view.area.width / 2).saturating_sub(len_of_longest_left_align / 2) + 2;

        let are_any_left_aligned_lines_overflowing_x =
            (start_drawing_left_align_at_x + len_of_longest_left_align) > view.area.width;

        let are_any_center_aligned_lines_overflowing_x =
            len_of_longest_center_align as u16 > view.area.width;

        let is_help_x_overflowing =
            are_any_left_aligned_lines_overflowing_x || are_any_center_aligned_lines_overflowing_x;

        // we want `>=` so it does not get drawn over the status line
        // (essentially, it WON'T be marked as "overflowing" if the help
        // fully fits vertically in the viewport without touching the status line)
        let is_help_y_overflowing = (help_lines.len() as u16) >= view.area.height;

        // Not enough space to render the help text even without the logo. Render nothing.
        if is_help_x_overflowing || is_help_y_overflowing {
            return;
        }

        // At this point we know that there is enough vertical
        // and horizontal space to render the help text

        let width_of_help_with_logo = *LOGO_WIDTH + LOGO_LEFT_PADDING + len_of_longest_left_align;

        // If there is not enough space to show LOGO + HELP, then don't show the logo at all
        //
        // If we get here we know that there IS enough space to show just the help
        let show_logo = width_of_help_with_logo <= view.area.width;

        // Each "help" line is effectively "chained" with a line of the logo (if present).
        for (lines_drawn, (line, align)) in help_lines.iter().enumerate() {
            // Where to start drawing `AlignLine::Left` rows
            let x_start_left_help =
                start_drawing_left_align_at_x + if show_logo { *HELP_X_LOGO_OFFSET } else { 0 };

            // Where to start drawing `AlignLine::Center` rows
            let x_start_center_help = x_view_center - line.width() as u16 / 2
                + if show_logo { *HELP_X_LOGO_OFFSET } else { 0 };

            // Where to start drawing rows for the "help" section
            // Includes tips about commands. Excludes the logo.
            let x_start_help = match align {
                Left => x_start_left_help,
                Center => x_start_center_help,
            };

            let y = start_drawing_at_y + lines_drawn as u16;

            // Draw a single line of the help text
            surface.set_spans(x_start_help, y, line, line.width() as u16);

            if show_logo {
                // Draw a single line of the logo
                surface.set_spans(
                    x_start_left_help - LOGO_LEFT_PADDING - *LOGO_WIDTH,
                    y,
                    &logo[lines_drawn],
                    *LOGO_WIDTH,
                );
            }
        }
    }

    pub(crate) fn render_view(
        &mut self,
        vctx: &ViewRenderContext<'_>,
        surface: &mut Surface,
    ) -> RenderOutput {
        let ViewRenderContext {
            editor,
            doc,
            view,
            viewport,
            is_focused,
            cached_syntax,
            dirty_rows,
            seed_line_map,
        } = vctx;
        let is_focused = *is_focused;
        let inner = view.inner_area(doc);
        let area = view.area;
        let theme = &editor.theme;
        let config = editor.config();
        let loader = editor.syn_loader.load();

        let view_offset = doc.view_offset(view.id);

        let render_view_start = std::time::Instant::now();
        let text_annotations_start = std::time::Instant::now();
        let text_annotations = view.text_annotations(doc, Some(theme));
        helix_view::bench::log_run_phase(
            "render_view",
            "text_annotations",
            text_annotations_start.elapsed(),
            || format!("view_id={:?}", view.id),
        );
        let mut decorations = DecorationManager::default();

        if !(is_focused && self.terminal_focused) {
            surface.set_style(area, theme.get("ui.background.inactive"))
        }

        if is_focused && config.cursorline {
            decorations.add_decoration(Self::cursorline(doc, view, theme));
        }

        decorations.add_decoration(FoldDecoration::new(&text_annotations, theme));

        if is_focused && config.cursorcolumn {
            let cursorcolumn_start = std::time::Instant::now();
            Self::highlight_cursorcolumn(doc, view, surface, theme, inner, &text_annotations);
            helix_view::bench::log_run_phase(
                "render_view",
                "cursorcolumn",
                cursorcolumn_start.elapsed(),
                || format!("view_id={:?}", view.id),
            );
        }

        // Set DAP highlights, if needed.
        if let Some(frame) = editor.current_stack_frame() {
            let dap_line = frame.line.saturating_sub(1);
            let style = theme.get("ui.highlight.frameline");
            let line_decoration = move |renderer: &mut TextRenderer, pos: LinePos| {
                if pos.doc_line != dap_line {
                    return;
                }
                renderer.set_style(Rect::new(inner.x, pos.visual_line, inner.width, 1), style);
            };

            decorations.add_decoration(line_decoration);
        }

        let highlighter_start = std::time::Instant::now();
        let highlighter_input = match cached_syntax {
            Some(cache) => HighlighterInput::Cached(&cache.entries),
            None => {
                let syntax_highlighter = Self::doc_syntax_highlighter(
                    doc,
                    &text_annotations,
                    view_offset.anchor,
                    inner.height,
                    &loader,
                );
                HighlighterInput::Live(syntax_highlighter)
            }
        };
        helix_view::bench::log_run_phase(
            "render_view",
            "highlighter_input",
            highlighter_start.elapsed(),
            || format!("view_id={:?} cached={}", view.id, cached_syntax.is_some()),
        );
        let mut overlays = Vec::new();

        let overlays_start = std::time::Instant::now();
        overlays.push(Self::overlay_syntax_highlights(
            doc,
            view_offset.anchor,
            inner.height,
            &text_annotations,
        ));

        if doc
            .language_config()
            .and_then(|config| config.rainbow_brackets)
            .unwrap_or(config.rainbow_brackets)
        {
            if let Some(overlay) = Self::doc_rainbow_highlights(
                doc,
                &text_annotations,
                view_offset.anchor,
                inner.height,
                theme,
                &loader,
            ) {
                overlays.push(overlay);
            }
        }

        let viewport_range = Self::viewport_byte_range(
            doc.text().slice(..),
            &text_annotations,
            view_offset.anchor,
            inner.height,
        );
        Self::doc_diagnostics_highlights_into(doc, theme, &mut overlays, Some(viewport_range));

        if is_focused {
            if let Some(tabstops) = Self::tabstop_highlights(doc, theme) {
                overlays.push(tabstops);
            }
            overlays.push(self.doc_selection_highlights(
                editor.mode(),
                doc,
                view,
                theme,
                &config.cursor_shape,
                self.terminal_focused,
            ));
            if let Some(overlay) = Self::highlight_focused_view_elements(view, doc, theme) {
                overlays.push(overlay);
            }
        }
        helix_view::bench::log_run_phase(
            "render_view",
            "overlays",
            overlays_start.elapsed(),
            || format!("view_id={:?} count={}", view.id, overlays.len()),
        );

        let gutter_overflow = view.gutter_offset(doc) == 0;
        if !gutter_overflow {
            let gutter_start = std::time::Instant::now();
            Self::render_gutter(
                editor,
                doc,
                view,
                view.area,
                theme,
                is_focused & self.terminal_focused,
                &mut decorations,
            );
            helix_view::bench::log_run_phase(
                "render_view",
                "gutter",
                gutter_start.elapsed(),
                || format!("view_id={:?}", view.id),
            );
        }

        let inline_blame_start = std::time::Instant::now();
        Self::render_inline_blame(&config.inline_blame, doc, view, &mut decorations, theme);
        helix_view::bench::log_run_phase(
            "render_view",
            "inline_blame",
            inline_blame_start.elapsed(),
            || format!("view_id={:?}", view.id),
        );

        if config.welcome_screen && doc.version() == 0 && doc.is_welcome() {
            Self::render_welcome(
                theme,
                view,
                surface,
                config.true_color || crate::true_color(),
            );
        }

        let primary_cursor = doc
            .selection(view.id)
            .primary()
            .cursor(doc.text().slice(..));
        if is_focused {
            decorations.add_decoration(text_decorations::Cursor {
                cache: &editor.cursor_cache,
                primary_cursor,
            });
        }
        let width = view.inner_width(doc);
        let config = doc.config.load();
        let enable_cursor_line = view
            .diagnostics_handler
            .show_cursorline_diagnostics(doc, view.id);
        let inline_diagnostic_config = config.inline_diagnostics.prepare(width, enable_cursor_line);
        decorations.add_decoration(InlineDiagnostics::new(
            doc,
            theme,
            primary_cursor,
            inline_diagnostic_config,
            config.end_of_line_diagnostics,
        ));

        decorations.add_decoration(PluginDecoration::new(doc, theme, view.id));

        let top_doc_line = doc
            .text()
            .char_to_line(view_offset.anchor.min(doc.text().len_chars()));
        let render_seed =
            choose_render_seed(*seed_line_map, top_doc_line, view_offset.horizontal_offset);
        let render_document_start = std::time::Instant::now();
        let render_output = render_document(
            surface,
            inner,
            doc,
            view_offset,
            &text_annotations,
            highlighter_input,
            overlays,
            theme,
            decorations,
            *dirty_rows,
            render_seed,
            *seed_line_map,
        );
        helix_view::bench::log_run_phase(
            "render_view",
            "render_document",
            render_document_start.elapsed(),
            || format!("view_id={:?}", view.id),
        );

        // Draw rulers after document. Skip cells that already have content.
        let rulers_start = std::time::Instant::now();
        Self::render_rulers(editor, doc, view, inner, surface, theme);
        helix_view::bench::log_run_phase(
            "render_view",
            "rulers",
            rulers_start.elapsed(),
            || format!("view_id={:?}", view.id),
        );

        // if we're not at the edge of the screen, draw a right border
        if viewport.right() != view.area.right() {
            let border_start = std::time::Instant::now();
            let x = area.right();
            let border_style = theme.get("ui.window");
            for y in area.top()..area.bottom() {
                surface[(x, y)]
                    .set_symbol(tui::symbols::line::VERTICAL)
                    //.set_symbol(" ")
                    .set_style(border_style);
            }
            helix_view::bench::log_run_phase(
                "render_view",
                "right_border",
                border_start.elapsed(),
                || format!("view_id={:?} height={}", view.id, area.height),
            );
        }

        // if config.inline_diagnostics.disabled()
        //     && config.end_of_line_diagnostics == DiagnosticFilter::Disable
        // {
        //     Self::render_diagnostics(doc, view, inner, surface, theme);
        // }

        helix_view::bench::log_run_phase(
            "render_view",
            "total",
            render_view_start.elapsed(),
            || format!("view_id={:?}", view.id),
        );

        render_output
    }

    fn render_inline_blame(
        inline_blame: &InlineBlameConfig,
        doc: &Document,
        view: &View,
        decorations: &mut DecorationManager,
        theme: &Theme,
    ) {
        const INLINE_BLAME_SCOPE: &str = "ui.virtual.inline-blame";
        let text = doc.text();
        match inline_blame.show {
            InlineBlameShow::Never => (),
            InlineBlameShow::CursorLine => {
                let cursor_line_idx = doc.cursor_line(view.id);

                // do not render inline blame for empty lines to reduce visual noise
                if text.line(cursor_line_idx) != doc.line_ending().as_str() {
                    if let Ok(line_blame) =
                        doc.line_blame(cursor_line_idx as u32, &inline_blame.format)
                    {
                        decorations.add_decoration(InlineBlame::new(
                            theme.get(INLINE_BLAME_SCOPE),
                            text_decorations::blame::LineBlame::OneLine((
                                cursor_line_idx,
                                line_blame,
                            )),
                        ));
                    };
                }
            }
            InlineBlameShow::AllLines => {
                let mut blame_lines = vec![None; text.len_lines()];

                let blame_for_all_lines = view.line_range(doc).filter_map(|line_idx| {
                    // do not render inline blame for empty lines to reduce visual noise
                    if text.line(line_idx) != doc.line_ending().as_str() {
                        doc.line_blame(line_idx as u32, &inline_blame.format)
                            .ok()
                            .map(|blame| (line_idx, blame))
                    } else {
                        None
                    }
                });

                for (line_idx, blame) in blame_for_all_lines {
                    blame_lines[line_idx] = Some(blame);
                }

                decorations.add_decoration(InlineBlame::new(
                    theme.get(INLINE_BLAME_SCOPE),
                    text_decorations::blame::LineBlame::ManyLines(blame_lines),
                ));
            }
        }
    }

    pub fn render_rulers(
        editor: &Editor,
        doc: &Document,
        view: &View,
        viewport: Rect,
        surface: &mut Surface,
        theme: &Theme,
    ) {
        let editor_rulers = &editor.config().rulers;
        let ruler_char = &editor.config().ruler_char;
        // Base style from theme for rulers
        let base_style = theme.try_get("ui.virtual.ruler").unwrap_or_default();
        // Background style is used only for background-style rulers. If theme lacks a bg, reuse fg.
        let bg_style = if base_style.bg.is_none() {
            if let Some(fg) = base_style.fg {
                base_style.bg(fg)
            } else {
                // Fallback background to ensure visibility
                Style::default().bg(Color::Red)
            }
        } else {
            base_style
        };

        let rulers = doc
            .language_config()
            .and_then(|config| config.rulers.as_ref())
            .unwrap_or(editor_rulers);

        let view_offset = doc.view_offset(view.id);

        rulers
            .iter()
            // View might be horizontally scrolled, convert from absolute distance
            // from the 1st column to relative distance from left of viewport
            .filter_map(|ruler| ruler.checked_sub(1 + view_offset.horizontal_offset as u16))
            .filter(|ruler| ruler < &viewport.width)
            .map(|ruler| viewport.clip_left(ruler).with_width(1))
            .for_each(|area| {
                if ruler_char.is_empty() {
                    // Background-style ruler: only apply to cells without content
                    for y in area.top()..area.bottom() {
                        let cell = &surface[(area.x, y)];
                        // Skip cells that have non-whitespace content (like diagnostic bubbles)
                        if &*cell.symbol == " " || cell.symbol.is_empty() {
                            surface[(area.x, y)].set_style(bg_style);
                        }
                    }
                } else {
                    // Foreground glyph ruler: only draw on empty/space cells
                    let mut glyph_style = base_style;
                    glyph_style.bg = None;
                    if glyph_style.fg.is_none() {
                        glyph_style = glyph_style.fg(Color::Gray);
                    }
                    for y in area.top()..area.bottom() {
                        let cell = &surface[(area.x, y)];
                        // Only draw ruler glyph on empty/space cells to avoid overwriting content
                        if &*cell.symbol == " " || cell.symbol.is_empty() {
                            surface[(area.x, y)]
                                .set_symbol(ruler_char)
                                .set_style(glyph_style);
                        }
                    }
                }
            })
    }

    fn viewport_byte_range(
        text: helix_core::RopeSlice,
        annotations: &TextAnnotations,
        row: usize,
        height: u16,
    ) -> std::ops::Range<usize> {
        // Calculate viewport byte ranges:
        // Saturating subs to make it inclusive zero indexing.
        let last_line = text.len_lines().saturating_sub(1);
        let row = row.min(last_line);
        let last_visible_line = text
            .nth_next_folded_line(&annotations.folds, row, (height as usize).saturating_sub(1))
            .min(last_line);
        let start = text.line_to_byte(row);
        let end = text.line_to_byte(last_visible_line + 1);

        start..end
    }

    /// Get the syntax highlighter for a document in a view represented by the first line
    /// and column (`offset`) and the last line. This is done instead of using a view
    /// directly to enable rendering syntax highlighted docs anywhere (eg. picker preview)
    pub fn doc_syntax_highlighter<'editor>(
        doc: &'editor Document,
        annotations: &TextAnnotations,
        anchor: usize,
        height: u16,
        loader: &'editor syntax::Loader,
    ) -> Option<syntax::Highlighter<'editor>> {
        let syntax = doc.syntax()?;
        let text = doc.text().slice(..);
        let row = text.char_to_line(anchor.min(text.len_chars()));
        let range = Self::viewport_byte_range(text, annotations, row, height);
        let range = range.start as u32..range.end as u32;

        let highlighter = syntax.highlighter(text, loader, range);
        Some(highlighter)
    }

    pub fn overlay_syntax_highlights(
        doc: &Document,
        anchor: usize,
        height: u16,
        text_annotations: &TextAnnotations,
    ) -> OverlayHighlights {
        let text = doc.text().slice(..);
        let row = text.char_to_line(anchor.min(text.len_chars()));

        let mut range = Self::viewport_byte_range(text, text_annotations, row, height);
        range = text.byte_to_char(range.start)..text.byte_to_char(range.end);

        text_annotations.collect_overlay_highlights(range)
    }

    pub fn doc_rainbow_highlights(
        doc: &Document,
        annotations: &TextAnnotations,
        anchor: usize,
        height: u16,
        theme: &Theme,
        loader: &syntax::Loader,
    ) -> Option<OverlayHighlights> {
        let syntax = doc.syntax()?;
        let text = doc.text().slice(..);
        let row = text.char_to_line(anchor.min(text.len_chars()));
        let visible_range = Self::viewport_byte_range(text, annotations, row, height);
        let start = syntax::child_for_byte_range(
            &syntax.tree().root_node(),
            visible_range.start as u32..visible_range.end as u32,
        )
        .map_or(visible_range.start as u32, |node| node.start_byte());
        let range = start..visible_range.end as u32;

        Some(syntax.rainbow_highlights(text, theme.rainbow_length(), loader, range))
    }

    /// Get highlight spans for document diagnostics.
    /// If `viewport` is provided, only diagnostics overlapping that byte range are processed.
    pub fn doc_diagnostics_highlights_into(
        doc: &Document,
        theme: &Theme,
        overlay_highlights: &mut Vec<OverlayHighlights>,
        viewport: Option<std::ops::Range<usize>>,
    ) {
        use helix_core::diagnostic::{DiagnosticTag, Range, Severity};
        let get_scope_of = |scope| {
            theme
                .find_highlight_exact(scope)
                // get one of the themes below as fallback values
                .or_else(|| theme.find_highlight_exact("diagnostic"))
                .or_else(|| theme.find_highlight_exact("ui.cursor"))
                .or_else(|| theme.find_highlight_exact("ui.selection"))
                .expect(
                    "at least one of the following scopes must be defined in the theme: `diagnostic`, `ui.cursor`, or `ui.selection`",
                )
        };

        // Diagnostic tags
        let unnecessary = theme.find_highlight_exact("diagnostic.unnecessary");
        let deprecated = theme.find_highlight_exact("diagnostic.deprecated");

        let mut default_vec = Vec::new();
        let mut info_vec = Vec::new();
        let mut hint_vec = Vec::new();
        let mut warning_vec = Vec::new();
        let mut error_vec = Vec::new();
        let mut unnecessary_vec = Vec::new();
        let mut deprecated_vec = Vec::new();

        let push_diagnostic = |vec: &mut Vec<ops::Range<usize>>, range: Range| {
            // If any diagnostic overlaps ranges with the prior diagnostic,
            // merge the two together. Otherwise push a new span.
            match vec.last_mut() {
                Some(existing_range) if range.start <= existing_range.end => {
                    // This branch merges overlapping diagnostics, assuming that the current
                    // diagnostic starts on range.start or later. If this assertion fails,
                    // we will discard some part of `diagnostic`. This implies that
                    // `doc.diagnostics()` is not sorted by `diagnostic.range`.
                    debug_assert!(existing_range.start <= range.start);
                    existing_range.end = range.end.max(existing_range.end)
                }
                _ => vec.push(range.start..range.end),
            }
        };

        let diagnostics = doc.diagnostics();

        // Cull diagnostics outside the viewport using binary search.
        // Diagnostics are sorted by range.start, so we can efficiently skip.
        let (diag_start, diag_end) = if let Some(ref vp) = viewport {
            let start = diagnostics.partition_point(|d| d.range.end < vp.start);
            let end = diagnostics.partition_point(|d| d.range.start <= vp.end);
            (start, end)
        } else {
            (0, diagnostics.len())
        };

        for diagnostic in &diagnostics[diag_start..diag_end] {
            // Separate diagnostics into different Vecs by severity.
            let vec = match diagnostic.severity {
                Some(Severity::Info) => &mut info_vec,
                Some(Severity::Hint) => &mut hint_vec,
                Some(Severity::Warning) => &mut warning_vec,
                Some(Severity::Error) => &mut error_vec,
                _ => &mut default_vec,
            };

            // If the diagnostic has tags and a non-warning/error severity, skip rendering
            // the diagnostic as info/hint/default and only render it as unnecessary/deprecated
            // instead. For warning/error diagnostics, render both the severity highlight and
            // the tag highlight.
            if diagnostic.tags.is_empty()
                || matches!(
                    diagnostic.severity,
                    Some(Severity::Warning | Severity::Error)
                )
            {
                push_diagnostic(vec, diagnostic.range);
            }

            for tag in &diagnostic.tags {
                match tag {
                    DiagnosticTag::Unnecessary => {
                        if unnecessary.is_some() {
                            push_diagnostic(&mut unnecessary_vec, diagnostic.range)
                        }
                    }
                    DiagnosticTag::Deprecated => {
                        if deprecated.is_some() {
                            push_diagnostic(&mut deprecated_vec, diagnostic.range)
                        }
                    }
                }
            }
        }

        overlay_highlights.push(OverlayHighlights::Homogeneous {
            highlight: get_scope_of("diagnostic"),
            ranges: default_vec,
        });
        if let Some(highlight) = unnecessary {
            overlay_highlights.push(OverlayHighlights::Homogeneous {
                highlight,
                ranges: unnecessary_vec,
            });
        }
        if let Some(highlight) = deprecated {
            overlay_highlights.push(OverlayHighlights::Homogeneous {
                highlight,
                ranges: deprecated_vec,
            });
        }
        overlay_highlights.extend([
            OverlayHighlights::Homogeneous {
                highlight: get_scope_of("diagnostic.info"),
                ranges: info_vec,
            },
            OverlayHighlights::Homogeneous {
                highlight: get_scope_of("diagnostic.hint"),
                ranges: hint_vec,
            },
            OverlayHighlights::Homogeneous {
                highlight: get_scope_of("diagnostic.warning"),
                ranges: warning_vec,
            },
            OverlayHighlights::Homogeneous {
                highlight: get_scope_of("diagnostic.error"),
                ranges: error_vec,
            },
        ]);
    }

    /// Get highlight spans for selections in a document view.
    fn doc_selection_highlights(
        &self,
        mode: Mode,
        doc: &Document,
        view: &View,
        theme: &Theme,
        cursor_shape_config: &CursorShapeConfig,
        is_terminal_focused: bool,
    ) -> OverlayHighlights {
        let text = doc.text().slice(..);
        let selection = doc.selection(view.id);
        let primary_idx = selection.primary_index();

        let cursorkind = cursor_shape_config.from_mode(mode);
        let cursor_is_block = cursorkind == CursorKind::Block;

        let selection_scope = theme
            .find_highlight_exact("ui.selection")
            .expect("could not find `ui.selection` scope in the theme!");
        let primary_selection_scope = theme
            .find_highlight_exact("ui.selection.primary")
            .unwrap_or(selection_scope);

        let base_cursor_scope = theme
            .find_highlight_exact("ui.cursor")
            .unwrap_or(selection_scope);
        let base_primary_cursor_scope = theme
            .find_highlight("ui.cursor.primary")
            .unwrap_or(base_cursor_scope);

        let cursor_scope = match mode {
            Mode::Insert => theme.find_highlight_exact("ui.cursor.insert"),
            Mode::Select => theme.find_highlight_exact("ui.cursor.select"),
            Mode::Normal => theme.find_highlight_exact("ui.cursor.normal"),
        }
        .unwrap_or(base_cursor_scope);

        let primary_cursor_scope = match mode {
            Mode::Insert => theme.find_highlight_exact("ui.cursor.primary.insert"),
            Mode::Select => theme.find_highlight_exact("ui.cursor.primary.select"),
            Mode::Normal => theme.find_highlight_exact("ui.cursor.primary.normal"),
        }
        .unwrap_or(base_primary_cursor_scope);

        let mut spans = Vec::new();
        for (i, range) in selection.iter().enumerate() {
            let selection_is_primary = i == primary_idx;
            let (cursor_scope, selection_scope) = if selection_is_primary {
                (primary_cursor_scope, primary_selection_scope)
            } else {
                (cursor_scope, selection_scope)
            };

            // Special-case: cursor at end of the rope.
            if range.head == range.anchor && range.head == text.len_chars() {
                if !selection_is_primary || !is_terminal_focused || self.prompt_active {
                    // Primary cursor is drawn by the terminal when focused and no prompt is active
                    // Secondary cursors, unfocused primary cursor, and editor cursor when prompt is active are drawn manually
                    spans.push((cursor_scope, range.head..range.head + 1));
                }
                continue;
            }

            let range = range.min_width_1(text);
            if range.head > range.anchor {
                // Standard case.
                let cursor_start = prev_grapheme_boundary(text, range.head);
                // non block cursors look like they exclude the cursor
                let selection_end =
                    if selection_is_primary && !cursor_is_block && mode != Mode::Insert {
                        range.head
                    } else {
                        cursor_start
                    };
                spans.push((selection_scope, range.anchor..selection_end));
                // add cursors
                // skip primary cursor if terminal is focused and no prompt is active - terminal cursor is used in that case
                if !selection_is_primary || !is_terminal_focused || self.prompt_active {
                    spans.push((cursor_scope, cursor_start..range.head));
                }
            } else {
                // Reverse case.
                let cursor_end = next_grapheme_boundary(text, range.head);
                // add cursors
                // skip primary cursor if terminal is focused and no prompt is active - terminal cursor is used in that case
                if !selection_is_primary || !is_terminal_focused || self.prompt_active {
                    spans.push((cursor_scope, range.head..cursor_end));
                }
                // non block cursors look like they exclude the cursor
                let selection_start = if selection_is_primary
                    && !cursor_is_block
                    && !(mode == Mode::Insert && cursor_end == range.anchor)
                {
                    range.head
                } else {
                    cursor_end
                };
                spans.push((selection_scope, selection_start..range.anchor));
            }
        }

        OverlayHighlights::Heterogenous { highlights: spans }
    }

    /// Render brace match, etc (meant for the focused view only)
    pub fn highlight_focused_view_elements(
        view: &View,
        doc: &Document,
        theme: &Theme,
    ) -> Option<OverlayHighlights> {
        // Highlight matching braces
        let syntax = doc.syntax()?;
        let highlight = theme.find_highlight_exact("ui.cursor.match")?;
        let text = doc.text().slice(..);
        let pos = doc.selection(view.id).primary().cursor(text);
        let pos = helix_core::match_brackets::find_matching_bracket(syntax, text, pos)?;
        Some(OverlayHighlights::single(highlight, pos..pos + 1))
    }

    pub fn tabstop_highlights(doc: &Document, theme: &Theme) -> Option<OverlayHighlights> {
        let snippet = doc.active_snippet()?;
        let highlight = theme.find_highlight_exact("tabstop")?;
        let mut ranges = Vec::new();
        for tabstop in snippet.tabstops() {
            ranges.extend(tabstop.ranges.iter().map(|range| range.start..range.end));
        }
        Some(OverlayHighlights::Homogeneous { highlight, ranges })
    }

    /// Render bufferline at the top
    pub fn render_bufferline(&mut self, editor: &Editor, viewport: Rect, surface: &mut Surface) {
        self.bufferline_positions.clear();
        surface.clear_with(
            viewport,
            editor
                .theme
                .try_get("ui.bufferline.background")
                .unwrap_or_else(|| editor.theme.get("ui.statusline")),
        );

        let bufferline_active = editor
            .theme
            .try_get("ui.bufferline.active")
            .unwrap_or_else(|| editor.theme.get("ui.statusline.active"));

        let bufferline_inactive = editor
            .theme
            .try_get("ui.bufferline")
            .unwrap_or_else(|| editor.theme.get("ui.statusline.inactive"));

        let current_doc = view!(editor).doc;

        self.bufferline_info.clear();

        // First pass: calculate all buffer positions and determine if scrolling is needed
        let mut total_width = 0u16;
        let mut buffer_texts = Vec::new();
        let mut buffer_widths = Vec::new();

        for (idx, doc) in editor.documents().enumerate() {
            let fname = Self::make_document_name(doc, editor);

            // Add separator width if not the first document
            if idx > 0 {
                let sep = &editor.config().bufferline.separator;
                total_width += sep.len() as u16;
            }

            let icons = ICONS.load();

            let text = if let Some(icon) = icons.mime().get(doc.path(), doc.language_name()) {
                format!(
                    " {}  {} {}",
                    icon.glyph(),
                    fname,
                    if doc.is_modified() { "[+] " } else { "" }
                )
            } else {
                format!(" {} {}", fname, if doc.is_modified() { "[+] " } else { "" })
            };

            self.bufferline_positions.push(total_width);
            let text_width = text.len() as u16;
            buffer_texts.push(text);
            buffer_widths.push(text_width);
            total_width += text_width;
        }

        // Determine scroll offset
        let scroll_offset =
            if let Some(current_idx) = editor.documents().position(|d| d.id() == current_doc) {
                if let Some(&target_x) = self.bufferline_positions.get(current_idx) {
                    if target_x >= viewport.width / 2 {
                        target_x
                            .saturating_sub(viewport.width / 2)
                            .min(total_width.saturating_sub(viewport.width))
                    } else {
                        0
                    }
                } else {
                    0
                }
            } else {
                0
            };

        // Second pass: render with the calculated offset
        for (idx, doc) in editor.documents().enumerate() {
            let buffer_x = self.bufferline_positions[idx];
            let text = &buffer_texts[idx];

            // Render separator if not first document
            if idx > 0 {
                let sep = &editor.config().bufferline.separator;
                let sep_x = buffer_x
                    .saturating_sub(sep.len() as u16)
                    .saturating_sub(scroll_offset);
                if sep_x < viewport.width {
                    let render_x = viewport.x + sep_x;
                    surface.set_stringn(
                        render_x,
                        viewport.y,
                        sep,
                        (viewport.width - sep_x) as usize,
                        bufferline_inactive,
                    );
                }
            }

            // Skip buffers that are completely outside the visible area
            let render_x = buffer_x.saturating_sub(scroll_offset);
            if render_x >= viewport.width {
                break;
            }

            // Skip buffers that end before the visible area
            if buffer_x + buffer_widths[idx] < scroll_offset {
                continue;
            }

            let style = if current_doc == doc.id() {
                bufferline_active
            } else {
                bufferline_inactive
            };

            let mut visible_text = text.clone();
            let mut text_start_x = render_x;

            // Clip text if it starts before the visible area
            if buffer_x < scroll_offset {
                let chars_to_skip = (scroll_offset - buffer_x) as usize;
                visible_text = text.chars().skip(chars_to_skip).collect();
                text_start_x = viewport.x;
            }

            let actual_render_x = viewport.x + text_start_x;
            let available_width = viewport.width.saturating_sub(text_start_x);

            surface.set_stringn(
                actual_render_x,
                viewport.y,
                &visible_text,
                available_width as usize,
                style,
            );

            // Track buffer info for mouse clicks (adjust for scroll offset)
            let start_x = actual_render_x;
            let end_x =
                (actual_render_x + visible_text.len() as u16).min(viewport.x + viewport.width);
            self.bufferline_info
                .add_buffer_info(doc.id(), start_x..end_x);
        }
    }

    fn make_document_name(doc: &Document, editor: &Editor) -> String {
        let scratch = PathBuf::from(SCRATCH_BUFFER_NAME); // default filename to use for scratch buffer

        let paths: Vec<String> = editor
            .documents()
            .map(|d| {
                d.path()
                    .unwrap_or(&scratch)
                    .to_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();

        let components: Vec<Vec<String>> = paths
            .iter()
            .map(|p| p.split(MAIN_SEPARATOR).map(String::from).collect())
            .collect();

        let doc_path = doc
            .path()
            .unwrap_or(&scratch)
            .to_str()
            .unwrap_or_default()
            .to_string();

        let doc_index = paths.iter().position(|p| p == &doc_path).unwrap();
        let doc_components_len = components[doc_index].len();

        let mut k = 1;

        loop {
            let start = doc_components_len.saturating_sub(k);
            let curr_doc: Vec<&str> = components[doc_index][start..]
                .iter()
                .map(|s| s.as_str())
                .collect();

            let count = components
                .iter()
                .enumerate()
                .filter(|&(index, _)| index != doc_index)
                .filter(|&(_, parts)| {
                    let len = parts.len();
                    let start = len.saturating_sub(k);
                    parts[start..] == curr_doc
                })
                .count();

            if count == 0 {
                return curr_doc.join(MAIN_SEPARATOR_STR);
            }

            k += 1;
        }
    }

    pub fn render_gutter<'d>(
        editor: &'d Editor,
        doc: &'d Document,
        view: &View,
        viewport: Rect,
        theme: &Theme,
        is_focused: bool,
        decoration_manager: &mut DecorationManager<'d>,
    ) {
        let text = doc.text().slice(..);
        let cursors: Rc<[_]> = doc
            .selection(view.id)
            .iter()
            .map(|range| range.cursor_line(text))
            .collect();

        let mut offset = 0;

        let gutter_style = theme.get("ui.gutter");
        let gutter_selected_style = theme.get("ui.gutter.selected");
        let gutter_style_virtual = theme.get("ui.gutter.virtual");
        let gutter_selected_style_virtual = theme.get("ui.gutter.selected.virtual");

        for gutter_type in view.gutters() {
            let mut gutter = gutter_type.style(editor, doc, view, theme, is_focused);
            let width = gutter_type.width(view, doc);
            // avoid lots of small allocations by reusing a text buffer for each line
            let mut text = String::with_capacity(width);
            let cursors = cursors.clone();
            let gutter_decoration = move |renderer: &mut TextRenderer, pos: LinePos| {
                // TODO handle softwrap in gutters
                let selected = cursors.contains(&pos.doc_line);
                let x = viewport.x + offset;
                let y = pos.visual_line;

                let gutter_style = match (selected, pos.first_visual_line) {
                    (false, true) => gutter_style,
                    (true, true) => gutter_selected_style,
                    (false, false) => gutter_style_virtual,
                    (true, false) => gutter_selected_style_virtual,
                };

                if let Some(style) =
                    gutter(pos.doc_line, selected, pos.first_visual_line, &mut text)
                {
                    renderer.set_stringn(x, y, &text, width, gutter_style.patch(style));
                } else {
                    renderer.set_style(
                        Rect {
                            x,
                            y,
                            width: width as u16,
                            height: 1,
                        },
                        gutter_style,
                    );
                }
                text.clear();
            };
            decoration_manager.add_decoration(gutter_decoration);

            offset += width as u16;
        }
    }

    pub fn render_diagnostics(
        doc: &Document,
        view: &View,
        viewport: Rect,
        surface: &mut Surface,
        theme: &Theme,
    ) {
        use helix_core::diagnostic::Severity;
        use tui::{
            layout::Alignment,
            text::Text,
            widgets::{Paragraph, Widget, Wrap},
        };

        let cursor = doc
            .selection(view.id)
            .primary()
            .cursor(doc.text().slice(..));

        let diagnostics = doc.diagnostics().iter().filter(|diagnostic| {
            diagnostic.range.start <= cursor && diagnostic.range.end >= cursor
        });

        let warning = theme.get("warning");
        let error = theme.get("error");
        let info = theme.get("info");
        let hint = theme.get("hint");

        let mut lines = Vec::new();
        let background_style = theme.get("ui.background");
        for diagnostic in diagnostics {
            let style = Style::reset()
                .patch(background_style)
                .patch(match diagnostic.severity {
                    Some(Severity::Error) => error,
                    Some(Severity::Warning) | None => warning,
                    Some(Severity::Info) => info,
                    Some(Severity::Hint) => hint,
                });
            let text = Text::styled(&diagnostic.message, style);
            lines.extend(text.lines);
            let code = diagnostic.code.as_ref().map(|x| match x {
                NumberOrString::Number(n) => format!("({n})"),
                NumberOrString::String(s) => format!("({s})"),
            });
            if let Some(code) = code {
                let span = Span::styled(code, style);
                lines.push(span.into());
            }
        }

        let text = Text::from(lines);
        let paragraph = Paragraph::new(&text)
            .alignment(Alignment::Right)
            .wrap(Wrap { trim: true });
        let width = 100.min(viewport.width);
        let height = 15.min(viewport.height);
        paragraph.render(
            Rect::new(viewport.right() - width, viewport.y + 1, width, height),
            surface,
        );
    }

    /// Apply the highlighting on the lines where a cursor is active
    pub fn cursorline(doc: &Document, view: &View, theme: &Theme) -> impl Decoration {
        let text = doc.text().slice(..);
        // TODO only highlight the visual line that contains the cursor instead of the full visual line
        let primary_line = doc.selection(view.id).primary().cursor_line(text);

        // The secondary_lines do contain the primary_line, it doesn't matter
        // as the else-if clause in the loop later won't test for the
        // secondary_lines if primary_line == line.
        // It's used inside a loop so the collect isn't needless:
        // https://github.com/rust-lang/rust-clippy/issues/6164
        #[allow(clippy::needless_collect)]
        let secondary_lines: Vec<_> = doc
            .selection(view.id)
            .iter()
            .map(|range| range.cursor_line(text))
            .collect();

        let primary_style = theme.get("ui.cursorline.primary");
        let secondary_style = theme.get("ui.cursorline.secondary");
        let viewport = view.area;

        move |renderer: &mut TextRenderer, pos: LinePos| {
            let area = Rect::new(viewport.x, pos.visual_line, viewport.width, 1);
            if primary_line == pos.doc_line {
                renderer.set_style(area, primary_style);
            } else if secondary_lines.binary_search(&pos.doc_line).is_ok() {
                renderer.set_style(area, secondary_style);
            }
        }
    }

    /// Apply the highlighting on the columns where a cursor is active
    pub fn highlight_cursorcolumn(
        doc: &Document,
        view: &View,
        surface: &mut Surface,
        theme: &Theme,
        viewport: Rect,
        text_annotations: &TextAnnotations,
    ) {
        let text = doc.text().slice(..);

        // Manual fallback behaviour:
        // ui.cursorcolumn.{p/s} -> ui.cursorcolumn -> ui.cursorline.{p/s}
        let primary_style = theme
            .try_get_exact("ui.cursorcolumn.primary")
            .or_else(|| theme.try_get_exact("ui.cursorcolumn"))
            .unwrap_or_else(|| theme.get("ui.cursorline.primary"));
        let secondary_style = theme
            .try_get_exact("ui.cursorcolumn.secondary")
            .or_else(|| theme.try_get_exact("ui.cursorcolumn"))
            .unwrap_or_else(|| theme.get("ui.cursorline.secondary"));

        let inner_area = view.inner_area(doc);

        let selection = doc.selection(view.id);
        let view_offset = doc.view_offset(view.id);
        let primary = selection.primary();
        let text_format = doc.text_format(viewport.width, None);
        for range in selection.iter() {
            let is_primary = primary == *range;
            let cursor = range.cursor(text);

            let Position { col, .. } =
                visual_offset_from_block(text, cursor, cursor, &text_format, text_annotations).0;

            // if the cursor is horizontally in the view
            if col >= view_offset.horizontal_offset
                && inner_area.width > (col - view_offset.horizontal_offset) as u16
            {
                let area = Rect::new(
                    inner_area.x + (col - view_offset.horizontal_offset) as u16,
                    view.area.y,
                    1,
                    view.area.height,
                );
                if is_primary {
                    surface.set_style(area, primary_style)
                } else {
                    surface.set_style(area, secondary_style)
                }
            }
        }
    }

    /// Unified key dispatch: resolve keymap once, route to frontend or engine.
    ///
    /// Flow:
    /// 1. Engine `pre_resolve` — count/register/dot-repeat (no keymap needed)
    /// 2. If not consumed, resolve keymap ONCE
    /// 3. If frontend result → execute frontend command
    /// 4. Otherwise → convert to `KeymapLookup`, pass to engine's `process_lookup`
    fn dispatch_key(&mut self, cx: &mut commands::Context, key: KeyEvent) {
        let dispatch_start = std::time::Instant::now();
        self.sync_context_from_engine(cx);
        let mode_before = cx.editor.mode();

        // Resolve the editing context from the focused view.
        let focus = cx.editor.tree.focus;
        let focused_view = cx.editor.tree.get(focus);
        let view_id = focused_view.id;
        let doc_id = focused_view.doc;

        // Step 1: Engine pre-resolve (count, register, dot-repeat, escape).
        let mut engine = self.engine.take().expect("engine is always present");
        let pre_resolve_start = std::time::Instant::now();
        if let Some(result) = engine.pre_resolve(cx.editor, view_id, doc_id, &self.keymaps, key) {
            helix_view::bench::log_run_phase(
                "editor_dispatch",
                "pre_resolve",
                pre_resolve_start.elapsed(),
                || {
                    format!(
                        "key={} mode_before={:?} view_id={:?} doc_id={:?} consumed=true",
                        key.key_sequence_format(),
                        mode_before,
                        view_id,
                        doc_id
                    )
                },
            );
            self.engine = Some(engine);
            self.handle_engine_result(cx, key, result, mode_before);
            self.sync_context_from_engine(cx);
            self.publish_focused_modal_input(cx.editor);
            helix_view::bench::log_run_phase(
                "editor_dispatch",
                "total",
                dispatch_start.elapsed(),
                || format!("key={} path=pre_resolve", key.key_sequence_format()),
            );
            return;
        }
        helix_view::bench::log_run_phase(
            "editor_dispatch",
            "pre_resolve",
            pre_resolve_start.elapsed(),
            || {
                format!(
                    "key={} mode_before={:?} view_id={:?} doc_id={:?} consumed=false",
                    key.key_sequence_format(),
                    mode_before,
                    view_id,
                    doc_id
                )
            },
        );
        self.engine = Some(engine);

        // Step 2: Resolve keymap ONCE.
        let mode = cx.editor.mode();
        let keymap_start = std::time::Instant::now();
        let result = self.keymaps.get(mode, key);
        helix_view::bench::log_run_phase(
            "editor_dispatch",
            "keymap_get",
            keymap_start.elapsed(),
            || format!("key={} mode={:?}", key.key_sequence_format(), mode),
        );

        let is_frontend = crate::keymap::is_frontend_result(&result);

        // Step 3: If frontend result → execute frontend command.
        if is_frontend {
            // Reset engine pending state since frontend is taking over.
            let mut engine = self.engine.take().expect("engine is always present");
            if engine.is_pending() {
                engine.reset();
            }
            self.engine = Some(engine);

            // Update autoinfo from sticky keymap (clears stale Pending infobox).
            cx.editor.autoinfo = self.keymaps.sticky_infobox();

            let mut cmd_name: Option<&'static str> = None;
            match result {
                crate::keymap::KeymapResult::Pending(node) => {
                    cx.editor.autoinfo = Some(node.infobox());
                }
                crate::keymap::KeymapResult::Matched(cmd) => {
                    cmd_name = cmd.static_name();
                    self.execute_frontend_command(cx, &cmd);
                }
                crate::keymap::KeymapResult::MatchedSequence(cmds) => {
                    if let Some(first) = cmds.first() {
                        cmd_name = first.static_name();
                    }
                    for cmd in &cmds {
                        self.execute_frontend_command(cx, cmd);
                    }
                }
                crate::keymap::KeymapResult::NotFound
                | crate::keymap::KeymapResult::Cancelled(_)
                | crate::keymap::KeymapResult::Fallback(_, _) => unreachable!(),
            }

            self.handle_mode_change(cx, mode_before, cmd_name.or(Some("unknown")));
            self.sync_engine_from_context(cx);
            self.publish_focused_modal_input(cx.editor);
            helix_view::bench::log_run_phase(
                "editor_dispatch",
                "total",
                dispatch_start.elapsed(),
                || format!("key={} path=frontend", key.key_sequence_format()),
            );
            return;
        }

        // Step 4: Convert to engine KeymapLookup and process.
        let lookup = crate::keymap::resolve_keymap_result(&result);

        let mut engine = self.engine.take().expect("engine is always present");
        let process_start = std::time::Instant::now();
        let engine_result =
            engine.process_lookup(cx.editor, view_id, doc_id, &mut self.keymaps, key, lookup);
        helix_view::bench::log_run_phase(
            "editor_dispatch",
            "process_lookup",
            process_start.elapsed(),
            || {
                format!(
                    "key={} mode={:?} view_id={:?} doc_id={:?}",
                    key.key_sequence_format(),
                    mode,
                    view_id,
                    doc_id
                )
            },
        );
        self.engine = Some(engine);

        self.handle_engine_result(cx, key, engine_result, mode_before);
        self.sync_context_from_engine(cx);
        self.publish_focused_modal_input(cx.editor);
        helix_view::bench::log_run_phase(
            "editor_dispatch",
            "total",
            dispatch_start.elapsed(),
            || format!("key={} path=engine", key.key_sequence_format()),
        );
    }

    fn handle_engine_result(
        &mut self,
        cx: &mut commands::Context,
        key: KeyEvent,
        result: helix_view::engine::EngineResult,
        mode_before: Mode,
    ) {
        use helix_view::engine::EngineResult;

        match result {
            EngineResult::Executed => {
                self.handle_mode_change(cx, mode_before, None);
            }
            EngineResult::Pending => {
                // Engine consumed the key, waiting for more input.
            }
            EngineResult::InsertChar(ch) => {
                commands::insert::insert_char(cx, ch);
            }
            EngineResult::CancelledInsert(pending_keys) => {
                for ev in pending_keys.iter() {
                    if let Some(ch) = ev.char() {
                        commands::insert::insert_char(cx, ch);
                    }
                }
            }
            EngineResult::Unbound => {
                let is_synthetic_null = matches!(key.code, KeyCode::Null | KeyCode::Char('\0'));
                if !is_synthetic_null {
                    log::warn!("unbound key: {}", key.key_sequence_format());
                }
            }
            EngineResult::ReplayInsert {
                entry_command,
                keys,
            } => {
                self.replay_insert(cx, &entry_command, &keys);
            }
        }
    }

    /// Replay a recorded insert sequence for dot-repeat.
    fn replay_insert(
        &mut self,
        cx: &mut commands::Context,
        entry_command: &str,
        keys: &[KeyEvent],
    ) {
        if let Some(cmd) = commands::MappableCommand::builtin_commands()
            .iter()
            .find(|cmd| cmd.name() == entry_command)
        {
            let mode_before = cx.editor.mode();
            self.sync_context_from_engine(cx);
            cmd.execute(cx);
            self.sync_engine_from_context(cx);
            let mode_after = cx.editor.mode();
            if mode_after != mode_before {
                helix_event::dispatch(OnModeSwitch {
                    old_mode: mode_before,
                    new_mode: mode_after,
                    cx,
                });
            }
        } else {
            log::warn!("replay_insert: unknown entry command '{}'", entry_command);
            return;
        }

        for &key in keys {
            if cx.editor.mode() != Mode::Insert {
                break;
            }
            self.dispatch_key(cx, key);
        }

        if cx.editor.mode() == Mode::Insert {
            cx.editor.enter_normal_mode();
        }
    }

    fn execute_frontend_command(
        &mut self,
        cx: &mut commands::Context,
        command: &commands::MappableCommand,
    ) {
        self.sync_context_from_engine(cx);
        command.execute(cx);
        self.sync_engine_from_context(cx);

        if let Some(static_command) = commands::MappableCommand::builtin_commands()
            .iter()
            .find(|candidate| candidate.name() == command.name())
        {
            helix_event::dispatch(PostCommand {
                command: static_command,
                cx,
            });
        }
    }

    /// Track mode changes, fire events, and manage insert recording for dot-repeat.
    ///
    /// `command_name` is the name of the command that triggered the mode change,
    /// used as the entry command for insert recording. Pass `None` when the command
    /// name is not known (e.g., engine-dispatched mode changes).
    fn handle_mode_change(
        &mut self,
        cx: &mut commands::Context,
        mode_before: Mode,
        command_name: Option<&str>,
    ) {
        let mode_after = cx.editor.mode();
        if mode_after != mode_before {
            helix_event::dispatch(OnModeSwitch {
                old_mode: mode_before,
                new_mode: mode_after,
                cx,
            });

            let engine = self.engine.as_mut().expect("engine is always present");

            if mode_after == Mode::Insert && mode_before != Mode::Insert {
                // Entering insert mode — start recording for dot-repeat.
                let entry = command_name
                    .map(|n| std::borrow::Cow::Owned(n.to_string()))
                    .unwrap_or(std::borrow::Cow::Borrowed("insert_mode"));
                engine.begin_insert_recording(entry);
            } else if mode_before == Mode::Insert && mode_after != Mode::Insert {
                // Leaving insert mode — finalize recording.
                engine.end_insert_recording();
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_completion(
        &mut self,
        editor: &mut Editor,
        items: Vec<CompletionItem>,
        trigger_offset: usize,
        size: Rect,
    ) -> Option<Rect> {
        let mut completion = Completion::new(editor, items, trigger_offset);

        if completion.is_empty() {
            // skip if we got no completion results
            return None;
        }

        let area = completion.area(size, editor);
        editor.last_completion = Some(CompleteAction::Triggered);

        // TODO : propagate required size on resize to completion too
        self.completion = Some(completion);
        Some(area)
    }

    pub fn clear_completion(&mut self, editor: &mut Editor) -> Option<OnKeyCallback> {
        if let Some(ref completion) = self.completion {
            completion.remove_model_layer(editor);
        }
        self.completion = None;
        let mut on_next_key: Option<OnKeyCallback> = None;
        editor.handlers.completions.request_controller.restart();
        editor.handlers.completions.active_completions.clear();
        if let Some(last_completion) = editor.last_completion.take() {
            match last_completion {
                CompleteAction::Triggered => (),
                CompleteAction::Applied {
                    trigger_offset: _,
                    changes: _,
                    placeholder,
                } => {
                    on_next_key = placeholder.then_some(Box::new(|cx, key| {
                        if let Some(c) = key.char() {
                            let (view_id, doc) = focused!(cx.editor);
                            if let Some(snippet) = doc.active_snippet() {
                                doc.apply(&snippet.delete_placeholder(doc.text()), view_id);
                            }
                            commands::insert::insert_char(cx, c);
                        }
                    }))
                }
                CompleteAction::Selected { savepoint } => {
                    let (view_id, doc) = focused!(editor);
                    let view = view_mut!(editor, view_id);
                    doc.restore(view, &savepoint, false);
                }
            }
        }
        on_next_key
    }

    pub fn handle_idle_timeout(&mut self, cx: &mut commands::Context) -> EventResult {
        commands::compute_inlay_hints_for_all_views(cx.editor, cx.jobs);

        EventResult::Ignored(None)
    }
}

impl EditorView {
    /// must be called whenever the editor processed input that
    /// is not a `KeyEvent`. In these cases any pending keys/on next
    /// key callbacks must be canceled.
    fn handle_non_key_input(&mut self, cxt: &mut commands::Context) {
        cxt.editor.status_msg = None;
        cxt.editor.reset_idle_timer();
        // HACKS: create a fake key event that will never trigger any actual map
        // and therefore simply acts as "dismiss"
        let null_key_event = KeyEvent {
            code: KeyCode::Null,
            modifiers: KeyModifiers::empty(),
        };
        // dismiss any pending keys
        if let Some((on_next_key, _)) = self.on_next_key.take() {
            on_next_key(cxt, null_key_event);
        }
        // Feed the null key through the engine to dismiss any pending keymap state
        self.dispatch_key(cxt, null_key_event);
        self.pseudo_pending.clear();
    }

    fn handle_mouse_event(
        &mut self,
        event: &MouseEvent,
        cxt: &mut commands::Context,
    ) -> EventResult {
        if event.kind != MouseEventKind::Moved {
            self.handle_non_key_input(cxt)
        }

        let config = cxt.editor.config();
        let MouseEvent {
            kind,
            row,
            column,
            modifiers,
            ..
        } = *event;

        let pos_and_view = |editor: &Editor, row, column, ignore_virtual_text| {
            editor.tree.views().find_map(|(view, _focus)| {
                view.pos_at_screen_coords(
                    &editor.documents[&view.doc],
                    row,
                    column,
                    ignore_virtual_text,
                )
                .map(|pos| (pos, view.id))
            })
        };

        let gutter_coords_and_view = |editor: &Editor, row, column| {
            editor.tree.views().find_map(|(view, _focus)| {
                view.gutter_coords_at_screen_coords(row, column)
                    .map(|coords| (coords, view.id))
            })
        };

        match kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let editor = &mut cxt.editor;

                let config = editor.config();
                let bufferline_visible = match config.bufferline.render_mode {
                    helix_view::editor::BufferLineRenderMode::Always => true,
                    helix_view::editor::BufferLineRenderMode::Multiple => {
                        editor.documents.len() > 1
                    }
                    _ => false,
                };
                if bufferline_visible && row == 0 {
                    if let Some(buffer_info) = self.bufferline_info.get_clicked_buffer(column) {
                        editor.switch(buffer_info.document_id, helix_view::editor::Action::Replace);
                    }

                    return EventResult::Consumed(None);
                }

                if let Some((pos, view_id)) = pos_and_view(editor, row, column, true) {
                    editor.focus(view_id);

                    let prev_view_id = view!(editor).id;
                    let doc = doc_mut!(editor, &view!(editor, view_id).doc);

                    if modifiers == KeyModifiers::ALT {
                        let selection = doc.selection(view_id).clone();
                        doc.set_selection(view_id, selection.push(Range::point(pos)));
                    } else if editor.mode == Mode::Select {
                        // Discards non-primary selections for consistent UX with normal mode
                        let primary = doc.selection(view_id).primary().put_cursor(
                            doc.text().slice(..),
                            pos,
                            true,
                        );
                        editor.mouse_down_range = Some(primary);
                        doc.set_selection(view_id, Selection::single(primary.anchor, primary.head));
                    } else {
                        doc.set_selection(view_id, Selection::point(pos));
                    }

                    if view_id != prev_view_id {
                        self.clear_completion(editor);
                    }

                    editor.ensure_cursor_in_view(view_id);

                    return EventResult::Consumed(None);
                }

                if let Some((coords, view_id)) = gutter_coords_and_view(editor, row, column) {
                    editor.focus(view_id);

                    let (view_id, doc) = focused!(cxt.editor);
                    let view = view!(cxt.editor, view_id);

                    let path = match doc.path() {
                        Some(path) => path.clone(),
                        None => return EventResult::Ignored(None),
                    };

                    if let Some(char_idx) =
                        view.pos_at_visual_coords(doc, coords.row as u16, coords.col as u16, true)
                    {
                        let line = doc.text().char_to_line(char_idx);
                        commands::dap_toggle_breakpoint_impl(cxt, path, line);
                        return EventResult::Consumed(None);
                    }
                }

                EventResult::Ignored(None)
            }

            MouseEventKind::Drag(MouseButton::Left) => {
                let (view_id, doc) = focused!(cxt.editor);
                let view = view!(cxt.editor, view_id);

                let pos = match view.pos_at_screen_coords(doc, row, column, true) {
                    Some(pos) => pos,
                    None => return EventResult::Ignored(None),
                };

                let mut selection = doc.selection(view_id).clone();
                let primary = selection.primary_mut();
                *primary = primary.put_cursor(doc.text().slice(..), pos, true);
                doc.set_selection(view_id, selection);
                cxt.editor.ensure_cursor_in_view(view_id);
                EventResult::Consumed(None)
            }

            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let previous_view = cxt.editor.tree.focus;

                let direction = match event.kind {
                    MouseEventKind::ScrollUp => Direction::Backward,
                    MouseEventKind::ScrollDown => Direction::Forward,
                    _ => unreachable!(),
                };

                let scrolled_view = match pos_and_view(cxt.editor, row, column, false) {
                    Some((_, view_id)) => {
                        cxt.editor.tree.focus = view_id;
                        view_id
                    }
                    None => return EventResult::Ignored(None),
                };

                let offset = config.scroll_lines.unsigned_abs();
                commands::scroll(cxt, offset, direction, false);

                restore_focus_after_mouse_scroll(cxt.editor, previous_view, scrolled_view);

                EventResult::Consumed(None)
            }

            MouseEventKind::Up(MouseButton::Left) => {
                if !config.middle_click_paste {
                    return EventResult::Ignored(None);
                }

                let (view_id, doc) = focused!(cxt.editor);

                let should_yank = match cxt.editor.mouse_down_range.take() {
                    Some(down_range) => doc.selection(view_id).primary() != down_range,
                    None => {
                        // This should not happen under normal cases. We fall back to the original
                        // behavior of yanking on non-single-char selections.
                        doc.selection(view_id)
                            .primary()
                            .slice(doc.text().slice(..))
                            .len_chars()
                            > 1
                    }
                };

                if should_yank {
                    commands::MappableCommand::builtin_named(
                        "yank_main_selection_to_primary_clipboard",
                    )
                    .execute(cxt);
                    EventResult::Consumed(None)
                } else {
                    EventResult::Ignored(None)
                }
            }

            MouseEventKind::Up(MouseButton::Right) => {
                if let Some((pos, view_id)) = gutter_coords_and_view(cxt.editor, row, column) {
                    cxt.editor.focus(view_id);

                    if let Some((pos, _)) = pos_and_view(cxt.editor, row, column, true) {
                        focused!(cxt.editor)
                            .1
                            .set_selection(view_id, Selection::point(pos));
                    } else {
                        let (view_id, doc) = focused!(cxt.editor);
                        let view = view!(cxt.editor, view_id);

                        if let Some(pos) = view.pos_at_visual_coords(doc, pos.row as u16, 0, true) {
                            doc.set_selection(view_id, Selection::point(pos));
                            match modifiers {
                                KeyModifiers::ALT => {
                                    commands::MappableCommand::builtin_named("dap_edit_log")
                                        .execute(cxt)
                                }
                                _ => commands::MappableCommand::builtin_named("dap_edit_condition")
                                    .execute(cxt),
                            };
                        }
                    }

                    cxt.editor.ensure_cursor_in_view(view_id);
                    return EventResult::Consumed(None);
                }
                EventResult::Ignored(None)
            }

            MouseEventKind::Up(MouseButton::Middle) => {
                let editor = &mut cxt.editor;
                if !config.middle_click_paste {
                    return EventResult::Ignored(None);
                }

                if modifiers == KeyModifiers::ALT {
                    commands::MappableCommand::builtin_named(
                        "replace_selections_with_primary_clipboard",
                    )
                    .execute(cxt);

                    return EventResult::Consumed(None);
                }

                if let Some((pos, view_id)) = pos_and_view(editor, row, column, true) {
                    let doc = doc_mut!(editor, &view!(editor, view_id).doc);
                    doc.set_selection(view_id, Selection::point(pos));
                    cxt.editor.focus(view_id);
                    commands::MappableCommand::named("paste_primary_clipboard_before")
                        .expect("engine command must exist")
                        .execute(cxt);

                    return EventResult::Consumed(None);
                }

                EventResult::Ignored(None)
            }

            _ => EventResult::Ignored(None),
        }
    }
    fn on_next_key(
        &mut self,
        kind: OnKeyCallbackKind,
        ctx: &mut commands::Context,
        event: KeyEvent,
    ) -> bool {
        if let Some((on_next_key, kind_)) = self.on_next_key.take() {
            if kind == kind_ {
                on_next_key(ctx, event);
                true
            } else {
                self.on_next_key = Some((on_next_key, kind_));
                false
            }
        } else {
            false
        }
    }
}

impl Component for EditorView {
    fn handle_event(
        &mut self,
        event: &Event,
        context: &mut crate::compositor::Context,
    ) -> EventResult {
        self.publish_focused_modal_input(context.editor);
        let mut cx = commands::Context {
            editor: context.editor,
            registry: self.registry.clone(),
            count: self.engine_input_state().count,
            register: self.engine_input_state().selected_register,
            callback: Vec::new(),
            on_next_key_callback: None,
            jobs: context.jobs,
            plugin_manager: context.plugin_manager.clone(),
        };

        match event {
            Event::Paste(contents) => {
                self.handle_non_key_input(&mut cx);
                commands::paste_bracketed_value(&mut cx, contents.clone());
                cx.count = None;
                self.sync_engine_from_context(&mut cx);

                let config = cx.editor.config();
                let mode = cx.editor.mode();
                let (view_id, doc) = focused!(cx.editor);
                let view = view_mut!(cx.editor, view_id);
                view.ensure_cursor_in_view(doc, config.scrolloff);

                // Store a history state if not in insert mode. Otherwise wait till we exit insert
                // to include any edits to the paste in the history state.
                if mode != Mode::Insert {
                    doc.append_changes_to_history(view);
                }

                EventResult::Consumed(None)
            }
            Event::Resize(_width, _height) => {
                // Ignore this event, we handle resizing just before rendering to screen.
                // Handling it here but not re-rendering will cause flashing
                self.render_cache.entries.clear();
                EventResult::Consumed(None)
            }
            Event::Key(key) => {
                let key = *key;
                let key_dispatch_start = std::time::Instant::now();
                cx.editor.reset_idle_timer();
                // Key is already canonicalized by the compositor.

                // clear status
                cx.editor.status_msg = None;

                let mode = cx.editor.mode();

                self.sync_context_from_engine(&mut cx);
                if !self.on_next_key(OnKeyCallbackKind::PseudoPending, &mut cx, key) {
                    if mode == Mode::Insert {
                        // Let completion swallow the event first
                        let mut consumed = false;
                        if let Some(completion) = &mut self.completion {
                            let res = {
                                let mut cx = Context {
                                    editor: cx.editor,
                                    jobs: cx.jobs,
                                    scroll: None,
                                    plugin_manager: cx.plugin_manager.clone(),
                                };
                                if let EventResult::Consumed(callback) =
                                    completion.handle_event(event, &mut cx)
                                {
                                    consumed = true;
                                    Some(callback)
                                } else if let EventResult::Consumed(callback) =
                                    completion.handle_event(&Event::Key(key!(Enter)), &mut cx)
                                {
                                    Some(callback)
                                } else {
                                    None
                                }
                            };
                            if let Some(callback) = res {
                                if callback.is_some() {
                                    if let Some(cb) = self.clear_completion(cx.editor) {
                                        if consumed {
                                            cx.on_next_key_callback =
                                                Some((cb, OnKeyCallbackKind::Fallback))
                                        } else {
                                            self.on_next_key =
                                                Some((cb, OnKeyCallbackKind::Fallback));
                                        }
                                    }
                                }
                            }
                        }
                        if !consumed {
                            let dispatch_start = std::time::Instant::now();
                            self.dispatch_key(&mut cx, key);
                            helix_view::bench::log_run_phase(
                                "editor_key",
                                "dispatch_key",
                                dispatch_start.elapsed(),
                                || format!("key={} mode={:?}", key.key_sequence_format(), mode),
                            );
                        }
                    } else {
                        let dispatch_start = std::time::Instant::now();
                        self.dispatch_key(&mut cx, key);
                        helix_view::bench::log_run_phase(
                            "editor_key",
                            "dispatch_key",
                            dispatch_start.elapsed(),
                            || format!("key={} mode={:?}", key.key_sequence_format(), mode),
                        );
                    }
                } else {
                    self.sync_engine_from_context(&mut cx);
                }

                self.on_next_key = cx.on_next_key_callback.take();
                match self.on_next_key {
                    Some((_, OnKeyCallbackKind::PseudoPending)) => self.pseudo_pending.push(key),
                    _ => self.pseudo_pending.clear(),
                }

                // appease borrowck
                let callbacks = take(&mut cx.callback);

                // if the command consumed the last view, skip the render.
                // on the next loop cycle the Application will then terminate.
                if cx.editor.should_close() {
                    return EventResult::Ignored(None);
                }

                let config = cx.editor.config();
                let mode = cx.editor.mode();
                let (view_id, doc) = focused!(cx.editor);
                let view = view_mut!(cx.editor, view_id);

                let ensure_cursor_start = std::time::Instant::now();
                view.ensure_cursor_in_view(doc, config.scrolloff);
                helix_view::bench::log_run_phase(
                    "editor_key",
                    "ensure_cursor_in_view",
                    ensure_cursor_start.elapsed(),
                    || {
                        format!(
                            "key={} mode={:?} view_id={:?} doc_id={:?}",
                            key.key_sequence_format(),
                            mode,
                            view_id,
                            doc.id()
                        )
                    },
                );

                // Store a history state if not in insert mode. This also takes care of
                // committing changes when leaving insert mode.
                if mode != Mode::Insert {
                    let history_start = std::time::Instant::now();
                    doc.append_changes_to_history(view);
                    helix_view::bench::log_run_phase(
                        "editor_key",
                        "append_changes_to_history",
                        history_start.elapsed(),
                        || {
                            format!(
                                "key={} mode={:?} view_id={:?} doc_id={:?}",
                                key.key_sequence_format(),
                                mode,
                                view_id,
                                doc.id()
                            )
                        },
                    );
                }
                let callback = if callbacks.is_empty() {
                    None
                } else {
                    let callback: crate::compositor::Callback = Box::new(move |compositor, cx| {
                        for callback in callbacks {
                            callback(compositor, cx)
                        }
                    });
                    Some(callback)
                };

                helix_view::bench::log_run_phase(
                    "editor_key",
                    "total",
                    key_dispatch_start.elapsed(),
                    || format!("key={} mode={:?}", key.key_sequence_format(), mode),
                );

                EventResult::Consumed(callback)
            }

            Event::Mouse(event) => self.handle_mouse_event(event, &mut cx),
            Event::IdleTimeout => self.handle_idle_timeout(&mut cx),
            Event::FocusGained => {
                self.terminal_focused = true;
                self.render_cache.entries.clear();
                EventResult::Consumed(None)
            }
            Event::FocusLost => {
                if context.editor.config().auto_save.focus_lost {
                    let options = commands::WriteAllOptions {
                        force: false,
                        write_scratch: false,
                        auto_format: false,
                    };
                    if let Err(e) = commands::typed::write_all_impl(context, options) {
                        context.editor.set_error(format!("{}", e));
                    }
                }
                self.terminal_focused = false;
                self.render_cache.entries.clear();
                EventResult::Consumed(None)
            }
        }
    }

    fn sync(&mut self, editor: &mut Editor) {
        if editor.model.focus == FocusTarget::Editor {
            self.publish_focused_modal_input(editor);
        }

        // Pre-resolve completion item so the render phase doesn't need &mut Editor.
        if let Some(completion) = self.completion.as_mut() {
            completion.resolve_selected_item(editor);
        }
    }

    fn layout_role(&self) -> crate::compositor::LayoutRole {
        crate::compositor::LayoutRole::Fill
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &RenderContext) {

        // clear with background color
        static FRAME_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let frame_num = FRAME_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let render_start = std::time::Instant::now();
        let clear_start = std::time::Instant::now();
        surface.set_style(area, cx.editor.theme.get("ui.background"));
        helix_view::bench::log_run_phase("editor_render", "clear_background", clear_start.elapsed(), || {
            format!("area={}x{}", area.width, area.height)
        });
        let config = cx.editor.config();

        // check if bufferline should be rendered
        use helix_view::editor::BufferLineRenderMode;
        let use_bufferline = match config.bufferline.render_mode {
            BufferLineRenderMode::Always => true,
            BufferLineRenderMode::Multiple if cx.editor.documents.len() > 1 => true,
            _ => false,
        };

        // NOTE: editor.resize(editor_area) is now done in compositor pre-render.

        if use_bufferline {
            let bufferline_start = std::time::Instant::now();
            let bufferline_area = area.with_height(1);
            let mut output = crate::render::RenderOutput::new(bufferline_area);
            self.render_bufferline(cx.editor, bufferline_area, &mut output.surface);
            let prepared = PreparedRender::ready(output);
            self.chrome_cache.compose(prepared, surface);
            helix_view::bench::log_run_phase(
                "editor_render",
                "bufferline",
                bufferline_start.elapsed(),
                || format!("area={}x{}", area.width, 1),
            );
        }

        // Evict cache entries for views that no longer exist.
        {
            let active: std::collections::HashSet<ViewId> =
                cx.editor.tree.views().map(|(v, _)| v.id).collect();
            self.render_cache
                .entries
                .retain(|id, _| active.contains(id));
            self.chrome_cache
                .retain(|id| active.iter().any(|view_id| statusline::cache_id(*view_id) == id));
        }

        #[cfg(debug_assertions)]
        {
            self.render_cache.frames = self.render_cache.frames.wrapping_add(1);
        }

        log::warn!(
            "[editor_render] area=({},{} {}x{}) views={}",
            area.x, area.y, area.width, area.height,
            cx.editor.tree.views().count(),
        );
        for (view, is_focused) in cx.editor.tree.views() {
            let view_render_start = std::time::Instant::now();
            log::warn!(
                "[view] id={:?} focused={} area=({},{} {}x{}) inner_height={} statusline_row={}",
                view.id, is_focused,
                view.area.x, view.area.y, view.area.width, view.area.height,
                view.area.height.saturating_sub(1),
                view.area.y + view.area.height.saturating_sub(1),
            );
            let doc = cx.editor.document(view.doc).unwrap();

            let primary_cursor = doc
                .selection(view.id)
                .primary()
                .cursor(doc.text().slice(..));
            let cursor_line = doc.text().char_to_line(primary_cursor);

            let current_content = ViewContentState {
                doc_id: view.doc,
                doc_version: doc.version(),
                view_position: doc.view_offset(view.id),
                area: view.area,
                theme_name: Arc::from(cx.editor.theme.name()),
                config_gen: cx.editor.config_gen,
                annotation_gen: doc.annotation_gen(),
                cursor_line,
            };

            // Content hit — content unchanged, compute dirty lines from overlay fingerprints.
            // When 0 lines are dirty, this is a pure blit (zero extra work).
            if let Some(cached) = self.render_cache.entries.get(&view.id) {
                if cached.content_state == current_content {
                    let cached_syntax = cached.syntax_styles.clone();
                    let cached_line_map = cached.line_map.clone();
                    let old_fingerprints = cached.overlay_fingerprints.clone();

                    let cached_linemap_count = cached_line_map.lines.len();

                    // Blit entire cached surface as baseline
                    let blit_start = std::time::Instant::now();
                    Self::blit(&cached.cells, surface);
                    helix_view::bench::log_run_phase(
                        "editor_render_view",
                        "blit",
                        blit_start.elapsed(),
                        || format!("view_id={:?} area={}x{}", view.id, view.area.width, view.area.height),
                    );
                    update_cursor_cache_from_line_map(cx.editor, doc, view, &cached_line_map);

                    // Compute new per-line overlay fingerprints
                    let selection = doc.selection(view.id);
                    let fp_start = std::time::Instant::now();
                    let new_fingerprints = cached_line_map.overlay_fingerprints(
                        selection,
                        cx.editor.mode,
                        is_focused,
                        self.terminal_focused,
                    );
                    helix_view::bench::log_run_phase(
                        "editor_render_view",
                        "overlay_fingerprints",
                        fp_start.elapsed(),
                        || format!("view_id={:?} rows={}", view.id, cached_line_map.lines.len()),
                    );

                    // Find which visual rows actually changed
                    let dirty_start = std::time::Instant::now();
                    let dirty = LineMap::dirty_rows(&old_fingerprints, &new_fingerprints);
                    helix_view::bench::log_run_phase(
                        "editor_render_view",
                        "dirty_rows",
                        dirty_start.elapsed(),
                        || format!("view_id={:?} old={} new={}", view.id, old_fingerprints.len(), new_fingerprints.len()),
                    );

                    // Log selection + which linemap rows it touches
                    {
                        let sel = selection.primary();
                        let (sel_start, sel_end) = if sel.anchor <= sel.head {
                            (sel.anchor, sel.head)
                        } else {
                            (sel.head, sel.anchor)
                        };
                        let intersecting_rows: Vec<u16> = cached_line_map
                            .lines
                            .iter()
                            .filter(|l| {
                                sel_start < l.char_range_end && sel_end >= l.char_range_start
                            })
                            .map(|l| l.visual_row)
                            .collect();
                        log::info!(
                            "F{} CACHE HIT sel=({},{}) dirty={:?} sel_rows={:?} lines={}",
                            frame_num,
                            sel.anchor,
                            sel.head,
                            dirty,
                            intersecting_rows,
                            cached_linemap_count,
                        );
                    }

                    #[cfg(debug_assertions)]
                    {
                        self.render_cache.hits = self.render_cache.hits.wrapping_add(1);
                        self.render_cache.dirty_lines = self
                            .render_cache
                            .dirty_lines
                            .wrapping_add(dirty.len() as u64);
                        let total_lines = cached_line_map.lines.len() as u64;
                        self.render_cache.clean_lines = self
                            .render_cache
                            .clean_lines
                            .wrapping_add(total_lines.saturating_sub(dirty.len() as u64));
                    }

                    if dirty.is_empty() {
                        // Nothing changed — pure blit of content. Statusline
                        // is batched with all other views below.
                        log::info!("F{} CACHE PURE_BLIT view={:?}", frame_num, view.id);
                        if let Some(entry) = self.render_cache.entries.get_mut(&view.id) {
                            entry.overlay_fingerprints = new_fingerprints;
                        }
                        continue;
                    }

                    // Clear dirty rows before re-rendering.  Blitted cells may
                    // carry stale backgrounds (e.g. old selection highlight) that
                    // set_style(bg: None) wouldn't overwrite.
                    {
                        let inner = view.inner_area(doc);
                        for &row in &dirty {
                            if row < inner.height {
                                let y = inner.y + row;
                                let row_rect = Rect::new(inner.x, y, inner.width, 1);
                                surface.clear(row_rect);
                            }
                        }
                    }

                    // Re-render only dirty lines using cached syntax styles
                    let vctx = ViewRenderContext {
                        editor: cx.editor,
                        doc,
                        view,
                        viewport: area,
                        is_focused,
                        cached_syntax: Some(&cached_syntax),
                        dirty_rows: Some(&dirty),
                        seed_line_map: Some(&cached_line_map),
                    };
                    let render_output = self.render_view(&vctx, surface);
                    helix_view::bench::log_run_phase(
                        "editor_render_view",
                        "dirty_render_view",
                        render_start.elapsed(),
                        || format!("view_id={:?} dirty_rows={}", view.id, dirty.len()),
                    );

                    let new_syntax_count = render_output.syntax_styles.len();
                    let new_linemap_count = render_output.line_map.lines.len();
                    log::info!(
                        "F{} CACHE DIRTY_RERENDER view={:?} dirty_rows={:?} new_syntax={} new_linemap={} elapsed={:?}",
                        frame_num, view.id, dirty, new_syntax_count, new_linemap_count, render_start.elapsed(),
                    );

                    // Preserve cached syntax styles — the dirty rerender used
                    // SyntaxHighlighter::from_cache which doesn't record new styles.
                    let preserved_syntax = if render_output.syntax_styles.is_empty() {
                        cached_syntax
                    } else {
                        SyntaxStyleCache {
                            entries: Arc::from(render_output.syntax_styles),
                        }
                    };

                    // Update cache
                    let cursor_pos = if cx.editor.tree.focus == view.id {
                        cursor_position_from_line_map(doc, view, &render_output.line_map)
                    } else {
                        None
                    };

                    self.render_cache.entries.insert(
                        view.id,
                        ViewRenderCacheEntry {
                            content_state: current_content,
                            cells: {
                                let copy_start = std::time::Instant::now();
                                let cells = Self::copy_region(surface, Self::content_area(view));
                                helix_view::bench::log_run_phase(
                                    "editor_render_view",
                                    "copy_region",
                                    copy_start.elapsed(),
                                    || {
                                        let area = Self::content_area(view);
                                        format!("view_id={:?} area={}x{}", view.id, area.width, area.height)
                                    },
                                );
                                cells
                            },
                            syntax_styles: preserved_syntax,
                            line_map: render_output.line_map,
                            overlay_fingerprints: new_fingerprints,
                        },
                    );
                    if cx.editor.tree.focus == view.id {
                        cx.editor.cursor_cache.set(cursor_pos);
                    }
                    helix_view::bench::log_run_phase(
                        "editor_render_view",
                        "dirty_total",
                        view_render_start.elapsed(),
                        || format!("view_id={:?} path=dirty", view.id),
                    );
                    continue;
                }
            }

            // Content miss — full re-render.
            #[cfg(debug_assertions)]
            {
                self.render_cache.misses = self.render_cache.misses.wrapping_add(1);
            }

            let seed_line_map = self
                .render_cache
                .entries
                .get(&view.id)
                .filter(|entry| can_reuse_seed_line_map(&entry.content_state, &current_content))
                .map(|entry| entry.line_map.clone());

            let vctx = ViewRenderContext {
                editor: cx.editor,
                doc,
                view,
                viewport: area,
                is_focused,
                cached_syntax: None,
                dirty_rows: None,
                seed_line_map: seed_line_map.as_ref(),
            };
            let render_output = self.render_view(&vctx, surface);
            helix_view::bench::log_run_phase(
                "editor_render_view",
                "full_render_view",
                view_render_start.elapsed(),
                || format!("view_id={:?} path=full_before_fp", view.id),
            );

            let selection = doc.selection(view.id);
            let fp_start = std::time::Instant::now();
            let overlay_fingerprints = render_output.line_map.overlay_fingerprints(
                selection,
                cx.editor.mode,
                is_focused,
                self.terminal_focused,
            );
            helix_view::bench::log_run_phase(
                "editor_render_view",
                "overlay_fingerprints",
                fp_start.elapsed(),
                || format!("view_id={:?} rows={}", view.id, render_output.line_map.lines.len()),
            );

            log::info!(
                "F{} CACHE MISS view={:?} anchor={} voff={} area={}x{} syntax={} lines={} elapsed={:?}",
                frame_num,
                view.id,
                current_content.view_position.anchor,
                current_content.view_position.vertical_offset,
                view.area.width, view.area.height,
                render_output.syntax_styles.len(),
                render_output.line_map.lines.len(),
                render_start.elapsed(),
            );

            let cursor_pos = if cx.editor.tree.focus == view.id {
                cursor_position_from_line_map(doc, view, &render_output.line_map)
            } else {
                None
            };

            self.render_cache.entries.insert(
                view.id,
                ViewRenderCacheEntry {
                    content_state: current_content,
                    cells: {
                        let copy_start = std::time::Instant::now();
                        let cells = Self::copy_region(surface, Self::content_area(view));
                        helix_view::bench::log_run_phase(
                            "editor_render_view",
                            "copy_region",
                            copy_start.elapsed(),
                            || {
                                let area = Self::content_area(view);
                                format!("view_id={:?} area={}x{}", view.id, area.width, area.height)
                            },
                        );
                        cells
                    },
                    syntax_styles: SyntaxStyleCache {
                        entries: Arc::from(render_output.syntax_styles),
                    },
                    line_map: render_output.line_map,
                    overlay_fingerprints,
                },
            );
            if cx.editor.tree.focus == view.id {
                cx.editor.cursor_cache.set(cursor_pos);
            }
            helix_view::bench::log_run_phase(
                "editor_render_view",
                "full_total",
                view_render_start.elapsed(),
                || format!("view_id={:?} path=full", view.id),
            );
        }

        // Batch all statusline renders and execute deferred work in parallel.
        {
            let statusline_start = std::time::Instant::now();
            let batch: Vec<PreparedRender> = cx
                .editor
                .tree
                .views()
                .map(|(view, is_focused)| {
                    let doc = cx.editor.document(view.doc).unwrap();
                    self.prepare_statusline(cx.editor, doc, view, is_focused)
                })
                .collect();
            let count = batch.len();
            self.chrome_cache.compose_batch(batch, surface);
            helix_view::bench::log_run_phase(
                "editor_render",
                "statusline_batch",
                statusline_start.elapsed(),
                || format!("count={}", count),
            );
        }

        #[cfg(debug_assertions)]
        {
            if self.render_cache.frames.is_multiple_of(300) {
                log::debug!(
                    "ViewRenderCache: {} hits ({} dirty / {} clean lines), {} misses over 300 frames",
                    self.render_cache.hits,
                    self.render_cache.dirty_lines,
                    self.render_cache.clean_lines,
                    self.render_cache.misses,
                );
                self.render_cache.hits = 0;
                self.render_cache.dirty_lines = 0;
                self.render_cache.clean_lines = 0;
                self.render_cache.misses = 0;
            }
        }

        let key_width = 15u16; // for showing pending keys
        let mut status_msg_width = 0;

        // render status msg
        if let Some((status_msg, severity)) = &cx.editor.status_msg {
            let status_start = std::time::Instant::now();
            status_msg_width = status_msg.width();
            use helix_view::editor::Severity;
            let style = if *severity == Severity::Error {
                cx.editor.theme.get("error")
            } else {
                cx.editor.theme.get("ui.text")
            };

            surface.set_string(
                area.x,
                area.y + area.height.saturating_sub(1),
                status_msg,
                style,
            );
            helix_view::bench::log_run_phase(
                "editor_render",
                "status_msg",
                status_start.elapsed(),
                || format!("width={}", status_msg_width),
            );
        }

        if area.width.saturating_sub(status_msg_width as u16) > key_width {
            let pending_start = std::time::Instant::now();
            let mut disp = String::new();
            if let Some(count) = self.engine_input_state().count {
                disp.push_str(&count.to_string())
            }
            if let Some(ref engine) = self.engine {
                let pending = engine.pending_display();
                if !pending.is_empty() {
                    disp.push_str(pending);
                }
            }
            // Also show raw pending keys from the keymaps (engine may not surface all)
            for key in self.keymaps.pending() {
                disp.push_str(&key.key_sequence_format());
            }
            for key in &self.pseudo_pending {
                disp.push_str(&key.key_sequence_format());
            }
            let style = cx.editor.theme.get("ui.text");
            let macro_width = if cx.editor.macro_recording.is_some() {
                3
            } else {
                0
            };
            surface.set_string(
                area.x + area.width.saturating_sub(key_width + macro_width),
                area.y + area.height.saturating_sub(1),
                disp.get(disp.len().saturating_sub(key_width as usize)..)
                    .unwrap_or(&disp),
                style,
            );
            if let Some((reg, _)) = cx.editor.macro_recording {
                let disp = format!("[{}]", reg);
                let style = style
                    .fg(helix_view::graphics::Color::Yellow)
                    .add_modifier(Modifier::BOLD);
                surface.set_string(
                    area.x + area.width.saturating_sub(3),
                    area.y + area.height.saturating_sub(1),
                    &disp,
                    style,
                );
            }
            helix_view::bench::log_run_phase(
                "editor_render",
                "pending_keys",
                pending_start.elapsed(),
                || format!("display_width={}", disp.len()),
            );
        }

        // Batch completion + notification renders for parallel deferred execution.
        // NOTE: cleanup_notifications() is now done in compositor pre-render.
        {
            let chrome_start = std::time::Instant::now();

            let mut chrome_batch = Vec::with_capacity(2);
            if let Some(completion) = self.completion.as_mut() {
                chrome_batch.push(completion.prepare_render(area, cx));
            }
            if let Some(prepared) = self.notification_popup.prepare_snapshot(area, cx.editor) {
                chrome_batch.push(prepared);
            }
            if !chrome_batch.is_empty() {
                self.chrome_cache.compose_batch(chrome_batch, surface);
            }
            helix_view::bench::log_run_phase(
                "editor_render",
                "chrome_batch",
                chrome_start.elapsed(),
                || format!("area={}x{}", area.width, area.height),
            );
        }
        helix_view::bench::log_run_phase("editor_render", "final_total", render_start.elapsed(), || {
            format!("area={}x{} frame={}", area.width, area.height, frame_num)
        });
    }

    fn cursor(&self, _area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        let (pos, kind) = editor.cursor();
        if self.terminal_focused {
            (pos, kind)
        } else {
            // use underline cursor when terminal loses focus for visibility
            (pos, CursorKind::Underline)
        }
    }
}

#[derive(Debug, Default)]
struct BufferLineInfo {
    visible_buffers: Vec<BufferInfo>,
}

impl BufferLineInfo {
    fn clear(&mut self) {
        self.visible_buffers.clear();
    }

    fn add_buffer_info(&mut self, document_id: DocumentId, columns: std::ops::Range<u16>) {
        self.visible_buffers.push(BufferInfo {
            document_id,
            columns,
        });
    }

    fn get_clicked_buffer(&self, column: u16) -> Option<&BufferInfo> {
        self.visible_buffers
            .iter()
            .find(|cell| cell.columns.contains(&column))
    }
}

#[derive(Debug)]
struct BufferInfo {
    document_id: DocumentId,
    // The bufferline column span used to show the document name
    columns: std::ops::Range<u16>,
}

// Key canonicalization (SHIFT stripping from Char keys) is now done in
// the compositor's handle_event, so all components receive canonical keys.

fn restore_focus_after_mouse_scroll(
    editor: &mut Editor,
    previous_view: ViewId,
    scrolled_view: ViewId,
) {
    editor.tree.focus = previous_view;
    if previous_view != scrolled_view {
        editor.ensure_cursor_in_view(previous_view);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        can_reuse_seed_line_map, choose_render_seed, restore_focus_after_mouse_scroll,
        EditorView, ViewRenderContext,
    };
    use crate::handlers::Handlers;
    use crate::keymap::Keymaps;
    use arc_swap::ArcSwap;
    use helix_core::Rope;
    use helix_loader::runtime_dirs;
    use helix_modal::{helix::HelixEngine, populate::build_registry};
    use helix_view::graphics::Rect;
    use helix_view::theme;
    use helix_view::view::{LineMap, ViewContentState, ViewPosition, VisualLineInfo};
    use helix_view::{
        editor::{Action, Config, Editor},
        Document, DocumentId, View,
    };
    use std::sync::Arc;

    fn content_state(doc_version: i32, annotation_gen: u64, width: u16) -> ViewContentState {
        ViewContentState {
            doc_id: DocumentId::default(),
            doc_version,
            view_position: ViewPosition::default(),
            area: Rect::new(0, 0, width, 10),
            theme_name: Arc::<str>::from("test-theme"),
            config_gen: 1,
            annotation_gen,
            cursor_line: 0,
        }
    }

    fn test_editor_with_text(text: &str) -> (Editor, helix_view::ViewId, DocumentId) {
        let theme_loader = theme::Loader::new(runtime_dirs());
        let syn_loader = helix_core::config::default_lang_loader();
        let config = Arc::new(ArcSwap::from_pointee(Config::default()));
        let mut editor = Editor::new(
            Rect::new(0, 0, 80, 24),
            Arc::new(theme_loader),
            Arc::new(ArcSwap::from_pointee(syn_loader)),
            Arc::new(arc_swap::access::Map::new(config, |cfg: &Config| cfg)),
            Handlers::dummy(),
        );
        let doc = Document::from(
            Rope::from(text),
            None,
            editor.config.clone(),
            editor.syn_loader.clone(),
        );
        let doc_id = editor.new_file_from_document(Action::VerticalSplit, doc);
        let view_id = editor.tree.focus;
        (editor, view_id, doc_id)
    }

    fn test_editor_view() -> EditorView {
        let registry = Arc::new(build_registry());
        EditorView::new(
            Keymaps::default(),
            Box::new(HelixEngine::new(registry.clone())),
            registry,
        )
    }

    fn giant_multiline_fixture(lines: usize, bytes_per_line: usize) -> String {
        (0..lines)
            .map(|idx| char::from(b'a' + (idx % 26) as u8).to_string().repeat(bytes_per_line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn render_seed_prefers_nearest_visible_checkpoint() {
        let line_map = LineMap {
            lines: Arc::from(vec![VisualLineInfo {
                visual_row: 0,
                doc_line: 7,
                char_range_start: 0,
                char_range_end: 200,
                visible_char_start: 10,
                visible_col_start: 10,
                visible_char_last: 90,
                visible_col_last: 90,
                horizontal_checkpoints: Vec::new(),
            }]),
        };

        assert_eq!(
            choose_render_seed(Some(&line_map), 7, 100)
                .map(|seed| (seed.doc_line, seed.char_idx, seed.visual_col)),
            Some((7, 90, 90))
        );
    }

    #[test]
    fn render_seed_ignores_future_and_mismatched_rows() {
        let line_map = LineMap {
            lines: Arc::from(vec![
                VisualLineInfo {
                    visual_row: 1,
                    doc_line: 7,
                    char_range_start: 0,
                    char_range_end: 50,
                    visible_char_start: 10,
                    visible_col_start: 10,
                    visible_char_last: 20,
                    visible_col_last: 20,
                    horizontal_checkpoints: Vec::new(),
                },
                VisualLineInfo {
                    visual_row: 0,
                    doc_line: 8,
                    char_range_start: 50,
                    char_range_end: 100,
                    visible_char_start: 60,
                    visible_col_start: 60,
                    visible_char_last: 80,
                    visible_col_last: 120,
                    horizontal_checkpoints: Vec::new(),
                },
            ]),
        };

        assert_eq!(
            choose_render_seed(Some(&line_map), 8, 100)
                .map(|seed| (seed.doc_line, seed.char_idx, seed.visual_col)),
            Some((8, 60, 60))
        );
        assert!(choose_render_seed(Some(&line_map), 9, 100).is_none());
    }

    #[test]
    fn render_seed_ignores_far_cached_checkpoint() {
        let line_map = LineMap {
            lines: Arc::from(vec![VisualLineInfo {
                visual_row: 0,
                doc_line: 7,
                char_range_start: 0,
                char_range_end: 300_000,
                visible_char_start: 143_200,
                visible_col_start: 143_200,
                visible_char_last: 143_237,
                visible_col_last: 143_237,
                horizontal_checkpoints: Vec::new(),
            }]),
        };

        assert!(choose_render_seed(Some(&line_map), 7, 290_989).is_none());
        assert_eq!(
            choose_render_seed(Some(&line_map), 7, 143_296)
                .map(|seed| (seed.doc_line, seed.char_idx, seed.visual_col)),
            Some((7, 143_237, 143_237))
        );
    }

    #[test]
    fn seed_line_map_reuse_requires_stable_text_layout_inputs() {
        let previous = content_state(5, 7, 120);
        let same_text = content_state(5, 7, 120);
        let changed_doc = content_state(6, 7, 120);
        let changed_annotations = content_state(5, 8, 120);
        let changed_width = content_state(5, 7, 121);

        assert!(can_reuse_seed_line_map(&previous, &same_text));
        assert!(!can_reuse_seed_line_map(&previous, &changed_doc));
        assert!(!can_reuse_seed_line_map(
            &previous,
            &changed_annotations
        ));
        assert!(!can_reuse_seed_line_map(&previous, &changed_width));
    }

    #[test]
    fn restore_focus_after_mouse_scroll_does_not_recentre_same_view() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = runtime.enter();
        let text = (0..200).map(|idx| format!("line {idx}")).collect::<Vec<_>>().join("\n");
        let (mut editor, view_id, doc_id) = test_editor_with_text(&text);

        {
            let doc = editor.document_mut(doc_id).expect("document");
            doc.set_view_offset(
                view_id,
                ViewPosition {
                    anchor: doc.text().line_to_char(50),
                    vertical_offset: 0,
                    horizontal_offset: 0,
                },
            );
        }

        let before = editor.document(doc_id).expect("document").view_offset(view_id);
        restore_focus_after_mouse_scroll(&mut editor, view_id, view_id);
        let after = editor.document(doc_id).expect("document").view_offset(view_id);

        assert_eq!(after, before);
    }

    #[test]
    fn content_area_excludes_statusline_row() {
        let mut view = View::new(DocumentId::default(), Default::default());
        view.area = Rect::new(5, 7, 80, 10);

        assert_eq!(EditorView::content_area(&view), Rect::new(5, 7, 80, 9));
    }

    #[test]
    #[ignore = "targeted local repro for full render_view on many giant lines"]
    fn render_view_many_giant_lines_repro() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let _guard = runtime.enter();

        for (lines, bytes_per_line) in [(20, 4_000), (50, 5_000), (100, 18_500), (2, 900_000)] {
            let event_log_path = std::env::temp_dir().join(format!(
                "helix-render-view-giant-lines-{}-{}-{}.log",
                std::process::id(),
                lines,
                bytes_per_line
            ));
            let _ = std::fs::remove_file(&event_log_path);
            let _run_guard =
                helix_view::bench::enter_bench_run(helix_view::bench::BenchRunContext {
                    seed: 0,
                    event_log_path: event_log_path.clone(),
                });

            let text = giant_multiline_fixture(lines, bytes_per_line);
            let (mut editor, view_id, doc_id) = test_editor_with_text(&text);
            editor.resize(Rect::new(0, 0, 160, 61));

            {
                let doc = editor.document_mut(doc_id).expect("document");
                doc.set_view_offset(
                    view_id,
                    ViewPosition {
                        anchor: doc.text().line_to_char(lines / 2),
                        vertical_offset: 0,
                        horizontal_offset: 0,
                    },
                );
            }

            let mut editor_view = test_editor_view();
            let area = Rect::new(0, 0, 160, 61);
            let mut surface = tui::buffer::Buffer::empty(area);
            let start = std::time::Instant::now();
            let doc = editor.document(doc_id).expect("document");
            let view = view!(editor, view_id);
            let vctx = ViewRenderContext {
                editor: &editor,
                doc,
                view,
                viewport: area,
                is_focused: true,
                cached_syntax: None,
                dirty_rows: None,
                seed_line_map: None,
            };
            let output = editor_view.render_view(&vctx, &mut surface);

            eprintln!(
                "render_view_many_giant_lines_repro: lines={} bytes_per_line={} elapsed_us={} mapped_lines={} bytes={}",
                lines,
                bytes_per_line,
                start.elapsed().as_micros(),
                output.line_map.lines.len(),
                doc.text().len_bytes(),
            );
            if let Ok(trace) = std::fs::read_to_string(&event_log_path) {
                eprintln!("{trace}");
            }
        }
    }
}
