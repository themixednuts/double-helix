use crate::assistant::BackendDriver;
use helix_runtime::{Runtime, Sender as RuntimeSender};

use super::super::Editor;

impl Editor {
    pub fn assistant_update_tx(&self) -> RuntimeSender<crate::assistant::backend::Update> {
        self.assistant_runtime.updates_tx.clone()
    }

    pub fn assistant_backend(
        &self,
        backend: &crate::assistant::backend::Id,
    ) -> Option<crate::assistant::BackendHandle> {
        self.assistant_runtime.backends.get(backend).cloned()
    }

    pub fn assistant_terminals(&self) -> std::sync::Arc<helix_acp::TerminalManager> {
        self.assistant_services.terminals.clone()
    }

    pub fn assistant_runtime(&self) -> Runtime {
        self.runtime.clone()
    }

    pub fn assistant_agent(
        &self,
        backend: &crate::assistant::backend::Id,
    ) -> Option<crate::editor::AgentConfig> {
        self.config()
            .agents
            .iter()
            .find(|agent| agent.command == backend.as_ref())
            .cloned()
    }

    pub fn cache_assistant_backend(&mut self, handle: crate::assistant::BackendHandle) {
        self.assistant_runtime
            .backends
            .insert(handle.id.clone(), handle);
    }

    pub fn spawn_assistant_backend(
        &mut self,
        command: String,
        args: Vec<String>,
        mcp_servers: Vec<helix_acp::types::McpServer>,
    ) -> anyhow::Result<crate::assistant::BackendHandle> {
        let runtime = self.assistant_runtime();
        let cwd = std::env::current_dir().unwrap_or_default();
        let config = helix_acp::client::AgentConfig {
            command: command.clone(),
            args,
            env: Vec::new(),
            cwd: cwd.clone(),
            mcp_servers,
            timeout_secs: 120,
        };
        let client_info = helix_acp::types::Implementation::new("helix", env!("CARGO_PKG_VERSION"))
            .title("Helix Editor");

        let driver = crate::assistant::acp::Driver::new(config, client_info);
        let handle = driver.spawn(
            &runtime,
            crate::assistant::host::local_set(self),
            self.assistant_update_tx(),
            crate::assistant::backend::Connect {
                scope: crate::assistant::thread::Scope::new(cwd),
                context_servers: Vec::new(),
            },
        )?;
        self.cache_assistant_backend(handle.clone());
        Ok(handle)
    }

    pub fn ensure_assistant_backend(
        &mut self,
        backend: &crate::assistant::backend::Id,
    ) -> Option<crate::assistant::BackendHandle> {
        if let Some(handle) = self.assistant_backend(backend) {
            return Some(handle);
        }

        let agent = self.assistant_agent(backend)?;
        self.spawn_assistant_backend(agent.command, agent.args, agent.mcp_servers)
            .ok()
    }

    pub fn connect_assistant_backend(
        &mut self,
        command: String,
        args: Vec<String>,
    ) -> anyhow::Result<(
        crate::assistant::backend::Id,
        Vec<crate::assistant::effect::Effect>,
    )> {
        let cwd = std::env::current_dir().unwrap_or_default();
        let handle = self.spawn_assistant_backend(command, args, Vec::new())?;
        let effects =
            self.new_assistant_thread(handle.id.clone(), crate::assistant::thread::Scope::new(cwd));
        Ok((handle.id, effects))
    }
}
