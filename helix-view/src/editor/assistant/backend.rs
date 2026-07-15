use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use crate::assistant::BackendDriver;
use base64::Engine as _;
use helix_runtime::{Runtime, Sender as RuntimeSender};
use sha2::{Digest, Sha256};

use super::super::{AgentConfig, Editor};

const AGENT_BACKEND_ID_PREFIX: &str = "acp:agent:";
const DIRECT_BACKEND_ID_PREFIX: &str = "acp:direct:";
const MAX_EXPLICIT_AGENT_ID_LEN: usize = 128;

#[derive(Debug, Clone)]
pub struct AssistantBackendLaunch {
    pub backend_id: crate::assistant::backend::Id,
    pub display_name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub mcp_servers: Vec<helix_acp::types::McpServer>,
}

impl AgentConfig {
    pub fn backend_id(&self) -> anyhow::Result<crate::assistant::backend::Id> {
        let logical_id = match self.id.as_deref() {
            Some(id) => validate_explicit_agent_id(id)?,
            None => validate_agent_name(&self.name)?,
        };
        Ok(crate::assistant::backend::Id::new(format!(
            "{AGENT_BACKEND_ID_PREFIX}{logical_id}"
        )))
    }

    #[must_use]
    pub fn direct_backend_id(command: &str, args: &[String]) -> crate::assistant::backend::Id {
        let mut launch = Vec::new();
        for component in std::iter::once(command).chain(args.iter().map(String::as_str)) {
            let len = u64::try_from(component.len()).expect("command component length fits u64");
            launch.extend_from_slice(&len.to_be_bytes());
            launch.extend_from_slice(component.as_bytes());
        }
        let digest = Sha256::digest(launch);
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
        crate::assistant::backend::Id::new(format!("{DIRECT_BACKEND_ID_PREFIX}{encoded}"))
    }

    pub fn into_backend_launch(self) -> anyhow::Result<AssistantBackendLaunch> {
        Ok(AssistantBackendLaunch {
            backend_id: self.backend_id()?,
            display_name: self.name,
            command: self.command,
            args: self.args,
            env: self.env.into_iter().collect(),
            mcp_servers: self.mcp_servers,
        })
    }

    #[must_use]
    pub fn direct_backend_launch(
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        mcp_servers: Vec<helix_acp::types::McpServer>,
    ) -> AssistantBackendLaunch {
        AssistantBackendLaunch {
            backend_id: Self::direct_backend_id(&command, &args),
            display_name: command.clone(),
            command,
            args,
            env,
            mcp_servers,
        }
    }
}

fn validate_explicit_agent_id(id: &str) -> anyhow::Result<&str> {
    let is_valid = !id.is_empty()
        && id.len() <= MAX_EXPLICIT_AGENT_ID_LEN
        && id.as_bytes()[0].is_ascii_alphanumeric()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    anyhow::ensure!(
        is_valid,
        "ACP agent id must be 1..={MAX_EXPLICIT_AGENT_ID_LEN} ASCII letters, digits, '-', '_' or '.', and start with a letter or digit"
    );
    Ok(id)
}

fn validate_agent_name(name: &str) -> anyhow::Result<&str> {
    anyhow::ensure!(
        !name.trim().is_empty(),
        "ACP agent name must not be empty when no explicit id is configured"
    );
    Ok(name)
}

impl Editor {
    pub fn assistant_update_tx(&self) -> RuntimeSender<crate::assistant::backend::Update> {
        self.assistant_runtime.updates_tx.clone()
    }

    pub fn assistant_backend(
        &self,
        backend: &crate::assistant::backend::Id,
    ) -> Option<crate::assistant::BackendHandle> {
        self.assistant_runtime
            .backends
            .get(backend)
            .filter(|handle| !handle.is_closed())
            .cloned()
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
        self.assistant_agents()
            .into_iter()
            .find(|agent| matches!(agent.backend_id(), Ok(id) if id == *backend))
    }

    pub fn assistant_agents(&self) -> Vec<crate::editor::AgentConfig> {
        let mut agents = self.config().agents.clone();
        agents.extend(
            self.assistant_packaged_agents
                .agents
                .iter()
                .map(|(package_name, agent)| packaged_agent(package_name, agent.clone())),
        );
        #[cfg(feature = "mock-acp")]
        agents.push(mock_acp_agent());
        dedupe_agents(agents)
    }

    pub fn assistant_acp_package_agent(
        &self,
        package_name: &str,
    ) -> anyhow::Result<Option<crate::editor::AgentConfig>> {
        let packaged = self
            .assistant_packaged_agents
            .agents
            .get(package_name)
            .cloned()
            .map(|agent| packaged_agent(package_name, agent));
        let Some(agent) = packaged else {
            return Ok(None);
        };
        let backend_id = agent.backend_id()?;
        Ok(self.assistant_agent(&backend_id))
    }

    /// Resolves command-valued backend IDs written by older assistant history records.
    ///
    /// New connection paths must use [`AgentConfig::backend_id`] or
    /// [`AgentConfig::direct_backend_id`] instead of this compatibility lookup.
    pub fn resolve_legacy_assistant_history_backend(
        &self,
        backend: &crate::assistant::backend::Id,
    ) -> Option<crate::assistant::backend::Id> {
        if self.assistant_backend(backend).is_some() || self.assistant_agent(backend).is_some() {
            return Some(backend.clone());
        }
        if is_stable_backend_id(backend) {
            return None;
        }

        let mut matches = self
            .assistant_agents()
            .into_iter()
            .filter(|agent| agent.command == backend.as_ref())
            .filter_map(|agent| agent.backend_id().ok());
        let resolved = matches.next()?;
        matches.next().is_none().then_some(resolved)
    }

    /// Loads packaged ACP agents from explicit configuration and one coherent runtime snapshot.
    ///
    /// Registry and filesystem access happen here, so callers should run this on a background
    /// worker and publish the result with [`Editor::set_assistant_packaged_agents`].
    pub fn load_assistant_packaged_agents(
        config: helix_pkg::PkgConfig,
        runtime_assets: &helix_loader::RuntimeAssets,
    ) -> anyhow::Result<(u64, Arc<BTreeMap<String, crate::editor::AgentConfig>>)> {
        load_packaged_assistant_agents(config, runtime_assets)
    }

    /// Publishes a packaged-agent cache unless a newer runtime generation is already installed.
    pub fn set_assistant_packaged_agents(
        &mut self,
        generation: u64,
        agents: Arc<BTreeMap<String, crate::editor::AgentConfig>>,
    ) -> bool {
        if generation < self.assistant_packaged_agents.generation {
            return false;
        }
        self.assistant_packaged_agents.generation = generation;
        self.assistant_packaged_agents.agents = agents;
        true
    }

    pub fn cache_assistant_backend(&mut self, handle: crate::assistant::BackendHandle) {
        self.assistant_runtime
            .backends
            .insert(handle.id.clone(), handle);
    }

    fn take_live_assistant_backend(
        &mut self,
        backend: &crate::assistant::backend::Id,
    ) -> Option<crate::assistant::BackendHandle> {
        let is_closed = self
            .assistant_runtime
            .backends
            .get(backend)
            .is_some_and(crate::assistant::BackendHandle::is_closed);
        if is_closed {
            self.assistant_runtime.backends.remove(backend);
            return None;
        }
        self.assistant_runtime.backends.get(backend).cloned()
    }

    pub fn spawn_assistant_backend(
        &mut self,
        launch: AssistantBackendLaunch,
    ) -> anyhow::Result<crate::assistant::BackendHandle> {
        if let Some(handle) = self.take_live_assistant_backend(&launch.backend_id) {
            return Ok(handle);
        }

        let runtime = self.assistant_runtime();
        let cwd = std::env::current_dir().unwrap_or_default();
        let config = helix_acp::client::AgentConfig {
            command: launch.command.clone(),
            args: launch.args,
            env: launch.env,
            cwd: cwd.clone(),
            mcp_servers: launch.mcp_servers,
            timeout_secs: 120,
        };
        let client_info = helix_acp::types::Implementation::new("helix", env!("CARGO_PKG_VERSION"))
            .title("Helix Editor");

        let driver = crate::assistant::acp::Driver::new(
            launch.backend_id,
            launch.display_name,
            config,
            client_info,
        );
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
        if let Some(handle) = self.take_live_assistant_backend(backend) {
            return Some(handle);
        }

        let agent = self.assistant_agent(backend)?;
        self.spawn_assistant_backend(agent.into_backend_launch().ok()?)
            .ok()
    }

    pub fn connect_assistant_backend(
        &mut self,
        launch: AssistantBackendLaunch,
        profile: Option<crate::assistant::profile::Defaults>,
    ) -> anyhow::Result<(
        crate::assistant::backend::Id,
        Vec<crate::assistant::effect::Effect>,
    )> {
        let cwd = std::env::current_dir().unwrap_or_default();
        let handle = self.spawn_assistant_backend(launch)?;
        let effects = self.new_assistant_thread(
            handle.id.clone(),
            crate::assistant::thread::Scope::new(cwd),
            profile,
        );
        Ok((handle.id, effects))
    }
}

#[cfg(feature = "mock-acp")]
fn mock_acp_agent() -> crate::editor::AgentConfig {
    crate::editor::AgentConfig {
        id: Some("mock".to_owned()),
        name: "Mock Echo".to_owned(),
        command: "node".to_owned(),
        args: vec!["scripts/mock-acp-agent.js".to_owned()],
        env: Default::default(),
        mcp_servers: Vec::new(),
        theme: None,
    }
}

fn load_packaged_assistant_agents(
    config: helix_pkg::PkgConfig,
    runtime_assets: &helix_loader::RuntimeAssets,
) -> anyhow::Result<(u64, Arc<BTreeMap<String, crate::editor::AgentConfig>>)> {
    let snapshot = runtime_assets.snapshot();
    let generation = snapshot.generation();
    let store = helix_pkg::Store::open_default();
    let registry = helix_pkg::Registry::from_config(&config, &store)?;
    let mut agents = BTreeMap::new();
    for package in registry
        .iter()
        .filter(|package| package.kind == helix_pkg::PkgKind::Acp)
    {
        if let Some(agent) = packaged_agent_from_package(&snapshot, package)? {
            agents.insert(package.name.clone(), agent);
        }
    }
    Ok((generation, Arc::new(agents)))
}

fn packaged_agent_from_package(
    runtime_assets: &helix_loader::RuntimeAssetsSnapshot,
    package: &helix_pkg::PackageSpec,
) -> anyhow::Result<Option<crate::editor::AgentConfig>> {
    let Some(artifact) = package
        .artifacts_for(std::env::consts::OS, std::env::consts::ARCH)
        .next()
    else {
        return Ok(None);
    };

    let mut command_keys = vec![package.name.as_str(), artifact.bin.as_str()];
    command_keys.extend(artifact.source.bin.as_deref());
    command_keys.extend(artifact.source.system.as_deref());
    let mut launch = None;
    for key in command_keys {
        if let Some(resolved) = runtime_assets.resolve_command(key)? {
            launch = Some(resolved);
            break;
        }
    }
    let Some(launch) = launch else {
        return Ok(None);
    };

    let mut args = launch.prefix_args;
    args.extend(launch.default_args);
    let name = package
        .aliases
        .first()
        .cloned()
        .unwrap_or_else(|| package.name.clone());
    Ok(Some(crate::editor::AgentConfig {
        id: Some(package.name.clone()),
        name,
        command: launch.program.display().to_string(),
        args,
        env: launch.env.into_iter().collect(),
        mcp_servers: Vec::new(),
        theme: None,
    }))
}

fn packaged_agent(
    package_name: &str,
    mut agent: crate::editor::AgentConfig,
) -> crate::editor::AgentConfig {
    agent.id = Some(package_name.to_owned());
    agent
}

fn is_stable_backend_id(backend: &crate::assistant::backend::Id) -> bool {
    backend.as_str().starts_with(AGENT_BACKEND_ID_PREFIX)
        || backend.as_str().starts_with(DIRECT_BACKEND_ID_PREFIX)
}

fn dedupe_agents(agents: Vec<crate::editor::AgentConfig>) -> Vec<crate::editor::AgentConfig> {
    let mut seen = BTreeSet::new();
    agents
        .into_iter()
        .filter(|agent| match agent.backend_id() {
            Ok(backend_id) => seen.insert(backend_id),
            Err(_) => true,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, HashMap},
        fs,
        path::PathBuf,
        sync::Arc,
    };

    use super::*;
    use crate::graphics::Rect;

    fn named_agent(name: &str, command: &str, args: Vec<&str>) -> crate::editor::AgentConfig {
        crate::editor::AgentConfig {
            id: None,
            name: name.to_owned(),
            command: command.to_owned(),
            args: args.into_iter().map(ToOwned::to_owned).collect(),
            env: HashMap::new(),
            mcp_servers: Vec::new(),
            theme: None,
        }
    }

    fn packaged_agents(
        agents: impl IntoIterator<Item = (&'static str, crate::editor::AgentConfig)>,
    ) -> Arc<BTreeMap<String, crate::editor::AgentConfig>> {
        Arc::new(
            agents
                .into_iter()
                .map(|(package, agent)| (package.to_owned(), agent))
                .collect(),
        )
    }

    fn backend_launch(
        backend_id: crate::assistant::backend::Id,
        display_name: &str,
        command: &str,
    ) -> AssistantBackendLaunch {
        AssistantBackendLaunch {
            backend_id,
            display_name: display_name.to_owned(),
            command: command.to_owned(),
            args: Vec::new(),
            env: Vec::new(),
            mcp_servers: Vec::new(),
        }
    }

    fn test_editor(
        runtime: helix_runtime::Runtime,
        agents: Vec<crate::editor::AgentConfig>,
    ) -> Editor {
        let config = crate::editor::Config {
            agents,
            ..Default::default()
        };
        crate::editor::EditorBuilder::new(Rect::new(0, 0, 80, 24), runtime)
            .config(config)
            .build()
    }

    fn configured_and_packaged_agents(editor: &Editor) -> Vec<crate::editor::AgentConfig> {
        let agents = editor.assistant_agents();
        #[cfg(feature = "mock-acp")]
        return agents
            .into_iter()
            .filter(|agent| agent != &mock_acp_agent())
            .collect();
        #[cfg(not(feature = "mock-acp"))]
        return agents;
    }

    #[test]
    #[cfg(not(feature = "mock-acp"))]
    fn default_build_does_not_register_mock_agent() {
        let runtime = helix_runtime::test::RuntimeTest::default();
        runtime.block_on(async {
            let editor = test_editor(runtime.runtime(), Vec::new());
            assert!(editor.assistant_agents().is_empty());
        });
    }

    #[test]
    fn packaged_agent_cache_rejects_stale_generations() {
        let runtime = helix_runtime::test::RuntimeTest::default();
        runtime.block_on(async {
            let mut editor = test_editor(runtime.runtime(), Vec::new());
            assert!(editor.set_assistant_packaged_agents(
                8,
                packaged_agents([(
                    "current",
                    named_agent("Current", "current-agent", Vec::new()),
                )]),
            ));

            assert!(!editor.set_assistant_packaged_agents(
                7,
                packaged_agents([("stale", named_agent("Stale", "stale-agent", Vec::new()),)]),
            ));

            assert_eq!(editor.assistant_packaged_agents.generation, 8);
            assert!(editor
                .assistant_acp_package_agent("current")
                .unwrap()
                .is_some());
            assert!(editor
                .assistant_acp_package_agent("stale")
                .unwrap()
                .is_none());
        });
    }

    #[test]
    fn packaged_agent_cache_updates_and_removes_entries() {
        let runtime = helix_runtime::test::RuntimeTest::default();
        runtime.block_on(async {
            let mut editor = test_editor(runtime.runtime(), Vec::new());
            let connected_id = crate::assistant::backend::Id::new("connected-agent");
            let (connected_tx, _connected_rx) = helix_runtime::channel(1);
            editor.cache_assistant_backend(crate::assistant::BackendHandle::new(
                connected_id.clone(),
                connected_tx,
            ));
            assert!(editor.set_assistant_packaged_agents(
                3,
                packaged_agents([
                    ("updated", named_agent("Old", "old-agent", vec!["--old"]),),
                    (
                        "removed",
                        named_agent("Removed", "removed-agent", Vec::new()),
                    ),
                ]),
            ));

            assert!(editor.set_assistant_packaged_agents(
                4,
                packaged_agents([("updated", named_agent("New", "new-agent", vec!["--new"]),)]),
            ));

            let updated = editor
                .assistant_acp_package_agent("updated")
                .unwrap()
                .unwrap();
            assert_eq!(updated.name, "New");
            assert_eq!(updated.command, "new-agent");
            assert!(editor
                .assistant_acp_package_agent("removed")
                .unwrap()
                .is_none());
            assert_eq!(configured_and_packaged_agents(&editor), [updated]);
            assert!(editor.assistant_backend(&connected_id).is_some());
        });
    }

    #[test]
    fn connect_reuses_live_cached_backend_without_respawning() {
        let runtime = helix_runtime::test::RuntimeTest::default();
        runtime.block_on(async {
            let mut editor = test_editor(runtime.runtime(), Vec::new());
            let backend_id = crate::assistant::backend::Id::new("acp:agent:cached");
            let (tx, _rx) = helix_runtime::channel(1);
            editor.cache_assistant_backend(crate::assistant::BackendHandle::new(
                backend_id.clone(),
                tx,
            ));

            let (connected, effects) = editor
                .connect_assistant_backend(backend_launch(backend_id.clone(), "Cached", ""), None)
                .expect("cached backend avoids spawning the empty command");

            assert_eq!(connected, backend_id);
            assert!(effects.iter().any(|effect| matches!(
                effect,
                crate::assistant::effect::Effect::SendBackendCommand {
                    backend,
                    command: crate::assistant::backend::Command::NewThread { .. },
                } if backend == &connected
            )));
        });
    }

    #[test]
    fn closed_cached_backend_is_not_reused() {
        let runtime = helix_runtime::test::RuntimeTest::default();
        runtime.block_on(async {
            let mut editor = test_editor(runtime.runtime(), Vec::new());
            let backend_id = crate::assistant::backend::Id::new("acp:agent:closed");
            let (tx, rx) = helix_runtime::channel(1);
            drop(rx);
            editor.cache_assistant_backend(crate::assistant::BackendHandle::new(
                backend_id.clone(),
                tx,
            ));

            assert!(editor
                .connect_assistant_backend(backend_launch(backend_id.clone(), "Closed", ""), None,)
                .is_err());
            assert!(editor.assistant_runtime.backends.get(&backend_id).is_none());
        });
    }

    #[test]
    fn configured_agents_win_dedupe_over_packaged_agents() {
        let runtime = helix_runtime::test::RuntimeTest::default();
        runtime.block_on(async {
            let mut configured = named_agent("Configured", "shared-agent", vec!["--stdio"]);
            configured.id = Some("duplicate".to_owned());
            let mut editor = test_editor(runtime.runtime(), vec![configured.clone()]);
            assert!(editor.set_assistant_packaged_agents(
                1,
                packaged_agents([
                    (
                        "duplicate",
                        named_agent("Packaged duplicate", "shared-agent", vec!["--stdio"]),
                    ),
                    (
                        "unique",
                        named_agent("Packaged unique", "unique-agent", Vec::new()),
                    ),
                ]),
            ));

            let agents = configured_and_packaged_agents(&editor);
            assert_eq!(agents.len(), 2);
            assert_eq!(agents[0], configured);
            assert_eq!(agents[1].name, "Packaged unique");
        });
    }

    #[test]
    fn agents_with_distinct_ids_can_share_a_command() {
        let runtime = helix_runtime::test::RuntimeTest::default();
        runtime.block_on(async {
            let mut first = named_agent("First", "node", vec!["agent.js"]);
            first.id = Some("first".to_owned());
            let mut second = named_agent("Second", "node", vec!["agent.js"]);
            second.id = Some("second".to_owned());
            let editor = test_editor(runtime.runtime(), vec![first.clone(), second.clone()]);

            assert_eq!(configured_and_packaged_agents(&editor), [first, second]);
        });
    }

    #[test]
    fn configured_agent_wins_over_package_with_same_id() {
        let runtime = helix_runtime::test::RuntimeTest::default();
        runtime.block_on(async {
            let mut configured = named_agent("Configured", "custom-agent", Vec::new());
            configured.id = Some("shared-package".to_owned());
            let mut packaged = named_agent("Packaged", "packaged-agent", Vec::new());
            packaged.id = Some("shared-package".to_owned());
            let mut editor = test_editor(runtime.runtime(), vec![configured.clone()]);
            assert!(
                editor.set_assistant_packaged_agents(
                    1,
                    packaged_agents([("shared-package", packaged)]),
                )
            );

            assert_eq!(configured_and_packaged_agents(&editor), [configured]);
        });
    }

    #[test]
    fn legacy_history_command_resolves_without_enabling_command_lookup() {
        let runtime = helix_runtime::test::RuntimeTest::default();
        runtime.block_on(async {
            let mut configured = named_agent("Legacy", "node", vec!["agent.js"]);
            configured.id = Some("legacy".to_owned());
            let expected = configured.backend_id().unwrap();
            let editor = test_editor(runtime.runtime(), vec![configured]);
            let legacy = crate::assistant::backend::Id::new("node");

            assert!(editor.assistant_agent(&legacy).is_none());
            assert_eq!(
                editor.resolve_legacy_assistant_history_backend(&legacy),
                Some(expected)
            );
        });
    }

    #[test]
    fn legacy_history_command_does_not_guess_between_agents() {
        let runtime = helix_runtime::test::RuntimeTest::default();
        runtime.block_on(async {
            let mut first = named_agent("First", "node", vec!["first.js"]);
            first.id = Some("first".to_owned());
            let mut second = named_agent("Second", "node", vec!["second.js"]);
            second.id = Some("second".to_owned());
            let editor = test_editor(runtime.runtime(), vec![first, second]);

            assert_eq!(
                editor.resolve_legacy_assistant_history_backend(
                    &crate::assistant::backend::Id::new("node")
                ),
                None
            );
        });
    }

    #[test]
    fn direct_backend_id_is_deterministic_for_command_and_args() {
        let command = "private-agent-launcher";
        let secret = "super-secret-token";
        let args = vec!["--token".to_owned(), secret.to_owned()];
        let id = crate::editor::AgentConfig::direct_backend_id(command, &args);

        assert_eq!(
            id,
            crate::editor::AgentConfig::direct_backend_id(command, &args)
        );
        assert_ne!(
            id,
            crate::editor::AgentConfig::direct_backend_id(
                command,
                &["--token".to_owned(), "different-secret".to_owned()]
            )
        );
        assert!(id.as_str().starts_with("acp:direct:"));
        assert_eq!(id.as_str().trim_start_matches("acp:direct:").len(), 43);
        assert!(!id.as_str().contains(command));
        assert!(!id.as_str().contains(secret));
    }

    #[test]
    fn direct_backend_id_separates_length_delimited_launch_components() {
        let first = crate::editor::AgentConfig::direct_backend_id("ab", &["c".to_owned()]);
        let second = crate::editor::AgentConfig::direct_backend_id("a", &["bc".to_owned()]);

        assert_ne!(
            first, second,
            "component lengths must participate in the hash"
        );
    }

    #[test]
    fn configured_backend_id_uses_explicit_id_or_nonempty_name() {
        let named = named_agent("Named Agent", "node", Vec::new());
        let mut explicit = named_agent("Named Agent", "node", Vec::new());
        explicit.id = Some("named-agent".to_owned());

        assert_eq!(
            named.backend_id().unwrap().as_str(),
            "acp:agent:Named Agent"
        );
        assert_eq!(
            explicit.backend_id().unwrap().as_str(),
            "acp:agent:named-agent"
        );
    }

    #[test]
    fn invalid_configured_backend_ids_are_rejected_without_colliding() {
        for id in ["", " invalid", "-invalid", "invalid/id"] {
            let mut agent = named_agent("Valid Name", "node", Vec::new());
            agent.id = Some(id.to_owned());
            assert!(agent.backend_id().is_err(), "{id:?} must be rejected");
        }

        let empty_name = named_agent("   ", "node", Vec::new());
        assert!(empty_name.backend_id().is_err());
    }

    #[test]
    #[cfg(feature = "mock-acp")]
    fn mock_feature_adds_fixture_agent() {
        let runtime = helix_runtime::test::RuntimeTest::default();
        runtime.block_on(async {
            let editor = test_editor(runtime.runtime(), Vec::new());
            assert_eq!(editor.assistant_agents(), [mock_acp_agent()]);
        });
    }

    #[test]
    fn packaged_agent_loader_uses_explicit_config_and_pinned_generation() {
        let temp = tempfile::tempdir().unwrap();
        let registry_dir = temp.path().join("registry");
        fs::create_dir_all(&registry_dir).unwrap();
        fs::write(
            registry_dir.join("cache-loader-agent.toml"),
            format!(
                r#"name = "cache-loader-agent"
kind = "acp"
aliases = ["Cache Loader"]

[[artifact]]
os = "{}"
arch = "{}"
source = {{ system = "cache-loader-agent" }}
bin = "cache-loader-agent"
"#,
                std::env::consts::OS,
                std::env::consts::ARCH,
            ),
        )
        .unwrap();
        let program = temp.path().join("cache-loader-agent");
        fs::write(&program, b"test").unwrap();
        let asset = helix_loader::RuntimeAsset::from_spec(
            helix_loader::ActivePackage::new("acp", "cache-loader-agent", "1"),
            helix_loader::RuntimeAssetSpec::command("cache-loader-agent", &program)
                .with_default_args(["--stdio".to_owned()])
                .with_env([("CACHE_TEST".to_owned(), "1".to_owned())]),
        );
        let runtime_assets = helix_loader::RuntimeAssets::from_snapshot(
            helix_loader::RuntimeAssetsSnapshot::new(
                helix_loader::RuntimeSnapshot {
                    generation: 23,
                    assets: vec![asset],
                },
                Vec::new(),
                Vec::new(),
            )
            .with_search_path(None),
        );
        let config = helix_pkg::PkgConfig {
            registries: vec![registry_dir],
            ..helix_pkg::PkgConfig::default()
        };

        let (generation, agents) =
            Editor::load_assistant_packaged_agents(config, &runtime_assets).unwrap();
        let agent = agents.get("cache-loader-agent").unwrap();

        assert_eq!(generation, 23);
        assert_eq!(agent.id.as_deref(), Some("cache-loader-agent"));
        assert_eq!(agent.name, "Cache Loader");
        assert_eq!(PathBuf::from(&agent.command), program);
        assert_eq!(agent.args, ["--stdio"]);
        assert_eq!(agent.env.get("CACHE_TEST").map(String::as_str), Some("1"));
    }
}
