//! Modal editing engines for Helix.
//!
//! This crate provides pluggable editing engines that implement different
//! modal editing paradigms (Helix's select→act vs Vim's verb→object).
//! Both engines share the same command atoms from `helix_view::commands`
//! but implement different composition rules.

pub mod factory;
pub mod helix;
pub mod populate;
pub mod registry;
pub mod vim;

use std::{borrow::Cow, sync::Arc};

use helix_view::engine::{EngineResult, RecordedAction};
use helix_view::input::KeyEvent;

pub use factory::ModalEngineFactory;
pub use registry::CommandRegistry;

// ─── Shared utilities ───────────────────────────────────────────────

/// Check if a key event is an unmodified character.
pub(crate) fn is_char_key(key: KeyEvent, ch: char) -> bool {
    key.code == helix_view::keyboard::KeyCode::Char(ch) && key.modifiers.is_empty()
}

/// Extract a digit from an unmodified key event.
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
pub(crate) struct InsertRecording {
    pub entry_command: Cow<'static, str>,
    pub keys: Vec<KeyEvent>,
}

/// Record an insert-mode key into the recording based on the engine result.
///
/// Only records keys that produced observable effects (InsertChar, Executed,
/// CancelledInsert). Pending and Unbound keys are not recorded.
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
pub(crate) fn finalize_insert_recording(
    recording: Option<InsertRecording>,
) -> Option<RecordedAction> {
    recording.map(|rec| RecordedAction::InsertSequence {
        entry_command: rec.entry_command,
        keys: Arc::from(rec.keys.into_boxed_slice()),
    })
}
