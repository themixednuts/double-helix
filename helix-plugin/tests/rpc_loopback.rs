use helix_plugin::rpc::{Frame, FrameCodec, HostRequest, HostResponse, PluginRequest};
use helix_plugin::PluginConfig;
use helix_plugin_api::{
    events, CommandDescriptor, CommandFlagDescriptor, CommandKind, CommandScope,
    CommandSignatureDescriptor, ContractError, DocumentHandle, DynamicValue, KeymapHandle,
    PluginTaskRequest, PluginTaskResult, SubscriptionHandle, SyntaxCapture,
};
use std::io::{BufReader, BufWriter};
use std::num::NonZeroU64;
use std::process::{Command, Stdio};

type ToChild = Frame<HostRequest, HostResponse>;
type FromChild = Frame<PluginRequest, HostResponse>;

fn document(raw: u64) -> DocumentHandle {
    DocumentHandle::from_raw(NonZeroU64::new(raw).unwrap())
}

fn subscription(raw: u64) -> SubscriptionHandle {
    SubscriptionHandle::from_raw(NonZeroU64::new(raw).unwrap())
}

fn keymap(raw: u64) -> KeymapHandle {
    KeymapHandle::from_raw(NonZeroU64::new(raw).unwrap())
}

#[test]
fn plugin_host_loopback_dispatches_event_and_host_calls() {
    let temp = tempfile::TempDir::new().unwrap();
    let plugin_root = temp.path().join("plugins");
    let plugin_dir = plugin_root.join("loopback");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(
        plugin_dir.join("init.lua"),
        r#"
helix.keymaps.register({
  mode = "normal",
  scope = { language = "rust" },
  bindings = { { keys = { "F24" }, command = ":write" } },
})

helix.events.subscribe("host_ready", function()
  local doc = helix.workspace.focused_document()
  local command = helix.commands.get("w")
  local clients = helix.lsp.get_clients()
  helix.async(function()
    local captures = helix.syntax.query(doc, "(identifier) @name", { max_captures = 10 })
    local ok, err = pcall(function()
      helix.documents.open("contract-error")
    end)
    local code = "missing_error_table"
    if not ok and type(err) == "table" then
      code = err.code
    end
    local reply = helix.lsp.call(doc, "test/echo", { text = "ping", nested = { 1, true } }, { server = "mock-lsp" })
    helix.commands.execute("write", { "output file.rs" })
    helix.ui.info("done:" .. tostring(doc:id()) .. ":" .. code .. ":" .. command.name .. ":" .. tostring(command.signature.max_positionals) .. ":" .. captures[1].name .. ":" .. clients[1].name .. ":" .. reply.answer)
  end)
end)
"#,
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_helix-plugin-host"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = BufReader::new(child.stdout.take().unwrap());
    let mut output = BufWriter::new(child.stdin.take().unwrap());
    let mut codec = FrameCodec::new();

    codec
        .write_sync(
            &mut output,
            &ToChild::Notify {
                body: HostRequest::Init {
                    metadata: Default::default(),
                    config: PluginConfig {
                        plugin_dirs: vec![plugin_root],
                        plugins: vec![],
                        hosts: vec![],
                        ..Default::default()
                    },
                },
            },
        )
        .unwrap();

    let mut subscribed = false;
    let mut registered_keymap = false;
    while !(subscribed && registered_keymap) {
        match codec.read_sync::<FromChild, _>(&mut input).unwrap() {
            Frame::Request {
                id,
                body: PluginRequest::Subscribe { kind, .. },
            } => {
                assert_eq!(kind, events::EventKind::HostReady);
                codec
                    .write_sync(
                        &mut output,
                        &ToChild::Response {
                            id,
                            result: Ok(HostResponse::SubscriptionHandle(subscription(1))),
                        },
                    )
                    .unwrap();
                subscribed = true;
            }
            Frame::Request {
                id,
                body: PluginRequest::RegisterKeymap { definition, .. },
            } => {
                assert_eq!(definition.scope.language.as_deref(), Some("rust"));
                assert_eq!(definition.bindings[0].keys, ["F24"]);
                codec
                    .write_sync(
                        &mut output,
                        &ToChild::Response {
                            id,
                            result: Ok(HostResponse::KeymapHandle(keymap(1))),
                        },
                    )
                    .unwrap();
                registered_keymap = true;
            }
            Frame::Request {
                id,
                body: PluginRequest::ApiMetadata,
            } => {
                codec
                    .write_sync(
                        &mut output,
                        &ToChild::Response {
                            id,
                            result: Ok(HostResponse::ApiMetadata(Default::default())),
                        },
                    )
                    .unwrap();
            }
            other => panic!("unexpected init frame: {other:?}"),
        }
    }

    codec
        .write_sync(
            &mut output,
            &ToChild::Notify {
                body: HostRequest::Event(events::PluginEvent::HostReady(events::HostReadyEvent {
                    api_version: helix_plugin_api::metadata::API_VERSION,
                })),
            },
        )
        .unwrap();

    let mut saw_query = false;
    let mut saw_command_catalog = false;
    let mut saw_syntax_query = false;
    let mut saw_lsp_clients = false;
    let mut saw_lsp_call = false;
    let mut saw_command_run = false;
    let mut saw_mutation = false;
    let mut saw_error_table = false;
    while !(saw_query
        && saw_command_catalog
        && saw_syntax_query
        && saw_lsp_clients
        && saw_lsp_call
        && saw_command_run
        && saw_mutation
        && saw_error_table)
    {
        match codec.read_sync::<FromChild, _>(&mut input).unwrap() {
            Frame::Request {
                id,
                body: PluginRequest::FocusedDocument,
            } => {
                saw_query = true;
                codec
                    .write_sync(
                        &mut output,
                        &ToChild::Response {
                            id,
                            result: Ok(HostResponse::OptionDocumentHandle(Some(document(1)))),
                        },
                    )
                    .unwrap();
            }
            Frame::Request {
                id,
                body:
                    PluginRequest::StartTask {
                        operation,
                        request: PluginTaskRequest::OpenDocument(req),
                        ..
                    },
            } => {
                saw_mutation = true;
                assert_eq!(req.path, "contract-error");
                codec
                    .write_sync(
                        &mut output,
                        &ToChild::Notify {
                            body: HostRequest::TaskCompleted {
                                operation,
                                result: Err(ContractError::invalid_request("blocked by mock")),
                            },
                        },
                    )
                    .unwrap();
                codec
                    .write_sync(
                        &mut output,
                        &ToChild::Response {
                            id,
                            result: Ok(HostResponse::Unit),
                        },
                    )
                    .unwrap();
            }
            Frame::Request {
                id,
                body:
                    PluginRequest::StartTask {
                        operation,
                        request: PluginTaskRequest::SyntaxQuery(req),
                        ..
                    },
            } => {
                saw_syntax_query = true;
                assert_eq!(req.document, document(1));
                assert_eq!(req.max_captures, 10);
                codec
                    .write_sync(
                        &mut output,
                        &ToChild::Response {
                            id,
                            result: Ok(HostResponse::Unit),
                        },
                    )
                    .unwrap();
                codec
                    .write_sync(
                        &mut output,
                        &ToChild::Notify {
                            body: HostRequest::TaskCompleted {
                                operation,
                                result: Ok(PluginTaskResult::SyntaxCaptures(vec![SyntaxCapture {
                                    name: "name".into(),
                                    kind: "identifier".into(),
                                    start: helix_plugin_api::snapshots::Position {
                                        line: 0,
                                        column: 0,
                                    },
                                    end: helix_plugin_api::snapshots::Position {
                                        line: 0,
                                        column: 4,
                                    },
                                }])),
                            },
                        },
                    )
                    .unwrap();
            }
            Frame::Request {
                id,
                body: PluginRequest::CommandCatalog,
            } => {
                saw_command_catalog = true;
                codec
                    .write_sync(
                        &mut output,
                        &ToChild::Response {
                            id,
                            result: Ok(HostResponse::CommandCatalog(vec![CommandDescriptor {
                                name: "write".into(),
                                aliases: vec!["w".into()],
                                doc: "Write changes to disk".into(),
                                arguments: Vec::new(),
                                signature: Some(CommandSignatureDescriptor {
                                    min_positionals: 0,
                                    max_positionals: Some(1),
                                    raw_after: None,
                                    flags: vec![CommandFlagDescriptor {
                                        name: "no-format".into(),
                                        alias: None,
                                        doc: "Skip formatting".into(),
                                        takes_value: false,
                                        values: Vec::new(),
                                    }],
                                }),
                                kind: CommandKind::Typable,
                                scope: CommandScope::Frontend,
                            }])),
                        },
                    )
                    .unwrap();
            }
            Frame::Request {
                id,
                body: PluginRequest::LanguageServers,
            } => {
                saw_lsp_clients = true;
                codec
                    .write_sync(
                        &mut output,
                        &ToChild::Response {
                            id,
                            result: Ok(HostResponse::LanguageServers(vec![
                                helix_plugin_api::snapshots::LanguageServerSnapshot {
                                    id: "7".into(),
                                    name: "mock-lsp".into(),
                                },
                            ])),
                        },
                    )
                    .unwrap();
            }
            Frame::Request {
                id,
                body:
                    PluginRequest::StartTask {
                        operation,
                        request: PluginTaskRequest::LspCall(req),
                        ..
                    },
            } => {
                saw_lsp_call = true;
                assert_eq!(req.document, document(1));
                assert_eq!(req.server.as_deref(), Some("mock-lsp"));
                assert_eq!(req.method, "test/echo");
                let DynamicValue::Object(params) = req.params else {
                    panic!("expected object params")
                };
                assert_eq!(
                    params.get("text").and_then(DynamicValue::as_str),
                    Some("ping")
                );
                codec
                    .write_sync(
                        &mut output,
                        &ToChild::Response {
                            id,
                            result: Ok(HostResponse::Unit),
                        },
                    )
                    .unwrap();
                codec
                    .write_sync(
                        &mut output,
                        &ToChild::Notify {
                            body: HostRequest::TaskCompleted {
                                operation,
                                result: Ok(PluginTaskResult::Value(DynamicValue::Object(
                                    [("answer".into(), DynamicValue::String("pong".into()))]
                                        .into_iter()
                                        .collect(),
                                ))),
                            },
                        },
                    )
                    .unwrap();
            }
            Frame::Request {
                id,
                body:
                    PluginRequest::StartTask {
                        operation,
                        request: PluginTaskRequest::RunCommand(req),
                        ..
                    },
            } => {
                saw_command_run = true;
                assert_eq!(req.name, "write");
                assert_eq!(req.args, ["output file.rs"]);
                codec
                    .write_sync(
                        &mut output,
                        &ToChild::Response {
                            id,
                            result: Ok(HostResponse::Unit),
                        },
                    )
                    .unwrap();
                codec
                    .write_sync(
                        &mut output,
                        &ToChild::Notify {
                            body: HostRequest::TaskCompleted {
                                operation,
                                result: Ok(PluginTaskResult::Unit),
                            },
                        },
                    )
                    .unwrap();
            }
            Frame::Request {
                id,
                body: PluginRequest::Notify(req),
            } => {
                saw_error_table =
                    req.message == "done:1:invalid_request:write:1:name:mock-lsp:pong";
                codec
                    .write_sync(
                        &mut output,
                        &ToChild::Response {
                            id,
                            result: Ok(HostResponse::Unit),
                        },
                    )
                    .unwrap();
            }
            other => panic!("unexpected event frame: {other:?}"),
        }
    }

    codec
        .write_sync(
            &mut output,
            &ToChild::Notify {
                body: HostRequest::Shutdown,
            },
        )
        .unwrap();
    assert!(child.wait().unwrap().success());
}
