use std::collections::HashMap;

use super::{context, history, mode, plan, terminal, thread, Store};
use crate::collab::Location;
use crate::DocumentId;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Panel {
    pub tabs: Vec<Tab>,
    pub active: Option<ThreadView>,
    pub focused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    pub id: thread::Id,
    pub title: String,
    pub run: thread::Run,
    pub unread: bool,
    pub follow: Follow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Follow {
    Off,
    On,
    Paused,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadView {
    pub id: thread::Id,
    pub title: Option<String>,
    pub is_remote: bool,
    pub entries: Vec<EntryView>,
    pub draft: String,
    pub context: Vec<Pill>,
    pub run: thread::Run,
    pub unread: bool,
    pub focus: thread::Focus,
    pub follow: Follow,
    pub mode_name: Option<String>,
    pub model_label: Option<String>,
    pub plan: Vec<plan::Item>,
    pub selected: Option<thread::EntryId>,
    pub folded: Vec<thread::EntryId>,
    pub opened_docs: HashMap<thread::EntryId, DocumentId>,
    pub content_scroll: usize,
    pub terminals: Vec<Terminal>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryView {
    pub id: thread::EntryId,
    pub kind: EntryKind,
    pub locations: Vec<Location>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    UserPrompt {
        text: String,
    },
    AssistantText {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        state: String,
        output: String,
    },
    Status {
        text: String,
    },
    ChangeSummary {
        files: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pill {
    pub id: context::Id,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terminal {
    pub id: terminal::Id,
    pub title: Option<String>,
    pub state: String,
    pub output: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    pub scope: thread::Scope,
    pub entries: Vec<HistoryThread>,
    pub next: Option<history::Cursor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryThread {
    pub id: thread::Id,
    pub title: Option<String>,
    pub unread: bool,
    pub run: thread::Run,
}

impl Follow {
    #[must_use]
    pub fn to_model(self) -> crate::model::AssistantFollow {
        match self {
            Self::Off => crate::model::AssistantFollow::Off,
            Self::On => crate::model::AssistantFollow::On,
            Self::Paused => crate::model::AssistantFollow::Paused,
        }
    }
}

impl EntryView {
    #[must_use]
    pub fn to_model(self) -> crate::model::AssistantEntry {
        crate::model::AssistantEntry {
            id: self.id,
            locations: self.locations.len(),
            kind: match self.kind {
                EntryKind::UserPrompt { text } => {
                    crate::model::AssistantEntryKind::UserMessage(text)
                }
                EntryKind::AssistantText { text } => {
                    crate::model::AssistantEntryKind::AgentText(text)
                }
                EntryKind::ToolCall {
                    id,
                    name,
                    state,
                    output,
                } => crate::model::AssistantEntryKind::ToolCall {
                    id,
                    name,
                    status: state,
                    output,
                },
                EntryKind::Status { text } => crate::model::AssistantEntryKind::Status(text),
                EntryKind::ChangeSummary { files } => {
                    crate::model::AssistantEntryKind::ChangeSummary { files }
                }
            },
        }
    }

    #[must_use]
    pub fn markdown(&self, agent_name: &str) -> String {
        self.clone().to_model().details_markdown(agent_name)
    }
}

impl ThreadView {
    #[must_use]
    pub fn entry(&self, id: thread::EntryId) -> Option<&EntryView> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    #[must_use]
    pub fn selected_entry(&self) -> Option<thread::EntryId> {
        self.selected
    }

    #[must_use]
    pub fn entry_at(&self, index: usize) -> Option<thread::EntryId> {
        self.entries.get(index).map(|entry| entry.id)
    }

    #[must_use]
    pub fn is_folded(&self, entry: thread::EntryId) -> bool {
        self.folded.contains(&entry)
    }

    #[must_use]
    pub fn opened_doc(&self, entry: thread::EntryId) -> Option<DocumentId> {
        self.opened_docs.get(&entry).copied()
    }
}

impl Panel {
    #[must_use]
    pub fn active_entry(&self, id: thread::EntryId) -> Option<&EntryView> {
        self.active.as_ref()?.entry(id)
    }

    #[must_use]
    pub fn entry_markdown(&self, id: thread::EntryId) -> Option<String> {
        let thread = self.active.as_ref()?;
        let agent_name = thread.title.as_deref().unwrap_or("Agent");
        Some(thread.entry(id)?.markdown(agent_name))
    }

    #[must_use]
    pub fn selected_entry(&self) -> Option<&EntryView> {
        let thread = self.active.as_ref()?;
        thread.selected_entry().and_then(|id| thread.entry(id))
    }

    #[must_use]
    pub fn selected_entry_id(&self) -> Option<thread::EntryId> {
        self.active.as_ref()?.selected_entry()
    }

    #[must_use]
    pub fn selected_entry_locations(&self) -> Option<Vec<Location>> {
        Some(self.selected_entry()?.locations.clone())
    }

    #[must_use]
    pub fn entry_id_at(&self, index: usize) -> Option<thread::EntryId> {
        self.active.as_ref()?.entry_at(index)
    }

    #[must_use]
    pub fn is_entry_folded(&self, entry: thread::EntryId) -> bool {
        self.active
            .as_ref()
            .is_some_and(|thread| thread.is_folded(entry))
    }

    #[must_use]
    pub fn opened_doc(&self, entry: thread::EntryId) -> Option<DocumentId> {
        self.active.as_ref()?.opened_doc(entry)
    }

    #[must_use]
    pub fn active_id(&self) -> Option<thread::Id> {
        Some(self.active.as_ref()?.id)
    }
}

impl Tab {
    #[must_use]
    pub fn to_model(self) -> crate::model::AssistantTab {
        crate::model::AssistantTab {
            id: self.id,
            title: self.title,
            run: self.run,
            unread: self.unread,
            follow: self.follow.to_model(),
        }
    }
}

impl HistoryThread {
    #[must_use]
    pub fn to_model(self) -> crate::model::AssistantHistoryEntry {
        crate::model::AssistantHistoryEntry {
            id: self.id,
            title: self.title,
            unread: self.unread,
            run: self.run,
        }
    }
}

impl Terminal {
    #[must_use]
    pub fn to_model(self) -> crate::model::AssistantTerminal {
        crate::model::AssistantTerminal {
            id: self.id.to_string(),
            title: self.title,
            state: self.state,
            output: self.output,
        }
    }
}

impl Store {
    #[must_use]
    pub fn model(
        &self,
        focused: bool,
        history: Vec<history::Stub>,
        fallback_agent_name: String,
    ) -> crate::model::AssistantModel {
        let panel = self.panel(focused);
        let tabs = panel.tabs.into_iter().map(Tab::to_model).collect();
        let history = history
            .into_iter()
            .map(|entry| HistoryThread {
                id: entry.id,
                title: entry.title,
                unread: entry.unread,
                run: entry.run,
            })
            .map(HistoryThread::to_model)
            .collect();

        if let Some(active) = panel.active {
            crate::model::AssistantModel {
                tabs,
                history,
                active_thread: Some(active.id),
                entries: active
                    .entries
                    .into_iter()
                    .map(EntryView::to_model)
                    .collect(),
                agent_busy: matches!(active.run, thread::Run::Running | thread::Run::Waiting),
                agent_status: match &active.run {
                    thread::Run::Running => Some("working".to_string()),
                    thread::Run::Waiting => Some("canceling".to_string()),
                    thread::Run::Failed { message } => Some(format!("failed: {message}")),
                    thread::Run::Idle => None,
                },
                input: active.draft,
                context_items: active
                    .context
                    .into_iter()
                    .map(|item| crate::model::AssistantContextItem {
                        id: item.id,
                        label: item.label,
                    })
                    .collect(),
                plan_items: Some(
                    active
                        .plan
                        .into_iter()
                        .map(|item| crate::model::AssistantPlanItem {
                            content: item.content,
                            status: match item.status {
                                plan::Status::Pending => crate::model::AssistantPlanStatus::Pending,
                                plan::Status::InProgress => {
                                    crate::model::AssistantPlanStatus::InProgress
                                }
                                plan::Status::Completed => {
                                    crate::model::AssistantPlanStatus::Completed
                                }
                                plan::Status::Failed => crate::model::AssistantPlanStatus::Failed,
                            },
                        })
                        .collect(),
                )
                .filter(|items: &Vec<_>| !items.is_empty()),
                queued_messages: 0,
                selected_entry: active.selected,
                focus: Some(active.focus),
                folded_entries: active.folded,
                opened_docs: active.opened_docs,
                content_scroll: active.content_scroll,
                mode_name: active.mode_name,
                model_label: active.model_label,
                follow: Some(active.follow.to_model()),
                agent_name: active.title.unwrap_or_else(|| "Agent".to_string()),
                agent_version: String::new(),
                focused,
                insert_mode: false,
                error: None,
                input_cursor: 0,
                viewport_scroll: 0,
                viewport_max_scroll: 0,
                terminals: active
                    .terminals
                    .into_iter()
                    .map(Terminal::to_model)
                    .collect(),
            }
        } else {
            crate::model::AssistantModel {
                tabs,
                history,
                active_thread: None,
                entries: Vec::new(),
                viewport_scroll: 0,
                viewport_max_scroll: 0,
                selected_entry: None,
                focus: None,
                folded_entries: Vec::new(),
                opened_docs: HashMap::new(),
                content_scroll: 0,
                mode_name: None,
                model_label: None,
                follow: None,
                agent_name: fallback_agent_name,
                agent_version: String::new(),
                agent_busy: false,
                agent_status: None,
                focused,
                insert_mode: false,
                error: None,
                input: String::new(),
                context_items: Vec::new(),
                input_cursor: 0,
                plan_items: None,
                queued_messages: 0,
                terminals: Vec::new(),
            }
        }
    }

    #[must_use]
    pub fn history_model(&self, scope: &thread::Scope) -> Option<History> {
        self.history(scope).map(|page| History {
            scope: page.scope.clone(),
            entries: page
                .entries
                .iter()
                .map(|entry| HistoryThread {
                    id: entry.id,
                    title: entry.title.clone(),
                    unread: entry.unread,
                    run: entry.run.clone(),
                })
                .collect(),
            next: page.next.clone(),
        })
    }

    #[must_use]
    pub fn panel(&self, focused: bool) -> Panel {
        let tabs: Vec<_> = self
            .threads()
            .map(|thread| Tab {
                id: thread.id,
                title: thread
                    .title()
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| format!("thread {}", thread.id.value())),
                run: thread.run().clone(),
                unread: thread.unread(),
                follow: match thread.follow() {
                    crate::collab::FollowState::Off => Follow::Off,
                    crate::collab::FollowState::On { .. } => Follow::On,
                    crate::collab::FollowState::Paused { .. } => Follow::Paused,
                },
            })
            .collect();

        let active = self
            .active()
            .and_then(|id| self.thread(id))
            .map(|thread| ThreadView {
                id: thread.id,
                title: thread.title().map(ToOwned::to_owned),
                is_remote: matches!(thread.origin(), thread::Origin::Backend { .. }),
                entries: thread
                    .entries()
                    .iter()
                    .map(|entry| EntryView {
                        id: entry.id,
                        locations: {
                            let mut locations = entry.locations.clone();
                            if let thread::EntryKind::ChangeSummary(summary) = &entry.kind {
                                locations.extend(
                                    summary
                                        .locations()
                                        .into_iter()
                                        .map(|location| location.for_entry(entry.id)),
                                );
                            }
                            locations
                        },
                        kind: match &entry.kind {
                            thread::EntryKind::UserPrompt { text } => {
                                EntryKind::UserPrompt { text: text.clone() }
                            }
                            thread::EntryKind::AssistantText { text } => {
                                EntryKind::AssistantText { text: text.clone() }
                            }
                            thread::EntryKind::ToolCall(call) => EntryKind::ToolCall {
                                id: call.id.to_string(),
                                name: call.name.clone(),
                                state: match &call.state {
                                    super::tool::State::Pending => "pending".to_string(),
                                    super::tool::State::Running => "running".to_string(),
                                    super::tool::State::Completed => "completed".to_string(),
                                    super::tool::State::Failed { .. } => "failed".to_string(),
                                    super::tool::State::Canceled => "cancelled".to_string(),
                                    super::tool::State::Unknown(value) => value.to_string(),
                                },
                                output: call.output.clone(),
                            },
                            thread::EntryKind::Status { text } => {
                                EntryKind::Status { text: text.clone() }
                            }
                            thread::EntryKind::ChangeSummary(summary) => EntryKind::ChangeSummary {
                                files: summary.files.len(),
                            },
                        },
                    })
                    .collect(),
                draft: thread.draft().to_string(),
                context: thread
                    .context_items()
                    .iter()
                    .map(|item| Pill {
                        id: item.id.clone(),
                        label: match &item.kind {
                            context::Kind::Selection(sel) => {
                                sel.label.clone().unwrap_or_else(|| "selection".to_string())
                            }
                            context::Kind::Symbol(sym) => sym.name.clone(),
                            context::Kind::File(file) => file.path.display().to_string(),
                            context::Kind::Diagnostics(_) => "diagnostics".to_string(),
                            context::Kind::Diff(_) => "diff".to_string(),
                        },
                    })
                    .collect(),
                run: thread.run().clone(),
                unread: thread.unread(),
                focus: thread.focus(),
                follow: match thread.follow() {
                    crate::collab::FollowState::Off => Follow::Off,
                    crate::collab::FollowState::On { .. } => Follow::On,
                    crate::collab::FollowState::Paused { .. } => Follow::Paused,
                },
                mode_name: thread.mode().map(|mode| match mode.selected() {
                    mode::Selected::Current(id) => mode
                        .item(id)
                        .map(|item| item.name.clone())
                        .unwrap_or_else(|| id.to_string()),
                    mode::Selected::Pending { next, .. } => mode
                        .item(next)
                        .map(|item| item.name.clone())
                        .unwrap_or_else(|| next.to_string()),
                }),
                model_label: thread.config().selected_value_label("model"),
                plan: thread.plan().to_vec(),
                selected: thread.selected_entry(),
                folded: thread.folded_entries().collect(),
                opened_docs: thread.opened_docs().clone(),
                content_scroll: thread.content_scroll(),
                terminals: thread
                    .terminals()
                    .iter()
                    .map(|terminal| Terminal {
                        id: terminal.id.clone(),
                        title: terminal.title.clone(),
                        state: match &terminal.state {
                            super::terminal::State::Running => "running".to_string(),
                            super::terminal::State::Exited { code } => format!("exited:{code}"),
                            super::terminal::State::Failed { message } => {
                                format!("failed:{message}")
                            }
                        },
                        output: terminal.output.clone(),
                    })
                    .collect(),
            });

        Panel {
            tabs,
            active,
            focused,
        }
    }
}
