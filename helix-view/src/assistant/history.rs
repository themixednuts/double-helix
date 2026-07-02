mod local;

use std::sync::Arc;

use super::{config, context, mode, plan, review, terminal, thread};
use crate::collab::FollowState;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Cursor(Arc<str>);

impl Cursor {
    #[must_use]
    pub fn new(cursor: impl Into<Arc<str>>) -> Self {
        Self(cursor.into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Caps {
    pub list: bool,
    pub load: bool,
    pub close: bool,
    pub resume: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stub {
    pub id: thread::Id,
    pub title: Option<String>,
    pub scope: thread::Scope,
    pub unread: bool,
    pub run: thread::Run,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub id: thread::Id,
    pub origin: thread::Origin,
    pub title: Option<String>,
    pub entries: Vec<thread::Entry>,
    pub turns: Vec<thread::Turn>,
    pub plan: Vec<plan::Item>,
    pub draft: String,
    pub context: Vec<context::Item>,
    pub follow: FollowState,
    pub run: thread::Run,
    pub unread: bool,
    pub mode: Option<mode::Set>,
    pub config: config::State,
    pub review_mode: review::Mode,
    pub usage: thread::Usage,
    pub commands: Vec<thread::Command>,
    pub pending_elicitations: Vec<thread::Elicitation>,
    pub caps: Option<helix_acp::AgentCaps>,
    pub scope: thread::Scope,
    pub view: View,
    pub terminals: Vec<terminal::Terminal>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct View {
    pub focus: thread::Focus,
    pub selected: Option<thread::EntryId>,
    pub folded: Vec<thread::EntryId>,
    pub content_scroll: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub scope: thread::Scope,
    pub entries: Vec<Stub>,
    pub next: Option<Cursor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct State {
    pages: Vec<Page>,
}

impl State {
    #[must_use]
    pub fn page(&self, scope: &thread::Scope) -> Option<&Page> {
        self.pages.iter().find(|page| &page.scope == scope)
    }

    pub fn replace(&mut self, scope: thread::Scope, entries: Vec<Stub>, next: Option<Cursor>) {
        if let Some(page) = self.pages.iter_mut().find(|page| page.scope == scope) {
            page.entries = entries;
            page.next = next;
        } else {
            self.pages.push(Page {
                scope,
                entries,
                next,
            });
        }
    }

    pub fn sync_thread(&mut self, thread: &thread::Thread) {
        let Some(page) = self
            .pages
            .iter_mut()
            .find(|page| page.scope == *thread.scope())
        else {
            return;
        };

        let Some(entry) = page.entries.iter_mut().find(|entry| entry.id == thread.id) else {
            return;
        };

        entry.title = thread.title().map(ToOwned::to_owned);
        entry.scope = thread.clone_scope();
        entry.unread = thread.unread();
        entry.run = thread.run().clone();
    }

    pub fn upsert(&mut self, entry: Stub) {
        if let Some(page) = self.pages.iter_mut().find(|page| page.scope == entry.scope) {
            if let Some(current) = page
                .entries
                .iter_mut()
                .find(|current| current.id == entry.id)
            {
                *current = entry;
            } else {
                page.entries.push(entry);
            }
        } else {
            self.pages.push(Page {
                scope: entry.scope.clone(),
                entries: vec![entry],
                next: None,
            });
        }
    }
}

impl Stub {
    #[must_use]
    pub fn from_thread(thread: &thread::Thread) -> Self {
        Self {
            id: thread.id,
            title: thread.title().map(ToOwned::to_owned),
            scope: thread.clone_scope(),
            unread: thread.unread(),
            run: thread.run().clone(),
        }
    }
}

impl Record {
    #[must_use]
    pub fn from_thread(thread: &thread::Thread) -> Self {
        Self {
            id: thread.id,
            origin: thread.origin().clone(),
            title: thread.title().map(ToOwned::to_owned),
            entries: thread.entries().to_vec(),
            turns: thread.turns().to_vec(),
            plan: thread.plan().to_vec(),
            draft: thread.draft().to_string(),
            context: thread.context_items().to_vec(),
            follow: thread.follow().clone(),
            run: thread.run().clone(),
            unread: thread.unread(),
            mode: thread.mode().cloned(),
            config: thread.config().clone(),
            review_mode: thread.review_mode(),
            usage: thread.usage().clone(),
            commands: thread.commands().to_vec(),
            pending_elicitations: thread.pending_elicitations().to_vec(),
            caps: thread.caps().cloned(),
            scope: thread.clone_scope(),
            view: View {
                focus: thread.focus(),
                selected: thread.selected_entry(),
                folded: thread.folded_entries().collect(),
                content_scroll: thread.content_scroll(),
            },
            terminals: thread.terminals().to_vec(),
        }
    }

    #[must_use]
    pub fn into_thread(self) -> thread::Thread {
        let mut thread = thread::Thread::new(self.id, self.origin, self.scope);
        thread.restore_persisted_state(thread::PersistedState {
            title: self.title,
            entries: self.entries,
            turns: self.turns,
            plan: self.plan,
            draft: self.draft,
            context: self.context,
            follow: self.follow,
            run: self.run,
            unread: self.unread,
            mode: self.mode,
            config: self.config,
            review_mode: self.review_mode,
            terminals: self.terminals,
            usage: thread::Usage::default(),
            commands: Vec::new(),
            pending_elicitations: Vec::new(),
            caps: None,
        });
        thread.restore_transient_view(
            self.view.focus,
            self.view.selected,
            self.view.folded,
            self.view.content_scroll,
        );
        thread
    }
}

#[derive(Debug, Clone)]
#[allow(private_interfaces)]
pub enum Backend {
    Local(local::LocalHistory),
}

impl Backend {
    pub async fn load_scope(&self, scope: &thread::Scope) -> anyhow::Result<Vec<Stub>> {
        match self {
            Self::Local(backend) => backend.load_scope(scope).await,
        }
    }

    pub async fn load(&self, id: thread::Id) -> anyhow::Result<Option<Record>> {
        match self {
            Self::Local(backend) => backend.load(id).await,
        }
    }

    pub async fn save(&self, record: Record) -> anyhow::Result<()> {
        match self {
            Self::Local(backend) => backend.save(record).await,
        }
    }

    pub async fn delete(&self, id: thread::Id) -> anyhow::Result<()> {
        match self {
            Self::Local(backend) => backend.delete(id).await,
        }
    }
}

pub use local::local_backend;
