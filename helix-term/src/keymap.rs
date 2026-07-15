pub mod default;
pub mod macros;

pub use crate::commands::MappableCommand;
use arc_swap::{ArcSwap, Guard};
use helix_modal::registry::CommandScope;
use helix_view::{
    document::Mode,
    engine::{CharPendingBinding, KeymapLookup},
    info::Info,
    input::KeyEvent,
    keymap::{
        FrontendIntentId, ModalCommandBinding, ModalIntent, ModalIntentBinding, ModalIntentTrie,
        ModalIntentTrieNode, ModalKeyTrie, ModalKeyTrieNode,
    },
};
use serde::Deserialize;
use std::{
    borrow::Cow,
    collections::{BTreeSet, HashMap},
    ops::{Deref, DerefMut},
    sync::Arc,
};

pub use default::default;
use macros::key;

#[derive(Debug, Clone, Default)]
pub struct KeyTrieNode {
    /// A label for keys coming under this node, like "Goto mode"
    name: String,
    map: HashMap<KeyEvent, KeyTrie>,
    order: Vec<KeyEvent>,
    is_sticky: bool,
    fallback: Option<CharPendingBinding>,
}

impl<'de> Deserialize<'de> for KeyTrieNode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let map = HashMap::<KeyEvent, KeyTrie>::deserialize(deserializer)?;
        let order = map.keys().copied().collect::<Vec<_>>(); // NOTE: map.keys() has arbitrary order
        Ok(Self {
            map,
            order,
            ..Default::default()
        })
    }
}

impl KeyTrieNode {
    pub fn new(name: &str, map: HashMap<KeyEvent, KeyTrie>, order: Vec<KeyEvent>) -> Self {
        Self {
            name: name.to_string(),
            map,
            order,
            is_sticky: false,
            fallback: None,
        }
    }

    /// Merge another Node in. Leaves and subnodes from the other node replace
    /// corresponding keyevent in self, except when both other and self have
    /// subnodes for same key. In that case the merge is recursive.
    pub fn merge(&mut self, mut other: Self) {
        for (key, trie) in std::mem::take(&mut other.map) {
            if let Some(KeyTrie::Node(node)) = self.map.get_mut(&key) {
                if let KeyTrie::Node(other_node) = trie {
                    node.merge(other_node);
                    continue;
                }
            }
            self.map.insert(key, trie);
        }
        for &key in self.map.keys() {
            if !self.order.contains(&key) {
                self.order.push(key);
            }
        }
    }

    pub fn infobox(&self) -> Info {
        let mut body: Vec<(BTreeSet<KeyEvent>, &str)> = Vec::with_capacity(self.len());
        for (&key, trie) in self.iter() {
            let desc = match trie {
                KeyTrie::MappableCommand(cmd) => {
                    if cmd.name() == "no_op" {
                        continue;
                    }
                    cmd.doc()
                }
                KeyTrie::Node(n) => &n.name,
                KeyTrie::Sequence(_) => "[Multiple commands]",
            };
            match body.iter().position(|(_, d)| d == &desc) {
                Some(pos) => {
                    body[pos].0.insert(key);
                }
                None => body.push((BTreeSet::from([key]), desc)),
            }
        }
        body.sort_unstable_by_key(|(keys, _)| {
            self.order
                .iter()
                .position(|&k| k == *keys.iter().next().unwrap())
                .unwrap()
        });

        let mut body: Vec<_> = body
            .into_iter()
            .map(|(events, desc)| {
                let events = events.iter().map(ToString::to_string).collect::<Vec<_>>();
                (events.join(", "), desc)
            })
            .collect();
        if let Some(fallback) = self.fallback.as_ref() {
            body.push(("...".to_string(), fallback.doc()));
        }
        Info::new(self.name.clone(), &body)
    }
}

impl PartialEq for KeyTrieNode {
    fn eq(&self, other: &Self) -> bool {
        self.map == other.map
    }
}

impl Deref for KeyTrieNode {
    type Target = HashMap<KeyEvent, KeyTrie>;

    fn deref(&self) -> &Self::Target {
        &self.map
    }
}

impl DerefMut for KeyTrieNode {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.map
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum KeyTrie {
    MappableCommand(MappableCommand),
    Sequence(Vec<MappableCommand>),
    Node(KeyTrieNode),
}

impl<'de> Deserialize<'de> for KeyTrie {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(KeyTrieVisitor)
    }
}

struct KeyTrieVisitor;

impl<'de> serde::de::Visitor<'de> for KeyTrieVisitor {
    type Value = KeyTrie;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(formatter, "a command, list of commands, or sub-keymap")
    }

    fn visit_str<E>(self, command: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        command
            .parse::<MappableCommand>()
            .map(KeyTrie::MappableCommand)
            .map_err(E::custom)
    }

    fn visit_seq<S>(self, mut seq: S) -> Result<Self::Value, S::Error>
    where
        S: serde::de::SeqAccess<'de>,
    {
        let mut commands = Vec::new();
        while let Some(command) = seq.next_element::<String>()? {
            commands.push(
                command
                    .parse::<MappableCommand>()
                    .map_err(serde::de::Error::custom)?,
            )
        }

        // Prevent macro keybindings from being used in command sequences.
        // This is meant to be a temporary restriction pending a larger
        // refactor of how command sequences are executed.
        if commands
            .iter()
            .any(|cmd| matches!(cmd, MappableCommand::Macro { .. }))
        {
            return Err(serde::de::Error::custom(
                "macro keybindings may not be used in command sequences",
            ));
        }

        Ok(KeyTrie::Sequence(commands))
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: serde::de::MapAccess<'de>,
    {
        let mut mapping = HashMap::new();
        let mut order = Vec::new();
        while let Some((key, value)) = map.next_entry::<KeyEvent, KeyTrie>()? {
            mapping.insert(key, value);
            order.push(key);
        }
        Ok(KeyTrie::Node(KeyTrieNode::new("", mapping, order)))
    }
}

impl KeyTrie {
    pub fn reverse_map(&self) -> ReverseKeymap {
        // recursively visit all nodes in keymap
        fn map_node(cmd_map: &mut ReverseKeymap, node: &KeyTrie, keys: &mut Vec<KeyEvent>) {
            match node {
                KeyTrie::MappableCommand(MappableCommand::Macro { .. }) => {}
                KeyTrie::MappableCommand(cmd) => {
                    let name = cmd.name();
                    if name != "no_op" {
                        cmd_map.entry(name.into()).or_default().push(keys.clone())
                    }
                }
                KeyTrie::Node(next) => {
                    for (key, trie) in &next.map {
                        keys.push(*key);
                        map_node(cmd_map, trie, keys);
                        keys.pop();
                    }
                }
                KeyTrie::Sequence(_) => {}
            };
        }

        let mut res = HashMap::new();
        map_node(&mut res, self, &mut Vec::new());
        res
    }

    pub fn node(&self) -> Option<&KeyTrieNode> {
        match *self {
            KeyTrie::Node(ref node) => Some(node),
            KeyTrie::MappableCommand(_) | KeyTrie::Sequence(_) => None,
        }
    }

    pub fn node_mut(&mut self) -> Option<&mut KeyTrieNode> {
        match *self {
            KeyTrie::Node(ref mut node) => Some(node),
            KeyTrie::MappableCommand(_) | KeyTrie::Sequence(_) => None,
        }
    }

    /// Merge another KeyTrie in, assuming that this KeyTrie and the other
    /// are both Nodes. Panics otherwise.
    pub fn merge_nodes(&mut self, mut other: Self) {
        let node = std::mem::take(other.node_mut().unwrap());
        self.node_mut().unwrap().merge(node);
    }

    pub fn search(&self, keys: &[KeyEvent]) -> Option<&KeyTrie> {
        let mut trie = self;
        for key in keys {
            trie = match trie {
                KeyTrie::Node(map) => map.get(key),
                // leaf encountered while keys left to process
                KeyTrie::MappableCommand(_) | KeyTrie::Sequence(_) => None,
            }?
        }
        Some(trie)
    }

    pub fn search_fallback(&self, keys: &[KeyEvent]) -> Option<&CharPendingBinding> {
        // TODO: this is copied from above, hacky
        let mut trie = self;
        let mut keys = keys.iter().peekable();
        while let Some(key) = keys.next() {
            trie = match trie {
                KeyTrie::Node(map) => match map.get(key) {
                    Some(i) => Some(i),
                    None => {
                        if keys.peek().is_none() {
                            return map.fallback.as_ref();
                        }
                        None
                    }
                },
                // leaf encountered while keys left to process
                KeyTrie::MappableCommand(_) | KeyTrie::Sequence(_) => None,
            }?
        }
        None
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum KeymapResult {
    /// Needs more keys to execute a command. Contains valid keys for next keystroke.
    Pending(KeyTrieNode),
    Matched(MappableCommand),
    /// Matched a sequence of commands to execute.
    MatchedSequence(Vec<MappableCommand>),
    /// Key was not found in the root keymap
    NotFound,
    /// Key is invalid in combination with previous keys. Contains keys leading upto
    /// and including current (invalid) key.
    Cancelled(Vec<KeyEvent>),
    Fallback(helix_view::engine::CharPendingId, char),
}

/// A map of command names to keybinds that will execute the command.
pub type ReverseKeymap = HashMap<String, Vec<Vec<KeyEvent>>>;

#[derive(Debug, Clone)]
pub struct CompiledKeymapScope {
    pub language: Option<String>,
    pub path_prefix: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone)]
pub struct CompiledKeymapContribution {
    pub scope: CompiledKeymapScope,
    pub keymap: HashMap<Mode, KeyTrie>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ActiveKeymapContext {
    language: Option<String>,
    path: Option<std::path::PathBuf>,
}

pub struct Keymaps {
    base: HashMap<Mode, KeyTrie>,
    contributions: HashMap<helix_plugin_api::KeymapHandle, CompiledKeymapContribution>,
    effective: ArcSwap<HashMap<Mode, KeyTrie>>,
    active_context: ActiveKeymapContext,
    /// Stores pending keys waiting for the next key. This is relative to a
    /// sticky node if one is in use.
    state: Vec<KeyEvent>,
    /// Stores the sticky node if one is activated.
    pub sticky: Option<KeyTrieNode>,
}

impl Keymaps {
    pub fn new(base: HashMap<Mode, KeyTrie>) -> Self {
        let effective = ArcSwap::from_pointee(base.clone());
        Self {
            base,
            contributions: HashMap::new(),
            effective,
            active_context: ActiveKeymapContext::default(),
            state: Vec::new(),
            sticky: None,
        }
    }

    pub fn map(&self) -> Guard<Arc<HashMap<Mode, KeyTrie>>> {
        self.effective.load()
    }

    pub fn replace_base(&mut self, base: HashMap<Mode, KeyTrie>) {
        self.base = base;
        self.rebuild();
    }

    pub fn set_contribution(
        &mut self,
        handle: helix_plugin_api::KeymapHandle,
        contribution: CompiledKeymapContribution,
    ) {
        self.contributions.insert(handle, contribution);
        self.rebuild();
    }

    pub fn remove_contribution(&mut self, handle: helix_plugin_api::KeymapHandle) -> bool {
        let removed = self.contributions.remove(&handle).is_some();
        if removed {
            self.rebuild();
        }
        removed
    }

    /// Select the immutable keymap snapshot for the current dispatch context.
    /// Returns true only when the published snapshot changed.
    pub fn set_context(&mut self, language: Option<&str>, path: Option<&std::path::Path>) -> bool {
        if self.active_context.language.as_deref() == language
            && self.active_context.path.as_deref() == path
        {
            return false;
        }
        self.active_context = ActiveKeymapContext {
            language: language.map(str::to_owned),
            path: path.map(std::path::Path::to_owned),
        };
        self.rebuild();
        true
    }

    fn rebuild(&mut self) {
        let mut effective = self.base.clone();
        let mut contributions = self.contributions.iter().collect::<Vec<_>>();
        contributions.sort_unstable_by_key(|(handle, _)| handle.raw());
        for (_, contribution) in contributions {
            if contribution.scope.matches(&self.active_context) {
                merge_keys(&mut effective, contribution.keymap.clone());
            }
        }
        self.effective.store(Arc::new(effective));
        self.state.clear();
        self.sticky = None;
    }

    /// Returns list of keys waiting to be disambiguated in current mode.
    pub fn pending(&self) -> &[KeyEvent] {
        &self.state
    }

    pub fn contains_key(&self, mode: Mode, key: KeyEvent) -> bool {
        let keymaps = &*self.map();
        let keymap = &keymaps[&mode];
        keymap
            .search(self.pending())
            .and_then(KeyTrie::node)
            .is_some_and(|node| node.contains_key(&key))
    }

    /// Lookup `key` in the keymap to try and find a command to execute. Escape
    /// key cancels pending keystrokes. If there are no pending keystrokes but a
    /// sticky node is in use, it will be cleared.
    pub fn get(&mut self, mode: Mode, key: KeyEvent) -> KeymapResult {
        let keymaps = &*self.map();
        let keymap = &keymaps[&mode];
        lookup_keymap(keymap, &mut self.state, &mut self.sticky, key)
    }
}

impl CompiledKeymapScope {
    fn matches(&self, context: &ActiveKeymapContext) -> bool {
        self.language
            .as_deref()
            .is_none_or(|language| context.language.as_deref() == Some(language))
            && self.path_prefix.as_deref().is_none_or(|prefix| {
                context
                    .path
                    .as_deref()
                    .is_some_and(|path| path.starts_with(prefix))
            })
    }
}

impl Default for Keymaps {
    fn default() -> Self {
        Self::new(default())
    }
}

fn lookup_keymap(
    keymap: &KeyTrie,
    state: &mut Vec<KeyEvent>,
    sticky: &mut Option<KeyTrieNode>,
    key: KeyEvent,
) -> KeymapResult {
    if key!(Esc) == key {
        if !state.is_empty() {
            return KeymapResult::Cancelled(std::mem::take(state));
        }
        *sticky = None;
    }

    let first = state.first().unwrap_or(&key);
    let trie_node = match sticky.as_ref() {
        Some(trie) => Cow::Owned(KeyTrie::Node(trie.clone())),
        None => Cow::Borrowed(keymap),
    };

    let trie = match trie_node.search(&[*first]) {
        Some(KeyTrie::MappableCommand(cmd)) => return KeymapResult::Matched(cmd.clone()),
        Some(KeyTrie::Sequence(cmds)) => return KeymapResult::MatchedSequence(cmds.clone()),
        None => return KeymapResult::NotFound,
        Some(trie) => trie,
    };

    state.push(key);
    match trie.search(&state[1..]) {
        Some(KeyTrie::Node(map)) => {
            if map.is_sticky {
                state.clear();
                *sticky = Some(map.clone());
            }
            KeymapResult::Pending(map.clone())
        }
        Some(KeyTrie::MappableCommand(cmd)) => {
            state.clear();
            KeymapResult::Matched(cmd.clone())
        }
        Some(KeyTrie::Sequence(cmds)) => {
            state.clear();
            KeymapResult::MatchedSequence(cmds.clone())
        }
        None => {
            if let Some(ch) = key.char() {
                if let Some(fallback) = trie.search_fallback(&state[1..]) {
                    state.clear();
                    return KeymapResult::Fallback(fallback.id(), ch);
                }
            }

            KeymapResult::Cancelled(std::mem::take(state))
        }
    }
}

fn keytrie_is_frontend(trie: KeyTrie) -> bool {
    match trie {
        KeyTrie::MappableCommand(cmd) => is_frontend_command(&cmd),
        KeyTrie::Sequence(cmds) => cmds.iter().all(is_frontend_command),
        KeyTrie::Node(node) => {
            node.values()
                .all(|child| keytrie_is_frontend(child.clone()))
                && node.fallback.is_none()
        }
    }
}

pub fn is_frontend_result(result: &KeymapResult) -> bool {
    match result {
        KeymapResult::Pending(node) => keytrie_is_frontend(KeyTrie::Node(node.clone())),
        KeymapResult::Matched(cmd) => is_frontend_command(cmd),
        KeymapResult::MatchedSequence(cmds) => cmds.iter().all(is_frontend_command),
        KeymapResult::NotFound | KeymapResult::Cancelled(_) | KeymapResult::Fallback(_, _) => false,
    }
}

pub fn is_frontend_command(cmd: &MappableCommand) -> bool {
    cmd.scope() == CommandScope::Frontend
}

pub fn to_component_modal_keymaps(map: &HashMap<Mode, KeyTrie>) -> HashMap<Mode, ModalKeyTrie> {
    map.iter()
        .filter_map(|(&mode, trie)| to_component_modal_trie(trie).map(|trie| (mode, trie)))
        .collect()
}

fn to_component_modal_trie(trie: &KeyTrie) -> Option<ModalKeyTrie> {
    match trie {
        KeyTrie::MappableCommand(cmd @ MappableCommand::Engine { spec })
            if cmd.supports_component_region() =>
        {
            Some(ModalKeyTrie::Binding(ModalCommandBinding::new(
                spec.token(),
                spec.doc(),
            )))
        }
        KeyTrie::MappableCommand(
            MappableCommand::Engine { .. }
            | MappableCommand::Frontend { .. }
            | MappableCommand::Typable { .. }
            | MappableCommand::Macro { .. },
        ) => None,
        KeyTrie::Sequence(cmds) => {
            let commands = cmds
                .iter()
                .filter(|cmd| cmd.supports_component_region())
                .filter_map(|cmd| match cmd {
                    MappableCommand::Engine { spec } => {
                        Some(ModalCommandBinding::new(spec.token(), spec.doc()))
                    }
                    MappableCommand::Frontend { .. }
                    | MappableCommand::Typable { .. }
                    | MappableCommand::Macro { .. } => None,
                })
                .collect::<Vec<_>>();
            if commands.is_empty() {
                None
            } else {
                Some(ModalKeyTrie::Sequence(commands.into_boxed_slice()))
            }
        }
        KeyTrie::Node(node) => {
            let map = node
                .iter()
                .filter_map(|(&key, trie)| to_component_modal_trie(trie).map(|trie| (key, trie)))
                .collect::<HashMap<_, _>>();
            if map.is_empty() && node.fallback.is_none() {
                return None;
            }

            let mut modal = ModalKeyTrieNode::new(&node.name, map, node.order.clone());
            modal.is_sticky = node.is_sticky;
            modal.fallback = node.fallback;
            Some(ModalKeyTrie::Node(modal))
        }
    }
}

pub fn to_semantic_modal_keymaps(map: &HashMap<Mode, KeyTrie>) -> HashMap<Mode, ModalIntentTrie> {
    map.iter()
        .filter_map(|(&mode, trie)| to_semantic_modal_trie(trie).map(|trie| (mode, trie)))
        .collect()
}

fn to_semantic_modal_trie(trie: &KeyTrie) -> Option<ModalIntentTrie> {
    match trie {
        KeyTrie::MappableCommand(cmd) => semantic_modal_binding(cmd).map(ModalIntentTrie::Binding),
        KeyTrie::Sequence(cmds) => {
            let commands = cmds
                .iter()
                .filter_map(semantic_modal_binding)
                .collect::<Vec<_>>();
            if commands.is_empty() {
                None
            } else {
                Some(ModalIntentTrie::Sequence(commands.into_boxed_slice()))
            }
        }
        KeyTrie::Node(node) => {
            let map = node
                .iter()
                .filter_map(|(&key, trie)| to_semantic_modal_trie(trie).map(|trie| (key, trie)))
                .collect::<HashMap<_, _>>();
            if map.is_empty() && node.fallback.is_none() {
                return None;
            }

            let mut modal = ModalIntentTrieNode::new(&node.name, map, node.order.clone());
            modal.is_sticky = node.is_sticky;
            modal.fallback = node.fallback;
            Some(ModalIntentTrie::Node(modal))
        }
    }
}

fn semantic_modal_binding(cmd: &MappableCommand) -> Option<ModalIntentBinding> {
    match cmd {
        MappableCommand::Engine { spec } => Some(ModalIntentBinding::new(
            ModalIntent::engine(spec.token()),
            spec.doc(),
        )),
        MappableCommand::Frontend { spec } => Some(ModalIntentBinding::new(
            ModalIntent::frontend(FrontendIntentId::new(spec.name())),
            spec.doc(),
        )),
        MappableCommand::Typable { .. } | MappableCommand::Macro { .. } => None,
    }
}

pub fn to_modal_keymaps(map: &HashMap<Mode, KeyTrie>) -> HashMap<Mode, ModalKeyTrie> {
    map.iter()
        .filter_map(|(&mode, trie)| to_modal_trie(trie).map(|trie| (mode, trie)))
        .collect()
}

fn to_modal_trie(trie: &KeyTrie) -> Option<ModalKeyTrie> {
    match trie {
        KeyTrie::MappableCommand(MappableCommand::Engine { spec }) => Some(ModalKeyTrie::Binding(
            ModalCommandBinding::new(spec.token(), spec.doc()),
        )),
        KeyTrie::MappableCommand(
            MappableCommand::Frontend { .. }
            | MappableCommand::Typable { .. }
            | MappableCommand::Macro { .. },
        ) => None,
        KeyTrie::Sequence(cmds) => {
            let commands = cmds
                .iter()
                .filter_map(|cmd| match cmd {
                    MappableCommand::Engine { spec } => {
                        Some(ModalCommandBinding::new(spec.token(), spec.doc()))
                    }
                    MappableCommand::Frontend { .. }
                    | MappableCommand::Typable { .. }
                    | MappableCommand::Macro { .. } => None,
                })
                .collect::<Vec<_>>();
            if commands.is_empty() {
                None
            } else {
                Some(ModalKeyTrie::Sequence(commands.into_boxed_slice()))
            }
        }
        KeyTrie::Node(node) => {
            let map = node
                .iter()
                .filter_map(|(&key, trie)| to_modal_trie(trie).map(|trie| (key, trie)))
                .collect::<HashMap<_, _>>();
            if map.is_empty() && node.fallback.is_none() {
                return None;
            }

            let mut modal = ModalKeyTrieNode::new(&node.name, map, node.order.clone());
            modal.is_sticky = node.is_sticky;
            modal.fallback = node.fallback;
            Some(ModalKeyTrie::Node(modal))
        }
    }
}

/// Convert a frontend `KeymapResult` into an engine `KeymapLookup`.
///
/// Engine commands are resolved via `modal_command()` on `MappableCommand`.
/// Frontend-only commands (those returning `None` from `modal_command()`)
/// result in `NotFound` since the engine can't execute them.
pub fn resolve_keymap_result(result: &KeymapResult) -> KeymapLookup {
    match result {
        KeymapResult::Matched(cmd) => match cmd.modal_command() {
            Some(token) => KeymapLookup::Matched(token),
            None => KeymapLookup::NotFound, // frontend-only command
        },
        KeymapResult::MatchedSequence(cmds) => {
            let tokens: Vec<_> = cmds.iter().filter_map(|c| c.modal_command()).collect();
            if tokens.is_empty() {
                KeymapLookup::NotFound
            } else {
                KeymapLookup::MatchedSequence(tokens.into_boxed_slice())
            }
        }
        KeymapResult::Pending(node) => KeymapLookup::Pending(Some(node.infobox())),
        KeymapResult::NotFound => KeymapLookup::NotFound,
        KeymapResult::Cancelled(keys) => KeymapLookup::Cancelled(keys.clone().into_boxed_slice()),
        KeymapResult::Fallback(fallback, ch) => KeymapLookup::Fallback(*fallback, *ch),
    }
}

impl helix_view::engine::KeymapQuery for Keymaps {
    fn contains_key(&self, mode: Mode, key: KeyEvent) -> bool {
        Keymaps::contains_key(self, mode, key)
    }

    fn pending(&self) -> &[KeyEvent] {
        Keymaps::pending(self)
    }

    fn has_sticky(&self) -> bool {
        self.sticky.is_some()
    }

    fn sticky_infobox(&self) -> Option<Info> {
        self.sticky.as_ref().map(|node| node.infobox())
    }

    fn clear_sticky(&mut self) {
        self.sticky = None;
    }
}

/// Merge default config keys with user overwritten keys for custom user config.
pub fn merge_keys(dst: &mut HashMap<Mode, KeyTrie>, mut delta: HashMap<Mode, KeyTrie>) {
    for (mode, keys) in dst {
        keys.merge_nodes(
            delta
                .remove(mode)
                .unwrap_or_else(|| KeyTrie::Node(KeyTrieNode::default())),
        )
    }
}

pub fn compile_plugin_keymap(
    definition: &helix_plugin_api::KeymapDefinition,
) -> anyhow::Result<CompiledKeymapContribution> {
    use std::str::FromStr;

    let mode = match definition.mode {
        helix_plugin_api::KeymapMode::Normal => Mode::Normal,
        helix_plugin_api::KeymapMode::Insert => Mode::Insert,
        helix_plugin_api::KeymapMode::Select => Mode::Select,
    };
    let mut root = KeyTrieNode::default();
    for binding in &definition.bindings {
        anyhow::ensure!(!binding.keys.is_empty(), "key sequence cannot be empty");
        anyhow::ensure!(
            !binding.commands.is_empty(),
            "command sequence cannot be empty"
        );
        let keys = binding
            .keys
            .iter()
            .map(|key| KeyEvent::from_str(key).map(KeyEvent::canonicalize))
            .collect::<Result<Vec<_>, _>>()?;
        let commands = binding
            .commands
            .iter()
            .map(|command| MappableCommand::from_str(command))
            .collect::<anyhow::Result<Vec<_>>>()?;
        anyhow::ensure!(
            !commands
                .iter()
                .any(|command| matches!(command, MappableCommand::Macro { .. })),
            "macro commands cannot be used in plugin keymaps"
        );
        let command = if commands.len() == 1 {
            KeyTrie::MappableCommand(commands.into_iter().next().unwrap())
        } else {
            KeyTrie::Sequence(commands)
        };
        insert_compiled_binding(&mut root, &keys, command)?;
    }

    Ok(CompiledKeymapContribution {
        scope: CompiledKeymapScope {
            language: definition.scope.language.clone(),
            path_prefix: definition
                .scope
                .path_prefix
                .as_deref()
                .map(std::path::PathBuf::from),
        },
        keymap: HashMap::from([(mode, KeyTrie::Node(root))]),
    })
}

fn insert_compiled_binding(
    node: &mut KeyTrieNode,
    keys: &[KeyEvent],
    command: KeyTrie,
) -> anyhow::Result<()> {
    let key = keys[0];
    if keys.len() == 1 {
        anyhow::ensure!(
            !node.map.contains_key(&key),
            "duplicate or prefix-conflicting key binding: {}",
            key.key_sequence_format()
        );
        node.map.insert(key, command);
        node.order.push(key);
        return Ok(());
    }

    if !node.map.contains_key(&key) {
        node.map.insert(key, KeyTrie::Node(KeyTrieNode::default()));
        node.order.push(key);
    }
    let child = node
        .map
        .get_mut(&key)
        .and_then(KeyTrie::node_mut)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "key binding extends an existing command at {}",
                key.key_sequence_format()
            )
        })?;
    insert_compiled_binding(child, &keys[1..], command)
}

#[cfg(test)]
mod tests {
    use super::macros::keymap;
    use super::*;
    use arc_swap::ArcSwap;
    use helix_core::{hashmap, Selection, Transaction};
    use helix_view::{
        document::Mode,
        edit_region::EditRegion,
        engine::EngineResult,
        graphics::Rect,
        handlers::Handlers,
        theme,
        traits::{Bounded, Focusable, Modal},
        Editor,
    };
    use std::sync::Arc;

    fn named_command(name: &str) -> MappableCommand {
        MappableCommand::named(name).expect("named command must exist")
    }

    fn plugin_keymap(
        language: Option<&str>,
        path_prefix: Option<&str>,
        keys: &[&str],
        command: &str,
    ) -> helix_plugin_api::KeymapDefinition {
        helix_plugin_api::KeymapDefinition {
            mode: helix_plugin_api::KeymapMode::Normal,
            scope: helix_plugin_api::KeymapScope {
                language: language.map(str::to_owned),
                path_prefix: path_prefix.map(str::to_owned),
            },
            bindings: vec![helix_plugin_api::KeymapBinding {
                keys: keys.iter().map(|key| (*key).into()).collect(),
                commands: vec![command.into()],
            }],
        }
    }

    fn test_editor() -> Editor {
        let theme_loader = theme::Loader::new(&[]);
        let syn_loader = helix_core::config::default_lang_loader();
        let config = helix_view::editor::Config::default();
        let config = Arc::new(ArcSwap::from_pointee(config));
        let handlers = Handlers::dummy();
        let mut editor = Editor::new(
            Rect::new(0, 0, 80, 24),
            Arc::new(theme_loader),
            Arc::new(ArcSwap::from_pointee(syn_loader)),
            Arc::new(arc_swap::access::Map::new(
                config,
                |c: &helix_view::editor::Config| c,
            )),
            helix_runtime::test::runtime(),
            handlers,
        );
        editor.frontend_mut().modal_keymaps = Arc::new(ArcSwap::from_pointee(
            to_component_modal_keymaps(&default()),
        ));
        Arc::new(helix_modal::ModalEngineFactory::default()).install(&mut editor);
        editor
    }

    fn test_edit_region() -> (Editor, EditRegion) {
        let mut editor = test_editor();
        let mut region = EditRegion::default();
        region.set_area(Rect::new(0, 0, 40, 5));
        region.set_focused(true);
        region.ensure_init(&mut editor);
        (editor, region)
    }

    fn set_region_text(region: &EditRegion, editor: &mut Editor, text: &str) {
        let view_id = region.view_id();
        let doc = region.document_mut(editor).expect("component document");
        doc.set_selection(view_id, Selection::point(0));
        let transaction = Transaction::change(
            doc.text(),
            [(0, doc.text().len_chars(), Some(text.into()))].into_iter(),
        );
        doc.apply(&transaction, view_id);
    }

    fn set_region_cursor(region: &EditRegion, editor: &mut Editor, pos: usize) {
        let doc = region.document_mut(editor).expect("component document");
        doc.set_selection(region.view_id(), Selection::point(pos));
    }

    fn region_cursor(region: &EditRegion, editor: &Editor) -> usize {
        let doc = region.document(editor).expect("component document");
        doc.selection(region.view_id())
            .primary()
            .cursor(doc.text().slice(..))
    }

    #[test]
    #[should_panic]
    fn duplicate_keys_should_panic() {
        keymap!({ "Normal mode"
            "i" => normal_mode,
            "i" => goto_definition,
        });
    }

    #[test]
    fn check_duplicate_keys_in_default_keymap() {
        // will panic on duplicate keys, assumes that `Keymaps` uses keymap! macro
        Keymaps::default();
    }

    #[test]
    fn merge_partial_keys() {
        let keymap = hashmap! {
            Mode::Normal => keymap!({ "Normal mode"
                "i" => normal_mode,
                "无" => insert_mode,
                "z" => jump_backward,
                "g" => { "Merge into goto mode"
                    "$" => goto_line_end,
                    "g" => delete_char_forward,
                },
            })
        };
        let mut merged_keyamp = default();
        merge_keys(&mut merged_keyamp, keymap.clone());
        assert_ne!(keymap, merged_keyamp);

        let mut keymap = Keymaps::new(merged_keyamp.clone());
        assert_eq!(
            keymap.get(Mode::Normal, key!('i')),
            KeymapResult::Matched(named_command("normal_mode")),
            "Leaf should replace leaf"
        );
        assert_eq!(
            keymap.get(Mode::Normal, key!('无')),
            KeymapResult::Matched(named_command("insert_mode")),
            "New leaf should be present in merged keymap"
        );
        // Assumes that z is a node in the default keymap
        assert_eq!(
            keymap.get(Mode::Normal, key!('z')),
            KeymapResult::Matched(named_command("jump_backward")),
            "Leaf should replace node"
        );

        let keymap = merged_keyamp.get_mut(&Mode::Normal).unwrap();
        // Assumes that `g` is a node in default keymap
        assert_eq!(
            keymap.search(&[key!('g'), key!('$')]).unwrap(),
            &KeyTrie::MappableCommand(named_command("goto_line_end")),
            "Leaf should be present in merged subnode"
        );
        // Assumes that `gg` is in default keymap
        assert_eq!(
            keymap.search(&[key!('g'), key!('g')]).unwrap(),
            &KeyTrie::MappableCommand(named_command("delete_char_forward")),
            "Leaf should replace old leaf in merged subnode"
        );
        // Assumes that `ge` is in default keymap
        assert_eq!(
            keymap.search(&[key!('g'), key!('e')]).unwrap(),
            &KeyTrie::MappableCommand(named_command("goto_last_line")),
            "Old leaves in subnode should be present in merged node"
        );

        assert!(
            merged_keyamp
                .get(&Mode::Normal)
                .and_then(|key_trie| key_trie.node())
                .unwrap()
                .len()
                > 1
        );
        assert!(!merged_keyamp
            .get(&Mode::Insert)
            .and_then(|key_trie| key_trie.node())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn plugin_keymap_scope_switches_published_snapshot_without_hot_path_parsing() {
        let definition = plugin_keymap(Some("rust"), Some("workspace"), &["F24"], ":write");
        let contribution = compile_plugin_keymap(&definition).unwrap();
        let handle =
            helix_plugin_api::KeymapHandle::from_raw(std::num::NonZeroU64::new(1).unwrap());
        let mut keymaps = Keymaps::default();
        keymaps.set_contribution(handle, contribution);

        keymaps.set_context(
            Some("text"),
            Some(std::path::Path::new("workspace/main.rs")),
        );
        assert!(matches!(
            keymaps.get(Mode::Normal, "F24".parse().unwrap()),
            KeymapResult::NotFound
        ));

        keymaps.set_context(
            Some("rust"),
            Some(std::path::Path::new("workspace/main.rs")),
        );
        assert!(matches!(
            keymaps.get(Mode::Normal, "F24".parse().unwrap()),
            KeymapResult::Matched(MappableCommand::Typable { ref name, .. }) if name == "write"
        ));

        assert!(keymaps.remove_contribution(handle));
        assert!(matches!(
            keymaps.get(Mode::Normal, "F24".parse().unwrap()),
            KeymapResult::NotFound
        ));
    }

    #[test]
    fn plugin_keymap_compilation_rejects_prefix_conflicts() {
        let mut definition = plugin_keymap(None, None, &["F24"], ":write");
        definition.bindings.push(helix_plugin_api::KeymapBinding {
            keys: vec!["F24".into(), "x".into()],
            commands: vec![":quit".into()],
        });
        assert!(compile_plugin_keymap(&definition)
            .unwrap_err()
            .to_string()
            .contains("extends an existing command"));
    }

    #[test]
    fn order_should_be_set() {
        let keymap = hashmap! {
            Mode::Normal => keymap!({ "Normal mode"
                "space" => { ""
                    "s" => { ""
                        "v" => vsplit,
                        "c" => hsplit,
                    },
                },
            })
        };
        let mut merged_keyamp = default();
        merge_keys(&mut merged_keyamp, keymap.clone());
        assert_ne!(keymap, merged_keyamp);
        let keymap = merged_keyamp.get_mut(&Mode::Normal).unwrap();
        // Make sure mapping works
        assert_eq!(
            keymap.search(&[key!(' '), key!('s'), key!('v')]).unwrap(),
            &KeyTrie::MappableCommand(named_command("vsplit")),
            "Leaf should be present in merged subnode"
        );
        // Make sure an order was set during merge
        let node = keymap.search(&[crate::key!(' ')]).unwrap();
        assert!(!node.node().unwrap().order.as_slice().is_empty())
    }

    #[test]
    fn aliased_modes_are_same_in_default_keymap() {
        let keymaps = Keymaps::default().map();
        let root = keymaps.get(&Mode::Normal).unwrap();
        assert_eq!(
            root.search(&[key!(' '), key!('w')]).unwrap(),
            root.search(&["C-w".parse::<KeyEvent>().unwrap()]).unwrap(),
            "Mismatch for window mode on `Space-w` and `Ctrl-w`"
        );
        assert_eq!(
            root.search(&[key!('z')]).unwrap(),
            root.search(&[key!('Z')]).unwrap(),
            "Mismatch for view mode on `z` and `Z`"
        );
    }

    #[test]
    fn reverse_map() {
        let normal_mode = keymap!({ "Normal mode"
            "i" => insert_mode,
            "g" => { "Goto"
                "g" => goto_file_start,
                "e" => goto_file_end,
            },
            "j" | "k" => move_line_down,
        });
        let keymap = normal_mode;
        let mut reverse_map = keymap.reverse_map();

        // sort keybindings in order to have consistent tests
        // HashMaps can be compared but we can still get different ordering of bindings
        // for commands that have multiple bindings assigned
        for v in reverse_map.values_mut() {
            v.sort()
        }

        assert_eq!(
            reverse_map,
            HashMap::from([
                ("insert_mode".to_string(), vec![vec![key!('i')]]),
                (
                    "goto_file_start".to_string(),
                    vec![vec![key!('g'), key!('g')]]
                ),
                (
                    "goto_file_end".to_string(),
                    vec![vec![key!('g'), key!('e')]]
                ),
                (
                    "move_line_down".to_string(),
                    vec![vec![key!('j')], vec![key!('k')]]
                ),
            ]),
            "Mismatch"
        )
    }

    #[test]
    fn escaped_keymap() {
        use crate::commands::MappableCommand;
        use helix_view::input::{KeyCode, KeyEvent, KeyModifiers};

        let keys = r#"
"+" = [
    "select_all",
    ":pipe sed -E 's/\\s+$//g'",
]
        "#;

        let key = KeyEvent {
            code: KeyCode::Char('+'),
            modifiers: KeyModifiers::NONE,
        };

        let expectation = KeyTrie::Node(KeyTrieNode::new(
            "",
            hashmap! {
                key => KeyTrie::Sequence(vec!{
                    named_command("select_all"),
                    MappableCommand::Typable {
                        name: "pipe".to_string(),
                        args: "sed -E 's/\\s+$//g'".to_string(),
                        doc: "".to_string(),
                    },
                })
            },
            vec![key],
        ));

        assert_eq!(toml::from_str(keys), Ok(expectation));
    }

    #[test]
    fn gw_dispatch_routes_to_frontend() {
        // Verify that `gw` (goto_word) correctly routes through dispatch:
        // 1. `g` → Pending (mixed subtree, not all frontend) → engine path
        // 2. `w` → Matched(goto_word) which IS frontend → frontend path
        let mut keymaps = Keymaps::default();

        // First key: `g`
        let g_result = keymaps.get(Mode::Normal, key!('g'));
        assert!(
            matches!(g_result, KeymapResult::Pending(_)),
            "g should be Pending, got {g_result:?}"
        );
        // `g` subtree is mixed (engine + frontend), so is_frontend_result should be false
        assert!(
            !is_frontend_result(&g_result),
            "g subtree should NOT be all-frontend (it contains engine motions like move_line_up)"
        );

        // Second key: `w` (with `g` pending in keymap state)
        let w_result = keymaps.get(Mode::Normal, key!('w'));
        assert!(
            matches!(w_result, KeymapResult::Matched(ref cmd) if cmd.name() == "goto_word"),
            "gw should resolve to goto_word, got {w_result:?}"
        );
        // goto_word is a frontend command
        assert!(
            is_frontend_result(&w_result),
            "goto_word should be a frontend command"
        );
    }

    #[test]
    fn component_modal_keymaps_keep_viewport_safe_commands() {
        let modal = to_component_modal_keymaps(&default());
        let normal = modal.get(&Mode::Normal).expect("normal mode modal keymap");
        let insert = modal.get(&Mode::Insert).expect("insert mode modal keymap");

        assert!(
            normal.search(&[key!('u')]).is_some(),
            "component modal keymaps should keep viewport-backed undo"
        );
        assert!(
            normal
                .search(&["pageup".parse::<KeyEvent>().unwrap()])
                .is_some(),
            "component modal keymaps should keep viewport-backed page movement"
        );
        assert!(
            normal
                .search(&["C-o".parse::<KeyEvent>().unwrap()])
                .is_none(),
            "component modal keymaps must not expose editor jumplist traversal"
        );
        assert!(
            normal
                .search(&["C-s".parse::<KeyEvent>().unwrap()])
                .is_some(),
            "component modal keymaps should keep viewport-backed save-selection jumps"
        );
        assert!(
            normal.search(&["G".parse::<KeyEvent>().unwrap()]).is_some(),
            "component modal keymaps should keep viewport-backed goto-line motions"
        );
        assert!(
            normal
                .search(&[
                    "g".parse::<KeyEvent>().unwrap(),
                    "|".parse::<KeyEvent>().unwrap(),
                ])
                .is_some(),
            "component modal keymaps should keep viewport-backed goto-column motions"
        );
        assert!(
            normal.search(&["%".parse::<KeyEvent>().unwrap()]).is_some(),
            "component modal keymaps should keep viewport-backed bracket matching"
        );
        assert!(
            insert
                .search(&["backspace".parse::<KeyEvent>().unwrap()])
                .is_some(),
            "component modal keymaps should keep core insert-mode editing"
        );
        assert!(
            insert
                .search(&["left".parse::<KeyEvent>().unwrap()])
                .is_some(),
            "component modal keymaps should keep viewport-backed cursor motions"
        );
    }

    #[test]
    fn semantic_modal_keymaps_keep_frontend_command_intent() {
        let modal = to_semantic_modal_keymaps(&default());
        let normal = modal.get(&Mode::Normal).expect("normal mode modal keymap");
        let Some(ModalIntentTrie::Binding(binding)) = normal.search(&[key!(']'), key!('d')]) else {
            panic!("semantic modal keymaps should keep diagnostic navigation");
        };
        assert_eq!(
            binding.intent(),
            ModalIntent::frontend(FrontendIntentId::new("goto_next_diag"))
        );
    }

    #[test]
    fn semantic_modal_keymaps_keep_engine_command_intent() {
        let modal = to_semantic_modal_keymaps(&default());
        let normal = modal.get(&Mode::Normal).expect("normal mode modal keymap");
        let Some(ModalIntentTrie::Binding(binding)) = normal.search(&[key!('w')]) else {
            panic!("semantic modal keymaps should keep word motion");
        };
        assert!(matches!(binding.intent(), ModalIntent::Engine(_)));
    }

    #[tokio::test]
    async fn edit_region_dispatch_keeps_mode_local_to_component() {
        let (mut editor, mut region) = test_edit_region();

        let result = region
            .dispatch_key(&mut editor, key!('v'))
            .expect("component dispatch result");
        assert!(matches!(result, EngineResult::Executed));
        assert_eq!(editor.mode(), Mode::Normal);
        assert_eq!(region.mode(), Mode::Select);
    }

    #[tokio::test]
    async fn edit_region_dispatch_supports_counted_multi_key_file_start_motion() {
        let (mut editor, mut region) = test_edit_region();
        set_region_text(&region, &mut editor, "one\ntwo\nthree\n");

        let line_three = {
            let doc = region.document(&editor).expect("component document");
            doc.text().line_to_char(2)
        };
        set_region_cursor(&region, &mut editor, line_three);

        let first = region
            .dispatch_key(&mut editor, key!('g'))
            .expect("pending goto prefix");
        let second = region
            .dispatch_key(&mut editor, key!('g'))
            .expect("goto file start");
        assert!(matches!(first, EngineResult::Pending));
        assert!(matches!(second, EngineResult::Executed));
        assert_eq!(region_cursor(&region, &editor), 0);

        set_region_cursor(&region, &mut editor, line_three);
        let count = region
            .dispatch_key(&mut editor, key!('2'))
            .expect("count accumulation");
        let prefix = region
            .dispatch_key(&mut editor, key!('g'))
            .expect("goto prefix");
        let motion = region
            .dispatch_key(&mut editor, key!('g'))
            .expect("counted goto file start");
        assert!(matches!(count, EngineResult::Pending));
        assert!(matches!(prefix, EngineResult::Pending));
        assert!(matches!(motion, EngineResult::Executed));

        let line_two = {
            let doc = region.document(&editor).expect("component document");
            doc.text().line_to_char(1)
        };
        assert_eq!(region_cursor(&region, &editor), line_two);
    }

    #[tokio::test]
    async fn edit_region_dispatch_rejects_tree_only_jumplist_motion() {
        let (mut editor, mut region) = test_edit_region();

        let result = region
            .dispatch_key(&mut editor, "C-o".parse::<KeyEvent>().unwrap())
            .expect("component dispatch result");
        assert!(matches!(result, EngineResult::Unbound));
    }
}
