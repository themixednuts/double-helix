use locals::Locals;
use ropey::RopeSlice;

use slab::Slab;

use std::cell::RefCell;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::Duration;
use tree_sitter::{IncompatibleGrammarError, Node, Parser, Tree};

pub use crate::config::{read_query, LanguageConfig, LanguageLoader};
pub use crate::injections_query::{InjectionLanguageMarker, InjectionsQuery};
use crate::parse::LayerUpdateFlags;
pub use crate::tree_cursor::TreeCursor;
pub use tree_sitter;
// pub use pretty_print::pretty_print_tree;
// pub use tree_cursor::TreeCursor;

mod config;
pub mod highlighter;
mod injections_query;
mod parse;
#[cfg(all(test, feature = "fixtures"))]
mod tests;
mod trace;
// mod pretty_print;
#[cfg(feature = "fixtures")]
pub mod fixtures;
pub mod locals;
pub mod query_iter;
pub mod text_object;
mod tree_cursor;

#[cfg(feature = "bench-profile")]
const SLOW_TRACE_US: u64 = 2_000;

thread_local! {
    static TRACE_CONTEXT: RefCell<Option<TraceContext>> = const { RefCell::new(None) };
}

#[derive(Clone, Debug)]
pub struct TraceContext {
    pub log_path: PathBuf,
    pub seed: u64,
    pub elapsed_secs: f64,
    pub action_index: u64,
    pub category: &'static str,
    pub macro_str: &'static str,
    pub force_insert: bool,
}

pub struct TraceGuard;

impl Drop for TraceGuard {
    fn drop(&mut self) {
        TRACE_CONTEXT.with(|ctx| {
            *ctx.borrow_mut() = None;
        });
    }
}

pub fn enter_trace(ctx: TraceContext) -> TraceGuard {
    TRACE_CONTEXT.with(|slot| {
        *slot.borrow_mut() = Some(ctx);
    });
    TraceGuard
}

pub(crate) fn checked_byte_slice<'a>(
    source: RopeSlice<'a>,
    range: &std::ops::Range<u32>,
) -> Option<RopeSlice<'a>> {
    let start = range.start as usize;
    let end = range.end as usize;
    if start > end || end > source.len_bytes() {
        return None;
    }
    Some(source.byte_slice(start..end))
}

pub(crate) fn checked_byte_slice_usize<'a>(
    source: RopeSlice<'a>,
    range: &std::ops::Range<usize>,
) -> Option<RopeSlice<'a>> {
    if range.start > range.end || range.end > source.len_bytes() {
        return None;
    }
    Some(source.byte_slice(range.clone()))
}

/// A layer represents a single a single syntax tree that represents (part of)
/// a file parsed with a tree-sitter grammar. See [`Syntax`].
#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub struct Layer(u32);

impl Layer {
    fn idx(self) -> usize {
        self.0 as usize
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Language(pub u32);

impl Language {
    pub fn new(idx: u32) -> Language {
        Language(idx)
    }

    pub fn idx(self) -> usize {
        self.0 as usize
    }
}

/// The Tree sitter syntax tree for a single language.
///
/// This is really multiple (nested) different syntax trees due to tree sitter
/// injections. A single syntax tree/parser is called layer. Each layer
/// is parsed as a single "file" by tree sitter. There can be multiple layers
/// for the same language. A layer corresponds to one of three things:
/// * the root layer
/// * a singular injection limited to a single node in its parent layer
/// * Multiple injections (multiple disjoint nodes in parent layer) that are
///   parsed as though they are a single uninterrupted file.
///
/// An injection always refer to a single node into which another layer is
/// injected. As injections only correspond to syntax tree nodes injections in
/// the same layer do not intersect. However, the syntax tree in a an injected
/// layer can have nodes that intersect with nodes from the parent layer. For
/// example:
///
/// ``` no-compile
/// layer2: | Sibling A |      Sibling B (layer3)     | Sibling C |
/// layer1: | Sibling A (layer2) | Sibling B | Sibling C (layer2) |
/// ````
///
/// In this case Sibling B really spans across a "GAP" in layer2. While the syntax
/// node can not be split up by tree sitter directly, we can treat Sibling B as two
/// separate injections. That is done while parsing/running the query capture. As
/// a result the injections form a tree. Note that such other queries must account for
/// such multi injection nodes.
#[derive(Debug)]
pub struct Syntax {
    layers: Slab<LayerData>,
    root: Layer,
}

impl Syntax {
    pub fn new(
        source: RopeSlice,
        language: Language,
        timeout: Duration,
        loader: &impl LanguageLoader,
    ) -> Result<Self, Error> {
        let root_layer = LayerData {
            parse_tree: None,
            parser: Parser::new(),
            parse_incomplete: false,
            query_stale: false,
            language,
            flags: LayerUpdateFlags::default(),
            ranges: vec![tree_sitter::Range {
                start_byte: 0,
                end_byte: u32::MAX,
                start_point: tree_sitter::Point::ZERO,
                end_point: tree_sitter::Point::MAX,
            }],
            injections: Vec::new(),
            parent: None,
            locals: Locals::default(),
        };
        let mut layers = Slab::with_capacity(32);
        let root = layers.insert(root_layer);
        let mut syntax = Self {
            root: Layer(root as u32),
            layers,
        };

        syntax.update(source, timeout, &[], loader).map(|_| syntax)
    }

    pub fn layer(&self, layer: Layer) -> &LayerData {
        &self.layers[layer.idx()]
    }

    fn layer_mut(&mut self, layer: Layer) -> &mut LayerData {
        &mut self.layers[layer.idx()]
    }

    fn has_layer(&self, layer: Layer) -> bool {
        self.layers.contains(layer.idx())
    }

    pub fn root(&self) -> Layer {
        self.root
    }

    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    pub fn root_injection_count(&self) -> usize {
        self.layer(self.root).injections.len()
    }

    pub fn tree(&self) -> &Tree {
        self.layer(self.root)
            .tree()
            .expect("`Syntax::new` would err if the root layer's tree could not be parsed")
    }

    #[inline]
    pub fn tree_for_byte_range(&self, start: u32, end: u32) -> &Tree {
        self.layer_and_tree_for_byte_range(start, end).1
    }

    /// Finds the smallest layer which has a parse tree and covers the given range.
    pub(crate) fn layer_and_tree_for_byte_range(&self, start: u32, end: u32) -> (Layer, &Tree) {
        let mut layer = self.layer_for_byte_range(start, end);
        loop {
            // NOTE: this loop is guaranteed to terminate because the root layer always has a
            // tree.
            if let Some(tree) = self.layer(layer).tree() {
                return (layer, tree);
            }
            if let Some(parent) = self.layer(layer).parent {
                layer = parent;
            }
        }
    }

    #[inline]
    pub fn named_descendant_for_byte_range(&self, start: u32, end: u32) -> Option<Node<'_>> {
        self.tree_for_byte_range(start, end)
            .root_node()
            .named_descendant_for_byte_range(start, end)
    }

    #[inline]
    pub fn descendant_for_byte_range(&self, start: u32, end: u32) -> Option<Node<'_>> {
        self.tree_for_byte_range(start, end)
            .root_node()
            .descendant_for_byte_range(start, end)
    }

    /// Finds the smallest injection layer that fully includes the range `start..=end`.
    pub fn layer_for_byte_range(&self, start: u32, end: u32) -> Layer {
        self.layers_for_byte_range(start, end)
            .last()
            .expect("always includes the root layer")
    }

    /// Returns an iterator of layers which **fully include** the byte range `start..=end`,
    /// in decreasing order based on the size of each layer.
    ///
    /// The first layer is always the `root` layer.
    pub fn layers_for_byte_range(&self, start: u32, end: u32) -> impl Iterator<Item = Layer> + '_ {
        let mut parent_injection_layer = self.root;

        std::iter::once(self.root).chain(std::iter::from_fn(move || {
            let layer = &self.layers[parent_injection_layer.idx()];

            let injection_at_start = layer
                .injection_at_byte_idx(start)
                .filter(|injection| self.has_layer(injection.layer))?;

            // +1 because the end is exclusive.
            let injection_at_end = layer
                .injection_at_byte_idx(end + 1)
                .filter(|injection| self.has_layer(injection.layer))?;

            (injection_at_start.layer == injection_at_end.layer).then(|| {
                parent_injection_layer = injection_at_start.layer;

                injection_at_start.layer
            })
        }))
    }

    fn cleanup_stale_layer_refs(&mut self) {
        let valid: std::collections::HashSet<usize> =
            self.layers.iter().map(|(idx, _)| idx).collect();

        for (idx, layer) in &mut self.layers {
            layer
                .injections
                .retain(|injection| valid.contains(&injection.layer.idx()));

            if let Some(parent) = layer.parent.filter(|parent| !valid.contains(&parent.idx())) {
                debug_assert_eq!(idx, self.root.idx(), "non-root layer lost its parent");
                let _ = parent;
                layer.parent = None;
            }
        }
    }

    pub fn walk(&self) -> TreeCursor<'_> {
        TreeCursor::new(self)
    }
}

#[derive(Debug, Clone)]
pub struct Injection {
    pub range: Range,
    pub layer: Layer,
    matched_node_range: Range,
}

pub struct LayerData {
    pub language: Language,
    parse_tree: Option<Tree>,
    parser: Parser,
    parse_incomplete: bool,
    query_stale: bool,
    ranges: Vec<tree_sitter::Range>,
    /// a list of **sorted** non-overlapping injection ranges. Note that
    /// injection ranges are not relative to the start of this layer but the
    /// start of the root layer
    injections: Vec<Injection>,
    /// internal flags used during parsing to track incremental invalidation
    flags: LayerUpdateFlags,
    parent: Option<Layer>,
    locals: Locals,
}

impl fmt::Debug for LayerData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LayerData")
            .field("language", &self.language)
            .field("parse_tree", &self.parse_tree)
            .field("parse_incomplete", &self.parse_incomplete)
            .field("query_stale", &self.query_stale)
            .field("ranges", &self.ranges)
            .field("injections", &self.injections)
            .field("flags", &self.flags)
            .field("parent", &self.parent)
            .field("locals", &self.locals)
            .finish()
    }
}

/// This PartialEq implementation only checks if that
/// two layers are theoretically identical (meaning they highlight the same text range with the same language).
/// It does not check whether the layers have the same internal tree-sitter
/// state.
impl PartialEq for LayerData {
    fn eq(&self, other: &Self) -> bool {
        self.parent == other.parent
            && self.language == other.language
            && self.ranges == other.ranges
    }
}

/// Hash implementation belongs to PartialEq implementation above.
/// See its documentation for details.
impl Hash for LayerData {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.parent.hash(state);
        self.language.hash(state);
        self.ranges.hash(state);
    }
}

impl LayerData {
    /// Returns the parsed `Tree` for this layer.
    ///
    /// This can be `None` for layers that have never completed an initial parse, including
    /// injected layers whose parse timed out before producing a tree. The root layer is expected
    /// to have a tree after successful `Syntax::new`.
    pub fn tree(&self) -> Option<&Tree> {
        self.parse_tree.as_ref()
    }

    /// Returns the injection range **within this layers** that contains `idx`.
    /// This function will not descend into nested injections
    pub fn injection_at_byte_idx(&self, idx: u32) -> Option<&Injection> {
        self.injections_at_byte_idx(idx)
            .next()
            .filter(|injection| injection.range.start <= idx)
    }

    /// Returns the injection ranges **within this layers** that contain
    /// `idx` or start after idx. This function will not descend into nested
    /// injections.
    pub fn injections_at_byte_idx(&self, idx: u32) -> impl Iterator<Item = &Injection> {
        if self.query_stale {
            return self.injections[0..0].iter();
        }
        let i = self
            .injections
            .partition_point(|range| range.range.end < idx);
        self.injections[i..].iter()
    }

    pub fn query_stale(&self) -> bool {
        self.query_stale
    }
}

/// Represents the reason why syntax highlighting failed.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Timeout,
    ExceededMaximumSize,
    InvalidRanges,
    Unknown,
    NoRootConfig,
    IncompatibleGrammar(Language, IncompatibleGrammarError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => f.write_str("configured timeout was exceeded"),
            Self::ExceededMaximumSize => f.write_str("input text exceeds the maximum allowed size"),
            Self::InvalidRanges => f.write_str("invalid ranges"),
            Self::Unknown => f.write_str("an unknown error occurred"),
            Self::NoRootConfig => f.write_str(
                "`LanguageLoader::get_config` for the root layer language returned `None`",
            ),
            Self::IncompatibleGrammar(language, IncompatibleGrammarError { abi_version }) => {
                write!(
                    f,
                    "failed to load grammar for language {language:?} with ABI version {abi_version}"
                )
            }
        }
    }
}

/// The maximum number of in-progress matches a TS cursor can consider at once.
/// This is set to a constant in order to avoid performance problems for medium to large files. Set with `set_match_limit`.
/// Using such a limit means that we lose valid captures, so there is fundamentally a tradeoff here.
///
///
/// Old tree sitter versions used a limit of 32 by default until this limit was removed in version `0.19.5` (must now be set manually).
/// However, this causes performance issues for medium to large files.
/// In Helix, this problem caused tree-sitter motions to take multiple seconds to complete in medium-sized rust files (3k loc).
///
///
/// Neovim also encountered this problem and reintroduced this limit after it was removed upstream
/// (see <https://github.com/neovim/neovim/issues/14897> and <https://github.com/neovim/neovim/pull/14915>).
/// The number used here is fundamentally a tradeoff between breaking some obscure edge cases and performance.
///
///
/// Neovim chose 64 for this value somewhat arbitrarily (<https://github.com/neovim/neovim/pull/18397>).
/// 64 is too low for some languages though. In particular, it breaks some highlighting for record fields in Erlang record definitions.
/// This number can be increased if new syntax highlight breakages are found, as long as the performance penalty is not too high.
pub const TREE_SITTER_MATCH_LIMIT: u32 = 256;

// use 32 bit ranges since TS doesn't support files larger than 2GiB anyway
// and it allows us to save a lot memory/improve cache efficiency
type Range = std::ops::Range<u32>;

#[cfg(test)]
mod unit_tests {
    use super::*;

    fn test_range() -> tree_sitter::Range {
        tree_sitter::Range {
            start_byte: 0,
            end_byte: u32::MAX,
            start_point: tree_sitter::Point::ZERO,
            end_point: tree_sitter::Point::MAX,
        }
    }

    fn layer(parent: Option<Layer>) -> LayerData {
        LayerData {
            language: Language::new(0),
            parse_tree: None,
            parser: Parser::new(),
            parse_incomplete: false,
            query_stale: false,
            ranges: vec![test_range()],
            injections: Vec::new(),
            flags: LayerUpdateFlags::default(),
            parent,
            locals: Locals::default(),
        }
    }

    #[test]
    fn cleanup_stale_layer_refs_prunes_missing_injections() {
        let mut layers = Slab::new();
        let root = Layer(layers.insert(layer(None)) as u32);
        let child = Layer(layers.insert(layer(Some(root))) as u32);
        layers[root.idx()].injections.push(Injection {
            range: 0..16,
            layer: child,
            matched_node_range: 0..16,
        });

        let mut syntax = Syntax { layers, root };
        syntax.layers.remove(child.idx());
        syntax.cleanup_stale_layer_refs();

        assert!(syntax.layer(root).injections.is_empty());
        assert_eq!(
            syntax.layers_for_byte_range(0, 8).collect::<Vec<_>>(),
            vec![root]
        );
    }

    #[test]
    fn layers_for_byte_range_does_not_descend_into_query_stale_descendants() {
        let mut layers = Slab::new();
        let root = Layer(layers.insert(layer(None)) as u32);
        let child = Layer(layers.insert(layer(Some(root))) as u32);
        let grandchild = Layer(layers.insert(layer(Some(child))) as u32);
        layers[root.idx()].injections.push(Injection {
            range: 0..16,
            layer: child,
            matched_node_range: 0..16,
        });
        layers[child.idx()].injections.push(Injection {
            range: 0..16,
            layer: grandchild,
            matched_node_range: 0..16,
        });
        layers[child.idx()].query_stale = true;

        let syntax = Syntax { layers, root };

        assert_eq!(
            syntax.layers_for_byte_range(0, 8).collect::<Vec<_>>(),
            vec![root, child]
        );
        assert!(syntax.layer(child).injection_at_byte_idx(0).is_none());
        assert!(syntax.layer(child).query_stale());
    }

    #[test]
    fn checked_byte_slice_rejects_out_of_bounds_ranges() {
        let rope = ropey::Rope::from("abcdef");
        let slice = rope.slice(..);

        assert_eq!(
            checked_byte_slice(slice, &(1..4)).map(|s| s.to_string()),
            Some("bcd".to_owned())
        );
        assert!(checked_byte_slice(slice, &(4..8)).is_none());
        assert!(checked_byte_slice(slice, &(5..4)).is_none());
    }

    #[test]
    fn checked_byte_slice_usize_rejects_out_of_bounds_ranges() {
        let rope = ropey::Rope::from("abcdef");
        let slice = rope.slice(..);

        assert_eq!(
            checked_byte_slice_usize(slice, &(2..5)).map(|s| s.to_string()),
            Some("cde".to_owned())
        );
        assert!(checked_byte_slice_usize(slice, &(5..9)).is_none());
        assert!(checked_byte_slice_usize(slice, &(5..4)).is_none());
    }
}
