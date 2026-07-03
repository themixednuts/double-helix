//! Run an ACP agent, call initialize + session/new, and print the response.
//!
//! Usage:
//!   cargo run -p helix-acp --example call_session_new -- npm.cmd exec --yes @zed-industries/claude-agent-acp@0.20.2
//!   cargo run -p helix-acp --example call_session_new -- claude-agent-acp
//!   cargo run -p helix-acp --example call_session_new -- cursor agent acp

use helix_acp::{
    client::{AcpAgent, AgentConfig},
    jsonrpc, ClientCapabilities, Implementation, NewSessionResponse,
};
use serde_json::json;
use std::path::PathBuf;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let command = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "npm.cmd".to_string());
    let args: Vec<String> = std::env::args().skip(2).collect();

    let config = AgentConfig {
        command: command.clone(),
        args,
        env: vec![],
        cwd: PathBuf::from("."),
        mcp_servers: vec![],
        timeout_secs: 120,
    };

    eprintln!("Starting agent: {} ...", config.command);
    let (agent, mut incoming_rx) = AcpAgent::start_standalone(&config)?;

    // Drain agent requests so the transport doesn't block; reply with null.
    let agent_drain = agent.clone();
    tokio::spawn(async move {
        while let Some((_aid, call)) = incoming_rx.recv().await {
            if let jsonrpc::Call::MethodCall(m) = call {
                eprintln!("[agent request] {} -> reply null", m.method);
                agent_drain.reply(m.id, json!(null));
            }
        }
    });

    let client_info = Implementation::new("call_session_new", "0.1").title("Example");
    let caps = ClientCapabilities::default();

    eprintln!("Calling initialize ...");
    agent
        .clone()
        .initialize(client_info, caps)
        .await
        .map_err(|e| format!("initialize: {e}"))?;
    eprintln!("Initialize OK");

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    eprintln!("Calling session/new (cwd={}) ...", cwd.display());
    let resp: NewSessionResponse = agent
        .new_session(cwd)
        .await
        .map_err(|e| format!("session/new: {e}"))?;

    eprintln!("session/new OK\n");
    println!("{}", serde_json::to_string_pretty(&resp)?);
    // Exit immediately so we don't wait for the long-running agent process
    std::process::exit(0);
}
