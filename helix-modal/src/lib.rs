//! Modal editing engines.
//!
//! `helix-modal` has two layers:
//!
//! - [`core`] is a dependency-free modal-editing engine. It provides generic
//!   Helix-style and Vim-style state machines over an embedder-defined context.
//! - With the default `helix` feature, the crate also exposes the Helix editor
//!   integration: built-in command registration, `ModalEngineFactory`, and
//!   `helix_view::engine::EditingEngine` implementations.
//!
//! Embedders that do not use Helix can disable default features and build a
//! registry for their own context type with [`core::Builder`].

pub mod core;

#[cfg(feature = "helix")]
pub mod factory;
#[cfg(feature = "helix")]
pub mod helix;
#[cfg(feature = "helix")]
pub mod populate;
#[cfg(feature = "helix")]
pub mod registry;
#[cfg(feature = "helix")]
pub mod vim;

#[cfg(feature = "helix")]
use std::{borrow::Cow, sync::Arc};

#[cfg(feature = "helix")]
use helix_view::engine::{EngineResult, RecordedAction};
#[cfg(feature = "helix")]
use helix_view::input::KeyEvent;

#[cfg(feature = "helix")]
pub use factory::ModalEngineFactory;
#[cfg(feature = "helix")]
pub use registry::CommandRegistry;

// ─── Shared utilities ───────────────────────────────────────────────

/// Check if a key event is an unmodified character.
#[cfg(feature = "helix")]
pub(crate) fn is_char_key(key: KeyEvent, ch: char) -> bool {
    key.code == helix_view::keyboard::KeyCode::Char(ch) && key.modifiers.is_empty()
}

/// Extract a digit from an unmodified key event.
#[cfg(feature = "helix")]
pub(crate) fn key_to_digit(key: KeyEvent) -> Option<usize> {
    let ch = key.char()?;
    if ch.is_ascii_digit() && key.modifiers.is_empty() {
        Some(ch.to_digit(10).unwrap() as usize)
    } else {
        None
    }
}

// ─── Shared insert recording ────────────────────────────────────────

/// Active insert-mode key recording for dot-repeat.
///
/// Both engines record keys typed during insert mode so that dot-repeat
/// can replay the entire insert sequence.
#[cfg(feature = "helix")]
pub(crate) struct InsertRecording {
    pub entry_command: Cow<'static, str>,
    pub keys: Vec<KeyEvent>,
}

/// Record an insert-mode key into the recording based on the engine result.
///
/// Only records keys that produced observable effects (InsertChar, Executed,
/// CancelledInsert). Pending and Unbound keys are not recorded.
#[cfg(feature = "helix")]
pub(crate) fn record_insert_key(
    recording: &mut Option<InsertRecording>,
    key: KeyEvent,
    result: &EngineResult,
) {
    if let Some(ref mut rec) = recording {
        match result {
            EngineResult::InsertChar(_) | EngineResult::Executed => {
                rec.keys.push(key);
            }
            EngineResult::CancelledInsert(pending) => {
                rec.keys.extend_from_slice(pending);
            }
            EngineResult::Pending | EngineResult::Unbound | EngineResult::ReplayInsert { .. } => {}
        }
    }
}

/// Finalize an insert recording into a `RecordedAction::InsertSequence`.
///
/// Converts the mutable `Vec` into an immutable shared slice.
#[cfg(feature = "helix")]
pub(crate) fn finalize_insert_recording(
    recording: Option<InsertRecording>,
) -> Option<RecordedAction> {
    recording.map(|rec| RecordedAction::InsertSequence {
        entry_command: rec.entry_command,
        keys: Arc::from(rec.keys.into_boxed_slice()),
    })
}
