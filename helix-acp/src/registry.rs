//! Registry for managing multiple ACP agent connections.
//!
//! Follows the same pattern as `helix-lsp::Registry`.

use crate::{
    client::{AcpAgent, AgentConfig},
    jsonrpc, AgentId,
};
use slotmap::SlotMap;
use std::sync::Arc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use futures_util::stream::select_all::SelectAll;

/// Manages all active ACP agent connections.
pub struct Registry {
    inner: SlotMap<AgentId, Arc<AcpAgent>>,
    incoming: SelectAll<UnboundedReceiverStream<(AgentId, jsonrpc::Call)>>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            inner: SlotMap::default(),
            incoming: SelectAll::new(),
        }
    }

    /// Launch a new agent and add it to the registry.
    ///
    /// Returns the agent ID and a reference to the agent.
    pub fn launch(&mut self, config: &AgentConfig) -> crate::Result<(AgentId, Arc<AcpAgent>)> {
        let id = self.inner.try_insert_with_key(|id| {
            AcpAgent::start(id, config).map(|(agent, incoming_rx)| {
                self.incoming
                    .push(UnboundedReceiverStream::new(incoming_rx));
                agent
            })
        })?;

        let agent = self.inner[id].clone();
        log::info!("Launched ACP agent '{}' (id={:?})", config.command, id);
        Ok((id, agent))
    }

    /// Get an agent by ID.
    pub fn get(&self, id: AgentId) -> Option<&Arc<AcpAgent>> {
        self.inner.get(id)
    }

    /// Remove an agent from the registry.
    pub fn remove(&mut self, id: AgentId) -> Option<Arc<AcpAgent>> {
        log::warn!("[acp_transport] removing agent from registry id={:?}", id);
        self.inner.remove(id)
    }

    /// Close all agents. Drops each agent, which kills the child process via `kill_on_drop`.
    /// Call this during application shutdown to ensure clean process termination.
    pub fn close_all(&mut self) {
        log::warn!(
            "[acp_transport] closing all agents count={}",
            self.inner.len()
        );
        self.inner.clear();
    }

    /// Iterate over all agents.
    pub fn iter(&self) -> impl Iterator<Item = (AgentId, &Arc<AcpAgent>)> {
        self.inner.iter()
    }

    /// Number of active agents.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Get a mutable reference to the incoming message stream.
    ///
    /// This stream yields `(AgentId, Call)` pairs from all connected agents.
    /// The caller should poll this in a select loop (like `Application::handle_acp_message`).
    pub fn incoming(
        &mut self,
    ) -> &mut SelectAll<UnboundedReceiverStream<(AgentId, jsonrpc::Call)>> {
        &mut self.incoming
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}
