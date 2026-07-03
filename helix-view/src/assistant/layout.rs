use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use super::thread;

static ATOMIC_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);
static LAYOUT_SAVE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    let scope_key = scope_key(scope)?;
    match tokio::task::spawn_blocking(move || {
        crate::assistant::history::import_legacy_if_needed_blocking()?;
        let mut store = helix_store::Store::open_default()?;
        store
            .layout()
            .get(&scope_key)?
            .map(store_layout_into_domain)
            .transpose()
    })
    .await
    {
        Ok(Ok(layout)) => Ok(layout),
        Ok(Err(err)) => {
            log::warn!("assistant layout store load failed, falling back to JSON: {err}");
            load_layout_from(layout_path(), scope).await
        }
        Err(err) => {
            log::warn!("assistant layout store load task failed, falling back to JSON: {err}");
            load_layout_from(layout_path(), scope).await
        }
    }
}

async fn load_layout_from(path: PathBuf, scope: &thread::Scope) -> anyhow::Result<Option<Layout>> {
    let raw = match tokio::fs::read_to_string(&path).await {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let state: PersistedLayouts = match serde_json::from_str(&raw) {
        Ok(state) => state,
        Err(err) => {
            log::warn!("assistant layout decode failed {:?}: {}", path, err);
            return Ok(None);
        }
    };
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
    let layout = store_layout_from_domain(Layout {
        scope: scope.clone(),
        open: open.clone(),
        active,
    })?;
    match tokio::task::spawn_blocking(move || {
        crate::assistant::history::import_legacy_if_needed_blocking()?;
        let mut store = helix_store::Store::open_default()?;
        store.layout().upsert(layout)?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) => {
            log::warn!("assistant layout store save failed, falling back to JSON: {err}");
            save_layout_to(layout_path(), scope, open, active).await
        }
        Err(err) => {
            log::warn!("assistant layout store save task failed, falling back to JSON: {err}");
            save_layout_to(layout_path(), scope, open, active).await
        }
    }
}

async fn save_layout_to(
    path: PathBuf,
    scope: &thread::Scope,
    open: Vec<thread::Id>,
    active: Option<thread::Id>,
) -> anyhow::Result<()> {
    let _guard = LAYOUT_SAVE_LOCK.lock().await;
    let mut state = match tokio::fs::read_to_string(&path).await {
        Ok(raw) => match serde_json::from_str::<PersistedLayouts>(&raw) {
            Ok(state) => state,
            Err(err) => {
                log::warn!("assistant layout decode failed {:?}: {}", path, err);
                PersistedLayouts::default()
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => PersistedLayouts::default(),
        Err(err) => return Err(err.into()),
    };
    let layout = PersistedLayout::from_domain(Layout {
        scope: scope.clone(),
        open,
        active,
    });
    if let Some(entry) = state
        .scopes
        .iter_mut()
        .find(|entry| entry.scope == layout.scope)
    {
        *entry = layout;
    } else {
        state.scopes.push(layout);
    }
    atomic_write(&path, &serde_json::to_vec_pretty(&state)?).await?;
    Ok(())
}

pub(crate) async fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("atomic write target has no parent: {:?}", path))?;
    tokio::fs::create_dir_all(parent).await?;

    let temp_path = loop {
        let candidate = atomic_temp_path(path)?;
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
            .await
        {
            Ok(mut file) => {
                if let Err(err) = file.write_all(bytes).await {
                    let _ = tokio::fs::remove_file(&candidate).await;
                    return Err(err.into());
                }
                if let Err(err) = file.sync_all().await {
                    let _ = tokio::fs::remove_file(&candidate).await;
                    return Err(err.into());
                }
                drop(file);
                break candidate;
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.into()),
        }
    };

    if let Err(err) = tokio::fs::rename(&temp_path, path).await {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(err.into());
    }

    Ok(())
}

fn atomic_temp_path(path: &Path) -> anyhow::Result<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("atomic write target has no file name: {:?}", path))?;
    let mut temp_name = file_name.to_os_string();
    temp_name.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        ATOMIC_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    Ok(path.with_file_name(temp_name))
}

pub(crate) fn layout_path() -> PathBuf {
    helix_loader::cache_dir()
        .join("assistant")
        .join("layout.json")
}

pub(crate) fn legacy_layouts_from_path(
    path: impl AsRef<Path>,
) -> anyhow::Result<Vec<helix_store::AssistantLayout>> {
    let path = path.as_ref();
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let state: PersistedLayouts = match serde_json::from_str(&raw) {
        Ok(state) => state,
        Err(err) => {
            log::warn!("assistant layout decode failed {:?}: {}", path, err);
            return Ok(Vec::new());
        }
    };
    state
        .scopes
        .into_iter()
        .map(PersistedLayout::into_store)
        .collect()
}

pub(crate) fn scope_key(scope: &thread::Scope) -> anyhow::Result<String> {
    serde_json::to_string(&PersistedScope::from(scope)).context("serialize assistant scope")
}

fn store_layout_from_domain(layout: Layout) -> anyhow::Result<helix_store::AssistantLayout> {
    Ok(helix_store::AssistantLayout {
        scope: scope_key(&layout.scope)?,
        open_ids: layout
            .open
            .into_iter()
            .map(|id| id.value().get().to_string())
            .collect(),
        active_id: layout.active.map(|id| id.value().get().to_string()),
    })
}

fn store_layout_into_domain(layout: helix_store::AssistantLayout) -> anyhow::Result<Layout> {
    let scope: PersistedScope = serde_json::from_str(&layout.scope)?;
    Ok(Layout {
        scope: scope.into_domain(),
        open: layout
            .open_ids
            .into_iter()
            .map(|id| id.parse::<u64>().map(thread_id))
            .collect::<Result<Vec<_>, _>>()?,
        active: layout
            .active_id
            .map(|id| id.parse::<u64>().map(thread_id))
            .transpose()?,
    })
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

    fn into_store(self) -> anyhow::Result<helix_store::AssistantLayout> {
        Ok(helix_store::AssistantLayout {
            scope: serde_json::to_string(&self.scope)?,
            open_ids: self.open.into_iter().map(|id| id.to_string()).collect(),
            active_id: self.active.map(|id| id.to_string()),
        })
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

    #[tokio::test]
    async fn atomic_write_replaces_longer_file_without_trailing_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");

        atomic_write(&path, b"{\"value\":\"a much longer payload\"}")
            .await
            .unwrap();
        atomic_write(&path, b"{\"value\":1}").await.unwrap();

        assert_eq!(
            tokio::fs::read_to_string(path).await.unwrap(),
            "{\"value\":1}"
        );
    }

    #[tokio::test]
    async fn corrupt_layout_loads_as_empty_and_next_save_heals_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layout.json");
        let scope = thread::Scope::new(PathBuf::from("project"));
        let id = thread::Id::new(NonZeroU64::new(1).unwrap());
        tokio::fs::write(
            &path,
            br#"{"scopes":[{"scope":{"cwd":"project","worktrees":[]},"open":[1],"active":1}]}garbage"#,
        )
        .await
        .unwrap();

        assert_eq!(load_layout_from(path.clone(), &scope).await.unwrap(), None);

        save_layout_to(path.clone(), &scope, vec![id], Some(id))
            .await
            .unwrap();
        let raw = tokio::fs::read_to_string(path).await.unwrap();
        let state: PersistedLayouts = serde_json::from_str(&raw).unwrap();

        assert_eq!(state.scopes.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_layout_saves_merge_scopes_without_lost_update() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layout.json");
        let scope_a = thread::Scope::new(PathBuf::from("project-a"));
        let scope_b = thread::Scope::new(PathBuf::from("project-b"));
        let id_a = thread::Id::new(NonZeroU64::new(1).unwrap());
        let id_b = thread::Id::new(NonZeroU64::new(2).unwrap());

        let (save_a, save_b) = tokio::join!(
            save_layout_to(path.clone(), &scope_a, vec![id_a], Some(id_a)),
            save_layout_to(path.clone(), &scope_b, vec![id_b], Some(id_b)),
        );
        save_a.unwrap();
        save_b.unwrap();

        let loaded_a = load_layout_from(path.clone(), &scope_a)
            .await
            .unwrap()
            .unwrap();
        let loaded_b = load_layout_from(path, &scope_b).await.unwrap().unwrap();

        assert_eq!(loaded_a.open, vec![id_a]);
        assert_eq!(loaded_b.open, vec![id_b]);
    }

    #[test]
    fn store_layout_upserts_independent_scopes_concurrently() {
        let dir = tempfile::tempdir().unwrap();
        let paths = helix_store::StorePaths::new(
            dir.path().join("state.sqlite3"),
            dir.path().join("cache.sqlite3"),
        );
        let scope_a = thread::Scope::new(PathBuf::from("project-a"));
        let scope_b = thread::Scope::new(PathBuf::from("project-b"));
        let id_a = thread::Id::new(NonZeroU64::new(1).unwrap());
        let id_b = thread::Id::new(NonZeroU64::new(2).unwrap());
        let layout_a = store_layout_from_domain(Layout {
            scope: scope_a.clone(),
            open: vec![id_a],
            active: Some(id_a),
        })
        .unwrap();
        let layout_b = store_layout_from_domain(Layout {
            scope: scope_b.clone(),
            open: vec![id_b],
            active: Some(id_b),
        })
        .unwrap();
        helix_store::Store::open(paths.clone()).unwrap();

        let writer_a = {
            let paths = paths.clone();
            std::thread::spawn(move || {
                let mut store = helix_store::Store::open(paths).unwrap();
                store.layout().upsert(layout_a).unwrap();
            })
        };
        let writer_b = {
            let paths = paths.clone();
            std::thread::spawn(move || {
                let mut store = helix_store::Store::open(paths).unwrap();
                store.layout().upsert(layout_b).unwrap();
            })
        };
        writer_a.join().unwrap();
        writer_b.join().unwrap();

        let mut store = helix_store::Store::open(paths).unwrap();
        let loaded_a = store
            .layout()
            .get(&scope_key(&scope_a).unwrap())
            .unwrap()
            .map(store_layout_into_domain)
            .transpose()
            .unwrap()
            .unwrap();
        let loaded_b = store
            .layout()
            .get(&scope_key(&scope_b).unwrap())
            .unwrap()
            .map(store_layout_into_domain)
            .transpose()
            .unwrap()
            .unwrap();

        assert_eq!(loaded_a.open, vec![id_a]);
        assert_eq!(loaded_b.open, vec![id_b]);
    }
}
