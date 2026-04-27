/// Helix API exposed to Lua plugins
///
/// All Lua-facing API is registered through the contract-based facade.
pub mod facade;

// Re-exports for convenience
pub use facade::*;
