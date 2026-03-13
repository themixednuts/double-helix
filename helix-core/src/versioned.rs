use std::fmt;
use std::ops::{Deref, DerefMut};

/// A value with automatic change tracking via generation counter.
///
/// Read access is free (`Deref`, `get`). Any mutable access (`DerefMut`,
/// `set`) automatically bumps the generation counter so consumers can
/// detect changes via `gen()`.
pub struct Versioned<T> {
    inner: T,
    gen: u64,
}

impl<T> Versioned<T> {
    /// Creates a new `Versioned` value with generation 0.
    pub fn new(value: T) -> Self {
        Self {
            inner: value,
            gen: 0,
        }
    }

    /// Returns a reference to the inner value.
    pub fn get(&self) -> &T {
        &self.inner
    }

    /// Returns the current generation counter.
    pub fn gen(&self) -> u64 {
        self.gen
    }

    /// Replaces the inner value, bumping the generation counter.
    pub fn set(&mut self, value: T) {
        self.gen = self.gen.wrapping_add(1);
        self.inner = value;
    }

    /// Consumes the wrapper and returns the inner value.
    pub fn into_inner(self) -> T {
        self.inner
    }

    fn bump(&mut self) {
        self.gen = self.gen.wrapping_add(1);
    }
}

impl<T> Deref for Versioned<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T> DerefMut for Versioned<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.bump();
        &mut self.inner
    }
}

impl<T: Default> Default for Versioned<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: fmt::Debug> fmt::Debug for Versioned<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Versioned")
            .field("gen", &self.gen)
            .field("inner", &self.inner)
            .finish()
    }
}

impl<T> From<T> for Versioned<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}
