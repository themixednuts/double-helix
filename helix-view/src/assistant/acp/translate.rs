use std::collections::HashSet;
use std::path::PathBuf;

use super::super::{backend, config, history, host, mode, prompt, thread, tool};
use super::Session;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use helix_acp::types as acp;
use helix_core::Uri;

pub fn caps(init: &acp::InitializeResponse, _connect: &backend::Connect) -> backend::Caps {
    let can_load = init.agent_capabilities.load_session;
    let prompt_caps = &init.agent_capabilities.prompt_capabilities;
    backend::Caps {
        load_thread: can_load,
        close_thread: init.agent_capabilities.session_capabilities.close.is_some(),
        history: Some(history::Caps {
            list: init.agent_capabilities.session_capabilities.list.is_some(),
            load: can_load,
            close: init.agent_capabilities.session_capabilities.close.is_some(),
            resume: init
                .agent_capabilities
                .session_capabilities
                .resume
                .is_some(),
        }),
        mode: None,
        config: None,
        prompt: prompt::Caps {
            image: prompt_caps.image,
            audio: prompt_caps.audio,
            embedded_context: prompt_caps.embedded_context,
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
    acp::NewSessionRequest::new(scope.cwd.clone())
}

pub fn session_load(session: &Session) -> acp::LoadSessionRequest {
    acp::LoadSessionRequest::new(
        session.to_string(),
        std::env::current_dir().unwrap_or_default(),
    )
}

pub fn submit(session: &Session, request: &prompt::Request) -> acp::PromptRequest {
    acp::PromptRequest::new(
        session.to_string(),
        request.parts().iter().cloned().map(content_block).collect(),
    )
}

pub fn cancel(session: &Session) -> acp::CancelNotification {
    acp::CancelNotification::new(session.to_string())
}

pub fn set_mode(session: &Session, mode: &mode::Id) -> acp::SetSessionModeRequest {
    acp::SetSessionModeRequest::new(session.to_string(), mode.to_string())
}

pub fn set_config(
    session: &Session,
    option: &config::Id,
    value: &config::ValueId,
) -> acp::SetSessionConfigOptionRequest {
    acp::SetSessionConfigOptionRequest::new(
        session.to_string(),
        option.to_string(),
        config_value(value),
    )
}

pub fn mode_set(state: &acp::SessionModeState) -> Result<mode::Set, mode::Missing> {
    let items: Vec<_> = state
        .available_modes
        .iter()
        .map(|item| mode::Item {
            id: mode::Id::new(item.id.to_string()),
            name: item.name.clone(),
            description: item.description.clone(),
        })
        .collect();
    mode::Set::new(
        items,
        mode::Selected::Current(mode::Id::new(state.current_mode_id.to_string())),
    )
}

pub fn config_state(items: &[acp::ConfigOption]) -> Result<config::State, config::Missing> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let (current, values) = config_values(&item.kind);
        out.push(config::Item::new(
            config::Id::new(item.id.to_string()),
            item.name.clone(),
            item.category.as_ref().map(config_category),
            config::Selected::Current(current),
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
    mode_set(&acp::SessionModeState::new(
        data.current_mode_id,
        items.to_vec(),
    ))
    .map(thread::Event::Mode)
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
        acp::SessionUpdate::AgentThoughtChunk(chunk) => Some(thread::Event::Content(
            thread::Content::Append(thread::NewEntry {
                turn: None,
                kind: thread::EntryKind::Thought {
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
                    .fields
                    .content
                    .as_deref()
                    .map(tool_content_locations)
                    .unwrap_or_default(),
            }),
        )),
        acp::SessionUpdate::ConfigOptionUpdate(update) => config_event(update).ok(),
        acp::SessionUpdate::CurrentModeUpdate(_) => None,
        acp::SessionUpdate::AvailableCommandsUpdate(update) => {
            Some(thread::Event::Commands(commands(update)))
        }
        acp::SessionUpdate::UsageUpdate(update) => {
            Some(thread::Event::Usage(thread::UsageUpdate {
                input_tokens: None,
                output_tokens: None,
                total_input_tokens: Some(update.used),
                total_output_tokens: Some(update.size),
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            }))
        }
        acp::SessionUpdate::SessionInfoUpdate(_) => None,
        _ => None,
    }
}

pub fn update_locations(update: &acp::SessionUpdate) -> Vec<crate::collab::Location> {
    match update {
        acp::SessionUpdate::AgentMessageChunk(chunk) => {
            content_locations(std::slice::from_ref(&chunk.content))
        }
        acp::SessionUpdate::ToolCallUpdate(update) => update
            .fields
            .content
            .as_deref()
            .map(tool_content_locations)
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
                status: match entry.status {
                    acp::PlanEntryStatus::Pending => super::super::plan::Status::Pending,
                    acp::PlanEntryStatus::InProgress => super::super::plan::Status::InProgress,
                    acp::PlanEntryStatus::Completed => super::super::plan::Status::Completed,
                    _ => super::super::plan::Status::Pending,
                },
            })
            .collect(),
    )
}

fn tool_call(info: acp::ToolCallInfo) -> tool::Call {
    tool::Call {
        id: tool::Id::new(info.tool_call_id.to_string()),
        name: info.title,
        state: tool_state(info.status),
        output: tool_output(&info.content),
        subagent: subagent_info(info.meta.as_ref()),
        sandbox: sandbox_info(info.meta.as_ref()),
    }
}

fn tool_update(update: acp::ToolCallUpdate) -> tool::Call {
    let output = update
        .fields
        .content
        .as_deref()
        .map(tool_output)
        .unwrap_or_default();
    tool::Call {
        id: tool::Id::new(update.tool_call_id.to_string()),
        name: update.fields.title.unwrap_or_else(|| "tool".to_string()),
        state: update
            .fields
            .status
            .map(tool_state)
            .unwrap_or(tool::State::Pending),
        output,
        subagent: subagent_info(update.meta.as_ref()),
        sandbox: sandbox_info(update.meta.as_ref()),
    }
}

fn tool_state(state: acp::ToolCallStatus) -> tool::State {
    match state {
        acp::ToolCallStatus::Pending => tool::State::Pending,
        acp::ToolCallStatus::InProgress => tool::State::Running,
        acp::ToolCallStatus::Completed => tool::State::Completed,
        acp::ToolCallStatus::Failed => tool::State::Failed { message: None },
        _ => tool::State::Pending,
    }
}

pub(crate) fn elicitation(req: acp::CreateElicitationRequest) -> thread::Elicitation {
    thread::Elicitation {
        id: elicitation_id(&req),
        status: thread::ElicitationStatus::Pending,
        mode: elicitation_mode(req),
    }
}

fn elicitation_mode(req: acp::CreateElicitationRequest) -> thread::ElicitationMode {
    let message = req.message;
    match req.mode {
        acp::ElicitationMode::Form(form) => thread::ElicitationMode::Form {
            message,
            fields: form_fields(form.requested_schema),
        },
        acp::ElicitationMode::Url(url) => thread::ElicitationMode::Url {
            message,
            url: url.url,
        },
        _ => thread::ElicitationMode::Form {
            message,
            fields: Vec::new(),
        },
    }
}

fn elicitation_id(req: &acp::CreateElicitationRequest) -> String {
    match &req.mode {
        acp::ElicitationMode::Url(url) => url.elicitation_id.to_string(),
        acp::ElicitationMode::Form(form) => match &form.scope {
            acp::ElicitationScope::Session(scope) => format!("session:{}", scope.session_id),
            acp::ElicitationScope::Request(scope) => format!("request:{}", scope.request_id),
            _ => "elicitation".to_string(),
        },
        _ => "elicitation".to_string(),
    }
}

fn form_fields(schema: acp::ElicitationSchema) -> Vec<thread::ElicitationField> {
    let required: HashSet<_> = schema.required.unwrap_or_default().into_iter().collect();
    schema
        .properties
        .into_iter()
        .map(|(name, property)| field_from_schema(name, property, &required))
        .collect()
}

fn field_from_schema(
    name: String,
    property: acp::ElicitationPropertySchema,
    required: &HashSet<String>,
) -> thread::ElicitationField {
    let (field_type, label, options) = match property {
        acp::ElicitationPropertySchema::String(schema) => {
            let options: Vec<thread::ElicitationOption> = schema
                .one_of
                .map(|items| {
                    items
                        .into_iter()
                        .map(|item| thread::ElicitationOption {
                            value: item.value,
                            label: item.title,
                        })
                        .collect()
                })
                .or_else(|| {
                    schema.enum_values.map(|values| {
                        values
                            .into_iter()
                            .map(|value| thread::ElicitationOption {
                                value: value.clone(),
                                label: value,
                            })
                            .collect()
                    })
                })
                .unwrap_or_default();
            let field_type = if options.is_empty() {
                thread::ElicitationFieldType::Text
            } else {
                thread::ElicitationFieldType::Select
            };
            (field_type, schema.title, options)
        }
        acp::ElicitationPropertySchema::Boolean(schema) => {
            (thread::ElicitationFieldType::Bool, schema.title, Vec::new())
        }
        acp::ElicitationPropertySchema::Array(schema) => {
            let options = match schema.items {
                acp::MultiSelectItems::Untitled(items) => items
                    .values
                    .into_iter()
                    .map(|value| thread::ElicitationOption {
                        value: value.clone(),
                        label: value,
                    })
                    .collect(),
                acp::MultiSelectItems::Titled(items) => items
                    .options
                    .into_iter()
                    .map(|item| thread::ElicitationOption {
                        value: item.value,
                        label: item.title,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            (thread::ElicitationFieldType::Select, schema.title, options)
        }
        acp::ElicitationPropertySchema::Number(schema) => {
            (thread::ElicitationFieldType::Text, schema.title, Vec::new())
        }
        acp::ElicitationPropertySchema::Integer(schema) => {
            (thread::ElicitationFieldType::Text, schema.title, Vec::new())
        }
        _ => (thread::ElicitationFieldType::Text, None, Vec::new()),
    };
    thread::ElicitationField {
        name: name.clone(),
        field_type,
        label,
        required: required.contains(&name),
        options,
    }
}

pub(crate) fn content_block(part: prompt::Part) -> acp::ContentBlock {
    match part {
        prompt::Part::Text(text) => acp::ContentBlock::Text(acp::TextContent::new(text)),
        prompt::Part::Image(image) => acp::ContentBlock::Image(acp::ImageContent::new(
            STANDARD.encode(image.data),
            image.mime,
        )),
        prompt::Part::Audio(audio) => acp::ContentBlock::Audio(acp::AudioContent::new(
            STANDARD.encode(audio.data),
            audio.mime,
        )),
        prompt::Part::Link(link) => acp::ContentBlock::ResourceLink(acp::ResourceLink::new(
            link.label.unwrap_or_else(|| link.uri.clone()),
            link.uri,
        )),
        prompt::Part::Resource(resource) => {
            let body = match (resource.text, resource.data) {
                (Some(text), _) => acp::EmbeddedResourceResource::TextResourceContents(
                    acp::TextResourceContents::new(text, resource.uri).mime_type(resource.mime),
                ),
                (None, Some(data)) => acp::EmbeddedResourceResource::BlobResourceContents(
                    acp::BlobResourceContents::new(STANDARD.encode(data), resource.uri)
                        .mime_type(resource.mime),
                ),
                (None, None) => acp::EmbeddedResourceResource::TextResourceContents(
                    acp::TextResourceContents::new(String::new(), resource.uri)
                        .mime_type(resource.mime),
                ),
            };
            acp::ContentBlock::Resource(acp::EmbeddedResource::new(body))
        }
    }
}

fn content_text(block: acp::ContentBlock) -> String {
    match block {
        acp::ContentBlock::Text(text) => text.text,
        acp::ContentBlock::Image(_) => "<image>".to_string(),
        acp::ContentBlock::Audio(_) => "<audio>".to_string(),
        acp::ContentBlock::ResourceLink(link) => link.title.unwrap_or(link.name),
        acp::ContentBlock::Resource(resource) => match resource.resource {
            acp::EmbeddedResourceResource::TextResourceContents(resource) => resource.text,
            acp::EmbeddedResourceResource::BlobResourceContents(resource) => resource.uri,
            _ => "<resource>".to_string(),
        },
        _ => String::new(),
    }
}

fn tool_output(blocks: &[acp::ToolCallContent]) -> String {
    blocks
        .iter()
        .map(tool_content_text)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_content_text(block: &acp::ToolCallContent) -> String {
    match block {
        acp::ToolCallContent::Content(content) => content_text(content.content.clone()),
        acp::ToolCallContent::Diff(diff) => diff.new_text.clone(),
        acp::ToolCallContent::Terminal(term) => format!("terminal:{}", term.terminal_id),
        _ => String::new(),
    }
}

fn tool_content_locations(blocks: &[acp::ToolCallContent]) -> Vec<crate::collab::Location> {
    blocks
        .iter()
        .flat_map(|block| match block {
            acp::ToolCallContent::Content(content) => {
                content_location(&content.content).into_iter().collect()
            }
            acp::ToolCallContent::Diff(diff) => vec![crate::collab::Location::new(
                diff.path.clone(),
                crate::collab::location::Source::Tool,
            )],
            _ => Vec::new(),
        })
        .collect()
}

fn content_locations(blocks: &[acp::ContentBlock]) -> Vec<crate::collab::Location> {
    blocks.iter().filter_map(content_location).collect()
}

fn content_location(block: &acp::ContentBlock) -> Option<crate::collab::Location> {
    let uri = match block {
        acp::ContentBlock::ResourceLink(link) => link.uri.as_str(),
        acp::ContentBlock::Resource(resource) => match &resource.resource {
            acp::EmbeddedResourceResource::TextResourceContents(resource) => resource.uri.as_str(),
            acp::EmbeddedResourceResource::BlobResourceContents(resource) => resource.uri.as_str(),
            _ => return None,
        },
        _ => return None,
    };
    let url = url::Url::parse(uri).ok()?;
    let uri = Uri::try_from(url).ok()?;
    Some(crate::collab::Location::new(
        uri.as_path()?.to_path_buf(),
        crate::collab::location::Source::Tool,
    ))
}

fn config_values(kind: &acp::SessionConfigKind) -> (config::ValueId, Vec<config::Value>) {
    match kind {
        acp::SessionConfigKind::Select(select) => (
            config::ValueId::new(select.current_value.to_string()),
            select_options(&select.options),
        ),
        acp::SessionConfigKind::Boolean(boolean) => (
            config::ValueId::new(boolean.current_value.to_string()),
            vec![
                config::Value {
                    id: config::ValueId::new("true"),
                    label: "On".to_string(),
                    description: None,
                },
                config::Value {
                    id: config::ValueId::new("false"),
                    label: "Off".to_string(),
                    description: None,
                },
            ],
        ),
        _ => (config::ValueId::new(""), Vec::new()),
    }
}

fn select_options(options: &acp::SessionConfigSelectOptions) -> Vec<config::Value> {
    match options {
        acp::SessionConfigSelectOptions::Ungrouped(options) => {
            options.iter().map(select_value).collect()
        }
        acp::SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter().map(select_value))
            .collect(),
        _ => Vec::new(),
    }
}

fn select_value(value: &acp::SessionConfigSelectOption) -> config::Value {
    config::Value {
        id: config::ValueId::new(value.value.to_string()),
        label: value.name.clone(),
        description: value.description.clone(),
    }
}

fn config_category(category: &acp::SessionConfigOptionCategory) -> String {
    match category {
        acp::SessionConfigOptionCategory::Mode => "mode".to_string(),
        acp::SessionConfigOptionCategory::Model => "model".to_string(),
        acp::SessionConfigOptionCategory::ModelConfig => "model_config".to_string(),
        acp::SessionConfigOptionCategory::ThoughtLevel => "thought_level".to_string(),
        acp::SessionConfigOptionCategory::Other(value) => value.clone(),
        _ => "other".to_string(),
    }
}

pub fn config_value(value: &config::ValueId) -> acp::SessionConfigOptionValue {
    match value.as_str() {
        "true" => acp::SessionConfigOptionValue::boolean(true),
        "false" => acp::SessionConfigOptionValue::boolean(false),
        value => acp::SessionConfigOptionValue::value_id(value.to_string()),
    }
}

fn commands(update: acp::AvailableCommandsUpdateData) -> Vec<thread::Command> {
    update
        .available_commands
        .into_iter()
        .map(|command| thread::Command {
            name: command.name,
            description: Some(command.description),
            category: thread::CommandCategory::Native,
            arguments: command
                .input
                .map(|input| match input {
                    acp::AvailableCommandInput::Unstructured(input) => {
                        vec![thread::CommandArgument {
                            name: "input".to_string(),
                            description: Some(input.hint),
                            required: false,
                        }]
                    }
                    _ => Vec::new(),
                })
                .unwrap_or_default(),
        })
        .collect()
}

fn subagent_info(meta: Option<&acp::Meta>) -> Option<tool::SubagentSessionInfo> {
    let info = meta?.get("subagent_session_info")?;
    Some(tool::SubagentSessionInfo {
        session_id: info
            .get("session_id")
            .and_then(serde_json::Value::as_str)?
            .to_string(),
        message_start_index: info
            .get("message_start_index")
            .and_then(serde_json::Value::as_u64),
        message_end_index: info
            .get("message_end_index")
            .and_then(serde_json::Value::as_u64),
    })
}

fn sandbox_info(meta: Option<&acp::Meta>) -> Option<tool::SandboxAuthorization> {
    let sandbox = meta?.get("sandbox_authorization")?;
    let write_paths = sandbox
        .get("write_paths")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(PathBuf::from)
        .collect();
    let network_hosts = sandbox
        .get("network_hosts")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_string)
        .collect();
    Some(tool::SandboxAuthorization {
        command: sandbox
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        network_hosts,
        allow_fs_write_all: sandbox
            .get("allow_fs_write_all")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        write_paths,
        unsandboxed: sandbox
            .get("unsandboxed")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        reason: sandbox
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_prompt_caps() {
        let init = acp::InitializeResponse::new(acp::ProtocolVersion::V1).agent_capabilities(
            acp::AgentCapabilities::new()
                .load_session(true)
                .prompt_capabilities(
                    acp::PromptCapabilities::new()
                        .image(true)
                        .audio(false)
                        .embedded_context(true),
                ),
        );
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
        let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            "tool-1",
            acp::ToolCallUpdateFields::new().content(vec![acp::ToolCallContent::from(
                acp::ContentBlock::ResourceLink(acp::ResourceLink::new("example", uri)),
            )]),
        ));

        let locations = update_locations(&update);
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].path, path);
        assert_eq!(locations[0].source, crate::collab::location::Source::Tool);
    }

    #[test]
    fn thread_event_keeps_chunk_locations() {
        let path = std::env::temp_dir().join("example.ts");
        let uri = url::Url::from_file_path(&path).unwrap().to_string();
        let update = acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
            acp::ContentBlock::Resource(acp::EmbeddedResource::new(
                acp::EmbeddedResourceResource::TextResourceContents(
                    acp::TextResourceContents::new("example", uri).mime_type("text/plain"),
                ),
            )),
        ));

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
        let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
            "tool-1",
            acp::ToolCallUpdateFields::new()
                .status(acp::ToolCallStatus::InProgress)
                .content(vec![acp::ToolCallContent::from(acp::ContentBlock::Text(
                    acp::TextContent::new("partial output"),
                ))]),
        ));

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
