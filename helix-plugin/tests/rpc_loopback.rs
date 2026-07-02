use helix_plugin::contract::{events, ContractError, DocumentHandle, SubscriptionHandle};
use helix_plugin::rpc::{FrameCodec, HostRequest, HostResponse, PluginRequest, Rpc};
use helix_plugin::PluginConfig;
use std::io::{BufReader, BufWriter};
use std::num::NonZeroU64;
use std::process::{Command, Stdio};

type ToChild = Rpc<HostRequest, HostResponse>;
type FromChild = Rpc<PluginRequest, HostResponse>;

fn document(raw: u64) -> DocumentHandle {
    DocumentHandle::from_raw(NonZeroU64::new(raw).unwrap())
}

fn subscription(raw: u64) -> SubscriptionHandle {
    SubscriptionHandle::from_raw(NonZeroU64::new(raw).unwrap())
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
helix.events.subscribe("host_ready", function()
  local doc = helix.workspace.focused_document()
  local ok, err = pcall(function()
    helix.documents.open("contract-error")
  end)
  local code = "missing_error_table"
  if not ok and type(err) == "table" then
    code = err.code
  end
  helix.ui.info("done:" .. tostring(doc:id()) .. ":" .. code)
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
    while !subscribed {
        match codec.read_sync::<FromChild, _>(&mut input).unwrap() {
            Rpc::Request {
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
            Rpc::Request {
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
                    api_version: helix_plugin::contract::metadata::API_VERSION,
                })),
            },
        )
        .unwrap();

    let mut saw_query = false;
    let mut saw_mutation = false;
    let mut saw_error_table = false;
    while !(saw_query && saw_mutation && saw_error_table) {
        match codec.read_sync::<FromChild, _>(&mut input).unwrap() {
            Rpc::Request {
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
            Rpc::Request {
                id,
                body: PluginRequest::OpenDocument(req),
            } => {
                saw_mutation = true;
                assert_eq!(req.path, "contract-error");
                codec
                    .write_sync(
                        &mut output,
                        &ToChild::Response {
                            id,
                            result: Err(ContractError::invalid_request("blocked by mock")),
                        },
                    )
                    .unwrap();
            }
            Rpc::Request {
                id,
                body: PluginRequest::Notify(req),
            } => {
                saw_error_table = req.message == "done:1:invalid_request";
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
