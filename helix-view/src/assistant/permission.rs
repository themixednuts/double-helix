use std::borrow::Cow;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::thread;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RequestId(Arc<str>);

impl RequestId {
    #[must_use]
    pub fn new(id: impl Into<Arc<str>>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for RequestId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_ref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChoiceId(Arc<str>);

impl ChoiceId {
    #[must_use]
    pub fn new(id: impl Into<Arc<str>>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ChoiceId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ChoiceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_ref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    Custom(Cow<'static, str>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub id: ChoiceId,
    pub label: String,
    pub kind: Kind,
}

impl Choice {
    #[must_use]
    pub fn new(id: ChoiceId, label: impl Into<String>, kind: Kind) -> Self {
        Self {
            id,
            label: label.into(),
            kind,
        }
    }
}

impl Kind {
    #[must_use]
    pub fn from_label(id: &str, label: &str) -> Self {
        let text = format!("{id} {label}").to_ascii_lowercase();
        let always = text.contains("always");
        let allow = text.contains("allow") || text.contains("approve") || text == "yes";
        let reject = text.contains("reject") || text.contains("deny") || text == "no";
        match (allow, reject, always) {
            (true, false, true) => Self::AllowAlways,
            (true, false, false) => Self::AllowOnce,
            (false, true, true) => Self::RejectAlways,
            (false, true, false) => Self::RejectOnce,
            _ => Self::Custom(Cow::Borrowed("choice")),
        }
    }

    #[must_use]
    pub const fn is_always(&self) -> bool {
        matches!(self, Self::AllowAlways | Self::RejectAlways)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    id: RequestId,
    thread: thread::Id,
    title: String,
    body: String,
    default: Option<ChoiceId>,
    choices: Vec<Choice>,
}

pub struct Builder<S> {
    id: RequestId,
    thread: thread::Id,
    title: String,
    body: String,
    default: Option<ChoiceId>,
    choices: Vec<Choice>,
    _state: std::marker::PhantomData<S>,
}

pub struct Empty;
pub struct Ready;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("default choice is not present in request")]
pub struct MissingChoice;

impl Request {
    #[must_use]
    pub fn builder(
        id: RequestId,
        thread: thread::Id,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Builder<Empty> {
        Builder {
            id,
            thread,
            title: title.into(),
            body: body.into(),
            default: None,
            choices: Vec::new(),
            _state: std::marker::PhantomData,
        }
    }

    #[must_use]
    pub fn id(&self) -> &RequestId {
        &self.id
    }
    #[must_use]
    pub fn thread(&self) -> thread::Id {
        self.thread
    }
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }
    #[must_use]
    pub fn body(&self) -> &str {
        &self.body
    }
    #[must_use]
    pub fn default(&self) -> Option<&ChoiceId> {
        self.default.as_ref()
    }
    #[must_use]
    pub fn choices(&self) -> &[Choice] {
        &self.choices
    }
}

impl Builder<Empty> {
    #[must_use]
    pub fn choice(mut self, choice: Choice) -> Builder<Ready> {
        self.choices.push(choice);
        Builder {
            id: self.id,
            thread: self.thread,
            title: self.title,
            body: self.body,
            default: self.default,
            choices: self.choices,
            _state: std::marker::PhantomData,
        }
    }
}

impl Builder<Ready> {
    #[must_use]
    pub fn choice(mut self, choice: Choice) -> Self {
        self.choices.push(choice);
        self
    }

    pub fn default(mut self, id: ChoiceId) -> Result<Self, MissingChoice> {
        if self.choices.iter().any(|choice| choice.id == id) {
            self.default = Some(id);
            Ok(self)
        } else {
            Err(MissingChoice)
        }
    }

    #[must_use]
    pub fn build(self) -> Request {
        Request {
            id: self.id,
            thread: self.thread,
            title: self.title,
            body: self.body,
            default: self.default,
            choices: self.choices,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Choose(ChoiceId),
    Dismiss,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Rules {
    rules: Vec<Rule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rule {
    pub agent: String,
    pub tool: String,
    pub choice: String,
}

impl Rules {
    #[must_use]
    pub fn path() -> PathBuf {
        helix_loader::cache_dir()
            .join("assistant")
            .join("permissions.toml")
    }

    #[must_use]
    pub fn load() -> Self {
        match Self::load_from_store() {
            Ok(rules) => rules,
            Err(err) => {
                log::warn!(
                    "assistant permission rules store load failed, falling back to TOML: {err}"
                );
                Self::load_from_path(Self::path())
            }
        }
    }

    #[must_use]
    fn load_from_path(path: impl AsRef<std::path::Path>) -> Self {
        let Ok(raw) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        toml::from_str(&raw).unwrap_or_else(|err| {
            log::warn!("assistant permission rules decode failed: {err}");
            Self::default()
        })
    }

    pub fn save(&self) -> anyhow::Result<()> {
        match self.save_to_store() {
            Ok(()) => Ok(()),
            Err(err) => {
                log::warn!(
                    "assistant permission rules store save failed, falling back to TOML: {err}"
                );
                self.save_to_path(Self::path())
            }
        }
    }

    fn save_to_path(&self, path: impl AsRef<std::path::Path>) -> anyhow::Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn reset() -> anyhow::Result<()> {
        match Self::reset_store() {
            Ok(()) => Ok(()),
            Err(err) => {
                log::warn!(
                    "assistant permission rules store reset failed, falling back to TOML: {err}"
                );
                Self::reset_path(Self::path())
            }
        }
    }

    fn reset_path(path: impl AsRef<std::path::Path>) -> anyhow::Result<()> {
        let path = path.as_ref();
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    #[must_use]
    pub fn choice(&self, agent: &str, tool: &str, choices: &[Choice]) -> Option<ChoiceId> {
        let rule = self
            .rules
            .iter()
            .rev()
            .find(|rule| rule.agent == agent && rule.tool == tool)?;
        choices
            .iter()
            .find(|choice| choice.id.as_str() == rule.choice)
            .map(|choice| choice.id.clone())
    }

    pub fn remember(&mut self, agent: &str, tool: &str, choice: &Choice) -> anyhow::Result<()> {
        if !choice.kind.is_always() {
            return Ok(());
        }
        self.remember_in_memory(agent, tool, choice);
        self.save()
    }

    fn remember_in_memory(&mut self, agent: &str, tool: &str, choice: &Choice) {
        self.rules
            .retain(|rule| !(rule.agent == agent && rule.tool == tool));
        self.rules.push(Rule {
            agent: agent.to_string(),
            tool: tool.to_string(),
            choice: choice.id.as_str().to_string(),
        });
    }

    fn load_from_store() -> anyhow::Result<Self> {
        crate::assistant::history::import_legacy_if_needed_blocking()?;
        let mut store = helix_store::Store::open_default()?;
        Ok(Self {
            rules: store
                .permissions()
                .all()?
                .into_iter()
                .map(Rule::from_store)
                .collect(),
        })
    }

    fn save_to_store(&self) -> anyhow::Result<()> {
        crate::assistant::history::import_legacy_if_needed_blocking()?;
        let mut store = helix_store::Store::open_default()?;
        store
            .permissions()
            .replace_all(self.rules.iter().map(Rule::to_store).collect())?;
        Ok(())
    }

    fn reset_store() -> anyhow::Result<()> {
        crate::assistant::history::import_legacy_if_needed_blocking()?;
        let mut store = helix_store::Store::open_default()?;
        store.permissions().clear()?;
        Ok(())
    }
}

impl Rule {
    fn to_store(&self) -> helix_store::AssistantPermission {
        helix_store::AssistantPermission {
            agent: self.agent.clone(),
            tool: self.tool.clone(),
            choice: self.choice.clone(),
        }
    }

    fn from_store(rule: helix_store::AssistantPermission) -> Self {
        Self {
            agent: rule.agent,
            tool: rule.tool,
            choice: rule.choice,
        }
    }
}

pub(crate) fn legacy_permissions_from_path(
    path: impl AsRef<std::path::Path>,
) -> anyhow::Result<Vec<helix_store::AssistantPermission>> {
    Ok(Rules::load_from_path(path)
        .rules
        .into_iter()
        .map(|rule| rule.to_store())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;

    fn thread() -> thread::Id {
        thread::Id::new(NonZeroU64::new(1).unwrap())
    }

    #[test]
    fn default_choice_must_exist() {
        let req = Request::builder(RequestId::new("r1"), thread(), "title", "body")
            .choice(Choice::new(ChoiceId::new("c1"), "allow", Kind::AllowOnce));
        assert!(req.default(ChoiceId::new("missing")).is_err());
    }

    #[test]
    fn rules_remember_allow_always_and_reset() {
        let mut rules = Rules::default();
        let choice = Choice::new(
            ChoiceId::new("allow-always"),
            "Allow Always",
            Kind::AllowAlways,
        );
        rules.remember_in_memory("agent", "write", &choice);
        assert_eq!(
            rules.choice("agent", "write", std::slice::from_ref(&choice)),
            Some(choice.id.clone())
        );
        rules.rules.clear();
        assert_eq!(rules.choice("agent", "write", &[choice]), None);
    }

    #[test]
    fn rules_store_round_trips_and_reset_clears_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.toml");
        let choice = Choice::new(
            ChoiceId::new("reject-always"),
            "Reject Always",
            Kind::RejectAlways,
        );
        let mut rules = Rules::default();
        rules.remember_in_memory("agent", "shell", &choice);
        rules.save_to_path(&path).unwrap();

        let loaded = Rules::load_from_path(&path);
        assert_eq!(
            loaded.choice("agent", "shell", std::slice::from_ref(&choice)),
            Some(choice.id.clone())
        );

        Rules::reset_path(&path).unwrap();
        assert!(!path.exists());
    }
}
