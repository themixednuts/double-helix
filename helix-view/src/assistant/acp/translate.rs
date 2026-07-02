use super::super::{backend, config, history, host, mode, prompt, thread, tool};
use super::Session;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use helix_acp::types as acp;
use helix_core::Uri;

pub fn caps(init: &acp::InitializeResponse, _connect: &backend::Connect) -> backend::Caps {
    let can_load = init.agent_capabilities.load_session.unwrap_or(false);
    let prompt_caps = init
        .agent_capabilities
        .prompt_capabilities
        .clone()
        .unwrap_or_default();
    backend::Caps {
        load_thread: can_load,
        close_thread: false,
        history: Some(history::Caps {
            list: false,
            load: can_load,
            close: false,
            resume: can_load,
        }),
        mode: None,
        config: None,
        prompt: prompt::Caps {
            image: prompt_caps.image.unwrap_or(false),
            audio: prompt_caps.audio.unwrap_or(false),
            embedded_context: prompt_caps.embedded_context.unwrap_or(false),
        },
        host: host::Caps {
            fs: host::FsCaps {
                read_text: true,
                write_text: true,
            },
            terminal: Some(host::TerminalCaps),
            permission: host::PermissionCaps,
        },
    }
}

pub fn session_new(scope: &thread::Scope) -> acp::NewSessionRequest {
    acp::NewSessionRequest {
        mcp_servers: Vec::new(),
        cwd: scope.cwd.clone(),
    }
}

pub fn session_load(session: &Session) -> acp::LoadSessionRequest {
    acp::LoadSessionRequest {
        session_id: session.to_string(),
    }
}

pub fn submit(session: &Session, request: &prompt::Request) -> acp::PromptRequest {
    acp::PromptRequest {
        session_id: session.to_string(),
        prompt: request.parts().iter().cloned().map(content_block).collect(),
    }
}

pub fn cancel(session: &Session) -> acp::CancelNotification {
    acp::CancelNotification {
        session_id: session.to_string(),
    }
}

pub fn set_mode(session: &Session, mode: &mode::Id) -> acp::SetSessionModeRequest {
    acp::SetSessionModeRequest {
        session_id: session.to_string(),
        mode_id: mode.to_string(),
    }
}

pub fn set_config(
    session: &Session,
    option: &config::Id,
    value: &config::ValueId,
) -> acp::SetSessionConfigOptionRequest {
    acp::SetSessionConfigOptionRequest {
        session_id: session.to_string(),
        config_id: option.to_string(),
        value_id: value.to_string(),
    }
}

pub fn mode_set(current: &str, items: &[acp::SessionMode]) -> Result<mode::Set, mode::Missing> {
    let items: Vec<_> = items
        .iter()
        .map(|item| mode::Item {
            id: mode::Id::new(item.id.clone()),
            name: item.name.clone(),
            description: item.description.clone(),
        })
        .collect();
    mode::Set::new(
        items,
        mode::Selected::Current(mode::Id::new(current.to_string())),
    )
}

pub fn config_state(items: &[acp::ConfigOption]) -> Result<config::State, config::Missing> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let values: Vec<_> = item
            .options
            .iter()
            .map(|value| config::Value {
                id: config::ValueId::new(value.value.clone()),
                label: value.name.clone(),
                description: value.description.clone(),
            })
            .collect();
        out.push(config::Item::new(
            config::Id::new(item.id.clone()),
            item.name.clone(),
            item.category.clone(),
            config::Selected::Current(config::ValueId::new(item.current_value.clone())),
            values,
        )?);
    }
    Ok(config::State::new(out))
}

pub fn config_event(data: acp::ConfigOptionUpdateData) -> Result<thread::Event, config::Missing> {
    config_state(&data.config_options).map(thread::Event::Config)
}

pub fn mode_event(
    data: acp::CurrentModeUpdateData,
    items: &[acp::SessionMode],
) -> Result<thread::Event, mode::Missing> {
    mode_set(&data.mode_id, items).map(thread::Event::Mode)
}

pub fn thread_event(update: acp::SessionUpdate) -> Option<thread::Event> {
    match update {
        acp::SessionUpdate::Plan(plan) => Some(thread::Event::Plan(plan_event(plan))),
        acp::SessionUpdate::AgentMessageChunk(chunk) => Some(thread::Event::Content(
            thread::Content::Append(thread::NewEntry {
                turn: None,
                kind: thread::EntryKind::AssistantText {
                    text: content_text(chunk.content.clone()),
                },
                locations: content_locations(std::slice::from_ref(&chunk.content)),
            }),
        )),
        acp::SessionUpdate::ToolCall(info) => Some(thread::Event::Content(
            thread::Content::Append(thread::NewEntry {
                turn: None,
                kind: thread::EntryKind::ToolCall(tool_call(info)),
                locations: Vec::new(),
            }),
        )),
        acp::SessionUpdate::ToolCallUpdate(update) => Some(thread::Event::Content(
            thread::Content::Append(thread::NewEntry {
                turn: None,
                kind: thread::EntryKind::ToolCall(tool_update(update.clone())),
                locations: update
                    .content
                    .as_deref()
                    .map(content_locations)
                    .unwrap_or_default(),
            }),
        )),
        acp::SessionUpdate::ConfigOptionUpdate(_) => None,
        acp::SessionUpdate::CurrentModeUpdate(_) => None,
        acp::SessionUpdate::AvailableCommandsUpdate(_) => None,
    }
}

pub fn update_locations(update: &acp::SessionUpdate) -> Vec<crate::collab::Location> {
    match update {
        acp::SessionUpdate::AgentMessageChunk(chunk) => {
            content_locations(std::slice::from_ref(&chunk.content))
        }
        acp::SessionUpdate::ToolCallUpdate(update) => update
            .content
            .as_deref()
            .map(content_locations)
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn plan_event(plan: acp::Plan) -> super::super::plan::Event {
    super::super::plan::Event::Replace(
        plan.entries
            .into_iter()
            .map(|entry| super::super::plan::Item {
                content: entry.content,
                status: match entry.status.unwrap_or(acp::PlanEntryStatus::Pending) {
                    acp::PlanEntryStatus::Pending => super::super::plan::Status::Pending,
                    acp::PlanEntryStatus::InProgress => super::super::plan::Status::InProgress,
                    acp::PlanEntryStatus::Completed => super::super::plan::Status::Completed,
                    acp::PlanEntryStatus::Failed => super::super::plan::Status::Failed,
                },
            })
            .collect(),
    )
}

fn tool_call(info: acp::ToolCallInfo) -> tool::Call {
    tool::Call {
        id: tool::Id::new(info.tool_call_id),
        name: info.title.unwrap_or_else(|| "tool".to_string()),
        state: tool_state(info.status),
        output: String::new(),
    }
}

fn tool_update(update: acp::ToolCallUpdate) -> tool::Call {
    let output = update
        .content
        .map(|blocks| {
            blocks
                .into_iter()
                .map(content_text)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    tool::Call {
        id: tool::Id::new(update.tool_call_id),
        name: "tool".to_string(),
        state: update
            .status
            .map(tool_state)
            .unwrap_or(tool::State::Pending),
        output,
    }
}

fn tool_state(state: acp::ToolCallStatus) -> tool::State {
    match state {
        acp::ToolCallStatus::Running => tool::State::Running,
        acp::ToolCallStatus::Completed => tool::State::Completed,
        acp::ToolCallStatus::Failed => tool::State::Failed { message: None },
        acp::ToolCallStatus::Cancelled => tool::State::Canceled,
    }
}

pub(crate) fn content_block(part: prompt::Part) -> acp::ContentBlock {
    match part {
        prompt::Part::Text(text) => acp::ContentBlock::Text(acp::TextContent { text }),
        prompt::Part::Image(image) => acp::ContentBlock::Image(acp::ImageContent {
            data: STANDARD.encode(image.data),
            mime_type: image.mime,
        }),
        prompt::Part::Audio(audio) => acp::ContentBlock::Audio(acp::AudioContent {
            data: STANDARD.encode(audio.data),
            mime_type: audio.mime,
        }),
        prompt::Part::Link(link) => acp::ContentBlock::ResourceLink(acp::ResourceLink {
            uri: link.uri,
            name: link.label,
            mime_type: None,
        }),
        prompt::Part::Resource(resource) => acp::ContentBlock::Resource(acp::EmbeddedResource {
            uri: resource.uri,
            mime_type: resource.mime,
            text: resource.text,
            blob: resource.data.map(|data| STANDARD.encode(data)),
        }),
    }
}

fn content_text(block: acp::ContentBlock) -> String {
    match block {
        acp::ContentBlock::Text(text) => text.text,
        acp::ContentBlock::Image(_) => "<image>".to_string(),
        acp::ContentBlock::Audio(_) => "<audio>".to_string(),
        acp::ContentBlock::ResourceLink(link) => link.name.unwrap_or(link.uri),
        acp::ContentBlock::Resource(resource) => resource.text.unwrap_or(resource.uri),
    }
}

fn content_locations(blocks: &[acp::ContentBlock]) -> Vec<crate::collab::Location> {
    blocks.iter().filter_map(content_location).collect()
}

fn content_location(block: &acp::ContentBlock) -> Option<crate::collab::Location> {
    let uri = match block {
        acp::ContentBlock::ResourceLink(link) => link.uri.as_str(),
        acp::ContentBlock::Resource(resource) => resource.uri.as_str(),
        _ => return None,
    };
    let url = url::Url::parse(uri).ok()?;
    let uri = Uri::try_from(url).ok()?;
    Some(crate::collab::Location::new(
        uri.as_path()?.to_path_buf(),
        crate::collab::location::Source::Tool,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_prompt_caps() {
        let init = acp::InitializeResponse {
            protocol_version: 1,
            agent_capabilities: acp::AgentCapabilities {
                load_session: Some(true),
                prompt_capabilities: Some(acp::PromptCapabilities {
                    image: Some(true),
                    audio: Some(false),
                    embedded_context: Some(true),
                }),
                mcp: None,
            },
            agent_info: None,
            auth_methods: None,
        };
        let caps = caps(
            &init,
            &backend::Connect {
                scope: thread::Scope::new(std::path::PathBuf::from(".")),
                context_servers: Vec::new(),
            },
        );
        assert!(caps.load_thread);
        assert!(caps.prompt.image);
        assert!(!caps.prompt.audio);
        assert!(caps.prompt.embedded_context);
    }

    #[test]
    fn collects_locations_from_resource_updates() {
        let path = std::env::temp_dir().join("example.js");
        let uri = url::Url::from_file_path(&path).unwrap().to_string();
        let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate {
            tool_call_id: "tool-1".to_string(),
            status: None,
            content: Some(vec![acp::ContentBlock::ResourceLink(acp::ResourceLink {
                uri,
                name: Some("example".to_string()),
                mime_type: None,
            })]),
        });

        let locations = update_locations(&update);
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].path, path);
        assert_eq!(locations[0].source, crate::collab::location::Source::Tool);
    }

    #[test]
    fn thread_event_keeps_chunk_locations() {
        let path = std::env::temp_dir().join("example.ts");
        let uri = url::Url::from_file_path(&path).unwrap().to_string();
        let update = acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk {
            content: acp::ContentBlock::Resource(acp::EmbeddedResource {
                uri,
                mime_type: Some("text/plain".to_string()),
                text: Some("example".to_string()),
                blob: None,
            }),
            role: None,
        });

        let Some(thread::Event::Content(thread::Content::Append(entry))) = thread_event(update)
        else {
            panic!("expected content append event");
        };

        assert_eq!(entry.locations.len(), 1);
        assert_eq!(entry.locations[0].path, path);
        assert_eq!(
            entry.locations[0].source,
            crate::collab::location::Source::Tool
        );
    }

    #[test]
    fn thread_event_synthesizes_out_of_order_tool_update() {
        let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate {
            tool_call_id: "tool-1".to_string(),
            status: Some(acp::ToolCallStatus::Running),
            content: Some(vec![acp::ContentBlock::Text(acp::TextContent {
                text: "partial output".to_string(),
            })]),
        });

        let Some(thread::Event::Content(thread::Content::Append(entry))) = thread_event(update)
        else {
            panic!("expected content append event");
        };

        let thread::EntryKind::ToolCall(call) = entry.kind else {
            panic!("expected synthesized tool call");
        };
        assert_eq!(call.id.as_str(), "tool-1");
        assert_eq!(call.name, "tool");
        assert_eq!(call.state, tool::State::Running);
        assert_eq!(call.output, "partial output");
    }
}
