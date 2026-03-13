//! Generic typed identifier with compile-time domain safety.
//!
//! The marker type `K` prevents mixing IDs from different domains at compile
//! time. The value type `V` (default: `usize`) stores the actual identifier.
//!
//! ```ignore
//! // Define marker types:
//! enum PanelKind {}
//! enum LayerKind {}
//!
//! type PanelId = Id<PanelKind>;
//! type LayerId = Id<LayerKind>;
//!
//! // These are incompatible types — can't mix them:
//! let p: PanelId = Id::new(1);
//! let l: LayerId = Id::new(2);
//! // p == l  // won't compile
//! ```

use std::hash::{Hash, Hasher};
use std::marker::PhantomData;

/// A typed identifier. `K` is the domain marker, `V` is the backing value.
///
/// All trait impls bound only on `V`, not `K` — the marker is purely phantom.
pub struct Id<K, V: Copy = usize> {
    value: V,
    _kind: PhantomData<fn() -> K>,
}

// --- Manual impls to avoid derive's spurious bounds on K ---

impl<K, V: Copy + std::fmt::Debug> std::fmt::Debug for Id<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Id").field(&self.value).finish()
    }
}

impl<K, V: Copy> Clone for Id<K, V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K, V: Copy> Copy for Id<K, V> {}

impl<K, V: Copy + PartialEq> PartialEq for Id<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<K, V: Copy + Eq> Eq for Id<K, V> {}

impl<K, V: Copy + Hash> Hash for Id<K, V> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}

impl<K, V: Copy> Id<K, V> {
    pub const fn new(value: V) -> Self {
        Self {
            value,
            _kind: PhantomData,
        }
    }

    pub const fn value(self) -> V {
        self.value
    }
}

// --- String-valued IDs (command names, etc.) ---

impl<K> Id<K, &'static str> {
    pub const fn as_str(self) -> &'static str {
        self.value
    }
}

// --- Ord (delegates to V) ---

impl<K, V: Copy + Eq + Ord> PartialOrd for Id<K, V> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<K, V: Copy + Eq + Ord> Ord for Id<K, V> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.value.cmp(&other.value)
    }
}

// --- Display (delegates to V) ---

impl<K, V: Copy + std::fmt::Display> std::fmt::Display for Id<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.value.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    enum FooKind {}
    enum BarKind {}
    type FooId = Id<FooKind>;
    type BarId = Id<BarKind>;

    #[test]
    fn same_value_different_kinds_are_not_equal() {
        let _foo: FooId = Id::new(1);
        let _bar: BarId = Id::new(1);
        // These are different types — can't even compare them.
        // foo == bar would fail to compile.
    }

    #[test]
    fn same_kind_same_value_are_equal() {
        let a: FooId = Id::new(42);
        let b: FooId = Id::new(42);
        assert_eq!(a, b);
    }

    #[test]
    fn ord_works() {
        let a: FooId = Id::new(1);
        let b: FooId = Id::new(2);
        assert!(a < b);
    }

    enum NamedKind {}
    type NamedId = Id<NamedKind, &'static str>;

    #[test]
    fn string_valued_id() {
        let id: NamedId = Id::new("hello");
        assert_eq!(id.as_str(), "hello");
        assert_eq!(format!("{id}"), "hello");
    }
}
