use helix_core::diagnostic::Severity;
use helix_core::indent::IndentStyle;
use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::document::Mode;
use helix_view::editor::{
    StatusLineConfig, StatusLineElement as StatusLineElementId, WorkspaceDiagnosticCounts,
};
use helix_view::icons::ICONS;
use helix_view::statusline::{
    CursorStatusProvider, DiagnosticCounts, DiagnosticStatusProvider, DocumentStatusProvider,
    SelectionStatusProvider, StatuslineSnapshot,
};
use helix_view::theme::{Style as ThemeStyle, Theme};
use helix_view::{Document, DocumentId, View, ViewId};
use std::borrow::Cow;
use std::sync::{LazyLock, Mutex};
use tui::ratatui::{
    style::Style as RatatuiStyle,
    text::{Line, Span},
};

use crate::ui::{design::StatuslineStyles, ProgressSpinners};

#[derive(Debug, Clone, Copy)]
pub struct BenchOverlay {
    pub rolling_fps: f64,
    pub actions_executed: usize,
    pub remaining_seconds: f64,
}

#[derive(Clone)]
pub struct StatuslineModel {
    pub view_id: ViewId,
    pub config: StatusLineConfig,
    pub theme: Theme,
    pub theme_name: String,
    pub color_modes: bool,
    pub snapshot: StatuslineSnapshot<'static>,
    pub bench_overlay: Option<BenchOverlay>,
}

impl StatuslineModel {
    pub fn collect(
        context: StatuslineContext<'_>,
        doc: &Document,
        view: &View,
        focused: bool,
    ) -> Self {
        let cursor = doc.cursor_status(view.id);
        let selection = doc.selection_status(view.id);
        let spinner_frame = doc
            .language_servers()
            .next()
            .and_then(|server| {
                context
                    .spinners
                    .get(server.id())
                    .and_then(|spinner| spinner.frame())
            })
            .unwrap_or(" ");
        // Pre-collect language-server names so the LspStatus
        // element doesn't have to thread the live editor through
        // the render path. Iteration order is attach order — for
        // most buffers this is one server; rare languages have two
        // or three (e.g. typescript-language-server + eslint).
        let lsp_server_names: Vec<String> = doc
            .language_servers()
            .map(|server| server.name().to_string())
            .collect();
        let current_working_directory = helix_stdx::env::current_working_dir()
            .file_name()
            .map(|name| Cow::Owned(name.to_string_lossy().into_owned()))
            .unwrap_or_default();
        let function_name = get_current_function_name_cached(doc, view.id, cursor.char_idx);

        Self {
            view_id: view.id,
            config: context.config.clone(),
            theme: context.theme.clone(),
            theme_name: context.theme_name.to_string(),
            color_modes: context.color_modes,
            snapshot: StatuslineSnapshot {
                modal: helix_view::statusline::ModalStatus {
                    focused,
                    mode: context.mode,
                    selected_register: context.selected_register,
                },
                cursor,
                selection,
                document: doc.document_status(),
                diagnostics: doc.diagnostic_counts(),
                workspace_diagnostics: context.workspace_diagnostics,
                spinner_frame: Cow::Borrowed(spinner_frame),
                current_working_directory,
                function_name,
                lsp_server_names,
            }
            .into_owned(),
            bench_overlay: context.bench_overlay.map(|bench| BenchOverlay {
                rolling_fps: bench.rolling_fps,
                actions_executed: bench.actions_executed,
                remaining_seconds: bench.remaining_seconds,
            }),
        }
    }
}

pub struct StatuslineContext<'a> {
    pub config: &'a StatusLineConfig,
    pub theme: &'a Theme,
    pub theme_name: &'a str,
    pub color_modes: bool,
    pub workspace_diagnostics: WorkspaceDiagnosticCounts,
    pub bench_overlay: Option<helix_view::editor::BenchOverlay>,
    pub mode: Mode,
    pub selected_register: Option<char>,
    pub spinners: &'a ProgressSpinners,
}

pub(crate) fn cache_id(view_id: ViewId) -> crate::render::CacheId {
    crate::render::CacheId::hashed(&("statusline", view_id))
}

#[derive(Default)]
pub struct RenderBuffer<'a> {
    pub left: Line<'a>,
    pub center: Line<'a>,
    pub right: Line<'a>,
}

pub struct Statusline<'a> {
    model: StatuslineModel,
    parts: RenderBuffer<'a>,
}

impl<'a> Statusline<'a> {
    pub fn new(model: StatuslineModel) -> Self {
        Self {
            model,
            parts: RenderBuffer::default(),
        }
    }

    pub fn prepare(
        model: StatuslineModel,
        area: helix_view::graphics::Rect,
    ) -> crate::render::PreparedRender {
        use crate::render::{CacheKey, CacheTag, PreparedRender, RenderOutput};

        let tag = CacheTag {
            id: cache_id(model.view_id),
            key: CacheKey::hashed(&(
                &model.config,
                &model.theme_name,
                model.color_modes,
                &model.snapshot,
                model.bench_overlay.map(|overlay| {
                    (
                        overlay.rolling_fps.to_bits(),
                        overlay.actions_executed,
                        overlay.remaining_seconds.to_bits(),
                    )
                }),
            )),
            area,
        };
        PreparedRender::snapshot(tag, model, move |model| {
            let mut component = Statusline::new(model);
            let mut output = RenderOutput::new(area);
            component.render_surface(area, output.surface_mut());
            output
        })
    }

    pub fn render_surface(
        &mut self,
        viewport: helix_view::graphics::Rect,
        surface: &mut crate::render::CellSurface,
    ) {
        let statusline_styles = StatuslineStyles::from_theme(&self.model.theme);
        let base_style = if self.model.snapshot.modal.focused {
            rat_style(statusline_styles.base)
        } else {
            rat_style(statusline_styles.inactive)
        };

        surface.set_style(
            tui::ratatui::to_ratatui_rect(viewport.with_height(1)),
            base_style,
        );

        for element_id in self.model.config.left.clone() {
            let element_start = std::time::Instant::now();
            let render = get_render_function(element_id);
            (render)(self, |statusline, span| {
                append(&mut statusline.parts.left, span, base_style)
            });
            helix_view::bench::log_run_phase(
                "statusline",
                "element",
                element_start.elapsed(),
                || format!("side=left element={element_id:?}"),
            );
        }

        surface.set_line(
            viewport.x,
            viewport.y,
            &self.parts.left,
            self.parts.left.width() as u16,
        );

        for element_id in self.model.config.right.clone() {
            let element_start = std::time::Instant::now();
            let render = get_render_function(element_id);
            (render)(self, |statusline, span| {
                append(&mut statusline.parts.right, span, base_style)
            });
            helix_view::bench::log_run_phase(
                "statusline",
                "element",
                element_start.elapsed(),
                || format!("side=right element={element_id:?}"),
            );
        }

        if let Some(bench) = self.model.bench_overlay {
            let fps_text = format!(
                " BENCH {:.0}fps {}act {:.0}s ",
                bench.rolling_fps, bench.actions_executed, bench.remaining_seconds,
            );
            let bench_style = self
                .model
                .theme
                .get("ui.statusline")
                .fg(helix_view::graphics::Color::Black)
                .bg(helix_view::graphics::Color::Yellow);
            append(
                &mut self.parts.right,
                themed_span(fps_text, bench_style),
                base_style,
            );
        }

        surface.set_line(
            viewport.x
                + viewport
                    .width
                    .saturating_sub(self.parts.right.width() as u16),
            viewport.y,
            &self.parts.right,
            self.parts.right.width() as u16,
        );

        for element_id in self.model.config.center.clone() {
            let element_start = std::time::Instant::now();
            let render = get_render_function(element_id);
            (render)(self, |statusline, span| {
                append(&mut statusline.parts.center, span, base_style)
            });
            helix_view::bench::log_run_phase(
                "statusline",
                "element",
                element_start.elapsed(),
                || format!("side=center element={element_id:?}"),
            );
        }

        let spacing = 1u16;
        let edge_width = self.parts.left.width().max(self.parts.right.width()) as u16;
        let center_max_width = viewport.width.saturating_sub(2 * edge_width + 2 * spacing);
        let center_width = center_max_width.min(self.parts.center.width() as u16);

        surface.set_line(
            viewport.x + viewport.width / 2 - center_width / 2,
            viewport.y,
            &self.parts.center,
            center_width,
        );
    }
}

fn rat_style(style: ThemeStyle) -> RatatuiStyle {
    tui::ratatui::to_ratatui_style(style)
}

fn themed_span<'a>(content: impl Into<Cow<'a, str>>, style: ThemeStyle) -> Span<'a> {
    Span::styled(content, rat_style(style))
}

/// Resolve the "secondary" statusline text style — a muted variant for
/// less-important elements like position, selection counts, and encoding.
/// Falls back through several theme keys so any theme produces a sensible
/// dimmed colour without needing a custom statusline.muted scope.
fn statusline_muted(theme: &Theme) -> ThemeStyle {
    theme
        .try_get("ui.text.inactive")
        .or_else(|| theme.try_get("comment"))
        .unwrap_or_else(|| theme.get("ui.statusline"))
}

fn append<'a>(buffer: &mut Line<'a>, mut span: Span<'a>, base_style: RatatuiStyle) {
    span.style = base_style.patch(span.style);
    buffer.spans.push(span);
}

fn get_render_function<'a, F>(element_id: StatusLineElementId) -> impl Fn(&mut Statusline<'a>, F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    match element_id {
        StatusLineElementId::Mode => render_mode,
        StatusLineElementId::Spinner => render_lsp_spinner,
        StatusLineElementId::LspStatus => render_lsp_status,
        StatusLineElementId::FileBaseName => render_file_base_name,
        StatusLineElementId::FileName => render_file_name,
        StatusLineElementId::FileAbsolutePath => render_file_absolute_path,
        StatusLineElementId::FileModificationIndicator => render_file_modification_indicator,
        StatusLineElementId::ReadOnlyIndicator => render_read_only_indicator,
        StatusLineElementId::FileEncoding => render_file_encoding,
        StatusLineElementId::FileLineEnding => render_file_line_ending,
        StatusLineElementId::FileIndentStyle => render_file_indent_style,
        StatusLineElementId::FileType => render_file_type,
        StatusLineElementId::Diagnostics => render_diagnostics,
        StatusLineElementId::WorkspaceDiagnostics => render_workspace_diagnostics,
        StatusLineElementId::Selections => render_selections,
        StatusLineElementId::PrimarySelectionLength => render_primary_selection_length,
        StatusLineElementId::Position => render_position,
        StatusLineElementId::PositionPercentage => render_position_percentage,
        StatusLineElementId::TotalLineNumbers => render_total_line_numbers,
        StatusLineElementId::Separator => render_separator,
        StatusLineElementId::Spacer => render_spacer,
        StatusLineElementId::VersionControl => render_version_control,
        StatusLineElementId::Register => render_register,
        StatusLineElementId::CurrentWorkingDirectory => render_cwd,
        StatusLineElementId::FunctionName => render_function_name,
    }
}

fn render_mode<'a, F>(statusline: &mut Statusline<'a>, write: F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    let visible = statusline.model.snapshot.modal.focused;
    let modenames = &statusline.model.config.mode;
    let mode = statusline.model.snapshot.modal.mode;
    // Shared lookup with the file explorer / future surfaces.
    // `mode_name` returns the user-configured label; on
    // unfocused statuslines we pad to the same width so the
    // chrome doesn't shift when focus changes.
    let mode_str = helix_view::statusline_mode::mode_name(mode, modenames);
    let content = if visible {
        format!(" {mode_str} ")
    } else {
        " ".repeat(mode_str.width() + 2)
    };
    let style = if visible && statusline.model.color_modes {
        let styles = StatuslineStyles::from_theme(&statusline.model.theme);
        match mode {
            Mode::Insert => styles.insert,
            Mode::Select => styles.select,
            Mode::Normal => styles.normal,
        }
    } else {
        ThemeStyle::default()
    };
    write(statusline, themed_span(content, style));
}

fn render_lsp_spinner<'a, F>(statusline: &mut Statusline<'a>, write: F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    write(
        statusline,
        statusline
            .model
            .snapshot
            .spinner_frame
            .as_ref()
            .to_string()
            .into(),
    );
}

/// `LspStatus` element: a compact list of attached language server
/// names, e.g. ` rust-analyzer ` or ` rust-analyzer · ruff `. Renders
/// nothing when no LSP is attached so the statusline doesn't reserve
/// dead space for non-LSP buffers (binary files, scratch buffers,
/// languages with no configured server).
///
/// Pairs naturally with the `Spinner` element: place `LspStatus` next
/// to `Spinner` and you get "is anything attached" + "is it busy
/// right now" in two compact columns. Style uses `ui.text.inactive`
/// (chrome-like, never demands attention) — falls back through
/// `comment` and finally the statusline base.
fn render_lsp_status<'a, F>(statusline: &mut Statusline<'a>, write: F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    let names = &statusline.model.snapshot.lsp_server_names;
    if names.is_empty() {
        return;
    }
    let style = statusline
        .model
        .theme
        .try_get("ui.text.inactive")
        .or_else(|| statusline.model.theme.try_get("comment"))
        .unwrap_or_else(|| statusline.model.theme.get("ui.statusline"));
    // ` name1 · name2 ` — the `·` separator is the same idiom the
    // rest of the chrome uses (count cluster, follow indicator, etc.)
    // so the typography stays consistent.
    let body = names.join(" · ");
    write(statusline, themed_span(format!(" {body} "), style));
}

fn render_diagnostics<'a, F>(statusline: &mut Statusline<'a>, write: F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    render_diagnostic_counts(
        statusline,
        statusline.model.snapshot.diagnostics,
        statusline.model.config.diagnostics.clone(),
        write,
    );
}

fn render_workspace_diagnostics<'a, F>(statusline: &mut Statusline<'a>, write: F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    let counts = DiagnosticCounts {
        hints: statusline.model.snapshot.workspace_diagnostics.hints as usize,
        info: statusline.model.snapshot.workspace_diagnostics.info as usize,
        warnings: statusline.model.snapshot.workspace_diagnostics.warnings as usize,
        errors: statusline.model.snapshot.workspace_diagnostics.errors as usize,
    };
    let severities = statusline.model.config.workspace_diagnostics.clone();
    if !severities.iter().any(|severity| match severity {
        Severity::Hint => counts.hints != 0,
        Severity::Info => counts.info != 0,
        Severity::Warning => counts.warnings != 0,
        Severity::Error => counts.errors != 0,
    }) {
        return;
    }

    let icons = ICONS.load();
    let icon = icons.kind().workspace();
    if !icon.glyph().is_empty() {
        if let Some(style) = icon.color().map(|color| ThemeStyle::default().fg(color)) {
            write(statusline, themed_span(format!("{} ", icon.glyph()), style));
        } else {
            write(statusline, format!("{} ", icon.glyph()).into());
        }
    }

    render_diagnostic_counts(statusline, counts, severities, write);
}

fn render_diagnostic_counts<'a, F>(
    statusline: &mut Statusline<'a>,
    counts: DiagnosticCounts,
    severities: Vec<Severity>,
    write: F,
) where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    let icons = ICONS.load();
    for severity in severities {
        match severity {
            Severity::Hint if counts.hints > 0 => {
                write(
                    statusline,
                    themed_span(
                        icons.diagnostic().hint().to_string(),
                        statusline.model.theme.get("hint"),
                    ),
                );
                write(statusline, Span::raw(format!(" {} ", counts.hints)));
            }
            Severity::Info if counts.info > 0 => {
                write(
                    statusline,
                    themed_span(
                        icons.diagnostic().info().to_string(),
                        statusline.model.theme.get("info"),
                    ),
                );
                write(statusline, Span::raw(format!(" {} ", counts.info)));
            }
            Severity::Warning if counts.warnings > 0 => {
                write(
                    statusline,
                    themed_span(
                        icons.diagnostic().warning().to_string(),
                        statusline.model.theme.get("warning"),
                    ),
                );
                write(statusline, Span::raw(format!(" {} ", counts.warnings)));
            }
            Severity::Error if counts.errors > 0 => {
                write(
                    statusline,
                    themed_span(
                        icons.diagnostic().error().to_string(),
                        statusline.model.theme.get("error"),
                    ),
                );
                write(statusline, Span::raw(format!(" {} ", counts.errors)));
            }
            _ => {}
        }
    }
}

fn render_selections<'a, F>(statusline: &mut Statusline<'a>, write: F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    let selection = statusline.model.snapshot.selection;
    let muted = statusline_muted(&statusline.model.theme);
    let text = if selection.count == 1 {
        " 1 sel ".to_string()
    } else {
        format!(" {}/{} sels ", selection.primary_index + 1, selection.count)
    };
    write(statusline, themed_span(text, muted));
}

fn render_primary_selection_length<'a, F>(statusline: &mut Statusline<'a>, write: F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    let length = statusline.model.snapshot.selection.primary_length;
    let muted = statusline_muted(&statusline.model.theme);
    let text = format!(" {} char{} ", length, if length == 1 { "" } else { "s" });
    write(statusline, themed_span(text, muted));
}

fn render_position<'a, F>(statusline: &mut Statusline<'a>, write: F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    let position = statusline.model.snapshot.cursor.position;
    let muted = statusline_muted(&statusline.model.theme);
    let text = format!(" {}:{} ", position.row + 1, position.col + 1);
    write(statusline, themed_span(text, muted));
}

fn render_total_line_numbers<'a, F>(statusline: &mut Statusline<'a>, write: F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    let muted = statusline_muted(&statusline.model.theme);
    let text = format!(" {} ", statusline.model.snapshot.cursor.total_lines);
    write(statusline, themed_span(text, muted));
}

fn render_position_percentage<'a, F>(statusline: &mut Statusline<'a>, write: F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    let position = statusline.model.snapshot.cursor.position;
    let max_rows = statusline.model.snapshot.cursor.total_lines.max(1);
    let muted = statusline_muted(&statusline.model.theme);
    let text = format!("{}%", (position.row + 1) * 100 / max_rows);
    write(statusline, themed_span(text, muted));
}

fn render_file_encoding<'a, F>(statusline: &mut Statusline<'a>, write: F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    if let Some(encoding_name) = statusline.model.snapshot.document.encoding_name {
        write(statusline, format!(" {} ", encoding_name).into());
    }
}

fn render_file_line_ending<'a, F>(statusline: &mut Statusline<'a>, write: F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    use helix_core::LineEnding::*;
    let line_ending = match statusline.model.snapshot.document.line_ending {
        Crlf => "CRLF",
        LF => "LF",
        #[cfg(feature = "unicode-lines")]
        VT => "VT",
        #[cfg(feature = "unicode-lines")]
        FF => "FF",
        #[cfg(feature = "unicode-lines")]
        CR => "CR",
        #[cfg(feature = "unicode-lines")]
        Nel => "NEL",
        #[cfg(feature = "unicode-lines")]
        LS => "LS",
        #[cfg(feature = "unicode-lines")]
        PS => "PS",
    };

    write(statusline, format!(" {} ", line_ending).into());
}

fn render_file_type<'a, F>(statusline: &mut Statusline<'a>, write: F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    let file_type = statusline.model.snapshot.document.language_name.as_ref();
    let icons = ICONS.load();

    if let Some(icon) = icons.mime().get(
        statusline.model.snapshot.document.file_path.as_ref(),
        Some(file_type),
    ) {
        if let Some(style) = icon.color().map(|color| ThemeStyle::default().fg(color)) {
            write(
                statusline,
                themed_span(format!(" {} ", icon.glyph()), style),
            );
        } else {
            write(statusline, format!(" {} ", icon.glyph()).into());
        }
    } else {
        write(statusline, format!(" {} ", file_type).into());
    }
}

fn render_file_name<'a, F>(statusline: &mut Statusline<'a>, write: F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    write(
        statusline,
        format!(" {} ", statusline.model.snapshot.document.file_name).into(),
    );
}

fn render_file_absolute_path<'a, F>(statusline: &mut Statusline<'a>, write: F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    write(
        statusline,
        format!(
            " {} ",
            statusline.model.snapshot.document.file_absolute_path
        )
        .into(),
    );
}

fn render_file_modification_indicator<'a, F>(statusline: &mut Statusline<'a>, write: F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    if statusline.model.snapshot.document.modified {
        // Accent the unsaved marker so it stands out against the filename —
        // unsaved state is the one thing the eye should catch immediately.
        let accent = statusline
            .model
            .theme
            .try_get("ui.text.focus")
            .or_else(|| statusline.model.theme.try_get("warning"))
            .unwrap_or_else(|| statusline.model.theme.get("ui.statusline"));
        write(statusline, themed_span("[+]", accent));
    } else {
        // Preserve layout width when the file is clean.
        write(statusline, "   ".into());
    }
}

fn render_read_only_indicator<'a, F>(statusline: &mut Statusline<'a>, write: F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    write(
        statusline,
        if statusline.model.snapshot.document.readonly {
            " [readonly] ".into()
        } else {
            "".into()
        },
    );
}

fn render_file_base_name<'a, F>(statusline: &mut Statusline<'a>, write: F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    write(
        statusline,
        format!(" {} ", statusline.model.snapshot.document.file_base_name).into(),
    );
}

fn render_separator<'a, F>(statusline: &mut Statusline<'a>, write: F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    write(
        statusline,
        themed_span(
            statusline.model.config.separator.to_string(),
            StatuslineStyles::from_theme(&statusline.model.theme).separator,
        ),
    );
}

fn render_spacer<'a, F>(statusline: &mut Statusline<'a>, write: F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    write(statusline, " ".into());
}

fn render_version_control<'a, F>(statusline: &mut Statusline<'a>, write: F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    let head = statusline
        .model
        .snapshot
        .document
        .version_control_head
        .as_deref()
        .unwrap_or_default();
    let icons = ICONS.load();
    let icon = icons.vcs().branch();

    let text = if icon.is_empty() {
        format!(" {head} ")
    } else {
        format!(" {icon} {head} ")
    };

    write(statusline, text.into());
}

fn render_register<'a, F>(statusline: &mut Statusline<'a>, write: F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    if let Some(register) = statusline.model.snapshot.modal.selected_register {
        write(statusline, format!(" reg={} ", register).into());
    }
}

fn render_file_indent_style<'a, F>(statusline: &mut Statusline<'a>, write: F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    write(
        statusline,
        match statusline.model.snapshot.document.indent_style {
            IndentStyle::Tabs => " tabs ".into(),
            IndentStyle::Spaces(indent) => {
                format!(" {} space{} ", indent, if indent == 1 { "" } else { "s" }).into()
            }
        },
    );
}

fn render_cwd<'a, F>(statusline: &mut Statusline<'a>, write: F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    write(
        statusline,
        statusline
            .model
            .snapshot
            .current_working_directory
            .as_ref()
            .to_string()
            .into(),
    );
}

fn render_function_name<'a, F>(statusline: &mut Statusline<'a>, write: F)
where
    F: Fn(&mut Statusline<'a>, Span<'a>) + Copy,
{
    if let Some(name) = statusline.model.snapshot.function_name.as_deref() {
        let icons = ICONS.load();
        if let Some(icon) = icons.kind().get("function") {
            let glyph = icon.glyph();
            if let Some(style) = icon.color().map(|color| ThemeStyle::default().fg(color)) {
                write(
                    statusline,
                    themed_span(format!(" {} {}", glyph, name), style),
                );
            } else {
                write(statusline, format!(" {} {} ", glyph, name).into());
            }
        } else {
            write(statusline, format!(" {} ", name).into());
        }
    }
}

#[derive(Clone)]
struct FuncNameCacheEntry {
    doc_id: DocumentId,
    view_id: ViewId,
    cursor_byte: u32,
    doc_version: i32,
    name: Option<String>,
}

static FUNC_NAME_CACHE: LazyLock<Mutex<Option<FuncNameCacheEntry>>> =
    LazyLock::new(|| Mutex::new(None));

fn get_current_function_name_cached(
    doc: &Document,
    view_id: ViewId,
    cursor_char: usize,
) -> Option<String> {
    let text = doc.text().slice(..);
    let cursor_byte = text.char_to_byte(cursor_char) as u32;
    let doc_id = doc.id();
    let doc_version = doc.version();

    if let Some(entry) = FUNC_NAME_CACHE.lock().unwrap().as_ref() {
        if entry.doc_id == doc_id
            && entry.view_id == view_id
            && entry.cursor_byte == cursor_byte
            && entry.doc_version == doc_version
        {
            return entry.name.clone();
        }
    }

    let name = get_current_function_name(doc, cursor_char);
    *FUNC_NAME_CACHE.lock().unwrap() = Some(FuncNameCacheEntry {
        doc_id,
        view_id,
        cursor_byte,
        doc_version,
        name: name.clone(),
    });
    name
}

fn get_current_function_name(doc: &Document, cursor_char: usize) -> Option<String> {
    doc.function_name_at_char(cursor_char)
}
