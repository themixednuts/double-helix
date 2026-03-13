use arc_swap::{
    access::{DynAccess, DynGuard},
    ArcSwap,
};
use std::{
    borrow::Cow,
    collections::HashMap,
    ops::{Deref, DerefMut},
    sync::Arc,
};

use crate::document::Mode;
use crate::engine::{CharPendingBinding, CommandToken, KeymapLookup, KeymapQuery};
use crate::info::Info;
use crate::input::{KeyCode, KeyEvent};
use crate::keyboard::KeyModifiers;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModalCommandBinding {
    token: CommandToken,
    doc: &'static str,
}

impl ModalCommandBinding {
    pub const fn new(token: CommandToken, doc: &'static str) -> Self {
        Self { token, doc }
    }

    pub const fn token(self) -> CommandToken {
        self.token
    }

    pub const fn doc(self) -> &'static str {
        self.doc
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModalKeyTrieNode {
    pub name: String,
    pub map: HashMap<KeyEvent, ModalKeyTrie>,
    pub order: Vec<KeyEvent>,
    pub is_sticky: bool,
    pub fallback: Option<CharPendingBinding>,
}

impl ModalKeyTrieNode {
    pub fn new(name: &str, map: HashMap<KeyEvent, ModalKeyTrie>, order: Vec<KeyEvent>) -> Self {
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
                ModalKeyTrie::Command(cmd) => cmd.doc(),
                ModalKeyTrie::Node(node) => node.name.as_str(),
                ModalKeyTrie::Sequence(_) => "[Multiple commands]",
            };
            body.push((key.to_string(), desc));
        }
        if let Some(fallback) = self.fallback {
            body.push(("...".to_string(), fallback.doc()));
        }
        Info::new(self.name.clone(), &body)
    }
}

impl Deref for ModalKeyTrieNode {
    type Target = HashMap<KeyEvent, ModalKeyTrie>;

    fn deref(&self) -> &Self::Target {
        &self.map
    }
}

impl DerefMut for ModalKeyTrieNode {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.map
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModalKeyTrie {
    Command(ModalCommandBinding),
    Sequence(Box<[ModalCommandBinding]>),
    Node(ModalKeyTrieNode),
}

impl ModalKeyTrie {
    pub fn node(&self) -> Option<&ModalKeyTrieNode> {
        match self {
            Self::Node(node) => Some(node),
            Self::Command(_) | Self::Sequence(_) => None,
        }
    }

    pub fn search(&self, keys: &[KeyEvent]) -> Option<&ModalKeyTrie> {
        let mut trie = self;
        for key in keys {
            trie = match trie {
                Self::Node(map) => map.get(key),
                Self::Command(_) | Self::Sequence(_) => None,
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
                Self::Command(_) | Self::Sequence(_) => None,
            }?;
        }
        None
    }
}

pub struct ModalKeymaps {
    map: Box<dyn DynAccess<HashMap<Mode, ModalKeyTrie>> + Send + Sync>,
    state: Vec<KeyEvent>,
    sticky: Option<ModalKeyTrieNode>,
}

impl ModalKeymaps {
    pub fn new(map: Box<dyn DynAccess<HashMap<Mode, ModalKeyTrie>> + Send + Sync>) -> Self {
        Self {
            map,
            state: Vec::new(),
            sticky: None,
        }
    }

    pub fn from_shared(map: Arc<ArcSwap<HashMap<Mode, ModalKeyTrie>>>) -> Self {
        Self::new(Box::new(map))
    }

    pub fn map(&self) -> DynGuard<HashMap<Mode, ModalKeyTrie>> {
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
            .and_then(ModalKeyTrie::node)
            .is_some_and(|node| node.contains_key(&key))
    }

    pub fn get(&mut self, mode: Mode, key: KeyEvent) -> KeymapLookup {
        let keymaps = &*self.map();
        let Some(keymap) = keymaps.get(&mode) else {
            return KeymapLookup::NotFound;
        };
        lookup_keymap(keymap, &mut self.state, &mut self.sticky, key)
    }
}

impl Default for ModalKeymaps {
    fn default() -> Self {
        Self::new(Box::new(ArcSwap::new(Arc::new(HashMap::new()))))
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
        self.sticky.as_ref().map(ModalKeyTrieNode::infobox)
    }

    fn clear_sticky(&mut self) {
        self.sticky = None;
    }
}

fn lookup_keymap(
    keymap: &ModalKeyTrie,
    state: &mut Vec<KeyEvent>,
    sticky: &mut Option<ModalKeyTrieNode>,
    key: KeyEvent,
) -> KeymapLookup {
    if key
        == (KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::empty(),
        })
    {
        if !state.is_empty() {
            return KeymapLookup::Cancelled(std::mem::take(state).into_boxed_slice());
        }
        *sticky = None;
    }

    let first = state.first().unwrap_or(&key);
    let trie_node = match sticky.as_ref() {
        Some(trie) => Cow::Owned(ModalKeyTrie::Node(trie.clone())),
        None => Cow::Borrowed(keymap),
    };

    let trie = match trie_node.search(&[*first]) {
        Some(ModalKeyTrie::Command(cmd)) => return KeymapLookup::Matched(cmd.token()),
        Some(ModalKeyTrie::Sequence(cmds)) => {
            let tokens = cmds.iter().map(|cmd| cmd.token()).collect::<Vec<_>>();
            return KeymapLookup::MatchedSequence(tokens.into_boxed_slice());
        }
        None => return KeymapLookup::NotFound,
        Some(trie) => trie,
    };

    state.push(key);
    match trie.search(&state[1..]) {
        Some(ModalKeyTrie::Node(map)) => {
            if map.is_sticky {
                state.clear();
                *sticky = Some(map.clone());
            }
            KeymapLookup::Pending(Some(map.infobox()))
        }
        Some(ModalKeyTrie::Command(cmd)) => {
            state.clear();
            KeymapLookup::Matched(cmd.token())
        }
        Some(ModalKeyTrie::Sequence(cmds)) => {
            state.clear();
            let tokens = cmds.iter().map(|cmd| cmd.token()).collect::<Vec<_>>();
            KeymapLookup::MatchedSequence(tokens.into_boxed_slice())
        }
        None => {
            if let Some(ch) = key.char() {
                if let Some(fallback) = trie.search_fallback(&state[1..]) {
                    state.clear();
                    return KeymapLookup::Fallback(fallback.id(), ch);
                }
            }
            KeymapLookup::Cancelled(std::mem::take(state).into_boxed_slice())
        }
    }
}
