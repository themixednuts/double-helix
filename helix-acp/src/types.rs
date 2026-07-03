//! ACP protocol types.
//!
//! Wire types are sourced from the official `agent-client-protocol` schema. This
//! crate keeps only transport-facing parse wrappers and Helix-specific capability
//! summaries here.

pub use agent_client_protocol::schema::{v1::*, ProtocolVersion};

pub type ConfigOption = SessionConfigOption;
pub type ConfigOptionUpdateData = ConfigOptionUpdate;
pub type CurrentModeUpdateData = CurrentModeUpdate;
pub type AvailableCommandsUpdateData = AvailableCommandsUpdate;
pub type UsageUpdateData = UsageUpdate;
pub type ToolCallInfo = ToolCall;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentCaps {
    pub load_session: bool,
    pub list_sessions: bool,
    pub resume_session: bool,
    pub delete_session: bool,
    pub close_session: bool,
    pub mcp: bool,
    pub auth: bool,
    pub config_options: bool,
    pub additional_directories: bool,
    pub fork_session: bool,
}

#[must_use]
pub fn agent_caps(init: &InitializeResponse) -> AgentCaps {
    AgentCaps {
        load_session: init.agent_capabilities.load_session,
        list_sessions: init.agent_capabilities.session_capabilities.list.is_some(),
        resume_session: init
            .agent_capabilities
            .session_capabilities
            .resume
            .is_some(),
        delete_session: init
            .agent_capabilities
            .session_capabilities
            .delete
            .is_some(),
        close_session: init.agent_capabilities.session_capabilities.close.is_some(),
        mcp: init.agent_capabilities.mcp_capabilities.http
            || init.agent_capabilities.mcp_capabilities.sse
            || init.agent_capabilities.mcp_capabilities.acp,
        auth: !init.auth_methods.is_empty(),
        config_options: true,
        additional_directories: init
            .agent_capabilities
            .session_capabilities
            .additional_directories
            .is_some(),
        fork_session: init.agent_capabilities.session_capabilities.fork.is_some(),
    }
}

/// Parsed notification from the agent.
#[allow(
    clippy::large_enum_variant,
    reason = "protocol notifications are transient parse results; boxing would add churn at all match sites"
)]
pub enum AgentNotification {
    SessionUpdate(SessionNotification),
    CompleteElicitation(CompleteElicitationNotification),
}

impl AgentNotification {
    pub fn parse(
        method: &str,
        params: crate::jsonrpc::Params,
    ) -> std::result::Result<Self, crate::Error> {
        match method {
            crate::methods::SESSION_UPDATE => {
                let notif: SessionNotification =
                    params.parse().map_err(crate::Error::AgentError)?;
                Ok(AgentNotification::SessionUpdate(notif))
            }
            crate::methods::ELICITATION_COMPLETE => {
                let notif: CompleteElicitationNotification =
                    params.parse().map_err(crate::Error::AgentError)?;
                Ok(AgentNotification::CompleteElicitation(notif))
            }
            _ => Err(crate::Error::Unhandled(method.to_string())),
        }
    }
}

/// Parsed request from the agent (expects a response).
pub enum AgentMethodCall {
    ReadTextFile(ReadTextFileRequest),
    WriteTextFile(WriteTextFileRequest),
    CreateTerminal(CreateTerminalRequest),
    TerminalOutput(TerminalOutputRequest),
    WaitForTerminalExit(WaitForTerminalExitRequest),
    KillTerminal(KillTerminalRequest),
    ReleaseTerminal(ReleaseTerminalRequest),
    RequestPermission(RequestPermissionRequest),
    CreateElicitation(CreateElicitationRequest),
}

impl AgentMethodCall {
    pub fn parse(
        method: &str,
        params: crate::jsonrpc::Params,
    ) -> std::result::Result<Self, crate::Error> {
        match method {
            crate::methods::FS_READ_TEXT_FILE => Ok(AgentMethodCall::ReadTextFile(
                params.parse().map_err(crate::Error::AgentError)?,
            )),
            crate::methods::FS_WRITE_TEXT_FILE => Ok(AgentMethodCall::WriteTextFile(
                params.parse().map_err(crate::Error::AgentError)?,
            )),
            crate::methods::TERMINAL_CREATE => Ok(AgentMethodCall::CreateTerminal(
                params.parse().map_err(crate::Error::AgentError)?,
            )),
            crate::methods::TERMINAL_OUTPUT => Ok(AgentMethodCall::TerminalOutput(
                params.parse().map_err(crate::Error::AgentError)?,
            )),
            crate::methods::TERMINAL_WAIT_FOR_EXIT => Ok(AgentMethodCall::WaitForTerminalExit(
                params.parse().map_err(crate::Error::AgentError)?,
            )),
            crate::methods::TERMINAL_KILL => Ok(AgentMethodCall::KillTerminal(
                params.parse().map_err(crate::Error::AgentError)?,
            )),
            crate::methods::TERMINAL_RELEASE => Ok(AgentMethodCall::ReleaseTerminal(
                params.parse().map_err(crate::Error::AgentError)?,
            )),
            crate::methods::REQUEST_PERMISSION => Ok(AgentMethodCall::RequestPermission(
                params.parse().map_err(crate::Error::AgentError)?,
            )),
            crate::methods::ELICITATION_CREATE => Ok(AgentMethodCall::CreateElicitation(
                params.parse().map_err(crate::Error::AgentError)?,
            )),
            _ => Err(crate::Error::Unhandled(method.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_session_config_option_serializes_typed_boolean_value() {
        let value = SessionConfigOptionValue::boolean(true);
        let request = SetSessionConfigOptionRequest::new("session-1", "approval", value);
        let json = serde_json::to_value(request).expect("serialize request");

        assert_eq!(json["sessionId"], "session-1");
        assert_eq!(json["configId"], "approval");
        assert_eq!(json["type"], "boolean");
        assert_eq!(json["value"], true);
    }
}
