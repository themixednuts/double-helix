use std::borrow::Cow;
use std::fmt;
use std::collections::HashMap;
use std::sync::Arc;

use crate::collab::{Location, SurfaceId};
use crate::Editor;

use super::thread;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Id(Arc<str>);

impl Id {
    #[must_use]
    pub fn new(id: impl Into<Arc<str>>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for Id {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_ref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: Id,
    pub kind: Kind,
}

impl Item {
    #[must_use]
    pub fn new(id: Id, kind: Kind) -> Self {
        Self { id, kind }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Selection(Selection),
    Symbol(Symbol),
    File(File),
    Diagnostics(Diagnostics),
    Diff(Diff),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub path: std::path::PathBuf,
    pub range: Option<Location>,
    pub text: String,
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub path: std::path::PathBuf,
    pub name: String,
    pub kind: Cow<'static, str>,
    pub range: Option<Location>,
    pub text: String,
    pub breadcrumb: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub path: std::path::PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostics {
    pub path: std::path::PathBuf,
    pub items: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    pub path: std::path::PathBuf,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key(Cow<'static, str>);

impl Key {
    #[must_use]
    pub const fn core(name: &'static str) -> Self {
        Self(Cow::Borrowed(name))
    }

    #[must_use]
    pub fn new(name: impl Into<Cow<'static, str>>) -> Self {
        Self(name.into())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("context unavailable")]
    Unavailable,
    #[error(transparent)]
    Resolve(#[from] anyhow::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    File,
    Diagnostics,
    Diff,
}

pub struct Registry {
    providers: HashMap<Key, Provider>,
}

impl Registry {
    pub fn register(&mut self, provider: Provider) {
        self.providers.insert(provider.key(), provider);
    }

    pub fn provider(&self, key: &Key) -> Option<&Provider> {
        self.providers.get(key)
    }

    pub fn providers(&self) -> impl Iterator<Item = &Provider> {
        self.providers.values()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            providers: HashMap::default(),
        }
    }
}

impl Provider {
    #[must_use]
    pub fn key(self) -> Key {
        match self {
            Self::File => Key::core("file"),
            Self::Diagnostics => Key::core("diagnostics"),
            Self::Diff => Key::core("diff"),
        }
    }

    pub async fn provide(
        self,
        editor: &Editor,
        thread: &thread::Snapshot,
        surface: Option<SurfaceId>,
    ) -> Result<Kind, Error> {
        match self {
            Self::File => provide_file(editor, thread, surface).await,
            Self::Diagnostics => provide_diagnostics(editor, thread, surface).await,
            Self::Diff => provide_diff(editor, thread, surface).await,
        }
    }
}

pub fn core_registry() -> Registry {
    let mut registry = Registry::default();
    registry.register(Provider::File);
    registry.register(Provider::Diagnostics);
    registry.register(Provider::Diff);
    registry
}

async fn provide_file(
    editor: &Editor,
    _thread: &thread::Snapshot,
    _surface: Option<SurfaceId>,
) -> Result<Kind, Error> {
    let path = editor
        .focused_document()
        .and_then(|doc| doc.path().map(|path| path.to_path_buf()))
        .ok_or(Error::Unavailable)?;
    Ok(Kind::File(File { path }))
}

async fn provide_diagnostics(
    editor: &Editor,
    _thread: &thread::Snapshot,
    _surface: Option<SurfaceId>,
) -> Result<Kind, Error> {
    let (path, items) = editor
        .focused_document()
        .and_then(|doc| {
            let path = doc.path()?.to_path_buf();
            let items = doc
                .diagnostics()
                .iter()
                .map(|diagnostic| diagnostic.message.clone())
                .collect::<Vec<_>>();
            Some((path, items))
        })
        .ok_or(Error::Unavailable)?;
    if items.is_empty() {
        return Err(Error::Unavailable);
    }
    Ok(Kind::Diagnostics(Diagnostics { path, items }))
}

async fn provide_diff(
    editor: &Editor,
    _thread: &thread::Snapshot,
    _surface: Option<SurfaceId>,
) -> Result<Kind, Error> {
    let (path, summary) = editor
        .focused_document()
        .and_then(|doc| {
            let path = doc.path()?.to_path_buf();
            let handle = doc.diff_handle()?;
            let diff = handle.load();
            if diff.is_empty() {
                return None;
            }

            let mut summary = String::new();
            for index in 0..diff.len() {
                let hunk = diff.nth_hunk(index);
                use std::fmt::Write as _;
                let _ = writeln!(
                    summary,
                    "hunk {}: base {}..{} -> current {}..{}",
                    index + 1,
                    hunk.before.start + 1,
                    hunk.before.end + 1,
                    hunk.after.start + 1,
                    hunk.after.end + 1
                );
            }
            Some((path, summary.trim_end().to_string()))
        })
        .ok_or(Error::Unavailable)?;

    Ok(Kind::Diff(Diff { path, summary }))
}
