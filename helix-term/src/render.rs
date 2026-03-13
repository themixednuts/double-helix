use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use helix_view::graphics::Rect;
use tui::buffer::Buffer as Surface;

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
#[derive(Debug, Clone, Copy)]
pub struct CacheTag {
    pub id: CacheId,
    pub key: CacheKey,
    pub area: Rect,
}

#[derive(Debug)]
pub struct RenderOutput {
    pub area: Rect,
    pub surface: Surface,
}

impl RenderOutput {
    #[must_use]
    pub fn new(area: Rect) -> Self {
        Self {
            area,
            surface: Surface::empty(area),
        }
    }
}

pub enum RenderWork {
    Ready(RenderOutput),
    Deferred(Box<dyn FnOnce() -> RenderOutput + Send>),
}

impl RenderWork {
    fn run(self) -> RenderOutput {
        match self {
            Self::Ready(output) => output,
            Self::Deferred(work) => work(),
        }
    }
}

/// Common render interface. "I can produce pixels for a given area."
pub trait Renderable {
    fn render(&mut self, area: Rect, surface: &mut Surface);
}

/// A render artifact ready for composition. The universal output type
/// that all rendering paths produce and `CacheStore` consumes.
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
    pub fn deferred(work: impl FnOnce() -> RenderOutput + Send + 'static) -> Self {
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
    pub fn snapshot<S, F>(tag: CacheTag, snapshot: S, render: F) -> Self
    where
        S: Send + 'static,
        F: FnOnce(S) -> RenderOutput + Send + 'static,
    {
        Self {
            tag: Some(tag),
            work: RenderWork::Deferred(Box::new(move || render(snapshot))),
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

/// SoA (struct-of-arrays) cache store. The lookup hot path touches
/// only `meta` (contiguous 24-byte entries). Surfaces are stored in
/// a parallel array and only accessed on hit or store.
#[derive(Default)]
pub struct CacheStore {
    index: HashMap<CacheId, u32>,
    meta: Vec<CacheMeta>,
    surfaces: Vec<Surface>,
}

impl CacheStore {
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

    pub fn compose(&mut self, prepared: PreparedRender, surface: &mut Surface) -> CacheState {
        let PreparedRender { tag, work } = prepared;
        let Some(tag) = tag else {
            let output = work.run();
            blit(&output.surface, surface);
            return CacheState::Uncached;
        };

        if let Some(cached) = self.lookup(tag.id, tag.key, tag.area) {
            blit(cached, surface);
            return CacheState::Hit;
        }

        let output = work.run();
        blit(&output.surface, surface);
        self.store(tag, output.surface);
        CacheState::Miss
    }

    /// Compose a batch of [`PreparedRender`]s, running deferred work in
    /// parallel via rayon. Cache hits are resolved first; remaining work
    /// items execute on the rayon thread pool; results are blitted onto
    /// `surface` in the original submission order (preserving z-order).
    pub fn compose_batch(&mut self, batch: Vec<PreparedRender>, surface: &mut Surface) {
        use rayon::prelude::*;

        struct Pending {
            tag: Option<CacheTag>,
            work: RenderWork,
        }

        let mut pending: Vec<Pending> = Vec::with_capacity(batch.len());

        for prepared in batch {
            let PreparedRender { tag, work } = prepared;
            if let Some(tag) = tag {
                if let Some(cached) = self.lookup(tag.id, tag.key, tag.area) {
                    blit(cached, surface);
                    continue;
                }
                pending.push(Pending {
                    tag: Some(tag),
                    work,
                });
            } else {
                pending.push(Pending { tag: None, work });
            }
        }

        if pending.is_empty() {
            return;
        }

        let outputs: Vec<(Option<CacheTag>, RenderOutput)> = pending
            .into_par_iter()
            .map(|p| {
                let output = p.work.run();
                (p.tag, output)
            })
            .collect();

        for (tag, output) in outputs {
            blit(&output.surface, surface);
            if let Some(tag) = tag {
                self.store(tag, output.surface);
            }
        }
    }

    fn lookup(&self, id: CacheId, key: CacheKey, area: Rect) -> Option<&Surface> {
        let &idx = self.index.get(&id)?;
        let m = &self.meta[idx as usize];
        (m.key == key && m.area == area).then(|| &self.surfaces[idx as usize])
    }

    fn store(&mut self, tag: CacheTag, surface: Surface) {
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
}

/// Blit `src` onto `dst`, row-at-a-time for cache-friendly copying.
pub fn blit(src: &Surface, dst: &mut Surface) {
    let area = src.area;
    let src_w = area.width as usize;
    if src_w == 0 || area.height == 0 {
        return;
    }
    let dst_w = dst.area.width as usize;
    let dst_ox = (area.x - dst.area.x) as usize;
    let dst_oy = (area.y - dst.area.y) as usize;

    for row in 0..area.height as usize {
        let src_start = row * src_w;
        let dst_start = (dst_oy + row) * dst_w + dst_ox;
        dst.content[dst_start..dst_start + src_w]
            .clone_from_slice(&src.content[src_start..src_start + src_w]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tui::buffer::Cell;

    fn render_char(ch: char, area: Rect) -> RenderOutput {
        let mut surface = Surface::empty(area);
        let mut symbol = [0; 4];
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                let mut cell = Cell::default();
                cell.set_symbol(ch.encode_utf8(&mut symbol));
                surface[(x, y)] = cell;
            }
        }
        RenderOutput { area, surface }
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
        let mut surface = Surface::empty(area);

        let tag = test_tag("slot", 1);
        let prepared = PreparedRender::cached(tag, render_char('a', area));
        assert_eq!(store.compose(prepared, &mut surface), CacheState::Miss);

        // Same tag → hit, surface still has 'a' (not re-rendered).
        let prepared = PreparedRender::cached(tag, render_char('b', area));
        assert_eq!(store.compose(prepared, &mut surface), CacheState::Hit);
        assert_eq!(surface[(0, 0)].symbol.as_ref(), "a");
    }

    #[test]
    fn cache_store_composes_deferred_render_work() {
        let area = Rect::new(0, 0, 2, 1);
        let mut store = CacheStore::default();
        let mut surface = Surface::empty(area);
        let tag = CacheTag {
            id: CacheId::hashed(&"slot"),
            key: CacheKey::hashed(&1_u8),
            area,
        };

        let prepared = PreparedRender::snapshot(tag, 'z', move |ch| {
            let mut output = RenderOutput::new(area);
            let mut cell = Cell::default();
            let mut symbol = [0; 4];
            cell.set_symbol(ch.encode_utf8(&mut symbol));
            output.surface[(0, 0)] = cell;
            output
        });

        assert_eq!(store.compose(prepared, &mut surface), CacheState::Miss);
        assert_eq!(surface[(0, 0)].symbol.as_ref(), "z");
    }

    #[test]
    fn snapshot_builds_cached_deferred_work() {
        let area = Rect::new(0, 0, 1, 1);
        let mut store = CacheStore::default();
        let mut surface = Surface::empty(area);
        let tag = CacheTag {
            id: CacheId::hashed(&"slot"),
            key: CacheKey::hashed(&2_u8),
            area,
        };

        let prepared = PreparedRender::snapshot(tag, 'q', move |ch| {
            let mut output = RenderOutput::new(area);
            let mut symbol = [0; 4];
            let mut cell = Cell::default();
            cell.set_symbol(ch.encode_utf8(&mut symbol));
            output.surface[(0, 0)] = cell;
            output
        });

        assert_eq!(store.compose(prepared, &mut surface), CacheState::Miss);
        assert_eq!(surface[(0, 0)].symbol.as_ref(), "q");
    }
}
