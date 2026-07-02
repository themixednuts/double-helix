//! Dependency-free modal editing state machines.
//!
//! The core layer owns modal state: counts, operator-pending flow,
//! char-pending resolution, dot-repeat, insert recording, and reset behavior.
//! It is generic over a host context type so embedders can wire commands to any
//! editor model without depending on Helix crates.

pub mod engine;
mod key;
mod registry;

pub use engine::*;
pub use key::*;
pub use registry::*;

#[cfg(test)]
mod tests;
