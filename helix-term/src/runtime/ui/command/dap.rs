use std::path::PathBuf;

use helix_core::syntax::config::DebugConfigCompletion;
use helix_dap::{StackFrame, Thread, ThreadId};

/// Debugger UI ingress (async -> main thread).
pub enum DapCommand {
    /// Multi-field debug template parameter entry (`debug_parameter_prompt`).
    PushDebugParameterPrompt {
        completions: Vec<DebugConfigCompletion>,
        config_name: String,
        params: Vec<String>,
    },
    /// Edit breakpoint condition.
    PushBreakpointConditionPrompt {
        path: PathBuf,
        index: usize,
        initial: Option<String>,
    },
    /// Edit breakpoint log message.
    PushBreakpointLogPrompt {
        path: PathBuf,
        index: usize,
        initial: Option<String>,
    },
    /// `threads` response shown in a picker with a typed action.
    ThreadsPicker {
        threads: Vec<Thread>,
        action: DapThreadAction,
    },
    StackFramesPicker {
        thread_id: ThreadId,
        frames: Vec<StackFrame>,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum DapThreadAction {
    Switch,
    Pause,
}

impl std::fmt::Debug for DapCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PushDebugParameterPrompt { .. } => f.write_str("PushDebugParameterPrompt(..)"),
            Self::PushBreakpointConditionPrompt { .. } => {
                f.write_str("PushBreakpointConditionPrompt(..)")
            }
            Self::PushBreakpointLogPrompt { .. } => f.write_str("PushBreakpointLogPrompt(..)"),
            Self::ThreadsPicker { threads, action } => f
                .debug_struct("ThreadsPicker")
                .field("threads", threads)
                .field("action", action)
                .finish_non_exhaustive(),
            Self::StackFramesPicker { thread_id, frames } => f
                .debug_struct("StackFramesPicker")
                .field("thread_id", thread_id)
                .field("frames", frames)
                .finish_non_exhaustive(),
        }
    }
}
