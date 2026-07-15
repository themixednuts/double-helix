//! Fuzzy picker component.
//!
//! # Custom key handlers and the confirmation seam
//!
//! Pickers can register component-local key bindings via [`PickerKeyHandlers`]
//! and [`Picker::with_key_handlers`]. Three kinds of actions are supported:
//!
//! - [`PickerKeyHandlers::insert`] registers an action that runs immediately
//!   on the key press (with the currently selected item).
//! - [`PickerKeyHandlers::insert_confirmed`] registers an action that requires
//!   user confirmation. The handler runs at keypress time with the selected
//!   item and returns an optional [`Confirmation`]: the message to show
//!   and the deferred action to run once the user confirms. Returning `None`
//!   skips the prompt entirely (e.g. the action does not apply to the selected
//!   item); the handler may set a status message itself in that case.
//! - [`PickerKeyHandlers::insert_layer`] opens a picker-level component and
//!   does not require a selected result.
//!
//! Confirmation reuses the standard y/n prompt affordance. The picker pushes a prompt
//! reading `"<message> (y/n): "`; typing `y` and pressing Enter executes the
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
        document::{render_document, LinePos, SyntaxRenderSnapshot, TextRenderer},
        gradient_border::GradientBorder,
        menu::{Cell, Row},
        picker::query::PickerQuery,
        text_decorations::DecorationManager,
    },
    widgets::PickerTable,
};
use helix_core::unicode::width::UnicodeWidthStr;
use helix_runtime::{LatestAdmissionError, LatestByKeySender};
use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Matcher, Nucleo};
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
        atomic::{self, AtomicBool, AtomicU64, AtomicUsize},
        Arc,
    },
    time::Duration,
};

use crate::ui::{Confirmation, Prompt, PromptEvent};
use helix_core::{
    char_idx_at_visual_offset, movement::Direction, text_annotations::TextAnnotations,
    unicode::segmentation::UnicodeSegmentation, Position,
};
use helix_view::{
    content_region::ContentRegion,
    editor::{
        Action, DocumentOpenRole, DocumentOpenWork, FileExplorerConfig, PreparedDocumentOpen,
    },
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

fn directory_preview(
    root: &Path,
    config: &FileExplorerConfig,
) -> Result<Vec<(PathBuf, bool)>, std::io::Error> {
    let mut content = ignore::WalkBuilder::new(root)
        .hidden(config.hidden)
        .parents(config.parents)
        .ignore(config.ignore)
        .follow_links(config.follow_symlinks)
        .git_ignore(config.git_ignore)
        .git_global(config.git_global)
        .git_exclude(config.git_exclude)
        .max_depth(Some(1))
        .add_custom_ignore_filename(helix_loader::config_dir().join("ignore"))
        .add_custom_ignore_filename(helix_loader::workspace_ignore_file_name())
        .types(crate::ui::file_scan::excluded_types())
        .build()
        .filter_map(|entry| {
            entry
                .map(|entry| {
                    let path = entry.path();
                    let is_dir = path.is_dir();
                    let mut path = path.to_path_buf();
                    if is_dir && path != root && config.flatten_dirs {
                        while let Some(child) = crate::ui::file_scan::single_child_directory(&path)
                        {
                            path = child;
                        }
                    }
                    (path, is_dir)
                })
                .ok()
                .filter(|entry| entry.0 != root)
        })
        .collect::<Vec<_>>();

    content.sort_by(|(left, left_is_dir), (right, right_is_dir)| {
        (!left_is_dir, left).cmp(&(!right_is_dir, right))
    });
    if root.parent().is_some() {
        content.insert(0, (root.join(".."), true));
    }
    Ok(content)
}

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
    Document(Box<PreparedDocumentOpen>),
    Directory(Arc<[(String, bool)]>),
    Binary,
    LargeFile,
    NotFound,
}

struct PreviewLoadRequest {
    generation: u64,
    picker: PickerInstanceId,
    path: PathBuf,
    open: DocumentOpenWork,
    config: FileExplorerConfig,
}

struct PreviewLoadService {
    tx: LatestByKeySender<(), PreviewLoadRequest>,
    latest_generation: Arc<AtomicU64>,
    next_generation: u64,
}

impl PreviewLoadService {
    fn spawn(
        work: helix_runtime::Work,
        block: helix_runtime::Block,
        ingress: SharedIngress,
    ) -> Self {
        let (tx, mut rx) = helix_runtime::latest_by_key::<(), PreviewLoadRequest>(1);
        let latest_generation = Arc::new(AtomicU64::new(0));
        let actor_latest = Arc::clone(&latest_generation);
        work.spawn(async move {
            while let Some(((), request)) = rx.recv().await {
                let generation = request.generation;
                let picker = request.picker;
                let path = request.path.clone();
                let started = std::time::Instant::now();
                let preview = match block.spawn(move || load_preview(request)).await {
                    Ok(preview) => preview,
                    Err(error) => {
                        log::error!(
                            "picker preview worker failed picker={picker:?} generation={generation} error={error}"
                        );
                        CachedPreview::NotFound
                    }
                };
                let stale = actor_latest.load(atomic::Ordering::Acquire) != generation;
                log::info!(
                    target: PICKER_TRACE_TARGET,
                    "phase=preview_load picker={picker:?} generation={generation} path={} kind={} stale={} elapsed_us={}",
                    path.display(),
                    cached_preview_kind(&preview),
                    stale,
                    started.elapsed().as_micros(),
                );
                if stale {
                    continue;
                }
                let _ = ingress
                    .send_ui(crate::runtime::UiCommand::Picker(
                        crate::runtime::ui::command::PickerCommand::ApplyPreview {
                            picker,
                            generation,
                            path,
                            preview,
                        },
                    ))
                    .await;
            }
        })
        .detach();

        Self {
            tx,
            latest_generation,
            next_generation: 1,
        }
    }

    fn submit(&mut self, mut request: PreviewLoadRequest) -> Option<u64> {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        request.generation = generation;
        let previous = self
            .latest_generation
            .swap(generation, atomic::Ordering::AcqRel);
        match self.tx.try_send((), request) {
            Ok(_) => Some(generation),
            Err(LatestAdmissionError::Full((), _)) => {
                let _ = self.latest_generation.compare_exchange(
                    generation,
                    previous,
                    atomic::Ordering::AcqRel,
                    atomic::Ordering::Acquire,
                );
                log::error!("picker preview admission invariant was violated");
                None
            }
            Err(LatestAdmissionError::Closed((), _)) => {
                let _ = self.latest_generation.compare_exchange(
                    generation,
                    previous,
                    atomic::Ordering::AcqRel,
                    atomic::Ordering::Acquire,
                );
                log::error!("picker preview service is closed");
                None
            }
        }
    }

    fn cancel(&self) {
        self.latest_generation.store(0, atomic::Ordering::Release);
    }
}

fn load_preview(request: PreviewLoadRequest) -> CachedPreview {
    let path = request.path;
    (|| -> Result<CachedPreview, std::io::Error> {
        let metadata = std::fs::metadata(&path)?;
        if metadata.is_dir() {
            let files = directory_preview(&path, &request.config)?;
            let names = files
                .iter()
                .filter_map(|(path, is_dir)| {
                    let name = path.file_name()?.to_string_lossy();
                    Some(if *is_dir {
                        (format!("{name}/"), true)
                    } else {
                        (name.into_owned(), false)
                    })
                })
                .collect::<Vec<_>>();
            return Ok(CachedPreview::Directory(names.into()));
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

        let mut buffer = Vec::with_capacity(1024);
        let content_type = std::fs::File::open(&path).and_then(|file| {
            let length = file.take(1024).read_to_end(&mut buffer)?;
            Ok(content_inspector::inspect(&buffer[..length]))
        })?;
        if content_type.is_binary() {
            return Ok(CachedPreview::Binary);
        }

        request.open.execute().map_or(
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "cannot open document",
            )),
            |prepared| Ok(CachedPreview::Document(Box::new(prepared))),
        )
    })()
    .unwrap_or(CachedPreview::NotFound)
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
            Self::Cached(CachedPreview::Document(prepared)) => Some(prepared.document()),
            _ => None,
        }
    }

    fn dir_content(&self) -> Option<&Arc<[(String, bool)]>> {
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
    alive: Arc<AtomicBool>,
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
    version: usize,
    picker_version: Arc<AtomicUsize>,
    alive: Arc<AtomicBool>,
    trace: Option<PickerTrace>,
}

impl Drop for RuntimeRedrawOnDrop {
    fn drop(&mut self) {
        let current = self.picker_version.load(atomic::Ordering::Acquire);
        let should_redraw = self.alive.load(atomic::Ordering::Acquire) && current == self.version;
        if let Some(trace) = self.trace {
            trace.log(
                "injector_drop_redraw",
                format_args!(
                    "request_redraw={should_redraw} generation={} current_generation={current}",
                    self.version
                ),
            );
        }
        if should_redraw {
            self.redraw.request_redraw();
        }
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
            alive: self.alive.clone(),
            ingress: self.ingress.clone(),
            redraw: self.redraw.clone(),
            trace: self.trace,
            _redraw: RuntimeRedrawOnDrop {
                redraw: self.redraw.clone(),
                version: self.version,
                picker_version: self.picker_version.clone(),
                alive: self.alive.clone(),
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
    helix_runtime::Block,
) -> helix_runtime::Task<anyhow::Result<()>>;

enum DynamicQuery<T: 'static + Send + Sync, D: 'static> {
    Disabled,
    Debounced {
        debouncer: crate::runtime::RuntimeUiDebouncer,
        schedule: DynamicQuerySchedule,
        last_query: Arc<str>,
        callback: DynQueryCallback<T, D>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DynamicQuerySchedule {
    Immediate,
    Debounced(Duration),
}

impl DynamicQuerySchedule {
    pub const fn debounced_ms(milliseconds: u64) -> Self {
        Self::Debounced(Duration::from_millis(milliseconds))
    }

    const fn delay(self) -> Duration {
        match self {
            Self::Immediate => Duration::ZERO,
            Self::Debounced(delay) => delay,
        }
    }

    const fn is_immediate(self) -> bool {
        matches!(self, Self::Immediate)
    }
}

type PickerItemDataFn<T> = Box<dyn Fn(&T) -> helix_view::model::PickerItemData + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PickerRowsKey {
    matcher_revision: u64,
    mark_revision: u64,
    offset: u32,
    end: u32,
}

struct PickerTableRenderSnapshot {
    model: Arc<helix_view::model::PickerModel>,
    area: Rect,
    text_style: Style,
    placeholder_style: Style,
    selected_style: Style,
    highlight_style: Style,
    header_style: Style,
    header_column_style: Style,
    active_header_style: Style,
    highlight_symbol: Arc<str>,
}

struct PickerChromeRenderSnapshot {
    area: Rect,
    background: Style,
    rounded_corners: bool,
    gradient_border: Option<GradientBorder>,
    prompt_row: Rect,
    separator_row: Rect,
    hint_row: Rect,
    count: Arc<str>,
    count_width: u16,
    selected_style: Style,
    muted_style: Style,
    separator_style: Style,
    hints: Arc<[crate::widgets::Hint<'static>]>,
}

impl PickerChromeRenderSnapshot {
    fn paint(self, surface: &mut crate::render::CellSurface) {
        let area = tui::ratatui::to_ratatui_rect(self.area);
        tui::ratatui::widgets::Widget::render(tui::ratatui::widgets::Clear, area, surface);
        surface.set_style(area, tui::ratatui::to_ratatui_style(self.background));
        if let Some(border) = self.gradient_border {
            border.render_no_theme(self.area, surface, self.rounded_corners);
        } else {
            crate::widgets::Panel::framed(
                crate::widgets::PanelStyle::plain(self.background),
                self.rounded_corners,
            )
            .render(surface, self.area);
        }

        if self.prompt_row.width >= 2 {
            surface.set_stringn(
                self.prompt_row.x,
                self.prompt_row.y,
                "›",
                1,
                tui::ratatui::to_ratatui_style(self.selected_style),
            );
        }
        let prompt_area = self.prompt_row.clip_left(2);
        surface.set_stringn(
            (prompt_area.x + prompt_area.width).saturating_sub(self.count_width + 1),
            prompt_area.y,
            self.count.as_ref(),
            self.count_width.min(prompt_area.width) as usize,
            tui::ratatui::to_ratatui_style(self.muted_style),
        );
        crate::widgets::hdivider(surface, self.separator_row, self.separator_style);
        crate::widgets::hint_bar(
            surface,
            self.hint_row,
            self.hints.as_ref(),
            crate::widgets::HintBarStyle {
                background: self.background,
                key: self.selected_style,
                label: self.muted_style,
                separator: self.muted_style,
            },
        );
    }
}

struct PickerPreviewChromeRenderSnapshot {
    area: Rect,
    background: Style,
    border: Style,
    rounded_corners: bool,
    gradient_border: Option<GradientBorder>,
}

impl PickerPreviewChromeRenderSnapshot {
    fn paint(self, surface: &mut crate::render::CellSurface) {
        let area = tui::ratatui::to_ratatui_rect(self.area);
        tui::ratatui::widgets::Widget::render(tui::ratatui::widgets::Clear, area, surface);
        surface.set_style(area, tui::ratatui::to_ratatui_style(self.background));
        if let Some(border) = self.gradient_border {
            border.render_no_theme(self.area, surface, self.rounded_corners);
        } else {
            crate::widgets::Panel::framed(
                crate::widgets::PanelStyle::new(self.background, self.border, self.background),
                self.rounded_corners,
            )
            .render(surface, self.area);
        }
    }
}

impl PickerTableRenderSnapshot {
    fn render(
        &self,
        surface: &mut crate::render::CellSurface,
        cancellation: &crate::render::RenderCancellation,
    ) {
        if cancellation.is_cancelled() {
            return;
        }

        let mut rows = Vec::with_capacity(self.model.visible_items.len());
        for (index, row) in self.model.visible_items.iter().enumerate() {
            if index % 8 == 0 && cancellation.is_cancelled() {
                return;
            }

            let marked = row.marked;
            let cells = row
                .cells
                .iter()
                .map(|cell| picker_cell(cell, self.highlight_style));
            let row = Row::new(cells);
            if self.model.markable {
                let mut cells = Vec::with_capacity(row.cells.len() + 1);
                cells.push(Cell::from(if marked { "✓" } else { " " }));
                cells.extend(row.cells);
                rows.push(Row::new(cells));
            } else {
                rows.push(row);
            }
        }

        let header = (!self.model.headers.is_empty()).then(|| {
            let row = Row::new(
                self.model
                    .headers
                    .iter()
                    .enumerate()
                    .map(|(index, header)| {
                        let style = if self.model.active_column == Some(index) {
                            self.active_header_style
                        } else {
                            self.header_column_style
                        };
                        Cell::from(Span::styled(Cow::Borrowed(header.name.as_ref()), style))
                    }),
            )
            .style(self.header_style);

            if self.model.markable {
                let mut cells = Vec::with_capacity(row.cells.len() + 1);
                cells.push(Cell::default());
                cells.extend(row.cells);
                Row::new(cells).style(self.header_style)
            } else {
                row
            }
        });

        let mut widths =
            Vec::with_capacity(self.model.widths.len() + usize::from(self.model.markable));
        if self.model.markable {
            widths.push(Constraint::Length(1));
        }
        widths.extend(self.model.widths.iter().copied().map(Constraint::Length));

        if cancellation.is_cancelled() {
            return;
        }

        PickerTable {
            rows,
            header,
            widths: &widths,
            text_style: self.text_style,
            placeholder_style: self.placeholder_style,
            selected_style: self.selected_style,
            header_style: self.header_style,
            highlight_symbol: self.highlight_symbol.as_ref(),
            selected_row: (self.model.cursor < self.model.visible_items.len())
                .then_some(self.model.cursor),
            truncate_start: self.model.truncate_start,
        }
        .render(self.area, surface);
    }
}

fn picker_cell(cell: &helix_view::model::PickerCell, highlight_style: Style) -> Cell<'_> {
    let mut rendered = Vec::with_capacity(cell.spans.len() + cell.highlight_indices.len() * 2);
    let mut highlights = cell.highlight_indices.iter().copied();
    let mut next_highlight = highlights.next().unwrap_or(u32::MAX);
    let mut grapheme_index = 0u32;

    for span in cell.spans.iter() {
        let text = span.text.as_str();
        let mut run_start = 0;
        let mut run_style = None;

        for (byte_index, _) in text.grapheme_indices(true) {
            let style = if grapheme_index == next_highlight {
                next_highlight = highlights.next().unwrap_or(u32::MAX);
                span.style.patch(highlight_style)
            } else {
                span.style
            };

            if let Some(current_style) = run_style {
                if current_style != style {
                    rendered.push(Span::styled(
                        Cow::Borrowed(&text[run_start..byte_index]),
                        current_style,
                    ));
                    run_start = byte_index;
                }
            }
            run_style = Some(style);
            grapheme_index = grapheme_index.saturating_add(1);
        }

        if let Some(style) = run_style {
            rendered.push(Span::styled(Cow::Borrowed(&text[run_start..]), style));
        } else if !text.is_empty() {
            rendered.push(Span::styled(Cow::Borrowed(text), span.style));
        }
    }

    Cell::from(Spans::from(rendered))
}

enum PickerPreviewContentSnapshot {
    Document {
        document: crate::ui::document::DocumentRenderSnapshot,
        offset: ViewPosition,
        syntax: SyntaxRenderSnapshot,
        overlays: Vec<helix_core::syntax::OverlayHighlights>,
        theme: Arc<helix_view::Theme>,
        highlighted_lines: Option<(usize, usize, Style)>,
    },
    Directory(Arc<[(String, bool)]>),
    Placeholder(Arc<str>),
}

struct PickerPreviewRenderSnapshot {
    inner: Rect,
    text_style: Style,
    directory_style: Style,
    content: PickerPreviewContentSnapshot,
}

impl PickerPreviewRenderSnapshot {
    fn render(
        self,
        surface: &mut crate::render::CellSurface,
        cancellation: &crate::render::RenderCancellation,
    ) {
        if cancellation.is_cancelled() {
            return;
        }

        match self.content {
            PickerPreviewContentSnapshot::Document {
                document,
                offset,
                syntax,
                overlays,
                theme,
                highlighted_lines,
            } => {
                let annotations = TextAnnotations::default();
                let mut decorations = DecorationManager::default();
                if let Some((start, end, style)) = highlighted_lines {
                    let inner = self.inner;
                    decorations.add_decoration(move |renderer: &mut TextRenderer, pos: LinePos| {
                        if (start..=end).contains(&pos.doc_line) {
                            renderer.set_style(
                                Rect::new(inner.x, pos.visual_line, inner.width, 1),
                                style,
                            );
                        }
                    });
                }

                render_document(
                    surface,
                    self.inner,
                    &document,
                    offset,
                    &annotations,
                    syntax,
                    overlays,
                    theme.as_ref(),
                    decorations,
                    None,
                    None,
                    None,
                    cancellation,
                );
            }
            PickerPreviewContentSnapshot::Directory(entries) => {
                for (index, (path, is_dir)) in
                    entries.iter().take(self.inner.height as usize).enumerate()
                {
                    if index % 16 == 0 && cancellation.is_cancelled() {
                        return;
                    }
                    let style = if *is_dir {
                        self.directory_style
                    } else {
                        self.text_style
                    };
                    surface.set_stringn(
                        self.inner.x,
                        self.inner.y + index as u16,
                        path,
                        self.inner.width as usize,
                        tui::ratatui::to_ratatui_style(style),
                    );
                }
            }
            PickerPreviewContentSnapshot::Placeholder(text) => {
                let x = self.inner.x
                    + self
                        .inner
                        .width
                        .saturating_sub(UnicodeWidthStr::width(text.as_ref()) as u16)
                        / 2;
                let y = self.inner.y + self.inner.height / 2;
                surface.set_stringn(
                    x,
                    y,
                    text.as_ref(),
                    self.inner.width as usize,
                    tui::ratatui::to_ratatui_style(self.text_style),
                );
            }
        }
    }
}

pub struct Picker<T: 'static + Send + Sync, D: 'static> {
    columns: Arc<[Column<T, D>]>,
    primary_column: usize,
    editor_data: Arc<D>,
    version: Arc<AtomicUsize>,
    alive: Arc<AtomicBool>,
    matcher: Nucleo<T>,
    highlight_matcher: Matcher,
    matcher_running: bool,
    matcher_revision: u64,
    mark_revision: u64,
    rendered_rows_key: Option<PickerRowsKey>,

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
    widths: Vec<u16>,
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
    preview_load_service: PreviewLoadService,
    pending_preview_load: Option<(u64, Arc<Path>)>,
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
    render_model: Arc<helix_view::model::PickerModel>,
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
        let picker_version = Arc::new(AtomicUsize::new(0));
        let alive = Arc::new(AtomicBool::new(true));
        let matcher = Nucleo::new(
            Config::DEFAULT,
            Arc::new({
                let redraw = redraw.clone();
                let alive = alive.clone();
                move || {
                    if alive.load(atomic::Ordering::Acquire) {
                        redraw.request_redraw();
                    }
                }
            }),
            None,
            matcher_columns,
        );
        let streamer = Injector {
            dst: matcher.injector(),
            columns,
            editor_data: Arc::new(editor_data),
            version: 0,
            picker_version: picker_version.clone(),
            alive: alive.clone(),
            ingress: ingress.clone(),
            redraw: redraw.clone(),
            trace: None,
            _redraw: RuntimeRedrawOnDrop {
                redraw,
                version: 0,
                picker_version,
                alive,
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
        let picker_version = Arc::new(AtomicUsize::new(0));
        let alive = Arc::new(AtomicBool::new(true));
        let matcher = Nucleo::new(
            Config::DEFAULT,
            Arc::new({
                let redraw = redraw.clone();
                let alive = alive.clone();
                move || {
                    if alive.load(atomic::Ordering::Acquire) {
                        redraw.request_redraw();
                    }
                }
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
            picker_version: picker_version.clone(),
            alive: alive.clone(),
            ingress: ingress.clone(),
            redraw: redraw.clone(),
            trace: None,
            _redraw: RuntimeRedrawOnDrop {
                redraw: redraw.clone(),
                version: 0,
                picker_version,
                alive,
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
            alive,
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
            .filter(|column| !column.hidden)
            .map(|column| column.name.chars().count() as u16)
            .collect();

        let query = PickerQuery::new(columns.iter().map(|col| &col.name).cloned(), default_column);
        let PickerRuntime {
            work, clock, block, ..
        } = runtime;
        let instance_id = PickerInstanceId::next();
        let preview_load_service =
            PreviewLoadService::spawn(work.clone(), block.clone(), ingress.clone());

        Self {
            columns,
            primary_column: default_column,
            matcher,
            highlight_matcher: Matcher::default(),
            matcher_running: false,
            matcher_revision: 0,
            mark_revision: 0,
            rendered_rows_key: None,
            editor_data,
            version,
            alive,
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
            preview_load_service,
            pending_preview_load: None,
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
            render_model: Arc::new(helix_view::model::PickerModel::default()),
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
            alive: self.alive.clone(),
            ingress: self.ingress.clone(),
            redraw: self.redraw.clone(),
            trace: self.trace,
            _redraw: RuntimeRedrawOnDrop {
                redraw: self.redraw.clone(),
                version: self.version.load(atomic::Ordering::Acquire),
                picker_version: self.version.clone(),
                alive: self.alive.clone(),
                trace: self.trace,
            },
        }
    }

    pub fn instance_id(&self) -> PickerInstanceId {
        self.instance_id
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
        schedule: DynamicQuerySchedule,
    ) -> Self {
        let debouncer = crate::runtime::RuntimeUiDebouncer::new(
            schedule.delay(),
            self.work.clone(),
            self.clock.clone(),
            (*self.ingress).clone(),
        );
        self.dynamic_query = DynamicQuery::Debounced {
            debouncer,
            schedule,
            last_query: "".into(),
            callback,
        };
        if let Some(trace) = self.trace {
            trace.log(
                "dynamic_query_enabled",
                format_args!("schedule={schedule:?}"),
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
            schedule,
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
            crate::runtime::ui::command::PickerCommand::RunDynamicQuery {
                picker: self.instance_id,
                query,
            },
        );
        if is_paste || schedule.is_immediate() {
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

        if doc.document().has_syntax() {
            return;
        }

        let Some(language) = doc
            .document()
            .language_config()
            .map(|config| config.language())
        else {
            return;
        };

        let loader = editor.syn_loader.load();
        let text = doc.document().text().clone();
        let ingress = (*self.ingress).clone();
        let picker = self.instance_id;
        let path = path.to_path_buf();

        let blocking = self.block.clone().spawn(move || {
            let syntax = match helix_core::Syntax::new_with_timeout(
                text.slice(..),
                language,
                &loader,
                helix_core::syntax::BACKGROUND_PARSE_TIMEOUT,
            ) {
                Ok(syntax) => syntax,
                Err(err) => {
                    log::info!("highlighting picker preview failed: {err}");
                    return None;
                }
            };
            Some((path, syntax))
        });
        self.work
            .clone()
            .spawn(async move {
                let Ok(Some((path, syntax))) = blocking.await else {
                    return;
                };
                let _ = ingress
                    .send_ui(crate::runtime::UiCommand::Picker(
                        crate::runtime::ui::command::PickerCommand::ApplyPreviewSyntax {
                            picker,
                            path,
                            syntax,
                        },
                    ))
                    .await;
            })
            .detach();
    }

    fn apply_preview_syntax(
        &mut self,
        _editor: &mut Editor,
        path: PathBuf,
        syntax: helix_core::Syntax,
    ) {
        let path: Arc<Path> = Arc::from(path);
        let Some(CachedPreview::Document(ref mut doc)) = self.preview_cache.get_mut(&path) else {
            return;
        };
        doc.document_mut().set_syntax(Some(syntax));
    }

    fn queue_preview_load(&mut self, editor: &Editor, path: Arc<Path>) {
        self.cancel_pending_preview_except(Some(path.as_ref()));
        let request = PreviewLoadRequest {
            generation: 0,
            picker: self.instance_id,
            path: path.to_path_buf(),
            open: editor.prepare_document_open(&path, DocumentOpenRole::Preview),
            config: editor.config().file_explorer.clone(),
        };
        if let Some(generation) = self.preview_load_service.submit(request) {
            self.preview_cache
                .insert(path.clone(), CachedPreview::Loading);
            self.pending_preview_load = Some((generation, path));
        } else {
            self.preview_cache.insert(path, CachedPreview::NotFound);
        }
    }

    fn cancel_pending_preview_except(&mut self, keep: Option<&Path>) {
        let Some((generation, pending)) = self.pending_preview_load.take() else {
            return;
        };
        if keep.is_some_and(|keep| keep == pending.as_ref()) {
            self.pending_preview_load = Some((generation, pending));
            return;
        }
        self.preview_load_service.cancel();
        if matches!(
            self.preview_cache.get(pending.as_ref()),
            Some(CachedPreview::Loading)
        ) {
            self.preview_cache.remove(pending.as_ref());
        }
    }

    fn apply_preview(
        &mut self,
        editor: &mut Editor,
        generation: u64,
        path: PathBuf,
        preview: CachedPreview,
    ) {
        let path: Arc<Path> = Arc::from(path);
        if self
            .pending_preview_load
            .as_ref()
            .is_none_or(|(pending_generation, pending_path)| {
                *pending_generation != generation || pending_path.as_ref() != path.as_ref()
            })
        {
            return;
        }
        self.pending_preview_load = None;
        let should_highlight = matches!(
            &preview,
            CachedPreview::Document(prepared)
                if prepared.document().language_config().is_some()
                    && !prepared.document().has_syntax()
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
            self.block.clone(),
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
    /// post-query result is required. Interactive sync only polls with a zero
    /// timeout and publishes whichever complete matcher generation is ready.
    pub fn drain_matcher(&mut self) {
        // 5s is generously longer than any realistic in-process match — even
        // hundreds of thousands of items finish in tens of milliseconds.
        // Under heavy parallel test load this gives the worker thread room.
        const DRAIN_TIMEOUT_MS: u64 = 5000;
        let _ = self.matcher.tick(DRAIN_TIMEOUT_MS);
    }

    fn sync_matcher_for_current_selection(&mut self) {
        let status = self.matcher.tick(0);
        self.matcher_running = status.running;
        if status.changed {
            self.matcher_revision = self.matcher_revision.wrapping_add(1);
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

    fn activate_single_selection(&mut self, ctx: &mut Context, action: Action) {
        let preview_path = self.selection().and_then(|option| {
            let (path, _) = self.file_fn.as_ref()?(ctx.editor, option)?;
            match path {
                PathOrId::Path(path) => Some(path.to_path_buf()),
                PathOrId::Id(_) => None,
            }
        });
        if let Some(path) = preview_path {
            if let Some(CachedPreview::Document(prepared)) =
                self.preview_cache.remove(path.as_path())
            {
                ctx.editor.cache_prepared_document_open(*prepared);
            }
        }
        if let Some(option) = self.selection() {
            (self.callback_fn)(ctx, option, action);
        }
    }

    fn activate_selection(&mut self, ctx: &mut Context, action: Action) {
        let marked = self.marked_indices();
        if marked.is_empty() {
            self.activate_single_selection(ctx, action);
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
        if self.columns.iter().filter(|column| !column.hidden).count() > 1 {
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
        self.mark_revision = self.mark_revision.wrapping_add(1);
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
    /// Called during component sync. The snapshot contains the visible window of
    /// styled items and fuzzy-match indices so paint never re-enters Nucleo or the
    /// generic column formatters.
    pub fn sync_to_model(&mut self, editor: &mut Editor) {
        use helix_view::model::{
            PickerCell, PickerColumnHeader, PickerItemData, PickerModel, PickerRow, PickerSpan,
            Placement,
        };

        let snapshot = self.matcher.snapshot();
        let matched_count = snapshot.matched_item_count() as usize;
        let total_count = snapshot.item_count() as usize;
        let is_running = self.matcher_running || self.matcher.active_injectors() > 0;

        // Visible window pagination — mirrors render_picker logic.
        let rows = self.completion_height.max(1) as u32;
        let offset = ViewScrollable::scroll(&self.list_region) as u32;
        let end = offset
            .saturating_add(rows)
            .min(snapshot.matched_item_count());

        let rows_key = PickerRowsKey {
            matcher_revision: self.matcher_revision,
            mark_revision: self.mark_revision,
            offset,
            end,
        };
        let visible_items: Arc<[PickerRow]> = if self.rendered_rows_key == Some(rows_key) {
            Arc::clone(&self.render_model.visible_items)
        } else {
            // Build each visible row once. The picker owns this matcher so
            // highlighting cannot contend with completion rendering.
            self.highlight_matcher.config = Config::DEFAULT;
            if self.file_fn.is_some() {
                self.highlight_matcher.config.set_match_paths();
            }

            let mut widths = self.widths.clone();
            let mut indices = Vec::new();
            let mut visible_items = Vec::with_capacity(end.saturating_sub(offset) as usize);

            for (visible_index, item) in snapshot.matched_items(offset..end).enumerate() {
                let item_index = offset.saturating_add(visible_index as u32);
                let mut matcher_index = 0;
                let mut visible_column_index = 0;
                let mut cells = Vec::with_capacity(widths.len());

                for column in self.columns.iter() {
                    if column.hidden {
                        continue;
                    }

                    let cell = column.format(item.data, &self.editor_data);
                    let line = cell.content.lines.first();
                    let width = line.map_or(0, |line| line.width());
                    widths[visible_column_index] =
                        widths[visible_column_index].max(u16::try_from(width).unwrap_or(u16::MAX));
                    visible_column_index += 1;

                    let spans = line
                        .map(|line| {
                            line.0
                                .iter()
                                .map(|span| PickerSpan {
                                    text: span.content.to_string(),
                                    style: span.style,
                                })
                                .collect::<Vec<_>>()
                                .into()
                        })
                        .unwrap_or_else(|| Arc::from([]));

                    let highlight_indices = if column.filter {
                        indices.clear();
                        snapshot.pattern().column_pattern(matcher_index).indices(
                            item.matcher_columns[matcher_index].slice(..),
                            &mut self.highlight_matcher,
                            &mut indices,
                        );
                        indices.sort_unstable();
                        indices.dedup();
                        matcher_index += 1;
                        Arc::from(indices.as_slice())
                    } else {
                        Arc::from([])
                    };

                    cells.push(PickerCell {
                        spans,
                        highlight_indices,
                    });
                }

                let data = self
                    .item_data_fn
                    .as_ref()
                    .map(|f| f(item.data))
                    .unwrap_or(PickerItemData::Plain);
                let marked = self
                    .marked
                    .as_ref()
                    .is_some_and(|marked| marked.contains(&item_index));
                visible_items.push(PickerRow {
                    cells: cells.into(),
                    marked,
                    data,
                });
            }
            self.widths = widths;
            self.rendered_rows_key = Some(rows_key);
            visible_items.into()
        };

        // Column headers (stable across frames — only rebuilt if multi-column).
        let headers: Arc<[PickerColumnHeader]> = if self.header_height() > 0 {
            self.columns
                .iter()
                .filter(|c| !c.hidden)
                .map(|c| PickerColumnHeader {
                    name: c.name.to_string().into_boxed_str(),
                })
                .collect()
        } else {
            Arc::from([])
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
            markable: self.marked.is_some(),
            headers,
            active_column,
            widths: self.widths.clone().into(),
            truncate_start: self.truncate_start,
            visible_items,
            preview,
            show_preview: self.show_preview && self.file_fn.is_some(),
        };
        self.render_model = Arc::new(model.clone());

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
            (Some(PickerKeyAction::Layer(handler)), _) => {
                let layer = handler(cx, Arc::clone(&self.editor_data), self.instance_id);
                EventResult::Consumed(Some(compositor::PostAction::PushLayer(layer)))
            }
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
                EventResult::Consumed(confirmation.map(Confirmation::into_post_action))
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
            self.cancel_pending_preview_except(None);
            return;
        };
        let Some(file_fn) = self.file_fn.as_ref() else {
            self.cancel_pending_preview_except(None);
            return;
        };
        let Some((path_or_id, range)) = file_fn(editor, current) else {
            self.last_preview_selection = None;
            self.cancel_pending_preview_except(None);
            return;
        };

        match path_or_id {
            PathOrId::Path(path) => {
                let path: Arc<Path> = path.into();
                let selection_changed = should_request_preview_for_current_selection(
                    &mut self.last_preview_selection,
                    PreviewSelectionKey::Path(path.clone()),
                );
                if selection_changed {
                    self.cancel_pending_preview_except(Some(path.as_ref()));
                }

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
                        && matches!(
                            preview,
                            CachedPreview::Document(prepared)
                                if prepared.document().language_config().is_none()
                        )
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
                self.queue_preview_load(editor, path.clone());
                self.prepared_preview = Some(PreparedPreview {
                    source: PreparedPreviewSource::CachedPath(path),
                    range,
                });
            }
            PathOrId::Id(id) => {
                if should_request_preview_for_current_selection(
                    &mut self.last_preview_selection,
                    PreviewSelectionKey::Document(id),
                ) {
                    self.cancel_pending_preview_except(None);
                }
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

    fn prepare_picker_render(&mut self, area: Rect, cx: &RenderContext) {
        let render_start = std::time::Instant::now();
        let model = Arc::clone(&self.render_model);
        let matched_count = model.total_matched as u32;
        let theme = cx.theme();
        let config = cx.config();
        let text_style = theme.get("ui.text");
        let selected = theme.get("ui.text.focus");
        let highlight_style = theme
            .try_get("ui.picker.match")
            .unwrap_or_else(|| theme.get("special").add_modifier(Modifier::BOLD));
        let muted_style = theme
            .try_get("ui.text.inactive")
            .or_else(|| theme.try_get("comment"))
            .unwrap_or(text_style);
        let background = theme.get("ui.background");
        let gradient_border = config
            .gradient_borders
            .enable
            .then(|| self.gradient_border.clone())
            .flatten();
        let inner = if gradient_border.is_some() {
            let thickness = config.gradient_borders.thickness as u16;
            Rect {
                x: area.x.saturating_add(thickness),
                y: area.y.saturating_add(thickness),
                width: area.width.saturating_sub(thickness.saturating_mul(2)),
                height: area.height.saturating_sub(thickness.saturating_mul(2)),
            }
        } else {
            crate::widgets::Panel::framed(
                crate::widgets::PanelStyle::plain(background),
                config.rounded_corners,
            )
            .content_area(area)
        };

        use helix_view::layout::{split_vertical, Size};
        let vertical = split_vertical(
            inner,
            &[Size::fixed(1), Size::fixed(1), Size::Fill, Size::fixed(1)],
        );
        let prompt_row = vertical[0];
        let separator_row = vertical[1];
        let table_area = vertical[2];
        let hint_row = vertical[3];

        let count: Arc<str> = Arc::from(format!(
            "{}{} · {}",
            if model.is_running { "(running) " } else { "" },
            model.total_matched,
            model.total_items,
        ));
        let prompt_area = prompt_row.clip_left(2);
        let count_width = UnicodeWidthStr::width(count.as_ref()) as u16;
        let line_area = prompt_area.clip_right(count_width.saturating_add(1));
        let hints: Arc<[crate::widgets::Hint<'static>]> = PICKER_BINDINGS
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
            .chain(self.custom_hints.iter().cloned())
            .collect::<Vec<_>>()
            .into();

        let chrome = PickerChromeRenderSnapshot {
            area,
            background,
            rounded_corners: config.rounded_corners,
            gradient_border,
            prompt_row,
            separator_row,
            hint_row,
            count,
            count_width,
            selected_style: selected,
            muted_style,
            separator_style: theme.get("ui.background.separator"),
            hints,
        };
        cx.defer_paint("picker_chrome", move |surface, cancellation| {
            if !cancellation.is_cancelled() {
                chrome.paint(surface);
            }
        });

        let prompt = self.prompt.prepare_render(line_area, cx);
        cx.defer_prepared("picker_prompt", vec![prompt]);

        let table = PickerTableRenderSnapshot {
            model,
            area: table_area,
            text_style,
            placeholder_style: theme.get("ui.text.inactive"),
            selected_style: selected,
            highlight_style,
            header_style: theme.get("ui.picker.header"),
            header_column_style: theme.get("ui.picker.header.column"),
            active_header_style: theme.get("ui.picker.header.column.active"),
            highlight_symbol: Arc::from(config.picker_symbol.as_str()),
        };
        cx.defer_paint("picker_table", move |surface, cancellation| {
            table.render(surface, cancellation);
        });

        if let Some(trace) = self.trace {
            let offset = ViewScrollable::scroll(&self.list_region) as u32;
            let end = offset.saturating_add(self.render_model.visible_items.len() as u32);
            trace.log(
                "render_picker",
                format_args!(
                    "frame={} area={}x{}+{},{} total_elapsed_us={} status_running={} matched={} total={} rows={} offset={} end={} cursor={} relative_cursor={} scroll={}",
                    self.render_count,
                    area.width,
                    area.height,
                    area.x,
                    area.y,
                    render_start.elapsed().as_micros(),
                    self.render_model.is_running,
                    matched_count,
                    self.render_model.total_items,
                    self.render_model.visible_items.len(),
                    offset,
                    end,
                    self.cursor,
                    self.render_model.cursor,
                    ViewScrollable::scroll(&self.list_region),
                ),
            );
        }
    }

    fn prepare_preview_render(&mut self, area: Rect, cx: &RenderContext) {
        let theme = cx.theme();
        let config = cx.config();
        let background = theme.get("ui.background");
        let text = theme.get("ui.text");
        let directory = theme.get("ui.text.directory");
        let gradient_border = config
            .gradient_borders
            .enable
            .then(|| self.gradient_border.clone())
            .flatten();
        let border = theme
            .try_get("ui.window")
            .or_else(|| theme.try_get("comment"))
            .unwrap_or(background);
        let base_inner = if gradient_border.is_some() {
            let t = config.gradient_borders.thickness as u16;
            Rect {
                x: area.x.saturating_add(t),
                y: area.y.saturating_add(t),
                width: area.width.saturating_sub(t.saturating_mul(2)),
                height: area.height.saturating_sub(t.saturating_mul(2)),
            }
        } else {
            crate::widgets::Panel::framed(
                crate::widgets::PanelStyle::new(background, border, background),
                config.rounded_corners,
            )
            .content_area(area)
        };
        let chrome = PickerPreviewChromeRenderSnapshot {
            area,
            background,
            border,
            rounded_corners: config.rounded_corners,
            gradient_border,
        };
        cx.defer_paint("picker_preview_chrome", move |surface, cancellation| {
            if !cancellation.is_cancelled() {
                chrome.paint(surface);
            }
        });

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

        let prepared = preview.map(|(preview, range)| {
            let document = preview.document().filter(|doc| {
                range.is_none_or(|(start, end)| start <= end && end <= doc.text().len_lines())
            });

            let Some(doc) = document else {
                let content = preview.dir_content().map_or_else(
                    || PickerPreviewContentSnapshot::Placeholder(Arc::from(preview.placeholder())),
                    |entries| PickerPreviewContentSnapshot::Directory(Arc::clone(entries)),
                );
                return (content, None);
            };

            let mut offset = ViewPosition::default();
            if let Some((start_line, end_line)) = range {
                let height = end_line - start_line;
                let doc_text = doc.text().slice(..);
                let start = doc_text.line_to_char(start_line);
                let middle = doc_text.line_to_char(start_line + height / 2);
                if height < inner.height as usize {
                    let text_format = doc.text_format(inner.width, None);
                    let annotations = TextAnnotations::default();
                    (offset.anchor, offset.vertical_offset) = char_idx_at_visual_offset(
                        doc_text,
                        middle,
                        -(inner.height as isize / 2),
                        0,
                        &text_format,
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

            let theme = cx.theme_arc();
            let loader = cx.syntax_loader().load_full();
            let annotations = TextAnnotations::default();
            let viewport_range = doc.viewport_byte_range(&annotations, offset.anchor, inner.height);
            let syntax = SyntaxRenderSnapshot::live(
                doc.syntax_arc(),
                Arc::clone(&loader),
                viewport_range.clone(),
            );
            let mut overlays = Vec::new();
            if doc
                .language_config()
                .and_then(|config| config.rainbow_brackets)
                .unwrap_or(config.rainbow_brackets)
            {
                if let Some(overlay) = doc.viewport_rainbow_highlights(
                    &annotations,
                    offset.anchor,
                    inner.height,
                    theme.as_ref(),
                    &loader,
                ) {
                    overlays.push(overlay);
                }
            }
            overlays.extend(doc.diagnostic_highlights(theme.as_ref(), Some(viewport_range)));

            let highlighted_lines = range.map(|(start, end)| {
                let style = theme
                    .try_get("ui.highlight")
                    .unwrap_or_else(|| theme.get("ui.selection"));
                (start, end, style)
            });
            let document = crate::ui::document::DocumentRenderSnapshot::new(
                doc,
                inner.width,
                Some(theme.as_ref()),
            );
            (
                PickerPreviewContentSnapshot::Document {
                    document,
                    offset,
                    syntax,
                    overlays,
                    theme,
                    highlighted_lines,
                },
                Some(offset),
            )
        });

        if let Some((content, offset)) = prepared {
            if let Some(offset) = offset {
                self.preview_region.set_offset(offset);
            }
            let snapshot = PickerPreviewRenderSnapshot {
                inner,
                text_style: text,
                directory_style: directory,
                content,
            };
            cx.defer_paint("picker_preview", move |surface, cancellation| {
                snapshot.render(surface, cancellation);
            });
        }
    }

    fn prepare_render_steps(&mut self, area: Rect, cx: &RenderContext) {
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

        self.prepare_picker_render(picker_area, cx);

        if let Some(preview_area) = preview_area {
            self.preview_region.set_area(preview_area);
            self.prepare_preview_render(preview_area, cx);
        }
    }
}

impl<I: 'static + Send + Sync, D: 'static + Send + Sync> Component for Picker<I, D> {
    fn as_picker_component(&mut self) -> Option<&mut dyn PickerComponent> {
        Some(self)
    }

    fn sync(&mut self, viewport: Rect, editor: &mut Editor) {
        let picker_area =
            match picker_preview_layout(self.show_preview, self.file_fn.is_some(), viewport) {
                PickerPreviewLayout::Stacked => {
                    use helix_view::layout::{split_vertical, Size};
                    split_vertical(viewport, &[Size::Percent(33), Size::Fill])[0]
                }
                PickerPreviewLayout::SideBySide | PickerPreviewLayout::Hidden => viewport,
            };
        self.completion_height = picker_area.height.saturating_sub(4 + self.header_height());
        self.render_count = self.render_count.saturating_add(1);
        self.list_region.ensure_init(editor);
        self.preview_region.ensure_init(editor);
        self.sync_matcher_for_current_selection();
        let matched_count = self.matcher.snapshot().matched_item_count();
        self.sync_list_region_view(
            Rect {
                height: self.completion_height,
                ..picker_area
            },
            matched_count,
        );
        self.prepare_preview(editor);
        self.sync_to_model(editor);
    }

    fn prepare_render(&mut self, area: Rect, cx: &RenderContext) -> crate::render::PreparedRender {
        let config = cx.config();
        if config.gradient_borders.enable
            && self
                .gradient_border
                .as_ref()
                .is_none_or(|border| !border.matches_config(&config.gradient_borders))
        {
            self.gradient_border = Some(GradientBorder::from_theme(
                cx.theme(),
                &config.gradient_borders,
            ));
        } else if !config.gradient_borders.enable {
            self.gradient_border = None;
        }
        if self
            .gradient_border
            .as_ref()
            .is_some_and(GradientBorder::is_animated)
        {
            cx.request_frame_at(
                helix_runtime::FrameSource::new("picker.gradient-border"),
                cx.clock().now(),
            );
        }
        self.prepare_render_steps(area, cx);
        crate::render::PreparedRender::ready(crate::render::RenderOutput::sparse(area))
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
                self.activate_single_selection(ctx, Action::HorizontalSplit);
                return close_fn(self);
            }
            PickerBindingAction::VerticalSplit => {
                self.activate_single_selection(ctx, Action::VerticalSplit);
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

    fn apply_preview(
        &mut self,
        editor: &mut Editor,
        generation: u64,
        path: PathBuf,
        preview: CachedPreview,
    ) {
        Self::apply_preview(self, editor, generation, path, preview);
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

    fn refresh_dynamic_query(&mut self, editor: &mut Editor) {
        let query = self.primary_query();
        Self::run_dynamic_query(self, editor, query);
    }
}
impl<T: 'static + Send + Sync, D> Drop for Picker<T, D> {
    fn drop(&mut self) {
        // ensure we cancel any ongoing background threads streaming into the picker
        self.alive.store(false, atomic::Ordering::Release);
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
    Box<dyn Fn(&mut Context, &T, Arc<D>, u32) -> Option<Confirmation> + Send + 'static>;

enum PickerKeyAction<T, D> {
    Immediate(PickerKeyHandler<T, D>),
    Confirmed(PickerConfirmHandler<T, D>),
    Layer(PickerLayerHandler<D>),
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

    /// Register an action that opens a component layer using picker-level
    /// data. Unlike row actions, this remains available for an empty picker.
    pub fn insert_layer(&mut self, key: KeyEvent, handler: PickerLayerHandler<D>) {
        self.0.insert(key, PickerKeyAction::Layer(handler));
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
pub type PickerLayerHandler<D> =
    Box<dyn Fn(&mut Context, Arc<D>, PickerInstanceId) -> Box<dyn Component> + Send + 'static>;

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
        let (ingress, _rx) = crate::runtime::RuntimeIngress::channel(runtime.clone());
        let (plugin_events, _plugin_events_rx) = helix_runtime::channel(1);
        let idle_reset = crate::runtime::IdleResetGate::new().handle();
        let redraw = editor.redraw_handle();
        let notifier = crate::handlers::local::Notifier {
            redraw: redraw.clone(),
            plugin_events: plugin_events.into(),
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
            crate::plugin_registry::PluginRuntime::default(),
        );
        test(&mut cx);
    }

    fn key_event(code: KeyCode) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn stale_picker_injector_drop_does_not_request_frame() {
        let mut gate = helix_runtime::FrameGate::new();
        let redraw = Arc::new(gate.handle());
        let mut redraws = gate.take_receiver();
        let picker_version = Arc::new(AtomicUsize::new(2));
        let alive = Arc::new(AtomicBool::new(true));

        drop(RuntimeRedrawOnDrop {
            redraw,
            version: 1,
            picker_version,
            alive,
            trace: None,
        });

        assert!(matches!(
            redraws.try_recv(),
            Err(helix_runtime::TryRecvError::Empty)
        ));
    }

    #[test]
    fn closed_picker_matcher_does_not_request_frame() {
        let mut gate = helix_runtime::FrameGate::new();
        let redraw = Arc::new(gate.handle());
        let mut redraws = gate.take_receiver();
        let picker_version = Arc::new(AtomicUsize::new(1));
        let alive = Arc::new(AtomicBool::new(false));

        drop(RuntimeRedrawOnDrop {
            redraw,
            version: 1,
            picker_version,
            alive,
            trace: None,
        });

        assert!(matches!(
            redraws.try_recv(),
            Err(helix_runtime::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn picker_confirmation_confirm_runs_deferred_action() {
        let ran = Arc::new(AtomicBool::new(false));
        let ran_for_action = ran.clone();
        let confirmation = Confirmation::new("Run action?", move |_cx| {
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
        let confirmation = Confirmation::new("Run action?", move |_cx| {
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
    fn picker_layer_handlers_do_not_require_a_result_action() {
        let mut handlers = PickerKeyHandlers::<String, ()>::new();
        let key = KeyEvent {
            code: KeyCode::Char('o'),
            modifiers: KeyModifiers::ALT,
        };
        handlers.insert_layer(key, Box::new(|_, _, _| unreachable!()));

        assert!(matches!(
            handlers.get(&key),
            Some(PickerKeyAction::Layer(_))
        ));
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
    fn picker_preview_rejects_stale_generation_before_cache_mutation() {
        let rt = helix_runtime::test::RuntimeTest::default();
        rt.block_on(async {
            let runtime = rt.runtime();
            let mut editor = helix_view::editor::EditorBuilder::new(
                helix_view::graphics::Rect::new(0, 0, 80, 24),
                runtime.clone(),
            )
            .build();
            let (ingress, _receiver) = crate::runtime::RuntimeIngress::channel(runtime);
            let path = PathBuf::from("preview.rs");
            let key: Arc<Path> = Arc::from(path.clone());
            let mut picker = Picker::new(
                [Column::new("path", |item: &PathBuf, _: &()| {
                    Cell::from(item.display().to_string())
                })],
                0,
                [path.clone()],
                (),
                PickerRuntime::new(&editor),
                ingress,
                |_cx, _item, _action| {},
            );
            picker
                .preview_cache
                .insert(key.clone(), CachedPreview::Loading);
            picker.pending_preview_load = Some((2, key));

            picker.apply_preview(&mut editor, 1, path.clone(), CachedPreview::NotFound);
            assert!(matches!(
                picker.preview_cache.get(path.as_path()),
                Some(CachedPreview::Loading)
            ));
            assert_eq!(
                picker.pending_preview_load.as_ref().map(|(id, _)| *id),
                Some(2)
            );

            picker.apply_preview(&mut editor, 2, path.clone(), CachedPreview::NotFound);
            assert!(matches!(
                picker.preview_cache.get(path.as_path()),
                Some(CachedPreview::NotFound)
            ));
            assert!(picker.pending_preview_load.is_none());
        });
    }

    #[test]
    fn picker_sync_prepares_preview_for_initial_first_result() {
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
            let (ingress, _rx) = crate::runtime::RuntimeIngress::channel(runtime.clone());
            let mut picker = Picker::new(
                [Column::new("path", |item: &PathBuf, _: &()| {
                    Cell::from(item.display().to_string())
                })],
                0,
                [path.clone()],
                (),
                PickerRuntime::new(&editor),
                ingress,
                |_cx, _item, _action| {},
            )
            .with_preview(|_editor, item| Some((PathOrId::Path(item.as_path()), None)));

            <Picker<PathBuf, ()> as Component>::sync(
                &mut picker,
                Rect::new(0, 0, 120, 40),
                &mut editor,
            );

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

    #[tokio::test]
    async fn picker_activation_promotes_prepared_preview_without_rereading() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("preview.txt");
        std::fs::write(&path, "prepared\n").unwrap();

        with_test_context(|cx| {
            cx.editor.new_file(Action::VerticalSplit);
            let prepared = cx
                .editor
                .prepare_document_open(&path, DocumentOpenRole::Preview)
                .execute()
                .unwrap();
            let mut picker = Picker::new(
                [Column::new("path", |item: &PathBuf, _: &()| {
                    Cell::from(item.display().to_string())
                })],
                0,
                [path.clone()],
                (),
                PickerRuntime::new(cx.editor),
                cx.ingress.clone(),
                |cx, item, action| {
                    cx.editor.open(item, action).unwrap();
                },
            )
            .with_preview(|_editor, item| Some((PathOrId::Path(item.as_path()), None)));
            picker.drain_matcher();
            picker.preview_cache.insert(
                Arc::from(path.clone()),
                CachedPreview::Document(Box::new(prepared)),
            );
            std::fs::write(&path, "changed after preview\n").unwrap();

            picker.activate_single_selection(cx, Action::Replace);

            let document = cx.editor.focused_document().unwrap();
            assert_eq!(
                document.path(),
                Some(&helix_stdx::path::canonicalize(&path))
            );
            assert_eq!(document.text().to_string(), "prepared\n");
            assert!(!document.is_preview());
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
