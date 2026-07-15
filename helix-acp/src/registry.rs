//! Registry for managing multiple ACP agent connections.
//!
//! Follows the same pattern as `helix-lsp::Registry`.

use crate::{
    client::{AcpAgent, AgentConfig, IncomingReceiver},
    jsonrpc, AgentId,
};
use futures_util::Stream;
use slotmap::SlotMap;
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio_stream::StreamMap;

/// Incoming calls from every agent currently owned by a [`Registry`].
pub struct RegistryIncoming {
    streams: StreamMap<AgentId, IncomingReceiver>,
}

impl RegistryIncoming {
    fn new() -> Self {
        Self {
            streams: StreamMap::new(),
        }
    }

    fn insert(&mut self, id: AgentId, incoming: IncomingReceiver) {
        debug_assert!(self.streams.insert(id, incoming).is_none());
    }

    fn remove(&mut self, id: AgentId) {
        self.streams.remove(&id);
    }

    fn clear(&mut self) {
        self.streams.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.streams.is_empty()
    }
}

impl Stream for RegistryIncoming {
    type Item = (AgentId, jsonrpc::Call);

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.streams).poll_next(cx) {
            Poll::Ready(Some((registered_id, (reported_id, call)))) => {
                debug_assert_eq!(registered_id, reported_id);
                Poll::Ready(Some((registered_id, call)))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Manages all active ACP agent connections.
pub struct Registry {
    inner: SlotMap<AgentId, Arc<AcpAgent>>,
    incoming: RegistryIncoming,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            inner: SlotMap::default(),
            incoming: RegistryIncoming::new(),
        }
    }

    /// Launch a new agent and add it to the registry.
    ///
    /// Returns the agent ID and a reference to the agent.
    pub fn launch(&mut self, config: &AgentConfig) -> crate::Result<(AgentId, Arc<AcpAgent>)> {
        let id = self.inner.try_insert_with_key(|id| {
            AcpAgent::start(id, config).map(|(agent, incoming_rx)| {
                self.incoming.insert(id, incoming_rx);
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

    /// Remove an agent from the registry, terminating it before returning.
    pub async fn remove(&mut self, id: AgentId) -> crate::Result<Option<Arc<AcpAgent>>> {
        log::warn!("[acp_transport] removing agent from registry id={:?}", id);
        let Some(agent) = self.inner.remove(id) else {
            return Ok(None);
        };
        self.incoming.remove(id);

        agent.shutdown().await?;
        Ok(Some(agent))
    }

    /// Close and reap all agents.
    ///
    /// The registry is drained before shutdown begins so failures cannot leave
    /// stale entries behind. Every agent is attempted and repeated calls are
    /// no-ops.
    pub async fn close_all(&mut self) -> crate::Result<()> {
        log::warn!(
            "[acp_transport] closing all agents count={}",
            self.inner.len()
        );
        let agents = std::mem::take(&mut self.inner);
        self.incoming.clear();
        let mut first_error = None;

        for (_, agent) in &agents {
            agent.request_shutdown();
        }

        for (id, agent) in agents {
            if let Err(error) = agent.shutdown().await {
                log::warn!(
                    "[acp_transport] failed to close agent id={:?} err={}",
                    id,
                    error
                );
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
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
    pub fn incoming(&mut self) -> &mut RegistryIncoming {
        &mut self.incoming
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        for (_, agent) in &self.inner {
            agent.request_shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::tests::{child_agent_config, unique_test_path, wait_for_file};
    use std::{fs, time::Duration};

    #[tokio::test]
    async fn remove_terminates_agent_while_returned_arc_is_retained() {
        let started = unique_test_path("registry-remove-started");
        let late_marker = unique_test_path("registry-remove-late");
        let config = child_agent_config("hang", &started, Some(&late_marker));
        let mut registry = Registry::new();
        let (id, agent) = registry.launch(&config).expect("launch agent");
        wait_for_file(&started).await;

        let removed = registry
            .remove(id)
            .await
            .expect("remove agent")
            .expect("registered agent");
        assert!(Arc::ptr_eq(&agent, &removed));
        assert!(
            registry.incoming().is_empty(),
            "registry retained the removed agent's incoming stream"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;

        assert!(
            !late_marker.exists(),
            "registry removal left the retained agent process running"
        );
        assert!(registry.is_empty());
        let _ = fs::remove_file(started);
        let _ = fs::remove_file(late_marker);
    }

    #[tokio::test]
    async fn close_all_reaps_running_and_exited_agents_and_is_idempotent() {
        let exited_started = unique_test_path("registry-exited-started");
        let running_started = unique_test_path("registry-running-started");
        let mut registry = Registry::new();
        let (_, exited) = registry
            .launch(&child_agent_config("exit", &exited_started, None))
            .expect("launch exiting agent");
        let (_, running) = registry
            .launch(&child_agent_config("hang", &running_started, None))
            .expect("launch running agent");
        wait_for_file(&exited_started).await;
        wait_for_file(&running_started).await;
        tokio::time::timeout(Duration::from_secs(2), exited.wait())
            .await
            .expect("exiting agent did not exit")
            .expect("wait for naturally exited agent");

        tokio::time::timeout(Duration::from_secs(2), registry.close_all())
            .await
            .expect("registry close timed out")
            .expect("close registry");
        registry.close_all().await.expect("repeat registry close");

        assert!(registry.is_empty());
        assert!(registry.incoming().is_empty());
        running
            .wait()
            .await
            .expect("wait for running agent shutdown");
        let _ = fs::remove_file(exited_started);
        let _ = fs::remove_file(running_started);
    }
}
