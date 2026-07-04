//! Fuzzy picker component.
//!
//! # Custom key handlers and the confirmation seam
//!
//! Pickers can register component-local key bindings via [`PickerKeyHandlers`]
//! and [`Picker::with_key_handlers`]. Two kinds of actions are supported:
//!
//! - [`PickerKeyHandlers::insert`] registers an action that runs immediately
//!   on the key press (with the currently selected item).
//! - [`PickerKeyHandlers::insert_confirmed`] registers an action that requires
//!   user confirmation. The handler runs at keypress time with the selected
//!   item and returns an optional [`PickerConfirmation`]: the message to show
//!   and the deferred action to run once the user confirms. Returning `None`
//!   skips the prompt entirely (e.g. the action does not apply to the selected
//!   item); the handler may set a status message itself in that case.
//!
//! Confirmation reuses the standard [`Prompt`] y/n affordance — the same
//! interaction as the file explorer delete prompt: the picker pushes a prompt
//! reading `"<message> (y/n): "`, typing `y` and pressing Enter executes the
//! deferred action, any other input (or Esc) cancels it. Because the deferred
//! action is built at keypress time, handlers clone exactly the item state
//! they need and no `Clone` bound is imposed on picker items.
//!
//! The `:pkg` picker's `d` (remove) action is the reference adopter; other
//! pickers can opt in by switching `insert` to `insert_confirmed`.

mod handlers;
mod query;

use crate::{
    compositor::{self, Component, Context, Event, EventResult, PickerComponent, RenderContext},
    ui::{
        self,
        document::{render_document, HighlighterInput, LinePos, TextRenderer},
        gradient_border::GradientBorder,
        menu::{Cell, Row},
        picker::query::PickerQuery,
        text_decorations::DecorationManager,
    },
    widgets::PickerTable,
};
use helix_core::unicode::width::UnicodeWidthStr;
use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Nucleo};
use thiserror::Error;
use tui::{
    ratatui::layout::Constraint,
    text::{Span, Spans},
};

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fmt,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{self, AtomicU64, AtomicUsize},
        Arc,
    },
    time::Duration,
};

use crate::ui::{Prompt, PromptEvent};
use helix_core::{
    char_idx_at_visual_offset, fuzzy::MATCHER, movement::Direction,
    text_annotations::TextAnnotations, unicode::segmentation::UnicodeSegmentation, Position,
};
use helix_view::{
    content_region::ContentRegion,
    editor::Action,
    graphics::{CursorKind, Margin, Modifier, Rect},
    input::KeyEvent,
    keyboard::{KeyCode, KeyModifiers},
    theme::Style,
    traits::{Bounded, Scrollable as ViewScrollable, Viewport},
    view::ViewPosition,
    Document, DocumentId, Editor,
};

use self::handlers::PreviewHighlightHandler;

pub(super) type SharedIngress = Arc<crate::runtime::RuntimeIngress>;
type SharedRedraw = Arc<helix_runtime::FrameHandle>;

pub(crate) const PICKER_TRACE_TARGET: &str = "dhx_picker";

static NEXT_PICKER_TRACE_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_PICKER_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

// Picker keys are component-local hardcoded bindings, not user keymap entries,
// so this table is the single source for both dispatch and footer hints.
const PICKER_BINDINGS: &[PickerBinding] = &[
    PickerBinding::visible(
        KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::NONE,
        },
        PickerBindingAction::Open,
        "Enter",
        "open",
        220,
    ),
    PickerBinding::visible(
        KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::NONE,
        },
        PickerBindingAction::Close,
        "Esc",
        "close",
        210,
    ),
    PickerBinding::hidden(
        KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
        },
        PickerBindingAction::Close,
    ),
    PickerBinding::visible(
        KeyEvent {
            code: KeyCode::Tab,
            modifiers: KeyModifiers::NONE,
        },
        PickerBindingAction::Next,
        "Tab",
        "next",
        200,
    ),
    PickerBinding::visible(
        KeyEvent {
            code: KeyCode::Tab,
            modifiers: KeyModifiers::SHIFT,
        },
        PickerBindingAction::Previous,
        "S-Tab",
        "prev",
        190,
    ),
    PickerBinding::hidden(
        KeyEvent {
            code: KeyCode::Up,
            modifiers: KeyModifiers::NONE,
        },
        PickerBindingAction::Previous,
    ),
    PickerBinding::hidden(
        KeyEvent {
            code: KeyCode::Char('p'),
            modifiers: KeyModifiers::CONTROL,
        },
        PickerBindingAction::Previous,
    ),
    PickerBinding::hidden(
        KeyEvent {
            code: KeyCode::Down,
            modifiers: KeyModifiers::NONE,
        },
        PickerBindingAction::Next,
    ),
    PickerBinding::hidden(
        KeyEvent {
            code: KeyCode::Char('n'),
            modifiers: KeyModifiers::CONTROL,
        },
        PickerBindingAction::Next,
    ),
    PickerBinding::hidden(
        KeyEvent {
            code: KeyCode::PageDown,
            modifiers: KeyModifiers::NONE,
        },
        PickerBindingAction::PageDown,
    ),
    PickerBinding::hidden(
        KeyEvent {
            code: KeyCode::Char('d'),
            modifiers: KeyModifiers::CONTROL,
        },
        PickerBindingAction::PageDown,
    ),
    PickerBinding::hidden(
        KeyEvent {
            code: KeyCode::PageUp,
            modifiers: KeyModifiers::NONE,
        },
        PickerBindingAction::PageUp,
    ),
    PickerBinding::hidden(
        KeyEvent {
            code: KeyCode::Char('u'),
            modifiers: KeyModifiers::CONTROL,
        },
        PickerBindingAction::PageUp,
    ),
    PickerBinding::hidden(
        KeyEvent {
            code: KeyCode::Home,
            modifiers: KeyModifiers::NONE,
        },
        PickerBindingAction::Start,
    ),
    PickerBinding::hidden(
        KeyEvent {
            code: KeyCode::End,
            modifiers: KeyModifiers::NONE,
        },
        PickerBindingAction::End,
    ),
    PickerBinding::visible(
        KeyEvent {
            code: KeyCode::Char(' '),
            modifiers: KeyModifiers::NONE,
        },
        PickerBindingAction::ToggleMark,
        "Space",
        "mark",
        205,
    ),
    PickerBinding::hidden(
        KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::ALT,
        },
        PickerBindingAction::OpenKeep,
    ),
    PickerBinding::visible(
        KeyEvent {
            code: KeyCode::Char('s'),
            modifiers: KeyModifiers::CONTROL,
        },
        PickerBindingAction::HorizontalSplit,
        "C-s",
        "split",
        120,
    ),
    PickerBinding::visible(
        KeyEvent {
            code: KeyCode::Char('v'),
            modifiers: KeyModifiers::CONTROL,
        },
        PickerBindingAction::VerticalSplit,
        "C-v",
        "vsplit",
        110,
    ),
    PickerBinding::visible(
        KeyEvent {
            code: KeyCode::Char('t'),
            modifiers: KeyModifiers::CONTROL,
        },
        PickerBindingAction::TogglePreview,
        "C-t",
        "preview",
        100,
    ),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PickerBindingAction {
    Previous,
    Next,
    PageDown,
    PageUp,
    Start,
    End,
    ToggleMark,
    Close,
    OpenKeep,
    Open,
    HorizontalSplit,
    VerticalSplit,
    TogglePreview,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PickerBinding {
    key: KeyEvent,
    action: PickerBindingAction,
    hint: PickerHintPolicy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PickerHintPolicy {
    Visible(PickerHint),
    Hidden,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PickerHint {
    key: &'static str,
    label: &'static str,
    priority: u8,
}

impl PickerBinding {
    const fn visible(
        key: KeyEvent,
        action: PickerBindingAction,
        hint_key: &'static str,
        hint_label: &'static str,
        priority: u8,
    ) -> Self {
        Self {
            key,
            action,
            hint: PickerHintPolicy::Visible(PickerHint {
                key: hint_key,
                label: hint_label,
                priority,
            }),
        }
    }

    const fn hidden(key: KeyEvent, action: PickerBindingAction) -> Self {
        Self {
            key,
            action,
            hint: PickerHintPolicy::Hidden,
        }
    }
}

fn picker_binding_for_key(key: KeyEvent) -> Option<&'static PickerBinding> {
    PICKER_BINDINGS.iter().find(|binding| binding.key == key)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PickerInstanceId(u64);

impl PickerInstanceId {
    fn next() -> Self {
        Self(NEXT_PICKER_INSTANCE_ID.fetch_add(1, atomic::Ordering::Relaxed))
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PickerTrace {
    id: u64,
    label: &'static str,
    opened_at: std::time::Instant,
}

impl PickerTrace {
    pub(crate) fn new(label: &'static str, opened_at: std::time::Instant) -> Self {
        Self {
            id: NEXT_PICKER_TRACE_ID.fetch_add(1, atomic::Ordering::Relaxed),
            label,
            opened_at,
        }
    }

    pub(crate) fn log(self, phase: &'static str, details: fmt::Arguments<'_>) {
        log::info!(
            target: PICKER_TRACE_TARGET,
            "id={} label={} elapsed_us={} phase={} {}",
            self.id,
            self.label,
            self.opened_at.elapsed().as_micros(),
            phase,
            details,
        );
    }
}

#[derive(Clone)]
pub struct PickerRuntime {
    work: helix_runtime::Work,
    clock: helix_runtime::Clock,
    block: helix_runtime::Block,
    redraw: helix_runtime::FrameHandle,
}

impl PickerRuntime {
    pub fn new(editor: &Editor) -> Self {
        let runtime = editor.runtime();
        Self {
            work: runtime.work().clone(),
            clock: runtime.clock().clone(),
            block: runtime.block().clone(),
            redraw: editor.redraw_handle(),
        }
    }
}

pub const ID: &str = "picker";

pub const MIN_AREA_WIDTH_FOR_PREVIEW: u16 = 72;
pub const MIN_AREA_HEIGHT_FOR_PREVIEW: u16 = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PickerPreviewLayout {
    Hidden,
    Stacked,
    SideBySide,
}

fn picker_preview_layout(show_preview: bool, has_preview: bool, area: Rect) -> PickerPreviewLayout {
    if !show_preview
        || !has_preview
        || area.width < MIN_AREA_WIDTH_FOR_PREVIEW
        || area.height < MIN_AREA_HEIGHT_FOR_PREVIEW
    {
        return PickerPreviewLayout::Hidden;
    }

    if area.width > MIN_AREA_WIDTH_FOR_PREVIEW {
        PickerPreviewLayout::SideBySide
    } else {
        PickerPreviewLayout::Stacked
    }
}
/// Biggest file size to preview in bytes
pub const MAX_FILE_SIZE_FOR_PREVIEW: u64 = 10 * 1024 * 1024;

#[derive(PartialEq, Eq, Hash)]
pub enum PathOrId<'a> {
    Id(DocumentId),
    Path(&'a Path),
}

impl<'a> From<&'a Path> for PathOrId<'a> {
    fn from(path: &'a Path) -> Self {
        Self::Path(path)
    }
}

impl From<DocumentId> for PathOrId<'_> {
    fn from(v: DocumentId) -> Self {
        Self::Id(v)
    }
}

type FileCallback<T> = Box<dyn for<'a> Fn(&'a Editor, &'a T) -> Option<FileLocation<'a>> + Send>;

/// File path and range of lines (used to align and highlight lines)
pub type FileLocation<'a> = (PathOrId<'a>, Option<(usize, usize)>);

pub enum CachedPreview {
    Loading,
    Document(Box<Document>),
    Directory(Vec<(String, bool)>),
    Binary,
    LargeFile,
    NotFound,
}

fn cached_preview_kind(preview: &CachedPreview) -> &'static str {
    match preview {
        CachedPreview::Loading => "loading",
        CachedPreview::Document(_) => "document",
        CachedPreview::Directory(_) => "directory",
        CachedPreview::Binary => "binary",
        CachedPreview::LargeFile => "large_file",
        CachedPreview::NotFound => "not_found",
    }
}

#[derive(Clone)]
struct PreparedPreview {
    source: PreparedPreviewSource,
    range: Option<(usize, usize)>,
}

#[derive(Clone)]
enum PreparedPreviewSource {
    CachedPath(Arc<Path>),
    Document(DocumentId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PreviewSelectionKey {
    Path(Arc<Path>),
    Document(DocumentId),
}

fn should_request_preview_for_current_selection(
    last_preview_selection: &mut Option<PreviewSelectionKey>,
    current: PreviewSelectionKey,
) -> bool {
    if last_preview_selection.as_ref() == Some(&current) {
        return false;
    }

    *last_preview_selection = Some(current);
    true
}

enum RenderPreview<'a> {
    Cached(&'a CachedPreview),
    Document(&'a Document),
}

impl RenderPreview<'_> {
    fn document(&self) -> Option<&Document> {
        match self {
            Self::Document(doc) => Some(doc),
            Self::Cached(CachedPreview::Document(doc)) => Some(doc),
            _ => None,
        }
    }

    fn dir_content(&self) -> Option<&Vec<(String, bool)>> {
        match self {
            Self::Cached(CachedPreview::Directory(dir_content)) => Some(dir_content),
            _ => None,
        }
    }

    /// Alternate text to show for the preview.
    fn placeholder(&self) -> &str {
        match *self {
            Self::Document(_) => "<Invalid file location>",
            Self::Cached(preview) => match preview {
                CachedPreview::Loading => "<Loading preview>",
                CachedPreview::Document(_) => "<Invalid file location>",
                CachedPreview::Directory(_) => "<Invalid directory location>",
                CachedPreview::Binary => "<Binary file>",
                CachedPreview::LargeFile => "<File too large to preview>",
                CachedPreview::NotFound => "<File not found>",
            },
        }
    }
}

fn inject_nucleo_item<T, D>(
    injector: &nucleo::Injector<T>,
    columns: &[Column<T, D>],
    item: T,
    editor_data: &D,
) {
    injector.push(item, |item, dst| {
        for (column, text) in columns.iter().filter(|column| column.filter).zip(dst) {
            *text = column.format_text(item, editor_data).into()
        }
    });
}

pub struct Injector<T, D> {
    dst: nucleo::Injector<T>,
    columns: Arc<[Column<T, D>]>,
    editor_data: Arc<D>,
    version: usize,
    picker_version: Arc<AtomicUsize>,
    ingress: SharedIngress,
    redraw: SharedRedraw,
    trace: Option<PickerTrace>,
    /// A marker that requests a redraw when the injector drops.
    /// This marker causes the "running" indicator to disappear when a background job
    /// providing items is finished and drops. This could be wrapped in an [Arc] to ensure
    /// that the redraw is only requested when all Injectors drop for a Picker (which removes
    /// the "running" indicator) but the redraw handle is debounced so this is unnecessary.
    _redraw: RuntimeRedrawOnDrop,
}

struct RuntimeRedrawOnDrop {
    redraw: SharedRedraw,
    trace: Option<PickerTrace>,
}

impl Drop for RuntimeRedrawOnDrop {
    fn drop(&mut self) {
        if let Some(trace) = self.trace {
            trace.log("injector_drop_redraw", format_args!("request_redraw=true"));
        }
        self.redraw.request_redraw();
    }
}

impl<I, D> Clone for Injector<I, D> {
    fn clone(&self) -> Self {
        Injector {
            dst: self.dst.clone(),
            columns: self.columns.clone(),
            editor_data: self.editor_data.clone(),
            version: self.version,
            picker_version: self.picker_version.clone(),
            ingress: self.ingress.clone(),
            redraw: self.redraw.clone(),
            trace: self.trace,
            _redraw: RuntimeRedrawOnDrop {
                redraw: self.redraw.clone(),
                trace: self.trace,
            },
        }
    }
}

#[derive(Error, Debug)]
#[error("picker has been shut down")]
pub struct InjectorShutdown;

impl<T, D> Injector<T, D> {
    pub fn push(&self, item: T) -> Result<(), InjectorShutdown> {
        if self.version != self.picker_version.load(atomic::Ordering::Relaxed) {
            return Err(InjectorShutdown);
        }

        inject_nucleo_item(&self.dst, &self.columns, item, &self.editor_data);
        Ok(())
    }
}

type ColumnFormatFn<T, D> = for<'a> fn(&'a T, &'a D) -> Cell<'a>;

pub struct Column<T, D> {
    name: Arc<str>,
    format: ColumnFormatFn<T, D>,
    /// Whether the column should be passed to nucleo for matching and filtering.
    /// `DynamicPicker` uses this so that the dynamic column (for example regex in
    /// global search) is not used for filtering twice.
    filter: bool,
    hidden: bool,
}

impl<T, D> Column<T, D> {
    pub fn new(name: impl Into<Arc<str>>, format: ColumnFormatFn<T, D>) -> Self {
        Self {
            name: name.into(),
            format,
            filter: true,
            hidden: false,
        }
    }

    /// A column which does not display any contents
    pub fn hidden(name: impl Into<Arc<str>>) -> Self {
        let format = |_: &T, _: &D| unreachable!();

        Self {
            name: name.into(),
            format,
            filter: false,
            hidden: true,
        }
    }

    pub fn without_filtering(mut self) -> Self {
        self.filter = false;
        self
    }

    fn format<'a>(&self, item: &'a T, data: &'a D) -> Cell<'a> {
        (self.format)(item, data)
    }

    fn format_text<'a>(&self, item: &'a T, data: &'a D) -> Cow<'a, str> {
        let text: String = self.format(item, data).content.into();
        text.into()
    }
}

/// Returns a new list of options to replace the contents of the picker
/// when called with the current picker query,
type DynQueryCallback<T, D> = fn(
    &str,
    &mut Editor,
    Arc<D>,
    &Injector<T, D>,
    helix_runtime::Work,
) -> helix_runtime::Task<anyhow::Result<()>>;

enum DynamicQuery<T: 'static + Send + Sync, D: 'static> {
    Disabled,
    Debounced {
        debouncer: crate::runtime::RuntimeUiDebouncer,
        last_query: Arc<str>,
        callback: DynQueryCallback<T, D>,
    },
}

type PickerItemDataFn<T> = Box<dyn Fn(&T) -> helix_view::model::PickerItemData + Send + Sync>;

pub struct Picker<T: 'static + Send + Sync, D: 'static> {
    columns: Arc<[Column<T, D>]>,
    primary_column: usize,
    editor_data: Arc<D>,
    version: Arc<AtomicUsize>,
    matcher: Nucleo<T>,

    /// Current height of the completions box
    completion_height: u16,

    cursor: u32,
    prompt: Prompt,
    query: PickerQuery,

    /// Whether to show the preview panel (default true)
    show_preview: bool,
    /// When true, the picker source is responsible for query filtering and result ordering.
    external_filtering: bool,
    /// Constraints for tabular formatting
    widths: Vec<Constraint>,
    /// Read-only results viewport state.
    list_region: ContentRegion<()>,
    /// Cursor + wrap-arithmetic state machine shared with the file
    /// explorer and the menu. The picker is its third host. Owns
    /// `move_by` / `page_by` / `to_first` / `to_last` so the wrap
    /// math lives in one tested place. The picker keeps its
    /// `cursor: u32` field as a mirror because nucleo APIs and a
    /// handful of telemetry sites expect `u32` — every operation
    /// that goes through `nav` calls `sync_cursor_from_nav` to keep
    /// the mirror accurate. Scroll stays on `list_region` (its own
    /// `ContentRegion`-based math handles render-time clamping and
    /// the dynamic-query refresh loop).
    nav: helix_view::list_nav::ListNav,

    callback_fn: PickerCallback<T>,
    custom_key_handlers: PickerKeyHandlers<T, D>,

    pub truncate_start: bool,
    /// Caches paths to documents
    preview_cache: HashMap<Arc<Path>, CachedPreview>,
    /// Given an item in the picker, return the file path and line number to display.
    file_fn: Option<FileCallback<T>>,
    /// An event handler for syntax highlighting the currently previewed file.
    preview_highlight_handler: PreviewHighlightHandler,
    /// Read-only preview viewport state.
    preview_region: ContentRegion<()>,
    /// Preview source resolved during sync so render does not query editor state.
    prepared_preview: Option<PreparedPreview>,
    last_preview_selection: Option<PreviewSelectionKey>,
    dynamic_query: DynamicQuery<T, D>,
    /// Cached gradient border for rendering when enabled in config
    gradient_border: Option<GradientBorder>,
    trace: Option<PickerTrace>,
    render_count: u64,
    instance_id: PickerInstanceId,

    /// Layer ID in `Editor.model` for this picker. Set when the picker is first
    /// pushed to the compositor; used to sync render state to the shared UI model.
    model_layer_id: Option<helix_view::model::LayerId>,
    /// Optional callback to convert a picker item `T` into `PickerItemData` for the UI model.
    /// If `None`, items are stored as `PickerItemData::Plain`.
    item_data_fn: Option<PickerItemDataFn<T>>,
    selection_changed_handler: Option<PickerSelectionHandler<T, D>>,
    custom_hints: Vec<crate::widgets::Hint<'static>>,
    marked: Option<HashSet<u32>>,
    ingress: SharedIngress,
    redraw: SharedRedraw,
    work: helix_runtime::Work,
    block: helix_runtime::Block,
    clock: helix_runtime::Clock,
}

impl<T: 'static + Send + Sync, D: 'static + Send + Sync> Picker<T, D> {
    pub fn stream(
        columns: impl IntoIterator<Item = Column<T, D>>,
        editor_data: D,
        runtime: PickerRuntime,
        ingress: crate::runtime::RuntimeIngress,
    ) -> (Nucleo<T>, Injector<T, D>) {
        let columns: Arc<[_]> = columns.into_iter().collect();
        let matcher_columns = columns.iter().filter(|col| col.filter).count() as u32;
        assert!(matcher_columns > 0);
        let ingress = Arc::new(ingress);
        let redraw = Arc::new(runtime.redraw);
        let matcher = Nucleo::new(
            Config::DEFAULT,
            Arc::new({
                let redraw = redraw.clone();
                move || redraw.request_redraw()
            }),
            None,
            matcher_columns,
        );
        let streamer = Injector {
            dst: matcher.injector(),
            columns,
            editor_data: Arc::new(editor_data),
            version: 0,
            picker_version: Arc::new(AtomicUsize::new(0)),
            ingress: ingress.clone(),
            redraw: redraw.clone(),
            trace: None,
            _redraw: RuntimeRedrawOnDrop {
                redraw,
                trace: None,
            },
        };
        (matcher, streamer)
    }

    pub fn new<C, O, F>(
        columns: C,
        primary_column: usize,
        options: O,
        editor_data: D,
        runtime: PickerRuntime,
        ingress: crate::runtime::RuntimeIngress,
        callback_fn: F,
    ) -> Self
    where
        C: IntoIterator<Item = Column<T, D>>,
        O: IntoIterator<Item = T>,
        F: Fn(&mut Context, &T, Action) + Send + 'static,
    {
        let columns: Arc<[_]> = columns.into_iter().collect();
        let matcher_columns = columns.iter().filter(|col| col.filter).count() as u32;
        assert!(matcher_columns > 0);
        let ingress = Arc::new(ingress);
        let redraw = Arc::new(runtime.redraw.clone());
        let matcher = Nucleo::new(
            Config::DEFAULT,
            Arc::new({
                let redraw = redraw.clone();
                move || redraw.request_redraw()
            }),
            None,
            matcher_columns,
        );
        let injector = matcher.injector();
        for item in options {
            inject_nucleo_item(&injector, &columns, item, &editor_data);
        }
        let injector = Injector {
            dst: injector,
            columns: columns.clone(),
            editor_data: Arc::new(editor_data),
            version: 0,
            picker_version: Arc::new(AtomicUsize::new(0)),
            ingress: ingress.clone(),
            redraw: redraw.clone(),
            trace: None,
            _redraw: RuntimeRedrawOnDrop {
                redraw: redraw.clone(),
                trace: None,
            },
        };
        Self::with(matcher, primary_column, injector, runtime, callback_fn)
    }

    pub fn with_stream(
        matcher: Nucleo<T>,
        primary_column: usize,
        injector: Injector<T, D>,
        runtime: PickerRuntime,
        callback_fn: impl Fn(&mut Context, &T, Action) + Send + 'static,
    ) -> Self {
        Self::with(matcher, primary_column, injector, runtime, callback_fn)
    }

    fn with(
        matcher: Nucleo<T>,
        default_column: usize,
        injector: Injector<T, D>,
        runtime: PickerRuntime,
        callback_fn: impl Fn(&mut Context, &T, Action) + Send + 'static,
    ) -> Self {
        let Injector {
            columns,
            editor_data,
            picker_version: version,
            ingress,
            redraw,
            ..
        } = injector;
        assert!(!columns.is_empty());

        let prompt = Prompt::new(
            "".into(),
            None,
            ui::completers::none,
            |_editor: &mut Context, _pattern: &str, _event: PromptEvent| {},
        );

        let widths = columns
            .iter()
            .map(|column| Constraint::Length(column.name.chars().count() as u16))
            .collect();

        let query = PickerQuery::new(columns.iter().map(|col| &col.name).cloned(), default_column);
        let PickerRuntime {
            work, clock, block, ..
        } = runtime;
        let instance_id = PickerInstanceId::next();

        Self {
            columns,
            primary_column: default_column,
            matcher,
            editor_data,
            version,
            cursor: 0,
            prompt,
            query,
            truncate_start: true,
            show_preview: true,
            external_filtering: false,
            callback_fn: Box::new(callback_fn),
            completion_height: 0,
            widths,
            list_region: ContentRegion::default(),
            nav: helix_view::list_nav::ListNav::new(),
            preview_cache: HashMap::new(),
            custom_key_handlers: PickerKeyHandlers::new(),
            file_fn: None,
            preview_highlight_handler: PreviewHighlightHandler::new(
                instance_id,
                work.clone(),
                clock.clone(),
                ingress.clone(),
            ),
            preview_region: ContentRegion::default(),
            prepared_preview: None,
            last_preview_selection: None,
            dynamic_query: DynamicQuery::Disabled,
            gradient_border: None,
            trace: None,
            render_count: 0,
            instance_id,
            model_layer_id: None,
            item_data_fn: None,
            selection_changed_handler: None,
            custom_hints: Vec::new(),
            marked: None,
            ingress,
            redraw,
            work,
            block,
            clock,
        }
    }

    pub fn with_key_handlers(mut self, handlers: PickerKeyHandlers<T, D>) -> Self {
        self.custom_key_handlers = handlers;
        self
    }

    pub fn with_selection_changed_handler(mut self, handler: PickerSelectionHandler<T, D>) -> Self {
        self.selection_changed_handler = Some(handler);
        self
    }

    pub fn with_custom_hints(
        mut self,
        hints: impl IntoIterator<Item = crate::widgets::Hint<'static>>,
    ) -> Self {
        self.custom_hints.extend(hints);
        self
    }

    pub fn injector(&self) -> Injector<T, D> {
        Injector {
            dst: self.matcher.injector(),
            columns: self.columns.clone(),
            editor_data: self.editor_data.clone(),
            version: self.version.load(atomic::Ordering::Relaxed),
            picker_version: self.version.clone(),
            ingress: self.ingress.clone(),
            redraw: self.redraw.clone(),
            trace: self.trace,
            _redraw: RuntimeRedrawOnDrop {
                redraw: self.redraw.clone(),
                trace: self.trace,
            },
        }
    }

    pub(crate) fn with_trace(mut self, trace: PickerTrace) -> Self {
        let snapshot = self.matcher.snapshot();
        trace.log(
            "picker_constructed",
            format_args!(
                "columns={} external_filtering={} show_preview={} initial_total={} initial_matched={} active_injectors={}",
                self.columns.len(),
                self.external_filtering,
                self.show_preview,
                snapshot.item_count(),
                snapshot.matched_item_count(),
                self.matcher.active_injectors(),
            ),
        );
        self.trace = Some(trace);
        self
    }

    pub fn truncate_start(mut self, truncate_start: bool) -> Self {
        self.truncate_start = truncate_start;
        self
    }

    pub fn with_preview(
        mut self,
        preview_fn: impl for<'a> Fn(&'a Editor, &'a T) -> Option<FileLocation<'a>> + Send + 'static,
    ) -> Self {
        self.file_fn = Some(Box::new(preview_fn));
        // assumption: if we have a preview we are matching paths... If this is ever
        // not true this could be a separate builder function
        self.matcher.update_config(Config::DEFAULT.match_paths());
        self
    }

    pub fn with_history_register(mut self, history_register: Option<char>) -> Self {
        self.prompt.with_history_register(history_register);
        self
    }

    pub fn show_preview(mut self, show_preview: bool) -> Self {
        self.show_preview = show_preview;
        self
    }

    pub fn with_external_filtering(mut self) -> Self {
        self.external_filtering = true;
        self
    }

    pub fn with_dynamic_query(
        mut self,
        callback: DynQueryCallback<T, D>,
        debounce_ms: Option<u64>,
    ) -> Self {
        let debouncer = crate::runtime::RuntimeUiDebouncer::new(
            Duration::from_millis(debounce_ms.unwrap_or(100)),
            self.work.clone(),
            self.clock.clone(),
            (*self.ingress).clone(),
        );
        self.dynamic_query = DynamicQuery::Debounced {
            debouncer,
            last_query: "".into(),
            callback,
        };
        if let Some(trace) = self.trace {
            trace.log(
                "dynamic_query_enabled",
                format_args!("debounce_ms={}", debounce_ms.unwrap_or(100)),
            );
        }
        self.request_debounced_dynamic_query(self.primary_query(), true, false);
        self
    }

    pub fn with_initial_dynamic_query(mut self) -> Self {
        self.request_debounced_dynamic_query(self.primary_query(), true, true);
        self
    }

    fn request_debounced_dynamic_query(&mut self, query: Arc<str>, is_paste: bool, force: bool) {
        let DynamicQuery::Debounced {
            debouncer,
            last_query,
            ..
        } = &mut self.dynamic_query
        else {
            return;
        };

        if !force && query == *last_query {
            if let Some(trace) = self.trace {
                trace.log(
                    "dynamic_query_skip",
                    format_args!("query={query:?} reason=unchanged"),
                );
            }
            return;
        }

        *last_query = query.clone();
        if let Some(trace) = self.trace {
            trace.log(
                "dynamic_query_schedule",
                format_args!(
                    "query={query:?} is_paste={is_paste} force={force} immediate={}",
                    is_paste,
                ),
            );
        }
        let event = crate::runtime::UiCommand::Picker(
            crate::runtime::ui::command::PickerCommand::RunDynamicQuery { query },
        );
        if is_paste {
            debouncer.send_now(event);
        } else {
            debouncer.send(event);
        }
    }

    fn request_preview_highlight(&mut self, editor: &mut Editor, path: std::path::PathBuf) {
        let path: Arc<Path> = Arc::from(path);
        let Some(CachedPreview::Document(ref mut doc)) = self.preview_cache.get_mut(&path) else {
            return;
        };

        if doc.has_syntax() {
            return;
        }

        let Some(language) = doc.language_config().map(|config| config.language()) else {
            return;
        };

        let loader = editor.syn_loader.load();
        let text = doc.text().clone();
        let ingress = (*self.ingress).clone();
        let picker = self.instance_id;
        let path = path.to_path_buf();

        self.block
            .clone()
            .spawn(move || {
                let syntax = match helix_core::Syntax::new_with_timeout(
                    text.slice(..),
                    language,
                    &loader,
                    helix_core::syntax::BACKGROUND_PARSE_TIMEOUT,
                ) {
                    Ok(syntax) => syntax,
                    Err(err) => {
                        log::info!("highlighting picker preview failed: {err}");
                        return;
                    }
                };

                ingress.ui(crate::runtime::UiCommand::Picker(
                    crate::runtime::ui::command::PickerCommand::ApplyPreviewSyntax {
                        picker,
                        path,
                        syntax,
                    },
                ));
            })
            .detach();
    }

    fn apply_preview_syntax(
        &mut self,
        editor: &mut Editor,
        path: PathBuf,
        syntax: helix_core::Syntax,
    ) {
        let path: Arc<Path> = Arc::from(path);
        let Some(CachedPreview::Document(ref mut doc)) = self.preview_cache.get_mut(&path) else {
            return;
        };
        let diagnostics =
            helix_view::Editor::doc_diagnostics(&editor.language_servers, &editor.diagnostics, doc);
        doc.replace_diagnostics(diagnostics, &[], None);
        doc.set_syntax(Some(syntax));
    }

    fn queue_preview_load(&self, editor: &Editor, path: Arc<Path>) {
        let key = path.to_path_buf();
        let config = editor.config.clone();
        let syn_loader = editor.syn_loader.clone();
        let file_explorer_config = editor.config().file_explorer.clone();
        let picker = self.instance_id;

        crate::runtime::ui::snapshot::UiSnapshotRequest::new("[picker] preview_snapshot", key)
            .load_with(move |path| {
                let preview = (|| -> Result<CachedPreview, std::io::Error> {
                    let metadata = std::fs::metadata(&path)?;
                    if metadata.is_dir() {
                        let files =
                            ui::directory_content_with_config(&path, &file_explorer_config)?;
                        let file_names = files
                            .iter()
                            .filter_map(|(path, is_dir)| {
                                let name = path.file_name()?.to_string_lossy();
                                if *is_dir {
                                    Some((format!("{}/", name), true))
                                } else {
                                    Some((name.into_owned(), false))
                                }
                            })
                            .collect();
                        return Ok(CachedPreview::Directory(file_names));
                    }

                    if !metadata.is_file() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "neither a directory nor a file",
                        ));
                    }

                    if metadata.len() > MAX_FILE_SIZE_FOR_PREVIEW {
                        return Ok(CachedPreview::LargeFile);
                    }

                    let mut read_buffer = Vec::with_capacity(1024);
                    let content_type = std::fs::File::open(&path).and_then(|file| {
                        let n = file.take(1024).read_to_end(&mut read_buffer)?;
                        Ok(content_inspector::inspect(&read_buffer[..n]))
                    })?;
                    if content_type.is_binary() {
                        return Ok(CachedPreview::Binary);
                    }

                    Document::open(
                        &path,
                        None,
                        helix_view::document::LanguageInitialization::MetadataOnly,
                        config,
                        syn_loader.clone(),
                    )
                    .map_or(
                        Err(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "cannot open document",
                        )),
                        |doc| Ok(CachedPreview::Document(Box::new(doc))),
                    )
                })()
                .unwrap_or(CachedPreview::NotFound);
                Ok(preview)
            })
            .apply_with(move |path, preview| {
                crate::runtime::UiCommand::Picker(
                    crate::runtime::ui::command::PickerCommand::ApplyPreview {
                        picker,
                        path,
                        preview,
                    },
                )
            })
            .spawn(self.work.clone(), (*self.ingress).clone());
    }

    fn apply_preview(&mut self, editor: &mut Editor, path: PathBuf, preview: CachedPreview) {
        let path: Arc<Path> = Arc::from(path);
        let should_highlight = matches!(
            &preview,
            CachedPreview::Document(doc) if doc.language_config().is_some() && !doc.has_syntax()
        );
        if let Some(trace) = self.trace {
            trace.log(
                "preview_apply",
                format_args!(
                    "kind={} path={}",
                    cached_preview_kind(&preview),
                    path.display()
                ),
            );
        }
        self.preview_cache.insert(path.clone(), preview);
        if should_highlight {
            self.request_preview_highlight(editor, path.to_path_buf());
        }
    }

    fn run_dynamic_query(&mut self, editor: &mut Editor, query: Arc<str>) {
        if query != self.primary_query() {
            if let Some(trace) = self.trace {
                trace.log(
                    "dynamic_query_drop",
                    format_args!(
                        "query={query:?} current={:?} reason=query_mismatch",
                        self.primary_query()
                    ),
                );
            }
            return;
        }
        let generation = self.version.fetch_add(1, atomic::Ordering::Relaxed) + 1;
        if let Some(trace) = self.trace {
            let snapshot = self.matcher.snapshot();
            trace.log(
                "dynamic_query_run",
                format_args!(
                    "query={query:?} generation={generation} before_total={} before_matched={} active_injectors={}",
                    snapshot.item_count(),
                    snapshot.matched_item_count(),
                    self.matcher.active_injectors(),
                ),
            );
        }
        self.matcher.restart(false);
        let callback = match &self.dynamic_query {
            DynamicQuery::Disabled => return,
            DynamicQuery::Debounced { callback, .. } => *callback,
        };
        let injector = self.injector();
        let task = (callback)(
            &query,
            editor,
            self.editor_data.clone(),
            &injector,
            self.work.clone(),
        );
        self.work
            .clone()
            .spawn(async move {
                match task.await {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => log::info!("Dynamic request failed: {err}"),
                    Err(err) => log::info!("Dynamic request task failed: {err}"),
                }
            })
            .detach();
    }

    /// Sync `nav` with the latest matcher state + the current cursor,
    /// then sync the cursor mirror back after `op` runs. Centralizes
    /// the bookkeeping every nav-helper does so individual methods
    /// can express the actual movement in one line.
    fn with_nav<F>(&mut self, op: F)
    where
        F: FnOnce(&mut helix_view::list_nav::ListNav),
    {
        let len = self.matcher.snapshot().matched_item_count() as usize;
        if len == 0 {
            // Nothing to move through. Keep nav in a sane state so
            // a subsequent call after items appear starts from 0.
            self.nav.set_item_count(0);
            return;
        }
        self.nav.set_item_count(len);
        self.nav
            .set_viewport_height(self.completion_height as usize);
        // The picker's `cursor: u32` is the authoritative position
        // for nucleo + telemetry; push it into nav so the operation
        // starts from the right index.
        self.nav.set_selection(self.cursor as usize);
        op(&mut self.nav);
        self.cursor = self.nav.selection() as u32;
    }

    /// Move the cursor by a number of lines, either down (`Forward`) or up (`Backward`)
    pub fn move_by(&mut self, amount: u32, direction: Direction) {
        let delta = match direction {
            Direction::Forward => amount as isize,
            Direction::Backward => -(amount as isize),
        };
        self.with_nav(|nav| {
            // Pickers wrap on overflow — matches the previous
            // saturating-arithmetic mod-len behavior and the menu's.
            nav.move_by(delta, helix_view::list_nav::WrapBehavior::Wrap);
        });
    }

    /// Move the cursor down by exactly one page. After the last page comes the first page.
    pub fn page_up(&mut self) {
        let height = self.completion_height as usize;
        self.with_nav(|nav| {
            // The picker uses *full-viewport* pages (matches the
            // previous behavior of paging by `completion_height`),
            // wrapping on overflow.
            nav.page_by(
                -1,
                helix_view::list_nav::PageSize::Fixed(height),
                helix_view::list_nav::WrapBehavior::Wrap,
            );
        });
    }

    /// Move the cursor up by exactly one page. After the first page comes the last page.
    pub fn page_down(&mut self) {
        let height = self.completion_height as usize;
        self.with_nav(|nav| {
            nav.page_by(
                1,
                helix_view::list_nav::PageSize::Fixed(height),
                helix_view::list_nav::WrapBehavior::Wrap,
            );
        });
    }

    /// Move the cursor to the first entry
    pub fn to_start(&mut self) {
        self.with_nav(|nav| {
            nav.to_first();
        });
    }

    /// Move the cursor to the last entry
    pub fn to_end(&mut self) {
        self.with_nav(|nav| {
            nav.to_last();
        });
    }

    pub fn with_cursor(mut self, cursor: u32) -> Self {
        self.cursor = cursor;
        self
    }

    pub fn with_query(mut self, query: impl Into<String>, editor: &Editor) -> Self {
        self.prompt.set_line(query.into(), editor);
        self.handle_prompt_change(false);
        self
    }

    /// Block on the fuzzy matcher until any in-flight match work completes,
    /// so [`Self::render`] sees the final snapshot instead of a partial one.
    ///
    /// `Nucleo::tick` returns after at most one matcher pass: it acquires the
    /// worker lock (which the rayon-spawned match closure holds for the
    /// duration of its run), so when the lock returns the spawned work has
    /// finished and the snapshot is up to date. This is a single blocking
    /// wait, not a polling loop.
    ///
    /// Used by storybook fixtures and snapshot tests where a deterministic
    /// post-query result is required. The interactive picker uses the short
    /// `tick` inside `render` because it can afford to redraw on the next
    /// notify.
    pub fn drain_matcher(&mut self) {
        // 5s is generously longer than any realistic in-process match — even
        // hundreds of thousands of items finish in tens of milliseconds.
        // Under heavy parallel test load this gives the worker thread room.
        const DRAIN_TIMEOUT_MS: u64 = 5000;
        let _ = self.matcher.tick(DRAIN_TIMEOUT_MS);
    }

    fn sync_matcher_for_current_selection(&mut self) {
        let status = self.matcher.tick(10);
        if status.changed {
            let snapshot = self.matcher.snapshot();
            self.cursor = self
                .cursor
                .min(snapshot.matched_item_count().saturating_sub(1));
        }
    }

    pub fn selection(&self) -> Option<&T> {
        self.matcher
            .snapshot()
            .get_matched_item(self.cursor)
            .map(|item| item.data)
    }

    fn activate_selection(&self, ctx: &mut Context, action: Action) {
        let marked = self.marked_indices();
        if marked.is_empty() {
            if let Some(option) = self.selection() {
                (self.callback_fn)(ctx, option, action);
            }
            return;
        }

        let snapshot = self.matcher.snapshot();
        for index in marked {
            if let Some(option) = snapshot.get_matched_item(index) {
                (self.callback_fn)(ctx, option.data, action);
            }
        }
    }

    fn primary_query(&self) -> Arc<str> {
        self.query
            .get(&self.columns[self.primary_column].name)
            .cloned()
            .unwrap_or_else(|| "".into())
    }

    fn header_height(&self) -> u16 {
        if self.columns.len() > 1 {
            1
        } else {
            0
        }
    }

    pub fn toggle_preview(&mut self) {
        self.show_preview = !self.show_preview;
    }

    fn sync_list_region_view(&mut self, area: Rect, matched_count: u32) {
        self.list_region.set_area(area);
        self.list_region.set_content_height(matched_count as usize);

        if matched_count == 0 || area.height == 0 {
            ViewScrollable::scroll_to(&mut self.list_region, 0);
            return;
        }

        let next_scroll = helix_view::list_nav::ListViewport::new(
            matched_count as usize,
            Some(self.cursor.min(matched_count.saturating_sub(1)) as usize),
            area.height as usize,
            ViewScrollable::scroll(&self.list_region),
        )
        .scroll_to_selected();

        ViewScrollable::scroll_to(&mut self.list_region, next_scroll);
    }

    /// Set a callback to convert picker items into `PickerItemData` for the UI model.
    /// Without this, items are stored as `PickerItemData::Plain`.
    pub fn with_item_data(
        mut self,
        f: impl Fn(&T) -> helix_view::model::PickerItemData + Send + Sync + 'static,
    ) -> Self {
        self.item_data_fn = Some(Box::new(f));
        self
    }

    pub fn with_multi_select(mut self) -> Self {
        self.marked = Some(HashSet::new());
        self
    }

    pub fn toggle_mark(&mut self) {
        let Some(marked) = self.marked.as_mut() else {
            return;
        };
        if !marked.insert(self.cursor) {
            marked.remove(&self.cursor);
        }
    }

    pub fn marked_indices(&self) -> Vec<u32> {
        let Some(marked) = self.marked.as_ref() else {
            return Vec::new();
        };
        let mut marked_indices: Vec<_> = marked.iter().copied().collect();
        marked_indices.sort_unstable();
        marked_indices
    }

    /// Write a render-ready snapshot to `editor.model`.
    ///
    /// Called on each render frame. The snapshot contains the visible window of items
    /// with text and fuzzy-match highlight indices, plus metadata. Frontends (terminal,
    /// GUI, headless tests) can render or assert against this model without access to
    /// Nucleo, Column<T,D>, or the theme.
    pub fn sync_to_model(&mut self, editor: &mut Editor) {
        use helix_core::fuzzy::MATCHER;
        use helix_view::model::{
            PickerCell, PickerColumnHeader, PickerItemData, PickerModel, PickerRow, Placement,
        };

        let snapshot = self.matcher.snapshot();
        let matched_count = snapshot.matched_item_count() as usize;
        let total_count = snapshot.item_count() as usize;
        let is_running = self.matcher.active_injectors() > 0;

        // Visible window pagination — mirrors render_picker logic.
        let rows = self.completion_height.max(1) as u32;
        let offset = ViewScrollable::scroll(&self.list_region) as u32;
        let end = offset
            .saturating_add(rows)
            .min(snapshot.matched_item_count());

        // Build visible items. Lock MATCHER briefly per-row to avoid holding it
        // across the entire iteration (it's a global lock shared with render_picker).
        let match_paths = self.file_fn.is_some();
        let mut indices = Vec::new();

        let visible_items: Box<[PickerRow]> = snapshot
            .matched_items(offset..end)
            .map(|item| {
                let mut matcher_index = 0;
                let cells: Box<[PickerCell]> = self
                    .columns
                    .iter()
                    .filter(|c| !c.hidden)
                    .map(|column| {
                        let cell = column.format(item.data, &self.editor_data);
                        let text: String = cell.content.into();

                        let highlight_indices: Box<[u32]> = if column.filter {
                            // Acquire lock only for the indices() call, release immediately.
                            let mut matcher = MATCHER.lock();
                            matcher.config = nucleo::Config::DEFAULT;
                            if match_paths {
                                matcher.config.set_match_paths();
                            }
                            indices.clear();
                            snapshot.pattern().column_pattern(matcher_index).indices(
                                item.matcher_columns[matcher_index].slice(..),
                                &mut matcher,
                                &mut indices,
                            );
                            drop(matcher);

                            indices.sort_unstable();
                            indices.dedup();
                            matcher_index += 1;
                            // Move the vec out; a new empty vec is left for reuse on next iter.
                            std::mem::take(&mut indices).into_boxed_slice()
                        } else {
                            Box::default()
                        };

                        PickerCell {
                            text,
                            highlight_indices,
                        }
                    })
                    .collect();

                let data = self
                    .item_data_fn
                    .as_ref()
                    .map(|f| f(item.data))
                    .unwrap_or(PickerItemData::Plain);

                PickerRow { cells, data }
            })
            .collect();

        // Column headers (stable across frames — only rebuilt if multi-column).
        let headers: Box<[PickerColumnHeader]> = if self.columns.len() > 1 {
            self.columns
                .iter()
                .filter(|c| !c.hidden)
                .map(|c| PickerColumnHeader {
                    name: c.name.to_string().into_boxed_str(),
                })
                .collect()
        } else {
            Box::default()
        };

        let active_column = self
            .query
            .active_column(self.prompt.position())
            .and_then(|name| {
                self.columns
                    .iter()
                    .filter(|c| !c.hidden)
                    .position(|c| Arc::ptr_eq(&c.name, name))
            });

        // Preview info from file_fn.
        let preview = self.selection().and_then(|sel| {
            let file_fn = self.file_fn.as_ref()?;
            let (path_or_id, range) = file_fn(editor, sel)?;
            match path_or_id {
                PathOrId::Path(p) => Some(helix_view::model::PickerPreview::FilePath {
                    path: p.to_path_buf(),
                    line: range.map(|(start, _)| start),
                }),
                PathOrId::Id(_) => None,
            }
        });

        let model = PickerModel {
            query: self.prompt.line().to_string(),
            cursor: self.cursor.saturating_sub(offset) as usize,
            total_matched: matched_count,
            total_items: total_count,
            is_running,
            headers,
            active_column,
            visible_items,
            preview,
            show_preview: self.show_preview && self.file_fn.is_some(),
        };

        if let Some(trace) = self.trace {
            trace.log(
                "sync_model",
                format_args!(
                    "query={:?} total_matched={} total_items={} visible_items={} is_running={} active_injectors={} cursor={} offset={} rows={} completion_height={} layer_present={} preview_present={} show_preview={}",
                    self.prompt.line(),
                    matched_count,
                    total_count,
                    model.visible_items.len(),
                    is_running,
                    self.matcher.active_injectors(),
                    self.cursor,
                    offset,
                    rows,
                    self.completion_height,
                    self.model_layer_id.is_some(),
                    model.preview.is_some(),
                    model.show_preview,
                ),
            );
        }

        // Upsert into editor.model.
        let layer_id = self.model_layer_id;
        if let Some(id) = layer_id {
            if let Some(m) = editor.model.layer_model_mut::<PickerModel>(id) {
                *m = model;
                return;
            }
        }
        // First sync or layer was removed externally — register.
        self.model_layer_id = Some(editor.model.push_layer(
            Box::new(model),
            Placement::Centered {
                width: 0,
                height: 0,
            },
        ));
    }

    fn custom_key_event_handler(&mut self, event: &KeyEvent, cx: &mut Context) -> EventResult {
        if !self.prompt.line().is_empty()
            && event.modifiers == KeyModifiers::NONE
            && matches!(
                event.code,
                KeyCode::Char(_)
                    | KeyCode::Backspace
                    | KeyCode::Delete
                    | KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Home
                    | KeyCode::End
            )
        {
            return EventResult::Ignored(None);
        }

        match (self.custom_key_handlers.get(event), self.selection()) {
            (Some(PickerKeyAction::Immediate(callback)), Some(selected)) => {
                callback(cx, selected, Arc::clone(&self.editor_data), self.cursor);
                EventResult::Consumed(None)
            }
            (Some(PickerKeyAction::Confirmed(handler)), Some(selected)) => {
                // Confirmation seam: build the deferred action from the
                // selected item now, then hand the standard y/n prompt to the
                // compositor. See the module docs.
                let confirmation =
                    handler(cx, selected, Arc::clone(&self.editor_data), self.cursor);
                EventResult::Consumed(confirmation.map(PickerConfirmation::into_post_action))
            }
            _ => EventResult::Ignored(None),
        }
    }

    fn notify_selection_changed(&mut self, cx: &mut Context) {
        if let Some(callback) = &self.selection_changed_handler {
            callback(
                cx,
                self.selection(),
                Arc::clone(&self.editor_data),
                self.cursor,
            );
        }
    }

    fn prompt_handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        if let EventResult::Consumed(_) = self.prompt.handle_event(event, cx) {
            self.handle_prompt_change(matches!(event, Event::Paste(_)));
        }
        EventResult::Consumed(None)
    }

    fn handle_prompt_change(&mut self, is_paste: bool) {
        // TODO: better track how the pattern has changed
        let line = self.prompt.line();
        let old_query = self.query.parse(line);
        if self.query == old_query {
            return;
        }
        if let Some(trace) = self.trace {
            trace.log(
                "prompt_change",
                format_args!(
                    "line={:?} primary_query={:?} is_paste={} external_filtering={} cursor_before={}",
                    line,
                    self.primary_query(),
                    is_paste,
                    self.external_filtering,
                    self.cursor,
                ),
            );
        }
        // If the query has meaningfully changed, reset the cursor to the top of the results.
        self.cursor = 0;
        if !self.external_filtering {
            // Have nucleo reparse each changed column.
            for (i, column) in self
                .columns
                .iter()
                .filter(|column| column.filter)
                .enumerate()
            {
                let pattern = self
                    .query
                    .get(&column.name)
                    .map(|f| &**f)
                    .unwrap_or_default();
                let old_pattern = old_query
                    .get(&column.name)
                    .map(|f| &**f)
                    .unwrap_or_default();
                // Fastlane: most columns will remain unchanged after each edit.
                if pattern == old_pattern {
                    continue;
                }
                let is_append = pattern.starts_with(old_pattern);
                self.matcher.pattern.reparse(
                    i,
                    pattern,
                    CaseMatching::Smart,
                    Normalization::Smart,
                    is_append,
                );
            }
        }
        // If this is a dynamic picker, notify the query hook that the primary
        // query might have been updated.
        let query = self.primary_query();
        self.request_debounced_dynamic_query(query, is_paste, false);
    }

    /// Resolve preview source for the selected item during sync.
    ///
    /// Rendering only consumes `prepared_preview`; load/highlight requests stay
    /// in this sync phase where editor state is available intentionally.
    fn prepare_preview(&mut self, editor: &Editor) {
        self.prepared_preview = None;
        if !self.show_preview {
            return;
        }

        let preview_start = std::time::Instant::now();
        let Some(current) = self.selection() else {
            self.last_preview_selection = None;
            return;
        };
        let Some(file_fn) = self.file_fn.as_ref() else {
            return;
        };
        let Some((path_or_id, range)) = file_fn(editor, current) else {
            self.last_preview_selection = None;
            return;
        };

        match path_or_id {
            PathOrId::Path(path) => {
                let path: Arc<Path> = path.into();
                let selection_changed = should_request_preview_for_current_selection(
                    &mut self.last_preview_selection,
                    PreviewSelectionKey::Path(path.clone()),
                );

                if let Some(doc) = editor.document_by_path(path.as_ref()) {
                    if let Some(trace) = self.trace {
                        trace.log(
                            "preview_resolve",
                            format_args!(
                                "source=open_document path={} elapsed_us={}",
                                path.display(),
                                preview_start.elapsed().as_micros(),
                            ),
                        );
                    }
                    self.prepared_preview = Some(PreparedPreview {
                        source: PreparedPreviewSource::Document(doc.id()),
                        range,
                    });
                    return;
                }

                if self.preview_cache.contains_key(path.as_ref()) {
                    // NOTE: we use `HashMap::get_key_value` here instead of indexing so we can
                    // retrieve and cheaply clone the canonical `Arc<Path>` cache key for the
                    // preview highlight handler.
                    let (path, preview) = self.preview_cache.get_key_value(path.as_ref()).unwrap();
                    if selection_changed
                        && matches!(preview, CachedPreview::Document(doc) if doc.language_config().is_none())
                    {
                        self.preview_highlight_handler.request(path.clone());
                    }
                    if let Some(trace) = self.trace {
                        trace.log(
                            "preview_resolve",
                            format_args!(
                                "source=cache kind={} path={} elapsed_us={}",
                                cached_preview_kind(preview),
                                path.display(),
                                preview_start.elapsed().as_micros(),
                            ),
                        );
                    }
                    self.prepared_preview = Some(PreparedPreview {
                        source: PreparedPreviewSource::CachedPath(path.clone()),
                        range,
                    });
                    return;
                }

                let preview = CachedPreview::Loading;
                if let Some(trace) = self.trace {
                    trace.log(
                        "preview_resolve",
                        format_args!(
                            "source=queue_load kind={} path={} elapsed_us={}",
                            cached_preview_kind(&preview),
                            path.display(),
                            preview_start.elapsed().as_micros(),
                        ),
                    );
                }
                self.preview_cache.insert(path.clone(), preview);
                self.queue_preview_load(editor, path.clone());
                self.prepared_preview = Some(PreparedPreview {
                    source: PreparedPreviewSource::CachedPath(path),
                    range,
                });
            }
            PathOrId::Id(id) => {
                should_request_preview_for_current_selection(
                    &mut self.last_preview_selection,
                    PreviewSelectionKey::Document(id),
                );
                if !editor.documents.contains_key(&id) {
                    return;
                }
                if let Some(trace) = self.trace {
                    trace.log(
                        "preview_resolve",
                        format_args!(
                            "source=document_id id={id:?} elapsed_us={}",
                            preview_start.elapsed().as_micros(),
                        ),
                    );
                }
                self.prepared_preview = Some(PreparedPreview {
                    source: PreparedPreviewSource::Document(id),
                    range,
                });
            }
        }
    }

    fn render_picker<F>(
        &mut self,
        area: Rect,
        surface: &mut crate::render::CellSurface,
        cx: &RenderContext,
        render_prompt: F,
    ) where
        F: FnOnce(&mut Prompt, Rect, &mut crate::render::CellSurface, &RenderContext),
    {
        let render_start = std::time::Instant::now();
        let tick_start = std::time::Instant::now();
        let status = self.matcher.tick(10);
        let tick_elapsed = tick_start.elapsed();
        let matched_count = {
            let snapshot = self.matcher.snapshot();
            if status.changed {
                self.cursor = self
                    .cursor
                    .min(snapshot.matched_item_count().saturating_sub(1));
            }
            snapshot.matched_item_count()
        };
        let theme = cx.theme();
        let config = cx.config();
        let text_style = theme.get("ui.text");
        let selected = theme.get("ui.text.focus");
        // Picker match highlight: prefer a dedicated `ui.picker.match`
        // scope so themes can tune it (e.g. saturate the fg without
        // dragging other "special" syntax along), fall back to the
        // existing `special` syntax scope + bold so themes that
        // haven't been updated continue to work. The fallback chain
        // matches the chrome (`ui.text.inactive` → `comment` → base)
        // pattern used elsewhere in the picker.
        let highlight_style = theme
            .try_get("ui.picker.match")
            .unwrap_or_else(|| theme.get("special").add_modifier(Modifier::BOLD));
        // Muted variant for low-importance chrome: the count, separators,
        // header column labels. Falls back through common theme keys so any
        // theme yields a sensible dim colour without a dedicated picker.muted
        // scope.
        let muted_style = theme
            .try_get("ui.text.inactive")
            .or_else(|| theme.try_get("comment"))
            .unwrap_or(text_style);

        // -- Render the frame:
        // clear area
        let background = theme.get("ui.background");
        {
            let area = tui::ratatui::to_ratatui_rect(area);
            tui::ratatui::widgets::Widget::render(tui::ratatui::widgets::Clear, area, surface);
            surface.set_style(area, tui::ratatui::to_ratatui_style(background));
        };

        // calculate the inner area inside the box (respect gradient border thickness)
        let inner = if config.gradient_borders.enable {
            if self.gradient_border.is_none() {
                self.gradient_border =
                    Some(GradientBorder::from_theme(theme, &config.gradient_borders));
            }

            if let Some(ref mut gradient_border) = self.gradient_border {
                gradient_border.render(area, surface, theme, config.rounded_corners);
            }

            let t: u16 = config.gradient_borders.thickness as u16;
            Rect {
                x: area.x + t,
                y: area.y + t,
                width: area.width.saturating_sub(t * 2),
                height: area.height.saturating_sub(t * 2),
            }
        } else {
            crate::widgets::Panel::framed(
                crate::widgets::PanelStyle::plain(background),
                config.rounded_corners,
            )
            .render(surface, area)
        };

        // -- Layout: [prompt(1) | separator(1) | content(fill) | hints(1)]
        use helix_view::layout::{split_vertical, Size};
        let v_areas = split_vertical(
            inner,
            &[
                Size::fixed(1), // prompt
                Size::fixed(1), // separator
                Size::Fill,     // content
                Size::fixed(1), // hints
            ],
        );
        let prompt_row = v_areas[0];
        let separator_row = v_areas[1];
        let inner = v_areas[2]; // content area (reuse name for minimal diff below)
        let hint_row = v_areas[3];

        let list_area = inner.clip_top(self.header_height());
        self.sync_list_region_view(list_area, matched_count);

        let snapshot = self.matcher.snapshot();

        // -- Render the input bar:
        // Count uses a middle-dot separator and muted styling so it sits in
        // the background and the eye lands on the query itself.
        let count = format!(
            "{}{} · {}",
            if status.running || self.matcher.active_injectors() > 0 {
                "(running) "
            } else {
                ""
            },
            snapshot.matched_item_count(),
            snapshot.item_count(),
        );

        // Single accent chevron at the very left of the prompt row — a small
        // signature that gives every picker a consistent search affordance
        // without changing the prompt component's behaviour.
        if prompt_row.width >= 2 {
            surface.set_stringn(
                prompt_row.x,
                prompt_row.y,
                "›",
                1,
                tui::ratatui::to_ratatui_style(selected),
            );
        }

        // Reserve 2 cols (chevron + space) on the left and count+padding on
        // the right; the prompt component renders into the middle.
        let prompt_area = prompt_row.clip_left(2);
        let count_width = UnicodeWidthStr::width(count.as_str()) as u16;
        let line_area = prompt_area.clip_right(count_width + 1);

        // render the prompt first since it will clear its background
        render_prompt(&mut self.prompt, line_area, surface, cx);

        surface.set_stringn(
            (prompt_area.x + prompt_area.width).saturating_sub(count_width + 1),
            prompt_area.y,
            &count,
            count_width.min(prompt_area.width) as usize,
            tui::ratatui::to_ratatui_style(muted_style),
        );

        // -- Separator
        let sep_style = theme.get("ui.background.separator");
        crate::widgets::hdivider(surface, separator_row, sep_style);
        let rows = inner.height.saturating_sub(self.header_height()) as u32;
        let offset = ViewScrollable::scroll(&self.list_region) as u32;
        let cursor = self.cursor.saturating_sub(offset);
        let end = offset.saturating_add(rows).min(matched_count);
        let mut indices = Vec::new();
        let mut matcher = MATCHER.lock();
        matcher.config = Config::DEFAULT;
        if self.file_fn.is_some() {
            matcher.config.set_match_paths()
        }

        let options: Vec<_> = snapshot
            .matched_items(offset..end)
            .enumerate()
            .map(|(visible_index, item)| {
                let item_index = offset.saturating_add(visible_index as u32);
                let mut widths = self.widths.iter_mut();
                let mut matcher_index = 0;

                let row = Row::new(self.columns.iter().map(|column| {
                    if column.hidden {
                        return Cell::default();
                    }

                    let Some(Constraint::Length(max_width)) = widths.next() else {
                        unreachable!();
                    };
                    let mut cell = column.format(item.data, &self.editor_data);
                    let width = if column.filter {
                        snapshot.pattern().column_pattern(matcher_index).indices(
                            item.matcher_columns[matcher_index].slice(..),
                            &mut matcher,
                            &mut indices,
                        );
                        indices.sort_unstable();
                        indices.dedup();
                        let mut indices = indices.drain(..);
                        let mut next_highlight_idx = indices.next().unwrap_or(u32::MAX);
                        let mut span_list = Vec::new();
                        let mut current_span = String::new();
                        let mut current_style = Style::default();
                        let mut grapheme_idx = 0u32;
                        let mut width = 0;

                        let spans: &[Span] =
                            cell.content.lines.first().map_or(&[], |it| it.0.as_slice());
                        for span in spans {
                            // this looks like a bug on first glance, we are iterating
                            // graphemes but treating them as char indices. The reason that
                            // this is correct is that nucleo will only ever consider the first char
                            // of a grapheme (and discard the rest of the grapheme) so the indices
                            // returned by nucleo are essentially grapheme indecies
                            for grapheme in span.content.graphemes(true) {
                                let style = if grapheme_idx == next_highlight_idx {
                                    next_highlight_idx = indices.next().unwrap_or(u32::MAX);
                                    span.style.patch(highlight_style)
                                } else {
                                    span.style
                                };
                                if style != current_style {
                                    if !current_span.is_empty() {
                                        span_list.push(Span::styled(current_span, current_style))
                                    }
                                    current_span = String::new();
                                    current_style = style;
                                }
                                current_span.push_str(grapheme);
                                grapheme_idx += 1;
                            }
                            width += span.width();
                        }

                        span_list.push(Span::styled(current_span, current_style));
                        cell = Cell::from(Spans::from(span_list));
                        matcher_index += 1;
                        width
                    } else {
                        cell.content
                            .lines
                            .first()
                            .map(|line| line.width())
                            .unwrap_or_default()
                    };

                    if width as u16 > *max_width {
                        *max_width = width as u16;
                    }

                    cell
                }));

                if self.marked.is_some() {
                    let marked = self
                        .marked
                        .as_ref()
                        .is_some_and(|marked| marked.contains(&item_index));
                    let mut cells = Vec::with_capacity(row.cells.len() + 1);
                    cells.push(Cell::from(if marked { "✓" } else { " " }));
                    cells.extend(row.cells);
                    Row::new(cells)
                } else {
                    row
                }
            })
            .collect();

        let picker_symbol = config.picker_symbol.as_str();

        // -- Header
        let mut header = None;
        let mut header_style = Style::default();
        if self.columns.len() > 1 {
            let active_column = self.query.active_column(self.prompt.position());
            header_style = theme.get("ui.picker.header");
            let header_column_style = theme.get("ui.picker.header.column");

            let row = Row::new(self.columns.iter().map(|column| {
                if column.hidden {
                    Cell::default()
                } else {
                    let style = if active_column.is_some_and(|name| Arc::ptr_eq(name, &column.name))
                    {
                        theme.get("ui.picker.header.column.active")
                    } else {
                        header_column_style
                    };

                    Cell::from(Span::styled(Cow::from(&*column.name), style))
                }
            }))
            .style(header_style);
            header = Some(if self.marked.is_some() {
                let mut cells = Vec::with_capacity(row.cells.len() + 1);
                cells.push(Cell::default());
                cells.extend(row.cells);
                Row::new(cells).style(header_style)
            } else {
                row
            });
        }

        let mut table_widths = Vec::new();
        let table_widths = if self.marked.is_some() {
            table_widths.push(Constraint::Length(1));
            table_widths.extend(self.widths.iter().copied());
            table_widths.as_slice()
        } else {
            self.widths.as_slice()
        };

        PickerTable {
            rows: options,
            header,
            widths: table_widths,
            text_style,
            placeholder_style: theme.get("ui.text.inactive"),
            selected_style: selected,
            header_style,
            highlight_symbol: picker_symbol,
            selected_row: Some(cursor as usize),
            truncate_start: self.truncate_start,
        }
        .render(inner, surface);

        let mut hints: Vec<_> = PICKER_BINDINGS
            .iter()
            .filter_map(|binding| match binding.hint {
                PickerHintPolicy::Visible(hint)
                    if binding.action != PickerBindingAction::ToggleMark
                        || self.marked.is_some() =>
                {
                    Some(crate::widgets::Hint::new(hint.key, hint.label).priority(hint.priority))
                }
                _ => None,
            })
            .collect();
        hints.extend(self.custom_hints.iter().cloned());
        crate::widgets::hint_bar(
            surface,
            hint_row,
            hints.as_slice(),
            crate::widgets::HintBarStyle {
                background,
                key: selected,
                label: muted_style,
                separator: muted_style,
            },
        );

        if let Some(trace) = self.trace {
            trace.log(
                "render_picker",
                format_args!(
                    "frame={} area={}x{}+{},{} total_elapsed_us={} tick_us={} status_changed={} status_running={} active_injectors={} matched={} total={} rows={} offset={} end={} cursor={} relative_cursor={} scroll={}",
                    self.render_count,
                    area.width,
                    area.height,
                    area.x,
                    area.y,
                    render_start.elapsed().as_micros(),
                    tick_elapsed.as_micros(),
                    status.changed,
                    status.running,
                    self.matcher.active_injectors(),
                    matched_count,
                    snapshot.item_count(),
                    rows,
                    offset,
                    end,
                    self.cursor,
                    cursor,
                    ViewScrollable::scroll(&self.list_region),
                ),
            );
        }
    }

    fn render_preview(
        &mut self,
        area: Rect,
        surface: &mut crate::render::CellSurface,
        cx: &RenderContext,
    ) {
        // -- Render the frame:
        // clear area
        let theme = cx.theme();
        let config = cx.config();
        let background = theme.get("ui.background");
        let text = theme.get("ui.text");
        let directory = theme.get("ui.text.directory");
        {
            let area = tui::ratatui::to_ratatui_rect(area);
            tui::ratatui::widgets::Widget::render(tui::ratatui::widgets::Clear, area, surface);
            surface.set_style(area, tui::ratatui::to_ratatui_style(background));
        };

        // calculate the inner area inside the box (respect gradient border thickness)
        let base_inner = if config.gradient_borders.enable {
            if self.gradient_border.is_none() {
                self.gradient_border =
                    Some(GradientBorder::from_theme(theme, &config.gradient_borders));
            }
            if let Some(ref mut gradient_border) = self.gradient_border {
                gradient_border.render(area, surface, theme, config.rounded_corners);
            }
            let t: u16 = config.gradient_borders.thickness as u16;
            Rect {
                x: area.x + t,
                y: area.y + t,
                width: area.width.saturating_sub(t * 2),
                height: area.height.saturating_sub(t * 2),
            }
        } else {
            // Preview pane gets a subtly visible border (theme
            // `ui.window` → `comment` → background as fallback). This
            // distinguishes the preview from the picker list to its
            // left — previously both used `plain(background)` which
            // rendered border glyphs in the background color
            // (i.e. invisible), so the two panes blurred together
            // visually.
            let border_color = theme
                .try_get("ui.window")
                .or_else(|| theme.try_get("comment"))
                .unwrap_or(background);
            crate::widgets::Panel::framed(
                crate::widgets::PanelStyle::new(background, border_color, background),
                config.rounded_corners,
            )
            .render(surface, area)
        };
        // 1 column gap on either side
        let margin = Margin::horizontal(1);
        let inner = base_inner.inner(margin);

        let preview = self.prepared_preview.as_ref().and_then(|preview| {
            let render = match &preview.source {
                PreparedPreviewSource::Document(id) => RenderPreview::Document(cx.document(*id)?),
                PreparedPreviewSource::CachedPath(path) => {
                    RenderPreview::Cached(self.preview_cache.get(path)?)
                }
            };
            Some((render, preview.range))
        });

        if let Some((preview, range)) = preview {
            let doc = match preview.document() {
                Some(doc)
                    if range.is_none_or(|(start, end)| {
                        start <= end && end <= doc.text().len_lines()
                    }) =>
                {
                    doc
                }
                _ => {
                    if let Some(dir_content) = preview.dir_content() {
                        for (i, (path, is_dir)) in
                            dir_content.iter().take(inner.height as usize).enumerate()
                        {
                            let style = if *is_dir { directory } else { text };
                            surface.set_stringn(
                                inner.x,
                                inner.y + i as u16,
                                path,
                                inner.width as usize,
                                tui::ratatui::to_ratatui_style(style),
                            );
                        }
                        return;
                    }

                    let alt_text = preview.placeholder();
                    let x = inner.x
                        + inner
                            .width
                            .saturating_sub(UnicodeWidthStr::width(alt_text) as u16)
                            / 2;
                    let y = inner.y + inner.height / 2;
                    surface.set_stringn(
                        x,
                        y,
                        alt_text,
                        inner.width as usize,
                        tui::ratatui::to_ratatui_style(text),
                    );
                    return;
                }
            };

            let mut offset = ViewPosition::default();
            if let Some((start_line, end_line)) = range {
                let height = end_line - start_line;
                let text = doc.text().slice(..);
                let start = text.line_to_char(start_line);
                let middle = text.line_to_char(start_line + height / 2);
                if height < inner.height as usize {
                    let text_fmt = doc.text_format(inner.width, None);
                    let annotations = TextAnnotations::default();
                    (offset.anchor, offset.vertical_offset) = char_idx_at_visual_offset(
                        text,
                        middle,
                        // align to middle
                        -(inner.height as isize / 2),
                        0,
                        &text_fmt,
                        &annotations,
                    );
                    if start < offset.anchor {
                        offset.anchor = start;
                        offset.vertical_offset = 0;
                    }
                } else {
                    offset.anchor = start;
                }
            }

            let loader = cx.syntax_loader().load();

            let annotations = TextAnnotations::default();
            let syntax_highlighter =
                doc.viewport_syntax_highlighter(&loader, &annotations, offset.anchor, area.height);
            let mut overlay_highlights = Vec::new();
            if doc
                .language_config()
                .and_then(|config| config.rainbow_brackets)
                .unwrap_or(config.rainbow_brackets)
            {
                if let Some(overlay) = doc.viewport_rainbow_highlights(
                    &annotations,
                    offset.anchor,
                    area.height,
                    theme,
                    &loader,
                ) {
                    overlay_highlights.push(overlay);
                }
            }

            overlay_highlights.extend(doc.diagnostic_highlights(theme, None));

            let mut decorations = DecorationManager::default();

            if let Some((start, end)) = range {
                let style = theme
                    .try_get("ui.highlight")
                    .unwrap_or_else(|| theme.get("ui.selection"));
                let draw_highlight = move |renderer: &mut TextRenderer, pos: LinePos| {
                    if (start..=end).contains(&pos.doc_line) {
                        let area = Rect::new(
                            renderer.viewport.x,
                            pos.visual_line,
                            renderer.viewport.width,
                            1,
                        );
                        renderer.set_style(area, style)
                    }
                };
                decorations.add_decoration(draw_highlight);
            }

            render_document(
                surface,
                inner,
                doc,
                offset,
                // TODO: compute text annotations asynchronously here (like inlay hints)
                &TextAnnotations::default(),
                HighlighterInput::Live(syntax_highlighter),
                overlay_highlights,
                theme,
                decorations,
                None, // no dirty-row filtering for picker preview
                None,
                None,
            );

            self.preview_region.set_offset(offset);
        }
    }

    fn render_surface<F>(
        &mut self,
        area: Rect,
        surface: &mut crate::render::CellSurface,
        cx: &RenderContext,
        render_prompt: F,
    ) where
        F: FnOnce(&mut Prompt, Rect, &mut crate::render::CellSurface, &RenderContext),
    {
        use helix_view::layout::{split_horizontal, split_vertical, Size};

        let preview_layout = picker_preview_layout(self.show_preview, self.file_fn.is_some(), area);

        let (picker_area, preview_area) = match preview_layout {
            PickerPreviewLayout::Stacked => {
                let areas = split_vertical(area, &[Size::Percent(33), Size::Fill]);
                (areas[0], Some(areas[1]))
            }
            PickerPreviewLayout::SideBySide => {
                let areas = split_horizontal(area, &[Size::Percent(50), Size::Fill]);
                (areas[0], Some(areas[1]))
            }
            PickerPreviewLayout::Hidden => (area, None),
        };

        // Update completion_height from the actual picker area BEFORE syncing
        // to ensure the visible window calculation uses the correct row count.
        // (required_size may not have been called yet on the first frame.)
        self.completion_height = picker_area.height.saturating_sub(4 + self.header_height());
        self.render_count = self.render_count.saturating_add(1);
        if let Some(trace) = self.trace {
            trace.log(
                "render_layout",
                format_args!(
                    "frame={} area={}x{}+{},{} picker_area={}x{}+{},{} preview_area={} preview_layout={:?} completion_height={} prompt={:?}",
                    self.render_count,
                    area.width,
                    area.height,
                    area.x,
                    area.y,
                    picker_area.width,
                    picker_area.height,
                    picker_area.x,
                    picker_area.y,
                    preview_area
                        .map(|area| format!("{}x{}+{},{}", area.width, area.height, area.x, area.y))
                        .unwrap_or_else(|| "none".to_string()),
                    preview_layout,
                    self.completion_height,
                    self.prompt.line(),
                ),
            );
        }

        self.render_picker(picker_area, surface, cx, render_prompt);

        if let Some(preview_area) = preview_area {
            self.preview_region.set_area(preview_area);
            self.render_preview(preview_area, surface, cx);
        }
    }
}

impl<I: 'static + Send + Sync, D: 'static + Send + Sync> Component for Picker<I, D> {
    fn as_picker_component(&mut self) -> Option<&mut dyn PickerComponent> {
        Some(self)
    }

    fn sync(&mut self, editor: &mut Editor) {
        self.list_region.ensure_init(editor);
        self.preview_region.ensure_init(editor);
        self.sync_matcher_for_current_selection();
        self.prepare_preview(editor);
        self.sync_to_model(editor);
    }

    fn render(&mut self, area: Rect, surface: &mut crate::render::CellSurface, cx: &RenderContext) {
        self.render_surface(area, surface, cx, |prompt, area, surface, cx| {
            prompt.render(area, surface, cx);
        });
    }

    fn handle_event(&mut self, event: &Event, ctx: &mut Context) -> EventResult {
        // TODO: keybinds for scrolling preview

        let key_event = match event {
            Event::Key(event) => *event,
            Event::Paste(..) => return self.prompt_handle_event(event, ctx),
            Event::Resize(..) => return EventResult::Consumed(None),
            _ => return EventResult::Ignored(None),
        };

        let close_fn = |picker: &mut Self| {
            let ui_layer_id = picker.model_layer_id.take();
            // if the picker is very large don't store it as last_picker to avoid
            // excessive memory consumption
            let action = if picker.matcher.snapshot().item_count() > 100_000 {
                compositor::PostAction::PopLayer {
                    model_layer: ui_layer_id,
                    remember_picker: false,
                }
            } else {
                // stop streaming in new items in the background, really we should
                // be restarting the stream somehow once the picker gets
                // reopened instead (like for an FS crawl) that would also remove the
                // need for the special case above but that is pretty tricky
                picker.version.fetch_add(1, atomic::Ordering::Relaxed);
                compositor::PostAction::PopLayer {
                    model_layer: ui_layer_id,
                    remember_picker: true,
                }
            };
            EventResult::Consumed(Some(action))
        };

        // handle custom keybindings, if exist (a confirmed action surfaces its
        // y/n prompt as a `PushLayer` post action which must reach the
        // compositor, so the result is propagated rather than flattened)
        if let EventResult::Consumed(action) = self.custom_key_event_handler(&key_event, ctx) {
            return EventResult::Consumed(action);
        }

        let Some(binding) = picker_binding_for_key(key_event) else {
            self.prompt_handle_event(event, ctx);
            return EventResult::Consumed(None);
        };

        match binding.action {
            PickerBindingAction::Previous => {
                self.move_by(1, Direction::Backward);
                self.notify_selection_changed(ctx);
            }
            PickerBindingAction::Next => {
                self.move_by(1, Direction::Forward);
                self.notify_selection_changed(ctx);
            }
            PickerBindingAction::PageDown => {
                self.page_down();
                self.notify_selection_changed(ctx);
            }
            PickerBindingAction::PageUp => {
                self.page_up();
                self.notify_selection_changed(ctx);
            }
            PickerBindingAction::Start => {
                self.to_start();
                self.notify_selection_changed(ctx);
            }
            PickerBindingAction::End => {
                self.to_end();
                self.notify_selection_changed(ctx);
            }
            PickerBindingAction::ToggleMark => {
                if self.marked.is_some() {
                    self.toggle_mark();
                } else {
                    self.prompt_handle_event(event, ctx);
                }
            }
            PickerBindingAction::Close => return close_fn(self),
            PickerBindingAction::OpenKeep => {
                self.activate_selection(ctx, Action::Replace);
            }
            PickerBindingAction::Open => {
                // If the prompt has a history completion and is empty, use enter to accept
                // that completion
                if let Some(completion) = self
                    .prompt
                    .first_history_completion(ctx.editor)
                    .filter(|_| self.prompt.line().is_empty())
                {
                    // The percent character is used by the query language and needs to be
                    // escaped with a backslash.
                    let completion = if completion.contains('%') {
                        completion.replace('%', "\\%")
                    } else {
                        completion.into_owned()
                    };
                    self.prompt.set_line(completion, ctx.editor);

                    // Inserting from the history register is a paste.
                    self.handle_prompt_change(true);
                } else {
                    self.activate_selection(ctx, Action::Replace);
                    if let Some(history_register) = self.prompt.history_register() {
                        if let Err(err) = ctx
                            .editor
                            .registers
                            .push(history_register, self.primary_query().to_string())
                        {
                            ctx.editor.set_error(err.to_string());
                        }
                    }
                    return close_fn(self);
                }
            }
            PickerBindingAction::HorizontalSplit => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(ctx, option, Action::HorizontalSplit);
                }
                return close_fn(self);
            }
            PickerBindingAction::VerticalSplit => {
                if let Some(option) = self.selection() {
                    (self.callback_fn)(ctx, option, Action::VerticalSplit);
                }
                return close_fn(self);
            }
            PickerBindingAction::TogglePreview => {
                self.toggle_preview();
            }
        }

        EventResult::Consumed(None)
    }

    fn cursor(&self, area: Rect, editor: &Editor) -> (Option<Position>, CursorKind) {
        // calculate the inner area inside the box, honoring gradient border thickness
        let inner = if editor.config().gradient_borders.enable {
            let t: u16 = editor.config().gradient_borders.thickness as u16;
            Rect {
                x: area.x + t,
                y: area.y + t,
                width: area.width.saturating_sub(t * 2),
                height: area.height.saturating_sub(t * 2),
            }
        } else {
            crate::widgets::inset(area, 1, 1)
        };

        let picker_width =
            match picker_preview_layout(self.show_preview, self.file_fn.is_some(), area) {
                PickerPreviewLayout::SideBySide => inner.width / 2,
                PickerPreviewLayout::Stacked | PickerPreviewLayout::Hidden => inner.width,
            };
        let area = inner.clip_left(2).with_height(1).with_width(picker_width);

        self.prompt.cursor(area, editor)
    }

    fn required_size(&mut self, (width, height): (u16, u16)) -> Option<(u16, u16)> {
        self.completion_height = height.saturating_sub(4 + self.header_height());
        Some((width, height))
    }

    fn id(&self) -> Option<&str> {
        Some(ID)
    }
}

impl<T: 'static + Send + Sync, D: 'static + Send + Sync> PickerComponent for Picker<T, D> {
    fn instance_id(&self) -> PickerInstanceId {
        self.instance_id
    }

    fn request_preview_highlight(&mut self, editor: &mut Editor, path: std::path::PathBuf) {
        Self::request_preview_highlight(self, editor, path);
    }

    fn apply_preview(&mut self, editor: &mut Editor, path: PathBuf, preview: CachedPreview) {
        Self::apply_preview(self, editor, path, preview);
    }

    fn apply_preview_syntax(
        &mut self,
        editor: &mut Editor,
        path: PathBuf,
        syntax: helix_core::Syntax,
    ) {
        Self::apply_preview_syntax(self, editor, path, syntax);
    }

    fn run_dynamic_query(&mut self, editor: &mut Editor, query: Arc<str>) {
        Self::run_dynamic_query(self, editor, query);
    }
}
impl<T: 'static + Send + Sync, D> Drop for Picker<T, D> {
    fn drop(&mut self) {
        // ensure we cancel any ongoing background threads streaming into the picker
        self.version.fetch_add(1, atomic::Ordering::Relaxed);
        if let DynamicQuery::Debounced { debouncer, .. } = &self.dynamic_query {
            debouncer.cancel();
        }
    }
}

type PickerCallback<T> = Box<dyn Fn(&mut Context, &T, Action) + Send>;
pub type PickerKeyHandler<T, D> = Box<dyn Fn(&mut Context, &T, Arc<D>, u32) + Send + 'static>;

/// Handler for a key action that requires confirmation. Runs at keypress time
/// with the selected item; returns the confirmation to show, or `None` to skip
/// the prompt (the handler may set a status message itself in that case).
pub type PickerConfirmHandler<T, D> =
    Box<dyn Fn(&mut Context, &T, Arc<D>, u32) -> Option<PickerConfirmation> + Send + 'static>;

/// A pending confirmed picker action: the message rendered in the standard
/// y/n prompt plus the deferred action to run when the user confirms.
///
/// Built by [`PickerConfirmHandler`]s at keypress time so the deferred action
/// captures exactly the (cloned) item state it needs; see the module docs for
/// the full seam contract.
pub struct PickerConfirmation {
    message: String,
    on_confirm: Box<dyn FnOnce(&mut Context) + Send>,
}

impl PickerConfirmation {
    pub fn new(
        message: impl Into<String>,
        on_confirm: impl FnOnce(&mut Context) + Send + 'static,
    ) -> Self {
        Self {
            message: message.into(),
            on_confirm: Box::new(on_confirm),
        }
    }

    /// Render the confirmation as the standard y/n [`Prompt`] affordance,
    /// matching the file explorer delete interaction: `y` + Enter executes
    /// the deferred action, any other input (or Esc) cancels.
    pub(crate) fn into_post_action(self) -> compositor::PostAction {
        let Self {
            message,
            on_confirm,
        } = self;
        let mut on_confirm = Some(on_confirm);
        let prompt = Prompt::new(
            format!("{message} (y/n): ").into(),
            None,
            ui::completers::none,
            move |cx: &mut Context, input: &str, event: PromptEvent| {
                if event != PromptEvent::Validate {
                    return;
                }

                if input != "y" {
                    cx.editor.clear_status();
                    return;
                }

                if let Some(action) = on_confirm.take() {
                    action(cx);
                }
            },
        );
        compositor::PostAction::PushLayer(Box::new(prompt))
    }
}

enum PickerKeyAction<T, D> {
    Immediate(PickerKeyHandler<T, D>),
    Confirmed(PickerConfirmHandler<T, D>),
}

/// Component-local key bindings for picker actions (see module docs).
pub struct PickerKeyHandlers<T, D>(HashMap<KeyEvent, PickerKeyAction<T, D>>);

impl<T, D> PickerKeyHandlers<T, D> {
    pub fn new() -> Self {
        Self(HashMap::new())
    }

    /// Register an action that runs immediately when `key` is pressed.
    pub fn insert(&mut self, key: KeyEvent, handler: PickerKeyHandler<T, D>) {
        self.0.insert(key, PickerKeyAction::Immediate(handler));
    }

    /// Register an action that asks for y/n confirmation before running.
    pub fn insert_confirmed(&mut self, key: KeyEvent, handler: PickerConfirmHandler<T, D>) {
        self.0.insert(key, PickerKeyAction::Confirmed(handler));
    }

    fn get(&self, key: &KeyEvent) -> Option<&PickerKeyAction<T, D>> {
        self.0.get(key)
    }
}

impl<T, D> Default for PickerKeyHandlers<T, D> {
    fn default() -> Self {
        Self::new()
    }
}
pub type PickerSelectionHandler<T, D> =
    Box<dyn Fn(&mut Context, Option<&T>, Arc<D>, u32) + Send + 'static>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compositor::PostAction;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn with_test_context(test: impl FnOnce(&mut Context<'_>)) {
        let runtime = helix_runtime::test::runtime();
        let mut editor = helix_view::editor::EditorBuilder::new(
            helix_view::graphics::Rect::new(0, 0, 80, 24),
            runtime.clone(),
        )
        .build();
        let (ingress, _rx) = crate::runtime::RuntimeIngress::channel(runtime.work().clone());
        let (plugin_events, _plugin_events_rx) = helix_runtime::channel(1);
        let idle_reset = crate::runtime::IdleResetGate::new().handle();
        let redraw = editor.redraw_handle();
        let notifier = crate::handlers::local::Notifier {
            redraw: redraw.clone(),
            plugin_events,
        };
        let mut exit_tasks = crate::runtime::ExitTaskSet::default();
        let exit_task_work = editor.work();
        let mut cx = Context::new(
            &mut editor,
            &mut exit_tasks,
            exit_task_work,
            notifier,
            ingress,
            idle_reset,
            None,
        );
        test(&mut cx);
    }

    fn key_event(code: KeyCode) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[tokio::test]
    async fn picker_confirmation_confirm_runs_deferred_action() {
        let ran = Arc::new(AtomicBool::new(false));
        let ran_for_action = ran.clone();
        let confirmation = PickerConfirmation::new("Run action?", move |_cx| {
            ran_for_action.store(true, Ordering::Relaxed);
        });

        let action = confirmation.into_post_action();
        let PostAction::PushLayer(mut prompt) = action else {
            panic!("confirmation should push a prompt layer");
        };

        with_test_context(|cx| {
            prompt.handle_event(&key_event(KeyCode::Char('y')), cx);
            prompt.handle_event(&key_event(KeyCode::Enter), cx);
        });

        assert!(ran.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn picker_confirmation_cancel_skips_deferred_action() {
        let ran = Arc::new(AtomicBool::new(false));
        let ran_for_action = ran.clone();
        let confirmation = PickerConfirmation::new("Run action?", move |_cx| {
            ran_for_action.store(true, Ordering::Relaxed);
        });

        let action = confirmation.into_post_action();
        let PostAction::PushLayer(mut prompt) = action else {
            panic!("confirmation should push a prompt layer");
        };

        with_test_context(|cx| {
            prompt.handle_event(&key_event(KeyCode::Char('n')), cx);
            prompt.handle_event(&key_event(KeyCode::Enter), cx);
        });

        assert!(!ran.load(Ordering::Relaxed));
    }

    #[test]
    fn picker_bindings_all_have_hint_policy() {
        let mut keys = HashSet::new();
        for binding in PICKER_BINDINGS {
            assert!(keys.insert(binding.key), "duplicate picker key binding");
            match binding.hint {
                PickerHintPolicy::Visible(hint) => {
                    assert!(!hint.key.is_empty(), "visible hint key must be named");
                    assert!(!hint.label.is_empty(), "visible hint label must be named");
                }
                PickerHintPolicy::Hidden => {}
            }
            assert_eq!(
                picker_binding_for_key(binding.key).map(|binding| binding.action),
                Some(binding.action)
            );
        }
    }

    #[test]
    fn picker_preview_request_decision_triggers_initial_populate_and_dedupes() {
        let first: Arc<Path> = Arc::from(std::path::Path::new("first.rs"));
        let second: Arc<Path> = Arc::from(std::path::Path::new("second.rs"));
        let mut last = None;

        assert!(should_request_preview_for_current_selection(
            &mut last,
            PreviewSelectionKey::Path(first.clone())
        ));
        assert!(!should_request_preview_for_current_selection(
            &mut last,
            PreviewSelectionKey::Path(first)
        ));
        assert!(should_request_preview_for_current_selection(
            &mut last,
            PreviewSelectionKey::Path(second)
        ));

        last = None;
        assert!(should_request_preview_for_current_selection(
            &mut last,
            PreviewSelectionKey::Path(Arc::from(std::path::Path::new("first.rs")))
        ));
    }

    #[test]
    fn picker_sync_prepares_preview_for_initial_first_result() {
        fn path_cell(item: &PathBuf, _: &()) -> Cell<'static> {
            Cell::from(item.display().to_string())
        }

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("first.rs");
        std::fs::write(&path, "fn main() {}\n").unwrap();
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let runtime = rt.runtime();
            let mut editor = helix_view::editor::EditorBuilder::new(
                helix_view::graphics::Rect::new(0, 0, 80, 24),
                runtime.clone(),
            )
            .build();
            let (ingress, _rx) = crate::runtime::RuntimeIngress::channel(runtime.work().clone());
            let mut picker = Picker::new(
                [Column::new("path", path_cell)],
                0,
                [path.clone()],
                (),
                PickerRuntime::new(&editor),
                ingress,
                |_cx, _item, _action| {},
            )
            .with_preview(|_editor, item| Some((PathOrId::Path(item.as_path()), None)));

            <Picker<PathBuf, ()> as Component>::sync(&mut picker, &mut editor);

            assert!(picker.preview_cache.contains_key(path.as_path()));
            assert!(matches!(
                picker.prepared_preview,
                Some(PreparedPreview {
                    source: PreparedPreviewSource::CachedPath(ref preview_path),
                    ..
                }) if preview_path.as_ref() == path.as_path()
            ));
        });
    }

    #[test]
    fn picker_preview_layout_uses_side_by_side_once_window_is_wide_enough() {
        assert_eq!(
            picker_preview_layout(true, true, Rect::new(0, 0, 120, 30)),
            PickerPreviewLayout::SideBySide
        );
    }

    #[test]
    fn picker_preview_layout_keeps_stacked_fallback_at_minimum_width() {
        assert_eq!(
            picker_preview_layout(
                true,
                true,
                Rect::new(
                    0,
                    0,
                    MIN_AREA_WIDTH_FOR_PREVIEW,
                    MIN_AREA_HEIGHT_FOR_PREVIEW
                ),
            ),
            PickerPreviewLayout::Stacked
        );
    }

    #[test]
    fn picker_preview_layout_hides_preview_when_space_or_preview_is_missing() {
        assert_eq!(
            picker_preview_layout(
                true,
                true,
                Rect::new(
                    0,
                    0,
                    MIN_AREA_WIDTH_FOR_PREVIEW.saturating_sub(1),
                    MIN_AREA_HEIGHT_FOR_PREVIEW,
                ),
            ),
            PickerPreviewLayout::Hidden
        );
        assert_eq!(
            picker_preview_layout(
                true,
                true,
                Rect::new(
                    0,
                    0,
                    MIN_AREA_WIDTH_FOR_PREVIEW,
                    MIN_AREA_HEIGHT_FOR_PREVIEW.saturating_sub(1),
                ),
            ),
            PickerPreviewLayout::Hidden
        );
        assert_eq!(
            picker_preview_layout(false, true, Rect::new(0, 0, 120, 30)),
            PickerPreviewLayout::Hidden
        );
        assert_eq!(
            picker_preview_layout(true, false, Rect::new(0, 0, 120, 30)),
            PickerPreviewLayout::Hidden
        );
    }
}
