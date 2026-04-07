use std::num::NonZeroU64;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::thread;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub scope: thread::Scope,
    pub open: Vec<thread::Id>,
    pub active: Option<thread::Id>,
}

pub fn current_scope() -> thread::Scope {
    thread::Scope::new(std::env::current_dir().unwrap_or_default())
}

pub async fn load_layout(scope: &thread::Scope) -> anyhow::Result<Option<Layout>> {
    let path = layout_path();
    let raw = match tokio::fs::read_to_string(path).await {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let state: PersistedLayouts = serde_json::from_str(&raw)?;
    Ok(state
        .scopes
        .into_iter()
        .find(|entry| entry.scope == PersistedScope::from(scope))
        .map(PersistedLayout::into_domain))
}

pub async fn save_layout(
    scope: &thread::Scope,
    open: Vec<thread::Id>,
    active: Option<thread::Id>,
) -> anyhow::Result<()> {
    let path = layout_path();
    let mut state = match tokio::fs::read_to_string(&path).await {
        Ok(raw) => serde_json::from_str::<PersistedLayouts>(&raw)?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => PersistedLayouts::default(),
        Err(err) => return Err(err.into()),
    };
    let layout = PersistedLayout::from_domain(Layout {
        scope: scope.clone(),
        open,
        active,
    });
    if let Some(entry) = state.scopes.iter_mut().find(|entry| entry.scope == layout.scope) {
        *entry = layout;
    } else {
        state.scopes.push(layout);
    }
    tokio::fs::create_dir_all(path.parent().expect("layout root")).await?;
    tokio::fs::write(path, serde_json::to_vec_pretty(&state)?).await?;
    Ok(())
}

fn layout_path() -> PathBuf {
    helix_loader::cache_dir()
        .join("assistant")
        .join("layout.json")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedLayouts {
    scopes: Vec<PersistedLayout>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedLayout {
    scope: PersistedScope,
    open: Vec<u64>,
    active: Option<u64>,
}

impl PersistedLayout {
    fn from_domain(layout: Layout) -> Self {
        Self {
            scope: PersistedScope::from(&layout.scope),
            open: layout.open.into_iter().map(|id| id.value().get()).collect(),
            active: layout.active.map(|id| id.value().get()),
        }
    }

    fn into_domain(self) -> Layout {
        Layout {
            scope: self.scope.into_domain(),
            open: self.open.into_iter().map(thread_id).collect(),
            active: self.active.map(thread_id),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedScope {
    cwd: PathBuf,
    worktrees: Vec<PathBuf>,
}

impl From<&thread::Scope> for PersistedScope {
    fn from(scope: &thread::Scope) -> Self {
        Self {
            cwd: scope.cwd.clone(),
            worktrees: scope.worktrees.clone(),
        }
    }
}

impl PersistedScope {
    fn into_domain(self) -> thread::Scope {
        thread::Scope {
            cwd: self.cwd,
            worktrees: self.worktrees,
        }
    }
}

fn thread_id(raw: u64) -> thread::Id {
    thread::Id::new(NonZeroU64::new(raw).expect("persisted thread id must be non-zero"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_round_trips_scope_open_tabs_and_active() {
        let scope = thread::Scope::new(PathBuf::from("."));
        let open = vec![
            thread::Id::new(NonZeroU64::new(1).unwrap()),
            thread::Id::new(NonZeroU64::new(2).unwrap()),
        ];
        let active = Some(open[1]);

        let persisted = PersistedLayout::from_domain(Layout {
            scope: scope.clone(),
            open: open.clone(),
            active,
        });
        let loaded = persisted.into_domain();

        assert_eq!(loaded.scope, scope);
        assert_eq!(loaded.open, open);
        assert_eq!(loaded.active, active);
    }
}
