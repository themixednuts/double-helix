use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwapOption;
use helix_runtime::{LatestAdmissionError, LatestByKeySender};
use helix_view::assistant::{auth, elicitation, thread};
use helix_view::graphics::Style;
use helix_view::model::{
    AssistantEntry, AssistantEntryDisplay, AssistantEntryKind, AssistantEntryRow,
    AssistantEntryTone, AssistantModel, AssistantTextFormat,
};
use helix_view::theme::{Modifier, Theme};
use tui::text::{Span, Spans};

use crate::ui::markdown::{fit_bubble_width, wrap_text};
use crate::widgets::{Message, MessageAccessoryAlign, MessageAlign, MessageCorners, MessageStyle};

use super::markdown_service::{RequestKey as MarkdownRequestKey, Snapshot as MarkdownSnapshot};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RequestKey {
    pub thread: thread::Id,
    pub content_revision: u64,
    pub width: u16,
    pub theme_generation: u64,
    pub folded: Arc<[thread::EntryId]>,
    pub selected: Option<thread::EntryId>,
    pub editing: Option<thread::EntryId>,
    pub form: Option<elicitation::FormState>,
    pub auth: auth::State,
    pub auth_selected: usize,
    pub elicitations: Arc<[thread::Elicitation]>,
    terminals: Arc<[TerminalPresentation]>,
    agent_name: Arc<str>,
    corners: MessageCorners,
    pub markdown_generation: Option<u64>,
}

impl RequestKey {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model: &AssistantModel,
        width: u16,
        theme_generation: u64,
        editing: Option<thread::EntryId>,
        form: Option<&elicitation::FormState>,
        auth_selected: usize,
        corners: MessageCorners,
        markdown_generation: Option<u64>,
    ) -> Option<Self> {
        Some(Self {
            thread: model.active_thread?,
            content_revision: model.content_revision,
            width,
            theme_generation,
            folded: model.folded_entries.clone().into(),
            selected: model.selected_entry_id(),
            editing,
            form: form.cloned(),
            auth: model.auth.clone(),
            auth_selected,
            elicitations: model.pending_elicitations.clone().into(),
            terminals: model
                .terminals
                .iter()
                .map(TerminalPresentation::from_model)
                .collect::<Vec<_>>()
                .into(),
            agent_name: Arc::from(model.agent_name.as_str()),
            corners,
            markdown_generation,
        })
    }

    pub fn geometrically_compatible(&self, other: &Self) -> bool {
        self.thread == other.thread
            && self.width == other.width
            && self.theme_generation == other.theme_generation
            && self.folded == other.folded
            && self.form == other.form
            && self.auth == other.auth
            && self.auth_selected == other.auth_selected
            && self.elicitations == other.elicitations
            && self.terminals == other.terminals
            && self.agent_name == other.agent_name
            && self.corners == other.corners
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalPresentation {
    id: String,
    title: Option<String>,
    state: String,
    output_lines: Arc<[String]>,
}

impl TerminalPresentation {
    fn from_model(terminal: &helix_view::model::AssistantTerminal) -> Self {
        Self {
            id: terminal.id.clone(),
            title: terminal.title.clone(),
            state: terminal.state.clone(),
            output_lines: terminal
                .output
                .lines()
                .rev()
                .take(8)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .into(),
        }
    }
}

pub(super) struct Source {
    pub key: RequestKey,
    pub entries: Arc<[AssistantEntry]>,
    pub markdown_key: MarkdownRequestKey,
    pub markdown: Option<Arc<MarkdownSnapshot>>,
}

#[derive(Debug, Clone)]
pub(super) struct Metadata {
    pub total_height: usize,
    entry_ids: Arc<[Option<thread::EntryId>]>,
    thought_rows: Arc<[(thread::EntryId, usize)]>,
    running_tool_row: Option<usize>,
}

impl Metadata {
    pub fn selected_index(&self, selected: Option<thread::EntryId>) -> Option<usize> {
        let selected = selected?;
        self.entry_ids
            .iter()
            .position(|entry| *entry == Some(selected))
    }

    pub fn entry_id_at(&self, index: usize) -> Option<thread::EntryId> {
        self.entry_ids.get(index).copied().flatten()
    }

    pub fn active_animation(
        &self,
        active_thought: Option<thread::EntryId>,
        agent_busy: bool,
    ) -> Option<usize> {
        if !agent_busy {
            return None;
        }
        active_thought
            .and_then(|active| {
                self.thought_rows
                    .iter()
                    .find_map(|(entry, row)| (*entry == active).then_some(*row))
            })
            .or(self.running_tool_row)
    }
}

pub(super) struct Snapshot {
    key: RequestKey,
    messages: Arc<[Message]>,
    metadata: Metadata,
}

impl Snapshot {
    pub fn matches(&self, key: &RequestKey) -> bool {
        &self.key == key
    }

    pub fn geometrically_compatible(&self, key: &RequestKey) -> bool {
        self.key.geometrically_compatible(key)
    }

    pub fn messages(&self) -> Arc<[Message]> {
        Arc::clone(&self.messages)
    }

    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }
}

struct Request {
    generation: u64,
    source: Source,
    theme: Arc<Theme>,
}

struct WorkResult {
    snapshot: Snapshot,
    complete: bool,
}

pub(super) struct PresentationService {
    tx: LatestByKeySender<(), Request>,
    next_generation: u64,
    latest_generation: Arc<AtomicU64>,
    requested: Option<RequestKey>,
    requested_generation: u64,
    failed_generation: Arc<AtomicU64>,
    snapshot: Arc<ArcSwapOption<Snapshot>>,
}

impl PresentationService {
    pub fn spawn(
        work: helix_runtime::Work,
        block: helix_runtime::Block,
        redraw: helix_runtime::FrameHandle,
    ) -> Self {
        let (tx, mut rx) = helix_runtime::latest_by_key::<(), Request>(1);
        let latest_generation = Arc::new(AtomicU64::new(0));
        let actor_latest = Arc::clone(&latest_generation);
        let failed_generation = Arc::new(AtomicU64::new(0));
        let actor_failed = Arc::clone(&failed_generation);
        let snapshot = Arc::new(ArcSwapOption::empty());
        let actor_snapshot = Arc::clone(&snapshot);

        work.spawn(async move {
            while let Some(((), request)) = rx.recv().await {
                let generation = request.generation;
                let revision = request.source.key.content_revision;
                let entry_count = request.source.entries.len();
                let width = request.source.key.width;
                let worker_latest = Arc::clone(&actor_latest);
                let started = std::time::Instant::now();
                let result = block
                    .spawn(move || render(request, &worker_latest))
                    .await;
                helix_view::bench::log_run_phase(
                    "assistant_presentation_actor",
                    "layout",
                    started.elapsed(),
                    || {
                        format!(
                            "generation={generation} revision={revision} entries={entry_count} width={width}"
                        )
                    },
                );
                let Ok(result) = result else {
                    actor_failed.store(generation, Ordering::Release);
                    log::error!("assistant presentation worker failed generation={generation}");
                    redraw.request_redraw();
                    continue;
                };
                if publish_if_latest(&actor_snapshot, &actor_latest, generation, result) {
                    redraw.request_redraw();
                }
            }
        })
        .detach();

        Self {
            tx,
            next_generation: 1,
            latest_generation,
            requested: None,
            requested_generation: 0,
            failed_generation,
            snapshot,
        }
    }

    pub fn needs(&self, key: &RequestKey) -> bool {
        self.requested.as_ref() != Some(key)
            || self.failed_generation.load(Ordering::Acquire) == self.requested_generation
    }

    pub fn submit(&mut self, source: Source, theme: Arc<Theme>) {
        if !self.needs(&source.key) {
            return;
        }

        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let previous = self.latest_generation.swap(generation, Ordering::AcqRel);
        let key = source.key.clone();
        let request = Request {
            generation,
            source,
            theme,
        };
        match self.tx.try_send((), request) {
            Ok(_) => {
                self.requested = Some(key);
                self.requested_generation = generation;
            }
            Err(LatestAdmissionError::Full((), _)) => {
                let _ = self.latest_generation.compare_exchange(
                    generation,
                    previous,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                log::error!("assistant presentation admission invariant was violated");
            }
            Err(LatestAdmissionError::Closed((), _)) => {
                let _ = self.latest_generation.compare_exchange(
                    generation,
                    previous,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                log::error!("assistant presentation service is closed");
            }
        }
    }

    pub fn snapshot(&self) -> Option<Arc<Snapshot>> {
        self.snapshot.load_full()
    }
}

fn publish_if_latest(
    destination: &ArcSwapOption<Snapshot>,
    latest: &AtomicU64,
    generation: u64,
    result: WorkResult,
) -> bool {
    if !result.complete || latest.load(Ordering::Acquire) != generation {
        return false;
    }
    destination.store(Some(Arc::new(result.snapshot)));
    true
}

fn render(request: Request, latest: &AtomicU64) -> WorkResult {
    let mut messages = Vec::with_capacity(
        request.source.entries.len()
            + request.source.key.elicitations.len()
            + request.source.key.terminals.len()
            + 1,
    );
    let mut entry_ids = Vec::with_capacity(messages.capacity());
    let mut thought_rows = Vec::new();
    let mut running_tool_row = None;
    let key = &request.source.key;
    let theme = &request.theme;
    let border_style = accent_style(theme.get("ui.window"));
    let agent_style = theme.get("ui.text.info");
    let user_label_style = theme.get("keyword").add_modifier(Modifier::BOLD);
    let agent_label_style = theme
        .try_get("ui.assistant.agent.label")
        .unwrap_or_else(|| theme.get("ui.text.info").add_modifier(Modifier::BOLD));
    let user_text_style = theme.get("ui.text");
    let (min_bubble, max_bubble) = bubble_width_range(key.width);

    let mut complete = true;
    for entry in request.source.entries.iter() {
        if latest.load(Ordering::Acquire) != request.generation {
            complete = false;
            break;
        }
        let message_index = messages.len();
        let collapsed = key.folded.contains(&entry.id);
        let message = match &entry.kind {
            AssistantEntryKind::ToolCall {
                name,
                status,
                output,
                ..
            } => {
                if status == "running" {
                    running_tool_row = Some(message_index);
                }
                tool_call_message(
                    name,
                    status,
                    output,
                    status == "failed" || collapsed,
                    theme,
                    request.generation,
                    latest,
                )
            }
            AssistantEntryKind::ReviewSummary { mode, files } => {
                review_message(*mode, files, collapsed, theme, request.generation, latest)
            }
            _ => {
                let display = entry.display(&key.agent_name);
                match display {
                    AssistantEntryDisplay::Bubble(display) => {
                        let bubble_width = fit_bubble_width(
                            &display.text,
                            min_bubble as usize,
                            max_bubble as usize,
                        ) as u16;
                        let inner_width = bubble_width.saturating_sub(4) as usize;
                        let (label_style, content_lines) = match display.format {
                            AssistantTextFormat::Plain => {
                                let wrapped = if collapsed {
                                    vec![collapse_preview(&display.text, inner_width)]
                                } else {
                                    wrap_text(&display.text, inner_width)
                                };
                                let lines = wrapped
                                    .into_iter()
                                    .map(|line| Spans::from(Span::styled(line, user_text_style)))
                                    .collect::<Vec<_>>();
                                (user_label_style, Arc::from(lines))
                            }
                            AssistantTextFormat::Markdown => {
                                let lines = if collapsed {
                                    Arc::from([Spans::from(Span::styled(
                                        collapse_preview(&display.text, inner_width),
                                        agent_style,
                                    ))])
                                } else {
                                    request
                                        .source
                                        .markdown
                                        .as_ref()
                                        .and_then(|snapshot| {
                                            snapshot.lines(&request.source.markdown_key, entry.id)
                                        })
                                        .unwrap_or_else(|| {
                                            Arc::from([Spans::from(Span::styled(
                                                collapse_preview(&display.text, inner_width),
                                                agent_style,
                                            ))])
                                        })
                                };
                                (agent_label_style, lines)
                            }
                        };
                        Some(Message::bubble(
                            Some((display.meta.heading, label_style)),
                            content_lines,
                            bubble_width,
                            bubble_message_align(display.meta.side),
                            MessageStyle {
                                border: border_style,
                                corners: key.corners,
                                accent: None,
                                accent_progress: 0.0,
                            },
                        ))
                    }
                    AssistantEntryDisplay::Plain(row) => {
                        if matches!(&entry.kind, AssistantEntryKind::Thought(_)) {
                            thought_rows.push((entry.id, message_index));
                        }
                        Some(plain_row_message(&row, theme, key.width))
                    }
                }
            }
        };
        let Some(message) = message else {
            complete = false;
            break;
        };
        messages.push(message);
        entry_ids.push(Some(entry.id));
    }

    if complete {
        for elicitation in key
            .elicitations
            .iter()
            .filter(|item| item.status == thread::ElicitationStatus::Pending)
        {
            messages.push(elicitation_message(elicitation, key.form.as_ref(), theme));
            entry_ids.push(None);
        }
        if let Some(message) = auth_message(&key.auth, key.auth_selected, theme) {
            messages.push(message);
            entry_ids.push(None);
        }
        for terminal in key.terminals.iter() {
            messages.push(terminal_message(terminal, theme));
            entry_ids.push(None);
        }
    }

    complete &= latest.load(Ordering::Acquire) == request.generation;
    let selected_index = key
        .selected
        .and_then(|selected| entry_ids.iter().position(|entry| *entry == Some(selected)));
    let total_height = messages
        .iter()
        .enumerate()
        .map(|(index, message)| message.height(selected_index == Some(index)) as usize + 1)
        .sum();
    WorkResult {
        snapshot: Snapshot {
            key: key.clone(),
            messages: messages.into(),
            metadata: Metadata {
                total_height,
                entry_ids: entry_ids.into(),
                thought_rows: thought_rows.into(),
                running_tool_row,
            },
        },
        complete,
    }
}

fn bubble_width_range(width: u16) -> (u16, u16) {
    let max = ((width as u32 * 90 / 100) as u16).min(width).max(4);
    let min = ((width as u32 * 60 / 100) as u16).max(20).min(max);
    (min, max)
}

fn accent_style(style: Style) -> Style {
    let mut accent = Style::default();
    if let Some(fg) = style.fg {
        accent = accent.fg(fg);
    }
    if let Some(bg) = style.bg {
        accent = accent.bg(bg);
    }
    if let Some(underline_color) = style.underline_color {
        accent = accent.underline_color(underline_color);
    }
    if let Some(underline_style) = style.underline_style {
        accent = accent.underline_style(underline_style);
    }
    accent
}

fn entry_tone_style(theme: &Theme, tone: AssistantEntryTone) -> Style {
    match tone {
        AssistantEntryTone::Default => theme.get("ui.text"),
        AssistantEntryTone::Inactive => theme.get("ui.text.inactive"),
        AssistantEntryTone::Focus => theme.get("ui.text.focus"),
        AssistantEntryTone::Warning => theme.get("warning"),
        AssistantEntryTone::Success => theme.get("diff.plus"),
        AssistantEntryTone::Error => theme.get("error"),
    }
}

fn bubble_message_align(side: helix_view::model::AssistantBubbleSide) -> MessageAlign {
    match side {
        helix_view::model::AssistantBubbleSide::Left => MessageAlign::Left,
        helix_view::model::AssistantBubbleSide::Right => MessageAlign::Right,
    }
}

fn plain_row_message(row: &AssistantEntryRow, theme: &Theme, width: u16) -> Message {
    let body_style = entry_tone_style(theme, row.body_tone);
    let max_width = usize::from(width.max(1));
    let lines = if row.leading.is_empty() {
        wrap_text(&row.body, max_width)
            .into_iter()
            .map(|line| Spans::from(Span::styled(line, body_style)))
            .collect::<Vec<_>>()
    } else {
        let leading_style = entry_tone_style(theme, row.leading_tone);
        let leading_width =
            helix_core::unicode::width::UnicodeWidthStr::width(row.leading.as_str()).min(max_width);
        let body_width = max_width.saturating_sub(leading_width).max(1);
        let mut body_lines = wrap_text(&row.body, body_width);
        if body_lines.is_empty() {
            body_lines.push(String::new());
        }
        let indent = " ".repeat(leading_width);
        body_lines
            .into_iter()
            .enumerate()
            .map(|(index, line)| {
                if index == 0 {
                    Spans::from(vec![
                        Span::styled(row.leading.clone(), leading_style),
                        Span::styled(line, body_style),
                    ])
                } else {
                    Spans::from(vec![
                        Span::styled(indent.clone(), leading_style),
                        Span::styled(line, body_style),
                    ])
                }
            })
            .collect()
    };
    let mut message = Message::plain(lines);
    if let Some(accessory) = &row.accessory {
        message = message.with_accessory(
            vec![Spans::from(Span::styled(
                accessory.clone(),
                entry_tone_style(theme, row.accessory_tone),
            ))],
            MessageAccessoryAlign::Right,
        );
    }
    message
}

fn tool_call_message(
    name: &str,
    status: &str,
    output: &str,
    expanded: bool,
    theme: &Theme,
    generation: u64,
    latest: &AtomicU64,
) -> Option<Message> {
    let icon = AssistantEntry::status_icon(status);
    let icon_style = entry_tone_style(theme, AssistantEntry::status_tone(status));
    let title_style = theme.get("ui.text.focus").add_modifier(Modifier::BOLD);
    let muted_style = theme.get("ui.text.inactive");
    let summary = if output.is_empty() {
        status.to_string()
    } else {
        collapse_preview(output, 96)
    };
    let mut lines = vec![Spans::from(vec![
        Span::styled(format!(" {icon} "), icon_style),
        Span::styled(name.to_string(), title_style),
        Span::styled(format!("  {summary}"), muted_style),
    ])];
    if expanded && !output.is_empty() {
        let plus = theme.get("diff.plus");
        let minus = theme.get("diff.minus");
        let delta = theme.get("diff.delta");
        let text = theme.get("ui.text");
        for (index, line) in output.lines().enumerate() {
            if index % 64 == 0 && latest.load(Ordering::Acquire) != generation {
                return None;
            }
            let style = if line.starts_with('+') && !line.starts_with("+++") {
                plus
            } else if line.starts_with('-') && !line.starts_with("---") {
                minus
            } else if line.starts_with("@@") {
                delta
            } else {
                text
            };
            lines.push(Spans::from(Span::styled(format!("   {line}"), style)));
        }
    }
    Some(Message::plain(lines))
}

fn review_message(
    mode: helix_view::assistant::review::Mode,
    files: &[(
        std::path::PathBuf,
        String,
        helix_view::assistant::review::Status,
    )],
    expanded: bool,
    theme: &Theme,
    generation: u64,
    latest: &AtomicU64,
) -> Option<Message> {
    let title_style = theme.get("ui.text.focus").add_modifier(Modifier::BOLD);
    let muted_style = theme.get("ui.text.inactive");
    let pending_style = theme.get("warning");
    let diff_styles = crate::widgets::DiffStyles::from_theme(theme);
    let pending = files
        .iter()
        .filter(|(_, _, status)| status.is_pending())
        .count();
    let mut lines = vec![Spans::from(vec![
        Span::styled(" review ", title_style),
        Span::styled(mode.label().to_string(), muted_style),
        Span::styled(format!("  {} files", files.len()), muted_style),
        Span::styled(format!("  {pending} pending"), pending_style),
    ])];
    for (path, diff, status) in files {
        if latest.load(Ordering::Acquire) != generation {
            return None;
        }
        lines.push(Spans::from(vec![
            Span::styled("   ", muted_style),
            Span::styled(path.display().to_string(), title_style),
            Span::styled(format!("  {}", status.label()), muted_style),
        ]));
        if expanded {
            let doc = crate::widgets::DiffDocument::from_unified_diff(diff);
            let diff_lines = doc.layout_lines(
                &diff_styles,
                crate::widgets::DiffOptions {
                    context: 3,
                    line_numbers: false,
                },
                &BTreeSet::new(),
            );
            for line in diff_lines {
                let mut spans = vec![Span::styled("     ".to_string(), muted_style)];
                spans.extend(line.0);
                lines.push(Spans::from(spans));
            }
        }
    }
    Some(Message::plain(lines))
}

fn elicitation_message(
    elicitation: &thread::Elicitation,
    form: Option<&elicitation::FormState>,
    theme: &Theme,
) -> Message {
    let title_style = theme.get("ui.text.focus").add_modifier(Modifier::BOLD);
    let muted_style = theme.get("ui.text.inactive");
    let warning_style = theme.get("warning");
    let mut lines = Vec::new();
    match &elicitation.mode {
        thread::ElicitationMode::Form { message, fields } => {
            lines.push(Spans::from(vec![
                Span::styled(" ? ", warning_style),
                Span::styled(message.clone(), title_style),
                Span::styled("  tab field  enter submit  esc cancel", muted_style),
            ]));
            let form = form.filter(|form| form.request_id() == elicitation.id.as_str());
            for (index, field) in fields.iter().enumerate() {
                let required = if field.required { " *" } else { "" };
                let marker = if form.is_some_and(|form| form.focused() == index) {
                    " > "
                } else {
                    "   "
                };
                let value = match (form.and_then(|form| form.value(index)), field.field_type) {
                    (
                        Some(elicitation::FieldValue::Text(text)),
                        thread::ElicitationFieldType::Text | thread::ElicitationFieldType::Textarea,
                    ) => text.clone(),
                    (
                        Some(elicitation::FieldValue::Select(selected)),
                        thread::ElicitationFieldType::Select,
                    ) => field
                        .options
                        .get(*selected)
                        .map(|option| option.label.clone())
                        .unwrap_or_else(|| "<none>".to_string()),
                    (
                        Some(elicitation::FieldValue::Bool(value)),
                        thread::ElicitationFieldType::Bool,
                    ) => {
                        if *value {
                            "yes".to_string()
                        } else {
                            "no".to_string()
                        }
                    }
                    _ => match field.field_type {
                        thread::ElicitationFieldType::Select => field
                            .options
                            .first()
                            .map(|option| option.label.clone())
                            .unwrap_or_else(|| "<none>".to_string()),
                        thread::ElicitationFieldType::Bool => "no".to_string(),
                        thread::ElicitationFieldType::Text
                        | thread::ElicitationFieldType::Textarea => String::new(),
                    },
                };
                let label = field.label.as_deref().unwrap_or(&field.name);
                lines.push(Spans::from(Span::styled(
                    format!("{marker}{label}{required}: {value}"),
                    if marker.trim().is_empty() {
                        muted_style
                    } else {
                        title_style
                    },
                )));
            }
        }
        thread::ElicitationMode::Url { message, url } => {
            lines.push(Spans::from(vec![
                Span::styled(" ? ", warning_style),
                Span::styled(message.clone(), title_style),
                Span::styled("  y copy  esc cancel", muted_style),
            ]));
            lines.push(Spans::from(Span::styled(format!("   {url}"), muted_style)));
        }
    }
    Message::plain(lines)
}

fn auth_message(state: &auth::State, auth_selected: usize, theme: &Theme) -> Option<Message> {
    let (methods, error, authenticating) = match state {
        auth::State::Required { methods, error, .. } => {
            (methods.as_slice(), error.as_deref(), None)
        }
        auth::State::Failed { methods, error, .. } => {
            (methods.as_slice(), Some(error.as_str()), None)
        }
        auth::State::Authenticating { method, .. } => (&[][..], None, Some(method.name.as_str())),
        _ => return None,
    };
    let title_style = theme.get("ui.text.focus").add_modifier(Modifier::BOLD);
    let muted_style = theme.get("ui.text.inactive");
    let warning_style = theme.get("warning");
    let mut lines = vec![Spans::from(vec![
        Span::styled(" ! ", warning_style),
        Span::styled("Authentication required", title_style),
        Span::styled("  j/k select  enter authenticate", muted_style),
    ])];
    if let Some(name) = authenticating {
        lines.push(Spans::from(Span::styled(
            format!("   Authenticating with {name}..."),
            muted_style,
        )));
    }
    if let Some(error) = error {
        lines.push(Spans::from(Span::styled(
            format!("   {error}"),
            theme.get("error"),
        )));
    }
    for (index, method) in methods.iter().enumerate() {
        let selected = index == auth_selected.min(methods.len().saturating_sub(1));
        let marker = if selected { " > " } else { "   " };
        let suffix = if method.terminal.is_some() {
            " terminal"
        } else {
            ""
        };
        lines.push(Spans::from(Span::styled(
            format!("{marker}{}{suffix}", method.name),
            if selected { title_style } else { muted_style },
        )));
    }
    Some(Message::plain(lines))
}

fn terminal_message(terminal: &TerminalPresentation, theme: &Theme) -> Message {
    let title_style = theme.get("ui.text.focus").add_modifier(Modifier::BOLD);
    let muted_style = theme.get("ui.text.inactive");
    let status_style = match terminal.state.as_str() {
        "running" => theme.get("warning"),
        state if state.starts_with("exited:0") => theme.get("diff.plus"),
        state if state.starts_with("failed:") || state.starts_with("exited:") => theme.get("error"),
        _ => muted_style,
    };
    let mut lines = vec![Spans::from(vec![
        Span::styled(" $ ", status_style),
        Span::styled(
            terminal
                .title
                .clone()
                .unwrap_or_else(|| terminal.id.clone()),
            title_style,
        ),
        Span::styled(format!("  {}", terminal.state), status_style),
    ])];
    lines.extend(
        terminal
            .output_lines
            .iter()
            .map(|line| Spans::from(Span::styled(format!("   {line}"), muted_style))),
    );
    Message::plain(lines)
}

fn collapse_preview(text: &str, width: usize) -> String {
    let width = width.max(1);
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if helix_core::unicode::width::UnicodeWidthStr::width(compact.as_str()) <= width {
        return compact;
    }
    let mut preview = String::new();
    for character in compact.chars() {
        let next_width = helix_core::unicode::width::UnicodeWidthStr::width(preview.as_str())
            + helix_core::unicode::width::UnicodeWidthChar::width(character).unwrap_or(0);
        if next_width >= width {
            break;
        }
        preview.push(character);
    }
    preview.push('…');
    preview
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;

    fn id(value: u64) -> thread::Id {
        thread::Id::new(NonZeroU64::new(value).unwrap())
    }

    fn key(thread: thread::Id, width: u16, revision: u64) -> RequestKey {
        RequestKey {
            thread,
            content_revision: revision,
            width,
            theme_generation: 1,
            folded: Arc::from([]),
            selected: None,
            editing: None,
            form: None,
            auth: auth::State::default(),
            auth_selected: 0,
            elicitations: Arc::from([]),
            terminals: Arc::from([]),
            agent_name: Arc::from("Agent"),
            corners: MessageCorners::Squared,
            markdown_generation: None,
        }
    }

    fn result(key: RequestKey) -> WorkResult {
        WorkResult {
            snapshot: Snapshot {
                key,
                messages: Arc::from([]),
                metadata: Metadata {
                    total_height: 0,
                    entry_ids: Arc::from([]),
                    thought_rows: Arc::from([]),
                    running_tool_row: None,
                },
            },
            complete: true,
        }
    }

    #[test]
    fn newest_generation_wins_and_stale_result_is_rejected() {
        let destination = ArcSwapOption::empty();
        let latest = AtomicU64::new(2);
        let stale_key = key(id(1), 40, 1);
        let current_key = key(id(1), 40, 2);

        assert!(!publish_if_latest(
            &destination,
            &latest,
            1,
            result(stale_key)
        ));
        assert!(destination.load_full().is_none());
        assert!(publish_if_latest(
            &destination,
            &latest,
            2,
            result(current_key.clone())
        ));
        assert!(destination
            .load_full()
            .is_some_and(|snapshot| snapshot.matches(&current_key)));
    }

    #[test]
    fn fallback_rejects_wrong_thread_width_or_theme() {
        let base = key(id(1), 40, 1);
        let snapshot = result(base.clone()).snapshot;
        let mut changed = base.clone();
        changed.content_revision = 2;
        assert!(snapshot.geometrically_compatible(&changed));

        changed.thread = id(2);
        assert!(!snapshot.geometrically_compatible(&changed));
        changed = base.clone();
        changed.width = 41;
        assert!(!snapshot.geometrically_compatible(&changed));
        changed = base;
        changed.theme_generation = 2;
        assert!(!snapshot.geometrically_compatible(&changed));
    }

    #[test]
    fn animation_choice_does_not_change_presentation_key() {
        let entry = thread::EntryId::new(NonZeroU64::new(3).unwrap());
        let mut model = AssistantModel {
            active_thread: Some(id(1)),
            content_revision: 1,
            ..AssistantModel::default()
        };
        let idle_key =
            RequestKey::new(&model, 40, 1, None, None, 0, MessageCorners::Squared, None).unwrap();
        model.active_thought = Some(entry);
        model.agent_busy = true;
        let active_key =
            RequestKey::new(&model, 40, 1, None, None, 0, MessageCorners::Squared, None).unwrap();
        let metadata = Metadata {
            total_height: 1,
            entry_ids: Arc::from([Some(entry)]),
            thought_rows: Arc::from([(entry, 0)]),
            running_tool_row: None,
        };

        assert_eq!(metadata.active_animation(None, true), None);
        assert_eq!(metadata.active_animation(Some(entry), true), Some(0));
        assert_eq!(idle_key, active_key);
    }
}
