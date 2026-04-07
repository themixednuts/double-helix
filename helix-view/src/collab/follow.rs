use super::{Location, ParticipantId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    AutoSwitchAndReveal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pause {
    LocalMove,
    LocalScroll,
    LocalEdit,
    BufferSwitch,
    Explicit,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum State {
    #[default]
    Off,
    On {
        mode: Mode,
        participant: ParticipantId,
        last: Option<Location>,
    },
    Paused {
        mode: Mode,
        participant: ParticipantId,
        last: Option<Location>,
        reason: Pause,
    },
}
