use helix_plugin::rpc::{Frame, FrameCodec, HostRequest, HostResponse};
use helix_plugin::PluginConfig;
use std::io::BufWriter;
use std::process::{Command, Stdio};

#[test]
fn shipped_editor_binary_runs_the_private_plugin_host_mode() {
    let plugin_dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_dhx"))
        .arg("--plugin-host")
        .arg("--plugin-dir")
        .arg(plugin_dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut input = BufWriter::new(child.stdin.take().unwrap());
    let mut codec = FrameCodec::new();
    codec
        .write_sync(
            &mut input,
            &Frame::<HostRequest, HostResponse>::Notify {
                body: HostRequest::Init {
                    metadata: Default::default(),
                    config: PluginConfig::default(),
                },
            },
        )
        .unwrap();
    codec
        .write_sync(
            &mut input,
            &Frame::<HostRequest, HostResponse>::Notify {
                body: HostRequest::Shutdown,
            },
        )
        .unwrap();
    drop(input);

    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "plugin host failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
