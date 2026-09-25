//! Modal editing engines: built-in command registration, `ModalEngineFactory`, and the
//! Helix and Vim `helix_view::engine::EditingEngine` implementations.

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

/// Whether a command should become what `.` repeats: it has to have edited the document
/// (selection changes, scrolling and yanks never replace the last change), and history
/// navigation is never a change of its own.
#[cfg(feature = "helix")]
pub(crate) fn is_repeatable_edit(
    command: &str,
    version_before: Option<i32>,
    version_after: Option<i32>,
) -> bool {
    version_before != version_after && !matches!(command, "undo" | "redo" | "earlier" | "later")
}

#[cfg(feature = "helix")]
pub(crate) fn document_version(
    editor: &helix_view::Editor,
    doc_id: helix_view::DocumentId,
) -> Option<i32> {
    editor.document(doc_id).map(helix_view::Document::version)
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
                // Only the text: the frontend runs any other key of a cancelled sequence
                // (`j<Enter>`) on its own, and that run records it.
                rec.keys.extend(pending.iter().copied().filter(|key| {
                    key.char().is_some()
                        && !key.modifiers.intersects(
                            helix_view::keyboard::KeyModifiers::CONTROL
                                | helix_view::keyboard::KeyModifiers::ALT,
                        )
                }));
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
