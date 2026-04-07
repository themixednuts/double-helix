//! Target shape for a single application-level fan-in (see `docs/runtime-executor-architecture-spec.md`, Unified Event Ingress).
//!
//! Terminal input and editor idle streams still use their own `select!` arms today; this enum is the
//! typed home for runtime-originated work and can grow (`Shutdown`, merged terminal/editor, etc.).

use super::ingress::RuntimeEvent;

#[derive(Debug)]
pub enum AppEvent {
    Runtime(RuntimeEvent),
}
