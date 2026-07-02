use arc_swap::{
    access::{DynAccess, DynGuard},
    ArcSwap,
};
use std::{
    collections::HashMap,
    ops::{Deref, DerefMut},
    sync::Arc,
};

use crate::document::Mode;
use crate::engine::{CharPendingBinding, CharPendingId, CommandToken, KeymapLookup, KeymapQuery};
use crate::info::Info;
use crate::input::{KeyCode, KeyEvent};
use crate::keyboard::KeyModifiers;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModalBinding<T> {
    target: T,
    doc: &'static str,
}

impl<T> ModalBinding<T> {
    pub const fn new(target: T, doc: &'static str) -> Self {
        Self { target, doc }
    }

    pub const fn doc(&self) -> &'static str {
        self.doc
    }
}

impl<T: Copy> ModalBinding<T> {
    pub const fn target(&self) -> T {
        self.target
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModalTrieNode<T> {
    pub name: String,
    pub map: HashMap<KeyEvent, ModalTrie<T>>,
    pub order: Vec<KeyEvent>,
    pub is_sticky: bool,
    pub fallback: Option<CharPendingBinding>,
}

impl<T> ModalTrieNode<T> {
    pub fn new(name: &str, map: HashMap<KeyEvent, ModalTrie<T>>, order: Vec<KeyEvent>) -> Self {
        Self {
            name: name.to_string(),
            map,
            order,
            is_sticky: false,
            fallback: None,
        }
    }

    pub fn infobox(&self) -> Info {
        let mut body = Vec::with_capacity(self.len() + usize::from(self.fallback.is_some()));
        for key in &self.order {
            let Some(trie) = self.map.get(key) else {
                continue;
            };
            let desc = match trie {
                ModalTrie::Binding(binding) => binding.doc(),
                ModalTrie::Node(node) => node.name.as_str(),
                ModalTrie::Sequence(_) => "[Multiple commands]",
            };
            body.push((key.to_string(), desc));
        }
        if let Some(fallback) = self.fallback {
            body.push(("...".to_string(), fallback.doc()));
        }
        Info::new(self.name.clone(), &body)
    }
}

impl<T> Deref for ModalTrieNode<T> {
    type Target = HashMap<KeyEvent, ModalTrie<T>>;

    fn deref(&self) -> &Self::Target {
        &self.map
    }
}

impl<T> DerefMut for ModalTrieNode<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.map
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModalTrie<T> {
    Binding(ModalBinding<T>),
    Sequence(Box<[ModalBinding<T>]>),
    Node(ModalTrieNode<T>),
}

impl<T> ModalTrie<T> {
    pub fn node(&self) -> Option<&ModalTrieNode<T>> {
        match self {
            Self::Node(node) => Some(node),
            Self::Binding(_) | Self::Sequence(_) => None,
        }
    }

    pub fn search(&self, keys: &[KeyEvent]) -> Option<&Self> {
        let mut trie = self;
        for key in keys {
            trie = match trie {
                Self::Node(map) => map.get(key),
                Self::Binding(_) | Self::Sequence(_) => None,
            }?;
        }
        Some(trie)
    }

    pub fn search_fallback(&self, keys: &[KeyEvent]) -> Option<CharPendingBinding> {
        let mut trie = self;
        let mut keys = keys.iter().peekable();
        while let Some(key) = keys.next() {
            trie = match trie {
                Self::Node(map) => match map.get(key) {
                    Some(next) => Some(next),
                    None => {
                        if keys.peek().is_none() {
                            return map.fallback;
                        }
                        None
                    }
                },
                Self::Binding(_) | Self::Sequence(_) => None,
            }?;
        }
        None
    }
}

#[derive(Debug, Clone)]
pub enum ModalLookup<T> {
    Matched(T),
    MatchedSequence(Box<[T]>),
    Pending(Option<Info>),
    NotFound,
    Cancelled(Box<[KeyEvent]>),
    Fallback(CharPendingId, char),
}

pub struct ModalKeymapState<T> {
    map: Box<dyn DynAccess<HashMap<Mode, ModalTrie<T>>> + Send + Sync>,
    state: Vec<KeyEvent>,
    sticky: Option<ModalTrieNode<T>>,
}

impl<T: Copy + Send + Sync + 'static> ModalKeymapState<T> {
    pub fn new(map: Box<dyn DynAccess<HashMap<Mode, ModalTrie<T>>> + Send + Sync>) -> Self {
        Self {
            map,
            state: Vec::new(),
            sticky: None,
        }
    }

    pub fn from_shared(map: Arc<ArcSwap<HashMap<Mode, ModalTrie<T>>>>) -> Self {
        Self::new(Box::new(map))
    }

    pub fn map(&self) -> DynGuard<HashMap<Mode, ModalTrie<T>>> {
        self.map.load()
    }

    pub fn pending(&self) -> &[KeyEvent] {
        &self.state
    }

    pub fn contains_key(&self, mode: Mode, key: KeyEvent) -> bool {
        let keymaps = &*self.map();
        let Some(keymap) = keymaps.get(&mode) else {
            return false;
        };
        keymap
            .search(self.pending())
            .and_then(ModalTrie::node)
            .is_some_and(|node| node.contains_key(&key))
    }
}

impl<T: Copy + Send + Sync + 'static> Default for ModalKeymapState<T> {
    fn default() -> Self {
        Self::new(Box::new(ArcSwap::new(Arc::new(HashMap::new()))))
    }
}

fn lookup_keymap<T: Copy>(
    keymap: &ModalTrie<T>,
    state: &mut Vec<KeyEvent>,
    sticky: &mut Option<ModalTrieNode<T>>,
    key: KeyEvent,
) -> ModalLookup<T> {
    if key
        == (KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
        })
    {
        if !state.is_empty() {
            return ModalLookup::Cancelled(std::mem::take(state).into_boxed_slice());
        }
        *sticky = None;
    }

    let first = state.first().unwrap_or(&key);
    let trie = match sticky.as_ref() {
        Some(node) => node.map.get(first),
        None => keymap.search(&[*first]),
    };
    let trie = match trie {
        Some(ModalTrie::Binding(binding)) => return ModalLookup::Matched(binding.target()),
        Some(ModalTrie::Sequence(bindings)) => {
            let targets = bindings
                .iter()
                .map(ModalBinding::target)
                .collect::<Vec<_>>();
            return ModalLookup::MatchedSequence(targets.into_boxed_slice());
        }
        None => return ModalLookup::NotFound,
        Some(trie) => trie,
    };

    state.push(key);
    match trie.search(&state[1..]) {
        Some(ModalTrie::Node(map)) => {
            let next_sticky = map.is_sticky.then(|| map.clone());
            let info = map.infobox();
            if map.is_sticky {
                state.clear();
            }
            if let Some(next_sticky) = next_sticky {
                *sticky = Some(next_sticky);
            }
            ModalLookup::Pending(Some(info))
        }
        Some(ModalTrie::Binding(binding)) => {
            state.clear();
            ModalLookup::Matched(binding.target())
        }
        Some(ModalTrie::Sequence(bindings)) => {
            state.clear();
            let targets = bindings
                .iter()
                .map(ModalBinding::target)
                .collect::<Vec<_>>();
            ModalLookup::MatchedSequence(targets.into_boxed_slice())
        }
        None => {
            if let Some(ch) = key.char() {
                if let Some(fallback) = trie.search_fallback(&state[1..]) {
                    state.clear();
                    return ModalLookup::Fallback(fallback.id(), ch);
                }
            }
            ModalLookup::Cancelled(std::mem::take(state).into_boxed_slice())
        }
    }
}

/// Modal bindings that can be executed directly by the editing engine.
///
/// Use this keymap shape for normal editor input and for component-owned edit
/// regions that intentionally expose only engine commands.
pub type ModalCommandBinding = ModalBinding<CommandToken>;
pub type ModalKeyTrieNode = ModalTrieNode<CommandToken>;
pub type ModalKeyTrie = ModalTrie<CommandToken>;
pub type ModalKeymaps = ModalKeymapState<CommandToken>;

impl ModalCommandBinding {
    pub const fn token(&self) -> CommandToken {
        self.target()
    }
}

impl ModalKeymaps {
    pub fn get(&mut self, mode: Mode, key: KeyEvent) -> KeymapLookup {
        let keymaps = &*self.map();
        let Some(keymap) = keymaps.get(&mode) else {
            return KeymapLookup::NotFound;
        };
        lookup_keymap(keymap, &mut self.state, &mut self.sticky, key).into()
    }
}

impl From<ModalLookup<CommandToken>> for KeymapLookup {
    fn from(lookup: ModalLookup<CommandToken>) -> Self {
        match lookup {
            ModalLookup::Matched(token) => Self::Matched(token),
            ModalLookup::MatchedSequence(tokens) => Self::MatchedSequence(tokens),
            ModalLookup::Pending(info) => Self::Pending(info),
            ModalLookup::NotFound => Self::NotFound,
            ModalLookup::Cancelled(keys) => Self::Cancelled(keys),
            ModalLookup::Fallback(fallback, ch) => Self::Fallback(fallback, ch),
        }
    }
}

impl KeymapQuery for ModalKeymaps {
    fn contains_key(&self, mode: Mode, key: KeyEvent) -> bool {
        Self::contains_key(self, mode, key)
    }

    fn pending(&self) -> &[KeyEvent] {
        Self::pending(self)
    }

    fn has_sticky(&self) -> bool {
        self.sticky.is_some()
    }

    fn sticky_infobox(&self) -> Option<Info> {
        self.sticky.as_ref().map(ModalTrieNode::infobox)
    }

    fn clear_sticky(&mut self) {
        self.sticky = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FrontendIntentKind {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComponentIntentKind {}

pub type FrontendIntentId = crate::id::Id<FrontendIntentKind, &'static str>;
pub type ComponentIntentId = crate::id::Id<ComponentIntentKind, &'static str>;

/// Modal binding intent for components that need editor-like keymaps without
/// pretending every configured command is executable by the editing engine.
///
/// Choose the variant by execution boundary:
///
/// - `Engine`: the target is a `CommandToken` and can be sent directly to an
///   editing engine or component-owned edit region.
/// - `Frontend`: the target is a named frontend/editor command from user
///   config. Components must explicitly whitelist and reinterpret these.
/// - `Component`: the target is local to the receiving component/panel and has
///   no editor-global meaning.
///
/// This type is intentionally separate from `ModalKeymaps`: only
/// `ModalKeymaps` implements `KeymapQuery`, so code that needs to dispatch into
/// the core modal engine cannot accidentally receive frontend or component
/// actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModalIntent {
    Engine(CommandToken),
    Frontend(FrontendIntentId),
    Component(ComponentIntentId),
}

impl ModalIntent {
    pub const fn engine(token: CommandToken) -> Self {
        Self::Engine(token)
    }

    pub const fn frontend(id: FrontendIntentId) -> Self {
        Self::Frontend(id)
    }

    pub const fn component(id: ComponentIntentId) -> Self {
        Self::Component(id)
    }
}

pub type ModalIntentBinding = ModalBinding<ModalIntent>;
pub type ModalIntentTrieNode = ModalTrieNode<ModalIntent>;
pub type ModalIntentTrie = ModalTrie<ModalIntent>;
pub type ModalIntentLookup = ModalLookup<ModalIntent>;
pub type ModalIntentKeymaps = ModalKeymapState<ModalIntent>;

impl ModalIntentBinding {
    pub const fn intent(&self) -> ModalIntent {
        self.target()
    }
}

impl ModalIntentKeymaps {
    pub fn get(&mut self, mode: Mode, key: KeyEvent) -> ModalIntentLookup {
        let keymaps = &*self.map();
        let Some(keymap) = keymaps.get(&mode) else {
            return ModalLookup::NotFound;
        };
        lookup_keymap(keymap, &mut self.state, &mut self.sticky, key)
    }
}
