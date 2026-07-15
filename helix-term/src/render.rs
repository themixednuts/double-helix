use std::any::Any;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use helix_view::graphics::{CursorKind, Rect, Style};

/// Terminal-style cell grid used by the current render pipeline.
pub type CellSurface = tui::ratatui::buffer::Buffer;

/// Complete immutable frame handed from the renderer to the terminal presenter.
pub(crate) struct FramePacket {
    pub generation: helix_runtime::FrameGeneration,
    pub area: Rect,
    pub surface: CellSurface,
    pub cursor: Option<(u16, u16)>,
    pub cursor_kind: CursorKind,
    pub full_redraw: bool,
}

#[derive(Clone)]
pub struct RenderCancellation {
    latest: Arc<AtomicU64>,
    sequence: u64,
}

impl RenderCancellation {
    pub(crate) fn for_sequence(latest: Arc<AtomicU64>, sequence: u64) -> Self {
        Self { latest, sequence }
    }

    pub(crate) fn never() -> Self {
        Self {
            latest: Arc::new(AtomicU64::new(0)),
            sequence: 0,
        }
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.latest.load(Ordering::Acquire) != self.sequence
    }
}

type RenderStepWork = Box<dyn FnOnce(&mut CellSurface, &RenderCancellation) + Send>;
type StatefulRenderStepWork = Box<
    dyn FnOnce(&mut CellSurface, &mut CacheStore, &mut RenderMetadata, &RenderCancellation) + Send,
>;

enum RenderStepKind {
    Paint(RenderStepWork),
    Stateful(StatefulRenderStepWork),
    Prepared(Vec<PreparedRender>),
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RenderMetadata {
    cursor: Option<RenderCursor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RenderCursor {
    pub position: Option<(u16, u16)>,
    pub kind: CursorKind,
}

impl RenderMetadata {
    pub fn set_cursor(&mut self, position: Option<(u16, u16)>, kind: CursorKind) {
        self.cursor = Some(RenderCursor { position, kind });
    }

    pub fn cursor_override(self) -> Option<RenderCursor> {
        self.cursor
    }
}

pub(crate) struct RenderStep {
    name: &'static str,
    kind: RenderStepKind,
}

impl RenderStep {
    pub(crate) fn paint(
        name: &'static str,
        work: impl FnOnce(&mut CellSurface, &RenderCancellation) + Send + 'static,
    ) -> Self {
        Self {
            name,
            kind: RenderStepKind::Paint(Box::new(work)),
        }
    }

    pub(crate) fn prepared(name: &'static str, batch: Vec<PreparedRender>) -> Option<Self> {
        (!batch.is_empty()).then_some(Self {
            name,
            kind: RenderStepKind::Prepared(batch),
        })
    }

    pub(crate) fn stateful(
        name: &'static str,
        work: impl FnOnce(&mut CellSurface, &mut CacheStore, &mut RenderMetadata, &RenderCancellation)
            + Send
            + 'static,
    ) -> Self {
        Self {
            name,
            kind: RenderStepKind::Stateful(Box::new(work)),
        }
    }
}

/// Ordered, owned work for one coherent frame generation.
///
/// A plan can be seeded with previously prepared cells and extended with
/// `Send + 'static` paint steps. The render actor executes steps in z-order and
/// checks for supersession between them, so component snapshots never need to
/// borrow the foreground editor or compositor.
pub(crate) struct RenderPlan {
    area: Rect,
    seed: Option<CellSurface>,
    steps: Vec<RenderStep>,
}

pub(crate) struct RenderPlanResult {
    pub surface: CellSurface,
    pub complete: bool,
    pub metadata: RenderMetadata,
}

impl RenderPlan {
    /// Start from already prepared cells. This is also useful for partial-frame
    /// reuse: later steps may repaint only invalid regions in order.
    pub fn seeded(area: Rect, surface: CellSurface) -> Self {
        debug_assert_eq!(*surface.area(), tui::ratatui::to_ratatui_rect(area));
        Self {
            area,
            seed: Some(surface),
            steps: Vec::new(),
        }
    }

    pub fn extend(&mut self, steps: impl IntoIterator<Item = RenderStep>) {
        self.steps.extend(steps);
    }

    pub fn area(&self) -> Rect {
        self.area
    }

    pub fn take_seed(&mut self) -> Option<CellSurface> {
        self.seed.take()
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    pub fn execute(
        self,
        mut surface: CellSurface,
        cache: &mut CacheStore,
        cancellation: &RenderCancellation,
    ) -> RenderPlanResult {
        debug_assert_eq!(*surface.area(), tui::ratatui::to_ratatui_rect(self.area));
        if cancellation.is_cancelled() {
            return RenderPlanResult {
                surface,
                complete: false,
                metadata: RenderMetadata::default(),
            };
        }
        let mut metadata = RenderMetadata::default();
        let mut active_cache_ids = Vec::new();
        for step in &self.steps {
            if let RenderStepKind::Prepared(batch) = &step.kind {
                active_cache_ids.extend(
                    batch
                        .iter()
                        .filter_map(|prepared| prepared.tag.map(|tag| tag.id)),
                );
            }
        }
        cache.retain(|id| active_cache_ids.contains(&id));

        for step in self.steps {
            if cancellation.is_cancelled() {
                return RenderPlanResult {
                    surface,
                    complete: false,
                    metadata,
                };
            }
            let started = std::time::Instant::now();
            match step.kind {
                RenderStepKind::Paint(work) => work(&mut surface, cancellation),
                RenderStepKind::Stateful(work) => {
                    work(&mut surface, cache, &mut metadata, cancellation)
                }
                RenderStepKind::Prepared(batch) => {
                    for prepared in batch {
                        if cancellation.is_cancelled() {
                            return RenderPlanResult {
                                surface,
                                complete: false,
                                metadata,
                            };
                        }
                        let Some(prepared) = cache.resolve(prepared, cancellation) else {
                            return RenderPlanResult {
                                surface,
                                complete: false,
                                metadata,
                            };
                        };
                        blit_render(&prepared, &mut surface);
                    }
                }
            }
            helix_view::bench::log_run_phase("render_actor", step.name, started.elapsed(), || {
                format!("area={}x{}", self.area.width, self.area.height)
            });
        }
        RenderPlanResult {
            surface,
            complete: !cancellation.is_cancelled(),
            metadata,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheId(u64);

impl CacheId {
    pub fn hashed<T: Hash>(value: &T) -> Self {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        Self(hasher.finish())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey(u64);

impl CacheKey {
    pub fn hashed<T: Hash>(value: &T) -> Self {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        Self(hasher.finish())
    }
}

/// Cache identity bundle: stable slot + frame fingerprint + cached region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheTag {
    pub id: CacheId,
    pub key: CacheKey,
    pub area: Rect,
}

#[derive(Debug)]
pub struct RenderOutput {
    area: Rect,
    surface: CellSurface,
    blend: RenderBlend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenderBlend {
    Opaque,
    Sparse,
}

const SPARSE_CELL_MARKER: &str = "\u{fdd0}";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderCell<'a> {
    pub x: u16,
    pub y: u16,
    pub symbol: &'a str,
    pub style: Style,
}

#[derive(Debug, Clone, Copy)]
pub struct RenderCellRun<'a> {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub style: Style,
    cells: &'a [tui::ratatui::buffer::Cell],
}

pub struct RenderCellRuns<'a> {
    output: &'a RenderOutput,
    x: u16,
    y: u16,
}

/// Owned display-list representation of a rendered frame.
///
/// This is convenient for hosts that want self-contained styled text runs, but
/// it allocates row/run vectors and owned run text. Use [`RenderOutput::cells`]
/// or [`RenderOutput::cell_runs`] on hot paths that can consume the borrowed
/// cell buffer directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderScene {
    area: Rect,
    rows: Vec<RenderRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderRow {
    y: u16,
    runs: Vec<RenderRun>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderRun {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub text: String,
    pub style: Style,
}

impl RenderOutput {
    #[must_use]
    pub fn new(area: Rect) -> Self {
        Self {
            area,
            surface: CellSurface::empty(tui::ratatui::to_ratatui_rect(area)),
            blend: RenderBlend::Opaque,
        }
    }

    #[must_use]
    pub fn sparse(area: Rect) -> Self {
        let mut output = Self::new(area);
        output.blend = RenderBlend::Sparse;
        for cell in &mut output.surface.content {
            cell.set_symbol(SPARSE_CELL_MARKER);
        }
        output
    }

    #[must_use]
    pub fn reuse(area: Rect, mut surface: CellSurface) -> Self {
        if *surface.area() != tui::ratatui::to_ratatui_rect(area) {
            return Self::new(area);
        }
        surface.reset();
        Self {
            area,
            surface,
            blend: RenderBlend::Opaque,
        }
    }

    pub(crate) fn from_surface(area: Rect, surface: CellSurface) -> Self {
        debug_assert_eq!(*surface.area(), tui::ratatui::to_ratatui_rect(area));
        Self {
            area,
            surface,
            blend: RenderBlend::Opaque,
        }
    }

    #[must_use]
    pub fn area(&self) -> Rect {
        self.area
    }

    pub fn surface(&self) -> &CellSurface {
        &self.surface
    }

    pub fn surface_mut(&mut self) -> &mut CellSurface {
        &mut self.surface
    }

    pub fn into_surface(self) -> CellSurface {
        self.surface
    }

    fn into_resolved(self) -> ResolvedRender {
        ResolvedRender {
            surface: self.surface,
            blend: self.blend,
        }
    }

    pub fn cells(&self) -> impl Iterator<Item = RenderCell<'_>> {
        let area = *self.surface.area();
        self.surface
            .content
            .iter()
            .enumerate()
            .filter_map(move |(index, cell)| {
                if area.width == 0 {
                    return None;
                }
                let width = area.width as usize;
                let x = area.x + (index % width) as u16;
                let y = area.y + (index / width) as u16;
                Some(RenderCell {
                    x,
                    y,
                    symbol: cell.symbol(),
                    style: tui::ratatui::to_helix_style(cell.style()),
                })
            })
    }

    /// Iterate same-style cell runs without allocating owned run text.
    pub fn cell_runs(&self) -> RenderCellRuns<'_> {
        RenderCellRuns {
            output: self,
            x: self.area.left(),
            y: self.area.top(),
        }
    }

    /// Convert this frame to an owned display list.
    ///
    /// This allocates `RenderScene` row/run vectors and owned `String` text for
    /// each run. Prefer [`Self::cells`] or [`Self::cell_runs`] when the host can
    /// render directly from borrowed cell data.
    #[must_use]
    pub fn to_scene(&self) -> RenderScene {
        let mut rows = Vec::with_capacity(self.area.height as usize);
        for y in self.area.top()..self.area.bottom() {
            let mut row = RenderRow {
                y,
                runs: Vec::new(),
            };
            for x in self.area.left()..self.area.right() {
                let Some(cell) = self.surface.cell((x, y)) else {
                    continue;
                };
                let style = tui::ratatui::to_helix_style(cell.style());
                let symbol = cell.symbol();
                match row.runs.last_mut() {
                    Some(run) if run.style == style && run.x.saturating_add(run.width) == x => {
                        run.text.push_str(symbol);
                        run.width = run.width.saturating_add(1);
                    }
                    _ => row.runs.push(RenderRun {
                        x,
                        y,
                        width: 1,
                        text: symbol.to_owned(),
                        style,
                    }),
                }
            }
            rows.push(row);
        }
        RenderScene {
            area: self.area,
            rows,
        }
    }
}

impl<'a> RenderCellRun<'a> {
    pub fn cells(&self) -> &'a [tui::ratatui::buffer::Cell] {
        self.cells
    }

    pub fn symbols(&self) -> impl Iterator<Item = &str> {
        self.cells.iter().map(tui::ratatui::buffer::Cell::symbol)
    }

    pub fn write_text(&self, output: &mut String) {
        for symbol in self.symbols() {
            output.push_str(symbol);
        }
    }
}

impl<'a> Iterator for RenderCellRuns<'a> {
    type Item = RenderCellRun<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let area = self
            .output
            .area
            .intersection(tui::ratatui::to_helix_rect(*self.output.surface.area()));
        let surface_area = *self.output.surface.area();
        if area.width == 0 || surface_area.width == 0 {
            return None;
        }
        if self.y < area.top() {
            self.y = area.top();
        }
        if self.x < area.left() {
            self.x = area.left();
        }

        while self.y < area.bottom() {
            if self.x >= area.right() {
                self.x = area.left();
                self.y = self.y.saturating_add(1);
                continue;
            }

            let start_x = self.x;
            let y = self.y;
            let start = cell_index(surface_area, start_x, y)?;
            let first = self.output.surface.cell((start_x, y))?;
            let style = tui::ratatui::to_helix_style(first.style());
            let mut end_x = start_x.saturating_add(1);

            while end_x < area.right() {
                let Some(cell) = self.output.surface.cell((end_x, y)) else {
                    break;
                };
                if tui::ratatui::to_helix_style(cell.style()) != style {
                    break;
                }
                end_x = end_x.saturating_add(1);
            }

            self.x = end_x;
            let width = end_x.saturating_sub(start_x);
            let end = start + width as usize;
            return Some(RenderCellRun {
                x: start_x,
                y,
                width,
                style,
                cells: &self.output.surface.content[start..end],
            });
        }

        None
    }
}

fn cell_index(area: tui::ratatui::layout::Rect, x: u16, y: u16) -> Option<usize> {
    if x < area.x || y < area.y || x >= area.right() || y >= area.bottom() {
        return None;
    }
    let row = y.saturating_sub(area.y) as usize;
    let col = x.saturating_sub(area.x) as usize;
    Some(row * area.width as usize + col)
}

impl RenderScene {
    #[must_use]
    pub fn area(&self) -> Rect {
        self.area
    }

    pub fn rows(&self) -> &[RenderRow] {
        &self.rows
    }

    pub fn runs(&self) -> impl Iterator<Item = &RenderRun> {
        self.rows.iter().flat_map(RenderRow::runs)
    }
}

impl RenderRow {
    #[must_use]
    pub fn y(&self) -> u16 {
        self.y
    }

    pub fn runs(&self) -> &[RenderRun] {
        &self.runs
    }
}

pub enum RenderWork {
    Ready(RenderOutput),
    Deferred(Box<dyn FnOnce(&RenderCancellation) -> RenderOutput + Send>),
}

impl RenderWork {
    fn run(self, cancellation: &RenderCancellation) -> RenderOutput {
        match self {
            Self::Ready(output) => output,
            Self::Deferred(work) => work(cancellation),
        }
    }
}

/// A cell-grid render artifact ready for composition.
pub struct PreparedRender {
    tag: Option<CacheTag>,
    work: RenderWork,
}

impl PreparedRender {
    /// Uncached eager render.
    pub fn ready(output: RenderOutput) -> Self {
        Self {
            tag: None,
            work: RenderWork::Ready(output),
        }
    }

    /// Cached eager render.
    pub fn cached(tag: CacheTag, output: RenderOutput) -> Self {
        Self {
            tag: Some(tag),
            work: RenderWork::Ready(output),
        }
    }

    /// Uncached deferred render.
    pub fn deferred(
        work: impl FnOnce(&RenderCancellation) -> RenderOutput + Send + 'static,
    ) -> Self {
        Self {
            tag: None,
            work: RenderWork::Deferred(Box::new(work)),
        }
    }

    /// Cached deferred render (the snapshot pattern).
    ///
    /// Captures an owned `Send + 'static` snapshot and a render closure.
    /// The closure executes later — potentially on a rayon thread — with
    /// only the snapshot (no `&Editor` needed).
    pub fn snapshot<T, F>(tag: CacheTag, snapshot: T, render: F) -> Self
    where
        T: Send + 'static,
        F: FnOnce(T, &RenderCancellation) -> RenderOutput + Send + 'static,
    {
        Self {
            tag: Some(tag),
            work: RenderWork::Deferred(Box::new(move |cancellation| {
                render(snapshot, cancellation)
            })),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheState {
    Hit,
    Miss,
    Uncached,
}

/// Compact metadata for a single cache slot. Kept contiguous for
/// cache-friendly lookup (24 bytes per entry).
#[derive(Debug, Clone, Copy)]
struct CacheMeta {
    id: CacheId,
    key: CacheKey,
    area: Rect,
}

struct ResolvedRender {
    surface: CellSurface,
    blend: RenderBlend,
}

/// SoA (struct-of-arrays) cache store. The lookup hot path touches
/// only `meta` (contiguous 24-byte entries). Surfaces are stored in
/// a parallel array and only accessed on hit or store.
#[derive(Default)]
pub struct CacheStore {
    index: HashMap<CacheId, u32>,
    meta: Vec<CacheMeta>,
    surfaces: Vec<Arc<ResolvedRender>>,
    domains: HashMap<CacheId, Box<dyn Any + Send>>,
    domain_order: VecDeque<CacheId>,
}

impl CacheStore {
    const DOMAIN_CAPACITY: usize = 128;

    pub(crate) fn domain_mut<T>(&mut self, id: CacheId) -> &mut T
    where
        T: Default + Send + 'static,
    {
        self.domain_order.retain(|cached| *cached != id);
        self.domain_order.push_back(id);
        while self.domain_order.len() > Self::DOMAIN_CAPACITY {
            if let Some(retired) = self.domain_order.pop_front() {
                self.domains.remove(&retired);
            }
        }

        self.domains
            .entry(id)
            .or_insert_with(|| Box::new(T::default()))
            .downcast_mut()
            .expect("render cache domain ID must have one stable type")
    }

    pub fn retain(&mut self, mut keep: impl FnMut(CacheId) -> bool) {
        let mut i = 0;
        while i < self.meta.len() {
            if keep(self.meta[i].id) {
                i += 1;
            } else {
                self.index.remove(&self.meta[i].id);
                self.meta.swap_remove(i);
                self.surfaces.swap_remove(i);
                // Fix index for the element that was swapped into slot i.
                if i < self.meta.len() {
                    self.index.insert(self.meta[i].id, i as u32);
                }
            }
        }
    }

    pub fn compose(&mut self, prepared: PreparedRender, surface: &mut CellSurface) -> CacheState {
        let cancellation = RenderCancellation::never();
        let PreparedRender { tag, work } = prepared;
        let Some(tag) = tag else {
            let output = work.run(&cancellation);
            blit_render(&output.into_resolved(), surface);
            return CacheState::Uncached;
        };

        if let Some(cached) = self.lookup(tag.id, tag.key, tag.area) {
            blit_render(cached, surface);
            return CacheState::Hit;
        }

        let output = work.run(&cancellation);
        let output = output.into_resolved();
        blit_render(&output, surface);
        self.store(tag, output);
        CacheState::Miss
    }

    /// Compose a batch of [`PreparedRender`]s, running deferred work in
    /// parallel via rayon. Cache hits are resolved first; remaining work
    /// items execute on the rayon thread pool; results are blitted onto
    /// `surface` in the original submission order (preserving z-order).
    pub fn compose_batch(
        &mut self,
        batch: impl IntoIterator<Item = PreparedRender>,
        surface: &mut CellSurface,
    ) {
        use rayon::prelude::*;

        let cancellation = RenderCancellation::never();

        enum Slot {
            Hit(usize),
            Pending(usize),
        }

        let batch = batch.into_iter();
        let mut slots = Vec::with_capacity(batch.size_hint().0);
        let mut pending = Vec::with_capacity(slots.capacity());

        for prepared in batch {
            let PreparedRender { tag, work } = prepared;
            if let Some(tag) = tag {
                if let Some(index) = self.lookup_index(tag.id, tag.key, tag.area) {
                    slots.push(Slot::Hit(index));
                    continue;
                }
                let index = pending.len();
                pending.push((Some(tag), work));
                slots.push(Slot::Pending(index));
            } else {
                let index = pending.len();
                pending.push((None, work));
                slots.push(Slot::Pending(index));
            }
        }

        let mut outputs: Vec<Option<(Option<CacheTag>, RenderOutput)>> = pending
            .into_par_iter()
            .map(|(tag, work)| Some((tag, work.run(&cancellation))))
            .collect();

        for slot in slots {
            match slot {
                Slot::Hit(index) => blit_render(&self.surfaces[index], surface),
                Slot::Pending(index) => {
                    let (tag, output) = outputs[index]
                        .take()
                        .expect("each prepared render slot must be composed once");
                    let output = output.into_resolved();
                    blit_render(&output, surface);
                    if let Some(tag) = tag {
                        self.store(tag, output);
                    }
                }
            }
        }
    }

    fn lookup_index(&self, id: CacheId, key: CacheKey, area: Rect) -> Option<usize> {
        let &index = self.index.get(&id)?;
        let meta = &self.meta[index as usize];
        (meta.key == key && meta.area == area).then_some(index as usize)
    }

    fn lookup(&self, id: CacheId, key: CacheKey, area: Rect) -> Option<&ResolvedRender> {
        self.lookup_index(id, key, area)
            .map(|index| self.surfaces[index].as_ref())
    }

    fn store(&mut self, tag: CacheTag, surface: ResolvedRender) {
        self.store_shared(tag, Arc::new(surface));
    }

    fn store_shared(&mut self, tag: CacheTag, surface: Arc<ResolvedRender>) {
        if let Some(&idx) = self.index.get(&tag.id) {
            let i = idx as usize;
            self.meta[i] = CacheMeta {
                id: tag.id,
                key: tag.key,
                area: tag.area,
            };
            self.surfaces[i] = surface;
            return;
        }

        let idx = self.meta.len() as u32;
        self.meta.push(CacheMeta {
            id: tag.id,
            key: tag.key,
            area: tag.area,
        });
        self.surfaces.push(surface);
        self.index.insert(tag.id, idx);
    }

    fn resolve(
        &mut self,
        prepared: PreparedRender,
        cancellation: &RenderCancellation,
    ) -> Option<Arc<ResolvedRender>> {
        let PreparedRender { tag, work } = prepared;
        if let Some(tag) = tag {
            if let Some(index) = self.lookup_index(tag.id, tag.key, tag.area) {
                return Some(Arc::clone(&self.surfaces[index]));
            }
            let surface = Arc::new(work.run(cancellation).into_resolved());
            if cancellation.is_cancelled() {
                return None;
            }
            self.store_shared(tag, Arc::clone(&surface));
            Some(surface)
        } else {
            let surface = Arc::new(work.run(cancellation).into_resolved());
            (!cancellation.is_cancelled()).then_some(surface)
        }
    }
}

fn blit_render(src: &ResolvedRender, dst: &mut CellSurface) {
    match src.blend {
        RenderBlend::Opaque => blit_cells(&src.surface, dst),
        RenderBlend::Sparse => blit_sparse_cells(&src.surface, dst),
    }
}

fn blit_sparse_cells(src: &CellSurface, dst: &mut CellSurface) {
    let src_area = tui::ratatui::to_helix_rect(*src.area());
    let dst_area = tui::ratatui::to_helix_rect(*dst.area());
    let area = src_area.intersection(dst_area);
    if area.width == 0 || area.height == 0 {
        return;
    }

    let mut untouched = tui::ratatui::buffer::Cell::default();
    untouched.set_symbol(SPARSE_CELL_MARKER);
    let src_w = src.area().width as usize;
    let dst_w = dst.area().width as usize;
    let src_x = (area.x - src_area.x) as usize;
    let src_y = (area.y - src_area.y) as usize;
    let dst_x = (area.x - dst_area.x) as usize;
    let dst_y = (area.y - dst_area.y) as usize;

    for row in 0..area.height as usize {
        let src_start = (src_y + row) * src_w + src_x;
        let dst_start = (dst_y + row) * dst_w + dst_x;
        for column in 0..area.width as usize {
            let source = &src.content[src_start + column];
            if source == &untouched {
                continue;
            }
            let destination = &mut dst.content[dst_start + column];
            destination.clone_from(source);
            if destination.symbol() == SPARSE_CELL_MARKER {
                destination.set_symbol(" ");
            }
        }
    }
}

/// Blit one opaque cell surface onto another without style conversion.
pub fn blit_cells(src: &CellSurface, dst: &mut CellSurface) {
    let src_area = tui::ratatui::to_helix_rect(*src.area());
    let dst_area = tui::ratatui::to_helix_rect(*dst.area());
    let area = src_area.intersection(dst_area);
    if area.width == 0 || area.height == 0 {
        return;
    }

    let src_w = src.area().width as usize;
    let dst_w = dst.area().width as usize;
    let src_x = (area.x - src_area.x) as usize;
    let src_y = (area.y - src_area.y) as usize;
    let dst_x = (area.x - dst_area.x) as usize;
    let dst_y = (area.y - dst_area.y) as usize;
    let width = area.width as usize;

    for row in 0..area.height as usize {
        let src_start = (src_y + row) * src_w + src_x;
        let dst_start = (dst_y + row) * dst_w + dst_x;
        dst.content[dst_start..dst_start + width]
            .clone_from_slice(&src.content[src_start..src_start + width]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_output_reuse_resets_matching_surface() {
        let area = Rect::new(0, 0, 2, 1);
        let mut surface = CellSurface::empty(tui::ratatui::to_ratatui_rect(area));
        surface[(0, 0)].set_symbol("x");

        let output = RenderOutput::reuse(area, surface);

        assert_eq!(output.surface()[(0, 0)].symbol(), " ");
        assert_eq!(output.area(), area);
    }

    #[test]
    fn render_plan_executes_steps_in_z_order() {
        let area = Rect::new(0, 0, 1, 1);
        let mut plan = RenderPlan::seeded(
            area,
            CellSurface::empty(tui::ratatui::to_ratatui_rect(area)),
        );
        plan.extend([
            RenderStep::paint("base", |surface, _| {
                surface[(0, 0)].set_symbol("a");
            }),
            RenderStep::paint("overlay", |surface, _| {
                surface[(0, 0)].set_symbol("b");
            }),
        ]);

        let surface = plan.take_seed().unwrap();
        let result = plan.execute(
            surface,
            &mut CacheStore::default(),
            &RenderCancellation::never(),
        );

        assert!(result.complete);
        assert_eq!(result.surface[(0, 0)].symbol(), "b");
    }

    #[test]
    fn render_plan_stops_before_a_superseded_step() {
        let area = Rect::new(0, 0, 1, 1);
        let mut plan = RenderPlan::seeded(
            area,
            CellSurface::empty(tui::ratatui::to_ratatui_rect(area)),
        );
        plan.extend([
            RenderStep::paint("base", |surface, cancellation| {
                surface[(0, 0)].set_symbol("a");
                cancellation.latest.store(2, Ordering::Release);
            }),
            RenderStep::paint("overlay", |surface, _| {
                surface[(0, 0)].set_symbol("b");
            }),
        ]);
        let cancellation = RenderCancellation::for_sequence(Arc::new(AtomicU64::new(1)), 1);

        let surface = plan.take_seed().unwrap();
        let result = plan.execute(surface, &mut CacheStore::default(), &cancellation);

        assert!(!result.complete);
        assert_eq!(result.surface[(0, 0)].symbol(), "a");
    }

    #[test]
    fn stateful_render_steps_reuse_actor_owned_state_and_return_metadata() {
        #[derive(Default)]
        struct Counter(u8);

        let area = Rect::new(0, 0, 1, 1);
        let state_id = CacheId::hashed(&"stateful-render-test");
        let mut store = CacheStore::default();

        for expected in [1, 2] {
            let mut plan = RenderPlan::seeded(
                area,
                CellSurface::empty(tui::ratatui::to_ratatui_rect(area)),
            );
            plan.extend([RenderStep::stateful(
                "stateful",
                move |surface, store, metadata, _cancellation| {
                    let counter = store.domain_mut::<Counter>(state_id);
                    counter.0 += 1;
                    surface[(0, 0)].set_symbol(&counter.0.to_string());
                    metadata.set_cursor(Some((counter.0 as u16, 0)), CursorKind::Block);
                },
            )]);

            let surface = plan.take_seed().unwrap();
            let result = plan.execute(surface, &mut store, &RenderCancellation::never());

            assert!(result.complete);
            assert_eq!(result.surface[(0, 0)].symbol(), expected.to_string());
            assert_eq!(
                result.metadata.cursor_override(),
                Some(RenderCursor {
                    position: Some((expected as u16, 0)),
                    kind: CursorKind::Block,
                })
            );
        }
    }

    #[test]
    fn actor_owned_state_domains_evict_least_recently_used_entries() {
        let mut store = CacheStore::default();
        let ids = (0..=CacheStore::DOMAIN_CAPACITY)
            .map(|index| CacheId::hashed(&("domain", index)))
            .collect::<Vec<_>>();

        for (index, id) in ids.iter().copied().enumerate() {
            *store.domain_mut::<usize>(id) = index;
        }

        assert_eq!(store.domains.len(), CacheStore::DOMAIN_CAPACITY);
        assert_eq!(store.domain_order.len(), CacheStore::DOMAIN_CAPACITY);
        assert!(!store.domains.contains_key(&ids[0]));
        assert!(store.domains.contains_key(ids.last().unwrap()));
    }

    fn render_char(ch: char, area: Rect) -> RenderOutput {
        let mut output = RenderOutput::new(area);
        let symbol = ch.to_string();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                output.surface_mut()[(x, y)].set_symbol(&symbol);
            }
        }
        output
    }

    fn test_tag(slot: &str, ver: u8) -> CacheTag {
        CacheTag {
            id: CacheId::hashed(&slot),
            key: CacheKey::hashed(&ver),
            area: Rect::new(0, 0, 4, 1),
        }
    }

    #[test]
    fn cache_store_reuses_matching_entry() {
        let area = Rect::new(0, 0, 4, 1);
        let mut store = CacheStore::default();
        let mut surface = CellSurface::empty(tui::ratatui::to_ratatui_rect(area));

        let tag = test_tag("slot", 1);
        let prepared = PreparedRender::cached(tag, render_char('a', area));
        assert_eq!(store.compose(prepared, &mut surface), CacheState::Miss);

        // Same tag → hit, surface still has 'a' (not re-rendered).
        let prepared = PreparedRender::cached(tag, render_char('b', area));
        assert_eq!(store.compose(prepared, &mut surface), CacheState::Hit);
        assert_eq!(surface[(0, 0)].symbol(), "a");
    }

    #[test]
    fn cache_store_composes_deferred_render_work() {
        let area = Rect::new(0, 0, 2, 1);
        let mut store = CacheStore::default();
        let mut surface = CellSurface::empty(tui::ratatui::to_ratatui_rect(area));
        let tag = CacheTag {
            id: CacheId::hashed(&"slot"),
            key: CacheKey::hashed(&1_u8),
            area,
        };

        let prepared = PreparedRender::snapshot(tag, 'z', move |ch, _cancellation| {
            let mut output = RenderOutput::new(area);
            let symbol = ch.to_string();
            output.surface_mut()[(0, 0)].set_symbol(&symbol);
            output
        });

        assert_eq!(store.compose(prepared, &mut surface), CacheState::Miss);
        assert_eq!(surface[(0, 0)].symbol(), "z");
    }

    #[test]
    fn cache_batch_preserves_order_across_misses_and_hits() {
        let area = Rect::new(0, 0, 1, 1);
        let mut store = CacheStore::default();
        let tag = CacheTag {
            id: CacheId::hashed(&"top"),
            key: CacheKey::hashed(&1_u8),
            area,
        };
        let mut surface = CellSurface::empty(tui::ratatui::to_ratatui_rect(area));
        assert_eq!(
            store.compose(
                PreparedRender::cached(tag, render_char('b', area)),
                &mut surface,
            ),
            CacheState::Miss,
        );

        store.compose_batch(
            [
                PreparedRender::ready(render_char('a', area)),
                PreparedRender::cached(tag, render_char('x', area)),
            ],
            &mut surface,
        );

        assert_eq!(surface[(0, 0)].symbol(), "b");
    }

    #[test]
    fn snapshot_builds_cached_deferred_work() {
        let area = Rect::new(0, 0, 1, 1);
        let mut store = CacheStore::default();
        let mut surface = CellSurface::empty(tui::ratatui::to_ratatui_rect(area));
        let tag = CacheTag {
            id: CacheId::hashed(&"slot"),
            key: CacheKey::hashed(&2_u8),
            area,
        };

        let prepared = PreparedRender::snapshot(tag, 'q', move |ch, _cancellation| {
            let mut output = RenderOutput::new(area);
            let symbol = ch.to_string();
            output.surface_mut()[(0, 0)].set_symbol(&symbol);
            output
        });

        assert_eq!(store.compose(prepared, &mut surface), CacheState::Miss);
        assert_eq!(surface[(0, 0)].symbol(), "q");
    }

    #[test]
    fn ratatui_cache_store_reuses_matching_entry() {
        let area = Rect::new(0, 0, 4, 1);
        let mut store = CacheStore::default();
        let mut surface = CellSurface::empty(tui::ratatui::to_ratatui_rect(area));
        let tag = CacheTag {
            id: CacheId::hashed(&"ratatui-slot"),
            key: CacheKey::hashed(&1_u8),
            area,
        };

        let mut first = RenderOutput::new(area);
        first
            .surface_mut()
            .set_string(0, 0, "aaaa", tui::ratatui::style::Style::default());
        assert_eq!(
            store.compose(PreparedRender::cached(tag, first), &mut surface),
            CacheState::Miss
        );

        let mut second = RenderOutput::new(area);
        second
            .surface_mut()
            .set_string(0, 0, "bbbb", tui::ratatui::style::Style::default());
        assert_eq!(
            store.compose(PreparedRender::cached(tag, second), &mut surface),
            CacheState::Hit
        );
        assert_eq!(surface[(0, 0)].symbol(), "a");
    }

    #[test]
    fn sparse_render_only_replaces_cells_that_were_painted() {
        let area = Rect::new(0, 0, 2, 1);
        let mut destination = CellSurface::empty(tui::ratatui::to_ratatui_rect(area));
        destination[(0, 0)]
            .set_symbol("a")
            .set_fg(tui::ratatui::style::Color::Red);
        destination[(1, 0)].set_symbol("b");
        let mut patch = RenderOutput::sparse(area);
        patch.surface_mut()[(1, 0)]
            .set_symbol("x")
            .set_fg(tui::ratatui::style::Color::Green);

        blit_render(&patch.into_resolved(), &mut destination);

        assert_eq!(destination[(0, 0)].symbol(), "a");
        assert_eq!(destination[(0, 0)].fg, tui::ratatui::style::Color::Red);
        assert_eq!(destination[(1, 0)].symbol(), "x");
        assert_eq!(destination[(1, 0)].fg, tui::ratatui::style::Color::Green);
    }

    #[test]
    fn sparse_style_only_paint_clears_the_underlying_symbol() {
        let area = Rect::new(0, 0, 1, 1);
        let mut destination = CellSurface::empty(tui::ratatui::to_ratatui_rect(area));
        destination[(0, 0)].set_symbol("a");
        let mut patch = RenderOutput::sparse(area);
        patch.surface_mut()[(0, 0)].set_bg(tui::ratatui::style::Color::Blue);

        blit_render(&patch.into_resolved(), &mut destination);

        assert_eq!(destination[(0, 0)].symbol(), " ");
        assert_eq!(destination[(0, 0)].bg, tui::ratatui::style::Color::Blue);
    }

    #[test]
    fn cancelled_cache_miss_is_neither_stored_nor_composed() {
        let area = Rect::new(0, 0, 1, 1);
        let tag = CacheTag {
            id: CacheId::hashed(&"cancelled"),
            key: CacheKey::hashed(&1_u8),
            area,
        };
        let latest = Arc::new(AtomicU64::new(1));
        let cancellation = RenderCancellation::for_sequence(Arc::clone(&latest), 1);
        let prepared = PreparedRender::snapshot(tag, (), move |(), cancellation| {
            cancellation.latest.store(2, Ordering::Release);
            render_char('x', area)
        });
        let mut seed = CellSurface::empty(tui::ratatui::to_ratatui_rect(area));
        seed[(0, 0)].set_symbol("a");
        let mut plan = RenderPlan::seeded(area, seed);
        plan.extend([RenderStep::prepared("cancelled", vec![prepared]).unwrap()]);
        let surface = plan.take_seed().unwrap();
        let mut cache = CacheStore::default();

        let result = plan.execute(surface, &mut cache, &cancellation);

        assert!(!result.complete);
        assert_eq!(result.surface[(0, 0)].symbol(), "a");
        assert!(cache.lookup(tag.id, tag.key, tag.area).is_none());
    }

    #[test]
    fn render_plan_retires_cache_slots_absent_from_the_frame() {
        let area = Rect::new(0, 0, 1, 1);
        let first = CacheTag {
            id: CacheId::hashed(&"first"),
            key: CacheKey::hashed(&1_u8),
            area,
        };
        let retired = CacheTag {
            id: CacheId::hashed(&"retired"),
            key: CacheKey::hashed(&1_u8),
            area,
        };
        let mut cache = CacheStore::default();
        let mut surface = CellSurface::empty(tui::ratatui::to_ratatui_rect(area));
        cache.compose(
            PreparedRender::cached(first, render_char('a', area)),
            &mut surface,
        );
        cache.compose(
            PreparedRender::cached(retired, render_char('b', area)),
            &mut surface,
        );
        let mut plan = RenderPlan::seeded(
            area,
            CellSurface::empty(tui::ratatui::to_ratatui_rect(area)),
        );
        plan.extend([RenderStep::prepared(
            "first",
            vec![PreparedRender::cached(first, render_char('x', area))],
        )
        .unwrap()]);
        let surface = plan.take_seed().unwrap();

        let result = plan.execute(surface, &mut cache, &RenderCancellation::never());

        assert!(result.complete);
        assert_eq!(cache.meta.len(), 1);
        assert!(cache.lookup(first.id, first.key, first.area).is_some());
        assert!(cache
            .lookup(retired.id, retired.key, retired.area)
            .is_none());
    }

    #[test]
    fn blit_cells_preserves_symbol_and_style() {
        let area = Rect::new(0, 0, 2, 1);
        let mut src = CellSurface::empty(tui::ratatui::to_ratatui_rect(area));
        src[(1, 0)]
            .set_symbol("x")
            .set_fg(tui::ratatui::style::Color::LightGreen);
        let mut dst = CellSurface::empty(tui::ratatui::to_ratatui_rect(area));

        blit_cells(&src, &mut dst);

        assert_eq!(dst[(1, 0)].symbol(), "x");
        assert_eq!(dst[(1, 0)].fg, tui::ratatui::style::Color::LightGreen);
    }

    #[test]
    fn render_output_cells_expose_helix_cell_records() {
        let area = Rect::new(2, 3, 2, 1);
        let mut output = RenderOutput::new(area);
        output.surface_mut()[(2, 3)].set_symbol("x").set_style(
            tui::ratatui::style::Style::default()
                .fg(tui::ratatui::style::Color::LightGreen)
                .add_modifier(tui::ratatui::style::Modifier::BOLD),
        );

        let cells = output.cells().collect::<Vec<_>>();

        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].x, 2);
        assert_eq!(cells[0].y, 3);
        assert_eq!(cells[0].symbol, "x");
        assert_eq!(
            cells[0].style.fg,
            Some(helix_view::graphics::Color::LightGreen)
        );
        assert!(cells[0]
            .style
            .add_modifier
            .contains(helix_view::graphics::Modifier::BOLD));
    }

    #[test]
    fn render_output_cells_handles_empty_width() {
        let output = RenderOutput::new(Rect::new(0, 0, 0, 1));

        assert_eq!(output.cells().count(), 0);
    }

    #[test]
    fn render_output_scene_groups_adjacent_cells_by_style() {
        let area = Rect::new(0, 0, 4, 1);
        let mut output = RenderOutput::new(area);
        let red = tui::ratatui::style::Style::default().fg(tui::ratatui::style::Color::Red);
        let blue = tui::ratatui::style::Style::default().fg(tui::ratatui::style::Color::Blue);
        output.surface_mut()[(0, 0)].set_symbol("a").set_style(red);
        output.surface_mut()[(1, 0)].set_symbol("b").set_style(red);
        output.surface_mut()[(2, 0)].set_symbol("c").set_style(blue);
        output.surface_mut()[(3, 0)].set_symbol("d").set_style(blue);

        let scene = output.to_scene();
        let row = &scene.rows()[0];

        assert_eq!(scene.area(), area);
        assert_eq!(row.y(), 0);
        assert_eq!(row.runs().len(), 2);
        assert_eq!(row.runs()[0].x, 0);
        assert_eq!(row.runs()[0].width, 2);
        assert_eq!(row.runs()[0].text, "ab");
        assert_eq!(
            row.runs()[0].style.fg,
            Some(helix_view::graphics::Color::Red)
        );
        assert_eq!(row.runs()[1].x, 2);
        assert_eq!(row.runs()[1].width, 2);
        assert_eq!(row.runs()[1].text, "cd");
        assert_eq!(
            row.runs()[1].style.fg,
            Some(helix_view::graphics::Color::Blue)
        );
    }

    #[test]
    fn render_output_cell_runs_group_adjacent_cells_without_owned_text() {
        let area = Rect::new(0, 0, 4, 1);
        let mut output = RenderOutput::new(area);
        let red = tui::ratatui::style::Style::default().fg(tui::ratatui::style::Color::Red);
        let blue = tui::ratatui::style::Style::default().fg(tui::ratatui::style::Color::Blue);
        output.surface_mut()[(0, 0)].set_symbol("a").set_style(red);
        output.surface_mut()[(1, 0)].set_symbol("b").set_style(red);
        output.surface_mut()[(2, 0)].set_symbol("c").set_style(blue);
        output.surface_mut()[(3, 0)].set_symbol("d").set_style(blue);

        let runs = output.cell_runs().collect::<Vec<_>>();

        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].x, 0);
        assert_eq!(runs[0].width, 2);
        assert_eq!(runs[0].symbols().collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(runs[1].x, 2);
        assert_eq!(runs[1].width, 2);
        assert_eq!(runs[1].symbols().collect::<Vec<_>>(), ["c", "d"]);
    }

    #[test]
    fn render_cell_run_writes_into_caller_owned_buffer() {
        let area = Rect::new(0, 0, 2, 1);
        let mut output = RenderOutput::new(area);
        output.surface_mut()[(0, 0)].set_symbol("a");
        output.surface_mut()[(1, 0)].set_symbol("b");
        let run = output.cell_runs().next().unwrap();
        let mut text = String::with_capacity(run.width as usize);

        run.write_text(&mut text);

        assert_eq!(text, "ab");
    }

    #[test]
    fn render_output_cell_runs_clamps_to_surface_area() {
        let mut output = RenderOutput::new(Rect::new(0, 0, 4, 1));
        *output.surface_mut() = CellSurface::empty(tui::ratatui::layout::Rect::new(0, 0, 2, 1));
        output.surface_mut()[(0, 0)].set_symbol("a");
        output.surface_mut()[(1, 0)].set_symbol("b");

        let runs = output.cell_runs().collect::<Vec<_>>();

        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].x, 0);
        assert_eq!(runs[0].width, 2);
        assert_eq!(runs[0].symbols().collect::<Vec<_>>(), ["a", "b"]);
    }
}
