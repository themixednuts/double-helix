use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwapOption;
use helix_core::syntax;
use helix_runtime::{LatestAdmissionError, LatestByKeySender};
use helix_view::assistant::thread;
use helix_view::theme::{Style, Theme};
use tui::text::Spans;

use crate::ui::markdown::{fit_bubble_width, Doc, MarkdownCache, MarkdownLineStyles};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RequestKey {
    pub thread: thread::Id,
    pub content_revision: u64,
    pub width: u16,
    pub theme_generation: u64,
}

pub(super) struct RenderStyle {
    pub base: Style,
    pub lines: MarkdownLineStyles,
}

pub(super) struct Snapshot {
    key: RequestKey,
    generation: u64,
    lines: HashMap<thread::EntryId, Arc<[Spans<'static>]>>,
}

impl Snapshot {
    pub fn lines(&self, key: &RequestKey, entry: thread::EntryId) -> Option<Arc<[Spans<'static>]>> {
        (&self.key == key)
            .then(|| self.lines.get(&entry).cloned())
            .flatten()
    }

    pub fn matches(&self, key: &RequestKey) -> bool {
        &self.key == key
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
}

struct Request {
    generation: u64,
    key: RequestKey,
    entries: Arc<[helix_view::model::AssistantEntry]>,
    style: RenderStyle,
    theme: Arc<Theme>,
    loader: Arc<syntax::Loader>,
}

#[derive(Default)]
struct EntryCache {
    text: String,
    width: usize,
    theme_generation: u64,
    lines: Arc<[Spans<'static>]>,
    markdown: MarkdownCache,
}

struct WorkResult {
    caches: HashMap<thread::EntryId, EntryCache>,
    snapshot: Snapshot,
    complete: bool,
}

pub(super) struct MarkdownService {
    tx: LatestByKeySender<(), Request>,
    next_generation: u64,
    latest_generation: Arc<AtomicU64>,
    requested: Option<RequestKey>,
    requested_generation: u64,
    failed_generation: Arc<AtomicU64>,
    theme: Option<(u64, Arc<Theme>)>,
    snapshot: Arc<ArcSwapOption<Snapshot>>,
}

impl MarkdownService {
    pub fn spawn(
        work: helix_runtime::Work,
        block: helix_runtime::Block,
        redraw: helix_runtime::FrameHandle,
    ) -> Self {
        let (tx, mut rx) = helix_runtime::latest_by_key::<(), Request>(1);
        let latest_generation = Arc::new(AtomicU64::new(0));
        let actor_latest = Arc::clone(&latest_generation);
        let failed_generation = Arc::new(AtomicU64::new(0));
        let actor_failed = Arc::clone(&failed_generation);
        let snapshot = Arc::new(ArcSwapOption::empty());
        let actor_snapshot = Arc::clone(&snapshot);

        work.spawn(async move {
            let mut caches = HashMap::new();
            while let Some(((), request)) = rx.recv().await {
                let generation = request.generation;
                let entry_count = request.entries.len();
                let content_revision = request.key.content_revision;
                let width = request.key.width;
                let worker_latest = Arc::clone(&actor_latest);
                let started = std::time::Instant::now();
                let result = block
                    .spawn(move || render(request, caches, &worker_latest))
                    .await;
                helix_view::bench::log_run_phase(
                    "assistant_markdown_actor",
                    "layout",
                    started.elapsed(),
                    || {
                        format!(
                            "generation={generation} revision={content_revision} entries={entry_count} width={width}"
                        )
                    },
                );
                let Ok(result) = result else {
                    actor_failed.store(generation, Ordering::Release);
                    log::error!("assistant markdown worker failed generation={generation}");
                    caches = HashMap::new();
                    redraw.request_redraw();
                    continue;
                };
                caches = result.caches;
                if !result.complete || actor_latest.load(Ordering::Acquire) != generation {
                    continue;
                }
                actor_snapshot.store(Some(Arc::new(result.snapshot)));
                redraw.request_redraw();
            }
        })
        .detach();

        Self {
            tx,
            next_generation: 1,
            latest_generation,
            requested: None,
            requested_generation: 0,
            failed_generation,
            theme: None,
            snapshot,
        }
    }

    pub fn needs(&self, key: &RequestKey) -> bool {
        self.requested.as_ref() != Some(key)
            || self.failed_generation.load(Ordering::Acquire) == self.requested_generation
    }

    pub fn submit(
        &mut self,
        key: RequestKey,
        entries: Arc<[helix_view::model::AssistantEntry]>,
        style: RenderStyle,
        theme: &Theme,
        loader: Arc<syntax::Loader>,
    ) {
        if !self.needs(&key) {
            return;
        }

        let theme = match &self.theme {
            Some((generation, theme)) if *generation == key.theme_generation => Arc::clone(theme),
            _ => {
                let theme = Arc::new(theme.clone());
                self.theme = Some((key.theme_generation, Arc::clone(&theme)));
                theme
            }
        };
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        let previous = self.latest_generation.swap(generation, Ordering::AcqRel);
        let entry_count = entries.len();
        let request = Request {
            generation,
            key: key.clone(),
            entries,
            style,
            theme,
            loader,
        };
        match self.tx.try_send((), request) {
            Ok(_) => {
                self.requested = Some(key);
                self.requested_generation = generation;
                log::trace!(
                    "assistant markdown submitted generation={generation} revision={} entries={}",
                    self.requested
                        .as_ref()
                        .map_or(0, |request| request.content_revision),
                    entry_count,
                );
            }
            Err(LatestAdmissionError::Full((), _)) => {
                let _ = self.latest_generation.compare_exchange(
                    generation,
                    previous,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                log::error!("assistant markdown admission invariant was violated");
            }
            Err(LatestAdmissionError::Closed((), _)) => {
                let _ = self.latest_generation.compare_exchange(
                    generation,
                    previous,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                log::error!("assistant markdown service is closed");
            }
        }
    }

    pub fn snapshot(&self) -> Option<Arc<Snapshot>> {
        self.snapshot.load_full()
    }
}

fn render(
    request: Request,
    mut caches: HashMap<thread::EntryId, EntryCache>,
    latest: &AtomicU64,
) -> WorkResult {
    let mut lines = HashMap::with_capacity(request.entries.len());
    let mut complete = true;
    let (min_bubble, max_bubble) = bubble_width_range(request.key.width);
    for entry in request.entries.iter() {
        if latest.load(Ordering::Acquire) != request.generation {
            complete = false;
            break;
        }

        let helix_view::model::AssistantEntryKind::AgentText(text) = &entry.kind else {
            continue;
        };
        let bubble_width = fit_bubble_width(text, min_bubble as usize, max_bubble as usize) as u16;
        let source_width = bubble_width.saturating_sub(4) as usize;

        let cache = caches.entry(entry.id).or_default();
        if cache.text != *text
            || cache.width != source_width
            || cache.theme_generation != request.key.theme_generation
        {
            let rendered = cache.markdown.layout(
                &Doc::new(text.clone()),
                source_width,
                request.style.base,
                &request.style.lines,
                Some(&request.theme),
                &request.loader,
            );
            cache.text.clone_from(text);
            cache.width = source_width;
            cache.theme_generation = request.key.theme_generation;
            cache.lines = Arc::from(rendered);
        }
        lines.insert(entry.id, Arc::clone(&cache.lines));
    }
    complete &= latest.load(Ordering::Acquire) == request.generation;
    WorkResult {
        caches,
        snapshot: Snapshot {
            key: request.key,
            generation: request.generation,
            lines,
        },
        complete,
    }
}

fn bubble_width_range(width: u16) -> (u16, u16) {
    let max = ((width as u32 * 90 / 100) as u16).min(width).max(4);
    let min = ((width as u32 * 60 / 100) as u16).max(20).min(max);
    (min, max)
}
