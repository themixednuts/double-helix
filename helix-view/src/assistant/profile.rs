use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{config, mode};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Definition {
    pub name: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub model: Option<Value>,
    #[serde(default)]
    pub config: BTreeMap<String, Value>,
    #[serde(default)]
    pub mcp_servers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    String(String),
    Bool(bool),
    Integer(i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Defaults {
    pub name: String,
    pub mode: Option<mode::Id>,
    pub config: Vec<(config::Id, config::ValueId)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDefaults {
    pub profile: Option<Defaults>,
    pub mcp_servers: Vec<helix_acp::types::McpServer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Active {
    defaults: Defaults,
    mode_applied: bool,
    config_applied: bool,
}

impl Value {
    #[must_use]
    pub fn to_value_id(&self) -> config::ValueId {
        match self {
            Self::String(value) => config::ValueId::new(value.clone()),
            Self::Bool(value) => config::ValueId::new(value.to_string()),
            Self::Integer(value) => config::ValueId::new(value.to_string()),
        }
    }
}

impl Definition {
    #[must_use]
    pub fn defaults(&self) -> Defaults {
        let mut config = Vec::new();
        if let Some(model) = &self.model {
            config.push((config::Id::new("model"), model.to_value_id()));
        }
        config.extend(
            self.config
                .iter()
                .map(|(option, value)| (config::Id::new(option.clone()), value.to_value_id())),
        );
        Defaults {
            name: self.name.clone(),
            mode: self.mode.as_ref().map(|mode| mode::Id::new(mode.clone())),
            config,
        }
    }
}

impl Active {
    #[must_use]
    pub fn new(defaults: Defaults) -> Self {
        Self {
            defaults,
            mode_applied: false,
            config_applied: false,
        }
    }

    #[must_use]
    pub fn restored(defaults: Defaults) -> Self {
        Self::new(defaults)
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.defaults.name
    }

    #[must_use]
    pub fn defaults(&self) -> &Defaults {
        &self.defaults
    }

    #[must_use]
    pub fn mode_pending(&self) -> Option<&mode::Id> {
        (!self.mode_applied)
            .then_some(self.defaults.mode.as_ref())
            .flatten()
    }

    #[must_use]
    pub fn config_pending(&self) -> Option<&[(config::Id, config::ValueId)]> {
        (!self.config_applied).then_some(self.defaults.config.as_slice())
    }

    pub fn mark_mode_applied(&mut self) {
        self.mode_applied = true;
    }

    pub fn mark_config_applied(&mut self) {
        self.config_applied = true;
    }
}

#[must_use]
pub fn assemble_session_defaults(
    profile: Option<&Definition>,
    configured_mcp_servers: &[helix_acp::types::McpServer],
) -> SessionDefaults {
    let Some(profile) = profile else {
        return SessionDefaults {
            profile: None,
            mcp_servers: configured_mcp_servers.to_vec(),
        };
    };

    let mcp_servers = if profile.mcp_servers.is_empty() {
        configured_mcp_servers.to_vec()
    } else {
        configured_mcp_servers
            .iter()
            .filter(|server| {
                profile
                    .mcp_servers
                    .iter()
                    .any(|name| name == mcp_server_name(server))
            })
            .cloned()
            .collect()
    };

    SessionDefaults {
        profile: Some(profile.defaults()),
        mcp_servers,
    }
}

#[must_use]
pub fn mcp_server_name(server: &helix_acp::types::McpServer) -> &str {
    match server {
        helix_acp::types::McpServer::Http(server) => &server.name,
        helix_acp::types::McpServer::Sse(server) => &server.name,
        helix_acp::types::McpServer::Stdio(server) => &server.name,
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_defaults_assemble_mode_config_and_selected_mcp_servers() {
        let profile = Definition {
            name: "review".to_string(),
            agent: Some("Claude".to_string()),
            mode: Some("review".to_string()),
            model: Some(Value::String("opus".to_string())),
            config: BTreeMap::from([("thinking".to_string(), Value::String("high".to_string()))]),
            mcp_servers: vec!["git".to_string()],
        };
        let servers = vec![
            helix_acp::types::McpServer::Stdio(helix_acp::types::McpServerStdio::new(
                "fs", "fs-mcp",
            )),
            helix_acp::types::McpServer::Stdio(helix_acp::types::McpServerStdio::new(
                "git", "git-mcp",
            )),
        ];

        let defaults = assemble_session_defaults(Some(&profile), &servers);
        let profile_defaults = defaults.profile.expect("profile defaults");

        assert_eq!(profile_defaults.name, "review");
        assert_eq!(
            profile_defaults.mode.as_ref().map(ToString::to_string),
            Some("review".to_string())
        );
        assert_eq!(
            profile_defaults
                .config
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect::<Vec<_>>(),
            vec![
                ("model".to_string(), "opus".to_string()),
                ("thinking".to_string(), "high".to_string())
            ]
        );
        assert_eq!(defaults.mcp_servers.len(), 1);
        assert_eq!(mcp_server_name(&defaults.mcp_servers[0]), "git");
    }

    #[test]
    fn no_profile_leaves_agent_mcp_servers_unchanged() {
        let servers = vec![helix_acp::types::McpServer::Stdio(
            helix_acp::types::McpServerStdio::new("fs", "fs-mcp"),
        )];

        let defaults = assemble_session_defaults(None, &servers);

        assert!(defaults.profile.is_none());
        assert_eq!(defaults.mcp_servers, servers);
    }
}
