use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Mutex, OnceLock},
    time::{Duration, SystemTime},
};

use helix_pkg::{
    release_age_label, OpEvent, Ops, PackageChange, PackageSpec, PkgKind, Receipt, RegistrySource,
    Source,
};
use helix_view::{
    graphics::{CursorKind, Rect, Style},
    input::{KeyEvent, MouseButton, MouseEventKind},
    keyboard::{KeyCode, KeyModifiers},
    Editor,
};
use nucleo::{
    pattern::{Atom, AtomKind, CaseMatching, Normalization},
    Utf32Str,
};

use crate::compositor::{Component, Context, Event, EventResult, PostAction, RenderContext};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PkgStatusline {
    pub label: String,
    pub percent: Option<u8>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PkgProgressState {
    active: BTreeMap<String, PkgStatusline>,
}

static PKG_PROGRESS_SNAPSHOT: OnceLock<Mutex<PkgProgressState>> = OnceLock::new();

impl PkgProgressState {
    pub fn apply(&mut self, event: &OpEvent) {
        self.apply_inner(event);
        let snapshot =
            PKG_PROGRESS_SNAPSHOT.get_or_init(|| Mutex::new(PkgProgressState::default()));
        if let Ok(mut snapshot) = snapshot.lock() {
            snapshot.apply_inner(event);
        }
    }

    fn apply_inner(&mut self, event: &OpEvent) {
        match event {
            OpEvent::Started { name } => {
                self.active.insert(
                    name.clone(),
                    PkgStatusline {
                        label: format!("pkg {name}"),
                        percent: None,
                    },
                );
            }
            OpEvent::Progress {
                name,
                message,
                percent,
            } => {
                self.active.insert(
                    name.clone(),
                    PkgStatusline {
                        label: format!("pkg {name}: {message}"),
                        percent: *percent,
                    },
                );
            }
            OpEvent::Done { name } | OpEvent::Failed { name, .. } => {
                self.active.remove(name);
            }
        }
    }

    pub fn statusline(&self) -> Option<PkgStatusline> {
        self.active.values().next().cloned()
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }
}

fn progress_snapshot() -> PkgProgressState {
    PKG_PROGRESS_SNAPSHOT
        .get_or_init(|| Mutex::new(PkgProgressState::default()))
        .lock()
        .map(|state| state.clone())
        .unwrap_or_default()
}

const ID: &str = "pkg-manager";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PkgManagerTab {
    Browse,
    Installed,
    Updates,
    Registries,
}

impl PkgManagerTab {
    const ALL: [Self; 4] = [
        Self::Browse,
        Self::Installed,
        Self::Updates,
        Self::Registries,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Browse => "Browse",
            Self::Installed => "Installed",
            Self::Updates => "Updates",
            Self::Registries => "Registries",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0)
    }

    fn from_index(index: usize) -> Option<Self> {
        Self::ALL.get(index).copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowState {
    Installed,
    Available,
    Update,
    Working,
    Problem,
}

impl RowState {
    const fn glyph(self) -> &'static str {
        match self {
            Self::Installed => "●",
            Self::Available => "○",
            Self::Update => "↑",
            Self::Working => "◍",
            Self::Problem => "⚠",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PkgManagerItem {
    pub name: String,
    pub kind: PkgKind,
    pub installed: Option<String>,
    pub latest: String,
    pub description: String,
    pub homepage: Option<String>,
    pub aliases: String,
    pub categories: String,
    pub languages: String,
    pub schemas: String,
    pub source: String,
    pub receipt: Option<Receipt>,
    pub doctor: String,
    pub problem: Option<String>,
    search_blob: String,
}

#[derive(Debug, Clone)]
struct RegistryItem {
    name: String,
    source: String,
    cache_age: String,
    doctor: String,
    problem: Option<String>,
}

#[derive(Debug, Clone)]
struct PkgManagerData {
    entries: Vec<PkgManagerItem>,
    updates: Vec<PackageChange>,
    registries: Vec<RegistryItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PkgRow {
    Section(PkgKind),
    Package(usize),
    Update(usize),
    Registry(usize),
}

#[derive(Debug, Clone, Copy)]
struct PkgManagerStyles {
    background: Style,
    border: Style,
    title: Style,
    text: Style,
    inactive: Style,
    selection: Style,
    installed: Style,
    available: Style,
    warning: Style,
    error: Style,
}

impl PkgManagerStyles {
    fn from_theme(theme: &helix_view::theme::Theme) -> Self {
        let background = theme.get("ui.background");
        let text = theme.get("ui.text");
        let inactive = theme
            .try_get("ui.text.inactive")
            .or_else(|| theme.try_get("comment"))
            .unwrap_or(text);
        Self {
            background,
            border: theme
                .try_get("ui.window")
                .unwrap_or_else(|| theme.get("ui.popup")),
            title: theme.get("ui.statusline"),
            text,
            inactive,
            selection: theme
                .try_get("ui.selection.primary")
                .unwrap_or_else(|| theme.get("ui.selection")),
            installed: theme.try_get("info").unwrap_or(text),
            available: inactive,
            warning: theme.try_get("warning").unwrap_or(text),
            error: theme.try_get("error").unwrap_or(text),
        }
    }

    fn row_state(self, state: RowState, selected: bool) -> Style {
        if selected {
            return self.text;
        }
        match state {
            RowState::Installed => self.installed,
            RowState::Available => self.available,
            RowState::Update | RowState::Working => self.warning,
            RowState::Problem => self.error,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct PkgManagerLayout {
    tabs: Rect,
    search: Rect,
    search_input: Rect,
    list: Rect,
    detail: Rect,
    status: Rect,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct FilterQuery {
    text: String,
    lang: Option<String>,
    kind: Option<PkgKind>,
    category: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PkgAction {
    MoveDown,
    MoveUp,
    Last,
    Search,
    ToggleMark,
    Tab1,
    Tab2,
    Tab3,
    Tab4,
    NextTab,
    PreviousTab,
    CycleKind,
    ToggleDetail,
    Install,
    Remove,
    UpdateSelected,
    ApplyUpdates,
    Rollback,
    Doctor,
    RegistryUpdate,
    Help,
    Escape,
}

#[derive(Debug, Clone, Copy)]
struct BindingKey {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl BindingKey {
    const fn new(code: KeyCode) -> Self {
        Self {
            code,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn matches(self, key: &KeyEvent) -> bool {
        key.code == self.code && key.modifiers == self.modifiers
    }
}

#[derive(Debug, Clone, Copy)]
struct PkgBinding {
    key: BindingKey,
    action: PkgAction,
    hint: Option<(&'static str, &'static str, u16)>,
}

impl PkgBinding {
    const fn new(
        key: BindingKey,
        action: PkgAction,
        hint: Option<(&'static str, &'static str, u16)>,
    ) -> Self {
        Self { key, action, hint }
    }
}

const PKG_BINDINGS: &[PkgBinding] = &[
    PkgBinding::new(
        BindingKey::new(KeyCode::Char('j')),
        PkgAction::MoveDown,
        Some(("j", "down", 300)),
    ),
    PkgBinding::new(BindingKey::new(KeyCode::Down), PkgAction::MoveDown, None),
    PkgBinding::new(
        BindingKey::new(KeyCode::Char('k')),
        PkgAction::MoveUp,
        Some(("k", "up", 299)),
    ),
    PkgBinding::new(BindingKey::new(KeyCode::Up), PkgAction::MoveUp, None),
    PkgBinding::new(
        BindingKey::new(KeyCode::Char('G')),
        PkgAction::Last,
        Some(("G", "last row", 290)),
    ),
    PkgBinding::new(
        BindingKey::new(KeyCode::Char('/')),
        PkgAction::Search,
        Some(("/", "search", 280)),
    ),
    PkgBinding::new(
        BindingKey::new(KeyCode::Char(' ')),
        PkgAction::ToggleMark,
        Some(("space", "mark / accept", 270)),
    ),
    PkgBinding::new(
        BindingKey::new(KeyCode::Char('1')),
        PkgAction::Tab1,
        Some(("1-4", "switch tabs", 260)),
    ),
    PkgBinding::new(BindingKey::new(KeyCode::Char('2')), PkgAction::Tab2, None),
    PkgBinding::new(BindingKey::new(KeyCode::Char('3')), PkgAction::Tab3, None),
    PkgBinding::new(BindingKey::new(KeyCode::Char('4')), PkgAction::Tab4, None),
    PkgBinding::new(
        BindingKey::new(KeyCode::Char(']')),
        PkgAction::NextTab,
        Some(("]", "next tab", 250)),
    ),
    PkgBinding::new(
        BindingKey::new(KeyCode::Char('[')),
        PkgAction::PreviousTab,
        Some(("[", "previous tab", 249)),
    ),
    PkgBinding::new(
        BindingKey::new(KeyCode::Char('f')),
        PkgAction::CycleKind,
        Some(("f", "filter kind", 240)),
    ),
    PkgBinding::new(
        BindingKey::new(KeyCode::Char('p')),
        PkgAction::ToggleDetail,
        Some(("p", "zoom detail", 230)),
    ),
    PkgBinding::new(
        BindingKey::new(KeyCode::Enter),
        PkgAction::Install,
        Some(("enter/i", "install", 220)),
    ),
    PkgBinding::new(
        BindingKey::new(KeyCode::Char('i')),
        PkgAction::Install,
        None,
    ),
    PkgBinding::new(
        BindingKey::new(KeyCode::Char('d')),
        PkgAction::Remove,
        Some(("d", "remove", 210)),
    ),
    PkgBinding::new(
        BindingKey::new(KeyCode::Char('u')),
        PkgAction::UpdateSelected,
        Some(("u", "update / refresh plan", 200)),
    ),
    PkgBinding::new(
        BindingKey::new(KeyCode::Char('U')),
        PkgAction::ApplyUpdates,
        Some(("U", "apply accepted updates", 199)),
    ),
    PkgBinding::new(
        BindingKey::new(KeyCode::Char('r')),
        PkgAction::Rollback,
        Some(("r", "rollback", 190)),
    ),
    PkgBinding::new(
        BindingKey::new(KeyCode::Char('!')),
        PkgAction::Doctor,
        Some(("!", "doctor", 180)),
    ),
    PkgBinding::new(
        BindingKey::new(KeyCode::Char('R')),
        PkgAction::RegistryUpdate,
        Some(("R", "update registry", 170)),
    ),
    PkgBinding::new(
        BindingKey::new(KeyCode::Char('?')),
        PkgAction::Help,
        Some(("?", "help", 160)),
    ),
    PkgBinding::new(
        BindingKey::new(KeyCode::Esc),
        PkgAction::Escape,
        Some(("esc", "clear / close", 150)),
    ),
];

pub struct PkgManager {
    entries: Vec<PkgManagerItem>,
    updates: Vec<PackageChange>,
    registries: Vec<RegistryItem>,
    rows: Vec<PkgRow>,
    tab: PkgManagerTab,
    selection: usize,
    scroll: usize,
    tab_scroll: u16,
    tab_state: crate::widgets::TabsState,
    tab_area: Rect,
    search_query: String,
    search_active: bool,
    kind_filter: Option<PkgKind>,
    marked: BTreeSet<String>,
    accepted_updates: BTreeSet<String>,
    detail_zoom: bool,
    g_pending: bool,
    shown_help: bool,
    ingress: crate::runtime::RuntimeIngress,
}

pub fn manager(
    _editor: &helix_view::Editor,
    ingress: crate::runtime::RuntimeIngress,
) -> anyhow::Result<PkgManager> {
    let ops = Ops::open_default()?;
    let data = load_data(&ops)?;
    Ok(PkgManager::new(data, ingress))
}

fn load_data(ops: &Ops) -> anyhow::Result<PkgManagerData> {
    let receipts: HashMap<(PkgKind, String), Receipt> = ops
        .store()
        .receipts()?
        .into_iter()
        .map(|receipt| ((receipt.kind, receipt.name.clone()), receipt))
        .collect();
    let doctor = ops.doctor().ok();

    let mut entries: Vec<_> = ops
        .registry()
        .iter()
        .map(|package| item_from_package(package, &receipts, doctor.as_ref()))
        .collect();
    entries.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.name.cmp(&right.name))
    });

    let updates = ops.plan_update(&[])?.changes;
    let registries = registry_items(ops, doctor.as_ref());
    Ok(PkgManagerData {
        entries,
        updates,
        registries,
    })
}

fn registry_items(ops: &Ops, report: Option<&helix_pkg::DoctorReport>) -> Vec<RegistryItem> {
    ops.config()
        .registry_sources
        .iter()
        .map(|source| registry_item(source, ops, report))
        .collect()
}

fn registry_item(
    source: &RegistrySource,
    ops: &Ops,
    report: Option<&helix_pkg::DoctorReport>,
) -> RegistryItem {
    let key = format!("registry:{}", source.name);
    let problem = report.and_then(|report| {
        report
            .bad
            .iter()
            .find(|(name, _)| name == &key)
            .map(|(_, message)| message.clone())
    });
    let doctor = if problem.is_some() {
        "problem".to_owned()
    } else if report.is_some_and(|report| report.ok.iter().any(|name| name == &key)) {
        "ok".to_owned()
    } else {
        "unknown".to_owned()
    };
    let cache_age = source
        .active_dir(ops.store())
        .ok()
        .and_then(|path| path.metadata().ok())
        .and_then(|metadata| metadata.modified().ok())
        .map(cache_age_label)
        .unwrap_or_else(|| "-".to_owned());

    RegistryItem {
        name: source.name.clone(),
        source: source.source_label(),
        cache_age,
        doctor,
        problem,
    }
}

fn cache_age_label(modified: SystemTime) -> String {
    let elapsed = SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::ZERO);
    let days = elapsed.as_secs() / 86_400;
    if days > 0 {
        format!("{days}d")
    } else {
        let hours = elapsed.as_secs() / 3_600;
        if hours > 0 {
            format!("{hours}h")
        } else {
            format!("{}m", elapsed.as_secs() / 60)
        }
    }
}

fn item_from_package(
    package: &PackageSpec,
    receipts: &HashMap<(PkgKind, String), Receipt>,
    report: Option<&helix_pkg::DoctorReport>,
) -> PkgManagerItem {
    let receipt = receipts.get(&(package.kind, package.name.clone())).cloned();
    let installed = receipt.as_ref().map(|receipt| receipt.version.clone());
    let latest = package
        .version
        .tag_source
        .as_deref()
        .unwrap_or("registry")
        .to_owned();
    let aliases = package.aliases.join(", ");
    let categories = package.categories.join(", ");
    let languages = package.languages.join(", ");
    let schemas = package
        .schemas
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let source = source_label(package);
    let search_blob = package.search_terms().collect::<Vec<_>>().join("\n");
    let problem = report.and_then(|report| {
        report
            .bad
            .iter()
            .find(|(name, _)| name == &package.name)
            .map(|(_, message)| message.clone())
    });
    let doctor = if problem.is_some() {
        "problem".to_owned()
    } else if report.is_some_and(|report| report.ok.iter().any(|name| name == &package.name)) {
        "ok".to_owned()
    } else if installed.is_some() {
        "unknown".to_owned()
    } else {
        "not installed".to_owned()
    };
    PkgManagerItem {
        name: package.name.clone(),
        kind: package.kind,
        installed,
        latest,
        description: package.description.clone(),
        homepage: package.homepage.clone(),
        aliases,
        categories,
        languages,
        schemas,
        source,
        receipt,
        doctor,
        problem,
        search_blob,
    }
}

fn source_label(package: &PackageSpec) -> String {
    let mut labels = Vec::new();
    let host_artifacts = package
        .artifacts_for(std::env::consts::OS, std::env::consts::ARCH)
        .collect::<Vec<_>>();
    let artifacts = if host_artifacts.is_empty() {
        package.artifacts.iter().collect::<Vec<_>>()
    } else {
        host_artifacts
    };
    for artifact in artifacts {
        if let Some(label) = source_kind(&artifact.source) {
            if !labels.contains(&label) {
                labels.push(label);
            }
        }
    }
    if labels.is_empty() {
        "unknown".to_owned()
    } else {
        labels.join(", ")
    }
}

fn source_kind(source: &Source) -> Option<String> {
    Some(source.kind().to_owned()).filter(|kind| kind != "unknown")
}

impl PkgManager {
    fn new(data: PkgManagerData, ingress: crate::runtime::RuntimeIngress) -> Self {
        let mut manager = Self {
            entries: data.entries,
            updates: data.updates,
            registries: data.registries,
            rows: Vec::new(),
            tab: PkgManagerTab::Browse,
            selection: 0,
            scroll: 0,
            tab_scroll: 0,
            tab_state: crate::widgets::TabsState::default(),
            tab_area: Rect::default(),
            search_query: String::new(),
            search_active: false,
            kind_filter: None,
            marked: BTreeSet::new(),
            accepted_updates: BTreeSet::new(),
            detail_zoom: false,
            g_pending: false,
            shown_help: false,
            ingress,
        };
        manager.accepted_updates = manager
            .updates
            .iter()
            .filter(|change| change.needs_apply())
            .map(|change| change.name.clone())
            .collect();
        manager.rebuild_filter();
        manager
    }

    fn selected_row(&self) -> Option<&PkgRow> {
        self.rows.get(self.selection)
    }

    fn selected_item(&self) -> Option<&PkgManagerItem> {
        match self.selected_row()? {
            PkgRow::Package(index) => self.entries.get(*index),
            PkgRow::Update(index) => self
                .updates
                .get(*index)
                .and_then(|change| self.entries.iter().find(|item| item.name == change.name)),
            PkgRow::Section(_) | PkgRow::Registry(_) => None,
        }
    }

    fn selected_registry(&self) -> Option<&RegistryItem> {
        match self.selected_row()? {
            PkgRow::Registry(index) => self.registries.get(*index),
            _ => None,
        }
    }

    fn selected_update(&self) -> Option<&PackageChange> {
        match self.selected_row()? {
            PkgRow::Update(index) => self.updates.get(*index),
            _ => None,
        }
    }

    fn selected_identity(&self) -> Option<String> {
        match self.selected_row()? {
            PkgRow::Package(index) => Some(format!("pkg:{}", self.entries.get(*index)?.name)),
            PkgRow::Update(index) => Some(format!("upd:{}", self.updates.get(*index)?.name)),
            PkgRow::Registry(index) => Some(format!("reg:{}", self.registries.get(*index)?.name)),
            PkgRow::Section(kind) => Some(format!("section:{kind}")),
        }
    }

    fn rebuild_filter(&mut self) {
        let selected = self.selected_identity();
        let query = parse_filter_query(&self.search_query);
        self.rows = match self.tab {
            PkgManagerTab::Browse | PkgManagerTab::Installed => self.package_rows(&query),
            PkgManagerTab::Updates => self.update_rows(&query),
            PkgManagerTab::Registries => (0..self.registries.len()).map(PkgRow::Registry).collect(),
        };

        if self.rows.is_empty() {
            self.selection = 0;
            self.scroll = 0;
            return;
        }

        self.selection = selected
            .and_then(|identity| {
                self.rows.iter().position(|row| {
                    row_identity(row, &self.entries, &self.updates, &self.registries).as_deref()
                        == Some(identity.as_str())
                })
            })
            .unwrap_or_else(|| self.selection.min(self.rows.len() - 1));
        if matches!(self.rows.get(self.selection), Some(PkgRow::Section(_))) {
            self.move_selection(1);
        }
        self.ensure_selection_visible();
    }

    fn package_rows(&self, query: &FilterQuery) -> Vec<PkgRow> {
        let mut grouped: BTreeMap<PkgKind, Vec<usize>> = BTreeMap::new();
        let mut matcher = nucleo::Matcher::default();
        let mut utf32 = Vec::new();
        let atom = (!query.text.is_empty()).then(|| {
            Atom::new(
                &query.text,
                CaseMatching::Smart,
                Normalization::Smart,
                AtomKind::Fuzzy,
                false,
            )
        });

        for (index, item) in self.entries.iter().enumerate() {
            if self.tab == PkgManagerTab::Installed && item.installed.is_none() {
                continue;
            }
            if !filter_item(item, query, self.kind_filter) {
                continue;
            }
            if atom.as_ref().is_some_and(|atom| {
                atom.score(Utf32Str::new(&item.search_blob, &mut utf32), &mut matcher)
                    .is_none()
            }) {
                continue;
            }
            grouped.entry(item.kind).or_default().push(index);
        }
        rows_from_groups(grouped, PkgRow::Package)
    }

    fn update_rows(&self, query: &FilterQuery) -> Vec<PkgRow> {
        let mut grouped: BTreeMap<PkgKind, Vec<usize>> = BTreeMap::new();
        for (index, change) in self.updates.iter().enumerate() {
            if self.kind_filter.is_some_and(|kind| kind != change.kind) {
                continue;
            }
            if query.kind.is_some_and(|kind| kind != change.kind) {
                continue;
            }
            if let Some(item) = self.entries.iter().find(|item| item.name == change.name) {
                if !filter_item(item, query, None) {
                    continue;
                }
            } else if !query.text.is_empty() && !change.name.contains(&query.text) {
                continue;
            }
            grouped.entry(change.kind).or_default().push(index);
        }
        rows_from_groups(grouped, PkgRow::Update)
    }

    fn set_tab(&mut self, tab: PkgManagerTab) {
        self.tab = tab;
        self.selection = 0;
        self.scroll = 0;
        self.g_pending = false;
        self.rebuild_filter();
    }

    fn set_tab_index(&mut self, index: usize) {
        if let Some(tab) = PkgManagerTab::from_index(index) {
            self.set_tab(tab);
        }
    }

    fn cycle_tab(&mut self, delta: isize) {
        let current = self.tab.index() as isize;
        let len = PkgManagerTab::ALL.len() as isize;
        let next = (current + delta).rem_euclid(len) as usize;
        self.set_tab(PkgManagerTab::ALL[next]);
    }

    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let max = self.rows.len().saturating_sub(1) as isize;
        let mut next = (self.selection as isize + delta).clamp(0, max) as usize;
        while matches!(self.rows.get(next), Some(PkgRow::Section(_))) {
            let candidate = (next as isize + delta.signum()).clamp(0, max) as usize;
            if candidate == next {
                break;
            }
            next = candidate;
        }
        self.selection = next;
        self.ensure_selection_visible();
    }

    fn move_first(&mut self) {
        self.selection = self
            .rows
            .iter()
            .position(|row| !matches!(row, PkgRow::Section(_)))
            .unwrap_or(0);
        self.ensure_selection_visible();
    }

    fn move_last(&mut self) {
        self.selection = self
            .rows
            .iter()
            .rposition(|row| !matches!(row, PkgRow::Section(_)))
            .unwrap_or(0);
        self.ensure_selection_visible();
    }

    fn ensure_selection_visible(&mut self) {
        if self.selection < self.scroll {
            self.scroll = self.selection;
        }
    }

    fn layout(area: Rect, detail_zoom: bool) -> PkgManagerLayout {
        use helix_view::layout::{split_vertical, Size};

        let inner = crate::widgets::Panel::framed(crate::widgets::PanelStyle::default(), false)
            .content_area(area)
            .inner(helix_view::graphics::Margin::horizontal(1));
        let detail_height = if detail_zoom {
            (inner.height / 2).max(6)
        } else {
            6.min(inner.height.saturating_sub(4))
        };
        let rows = split_vertical(
            inner,
            &[
                Size::fixed(1),
                Size::fixed(1),
                Size::Fill,
                Size::fixed(detail_height),
                Size::fixed(1),
            ],
        );
        let search_input = Rect::new(
            rows[1].x.saturating_add(3),
            rows[1].y,
            rows[1].width.saturating_sub(4),
            1,
        );

        PkgManagerLayout {
            tabs: rows[0],
            search: rows[1],
            search_input,
            list: rows[2].clip_bottom(1),
            detail: rows[3],
            status: rows[4],
        }
    }

    fn start_operation(&self, cx: &mut Context, operation: crate::runtime::PkgOperation) {
        crate::runtime::spawn_pkg_operation(operation, cx.editor.work(), self.ingress.clone());
    }

    fn selected_names_for_op(&self) -> Vec<String> {
        if !self.marked.is_empty() {
            return self.marked.iter().cloned().collect();
        }
        self.selected_item()
            .map(|item| vec![item.name.clone()])
            .or_else(|| {
                self.selected_update()
                    .map(|change| vec![change.name.clone()])
            })
            .unwrap_or_default()
    }

    fn install_selected(&self, cx: &mut Context) {
        let names = self.selected_names_for_op();
        if !names.is_empty() {
            self.start_operation(cx, crate::runtime::PkgOperation::Install(names));
        }
    }

    fn update_selected(&mut self, cx: &mut Context) {
        if self.tab == PkgManagerTab::Updates {
            self.reload(cx);
            return;
        }
        let names = self.selected_names_for_op();
        if !names.is_empty() {
            self.start_operation(cx, crate::runtime::PkgOperation::Update(names));
        }
    }

    fn apply_updates(&self, cx: &mut Context) {
        let names = self
            .updates
            .iter()
            .filter(|change| change.needs_apply())
            .filter(|change| self.accepted_updates.contains(&change.name))
            .map(|change| change.name.clone())
            .collect::<Vec<_>>();
        if names.is_empty() {
            cx.editor.set_status("No accepted package updates");
        } else {
            self.start_operation(cx, crate::runtime::PkgOperation::Update(names));
        }
    }

    fn rollback_selected(&self, cx: &mut Context) {
        for name in self.selected_names_for_op() {
            self.start_operation(cx, crate::runtime::PkgOperation::Rollback(name));
        }
    }

    fn doctor_selected(&self, cx: &mut Context) {
        let names = self.selected_names_for_op();
        if names.is_empty() {
            self.start_operation(cx, crate::runtime::PkgOperation::Doctor);
        } else {
            for name in names {
                self.start_operation(cx, crate::runtime::PkgOperation::DoctorPackage(name));
            }
        }
    }

    fn registry_update_selected(&self, cx: &mut Context) {
        let names = self
            .selected_registry()
            .map(|registry| vec![registry.name.clone()])
            .unwrap_or_default();
        self.start_operation(cx, crate::runtime::PkgOperation::UpdateRegistries(names));
    }

    fn remove_selected(&self, cx: &mut Context) -> EventResult {
        let names = self.selected_names_for_op();
        let installed = names
            .into_iter()
            .filter(|name| {
                self.entries
                    .iter()
                    .any(|item| item.name == *name && item.installed.is_some())
            })
            .collect::<Vec<_>>();
        if installed.is_empty() {
            cx.editor.set_status("No installed package selected");
            return EventResult::Consumed(None);
        }

        let label = if installed.len() == 1 {
            format!("Remove package {}?", installed[0])
        } else {
            format!("Remove {} packages?", installed.len())
        };
        let ingress = self.ingress.clone();
        let confirmation = crate::ui::picker::PickerConfirmation::new(label, move |cx| {
            crate::runtime::spawn_pkg_operation(
                crate::runtime::PkgOperation::Remove(installed.clone()),
                cx.editor.work(),
                ingress.clone(),
            );
        });
        EventResult::Consumed(Some(confirmation.into_post_action()))
    }

    fn reload(&mut self, cx: &mut Context) {
        let result: anyhow::Result<PkgManagerData> = (|| {
            let ops = Ops::open_default()?;
            load_data(&ops)
        })();
        match result {
            Ok(data) => {
                self.entries = data.entries;
                self.updates = data.updates;
                self.registries = data.registries;
                self.accepted_updates = self
                    .updates
                    .iter()
                    .filter(|change| change.needs_apply())
                    .map(|change| change.name.clone())
                    .collect();
                self.rebuild_filter();
                cx.editor.set_status("Package data refreshed");
            }
            Err(err) => cx
                .editor
                .set_error(format!("Failed to refresh package data: {err}")),
        }
    }

    fn toggle_mark(&mut self) {
        match self.selected_row() {
            Some(PkgRow::Package(index)) => {
                let name = self.entries[*index].name.clone();
                if !self.marked.remove(&name) {
                    self.marked.insert(name);
                }
            }
            Some(PkgRow::Update(index)) => {
                let name = self.updates[*index].name.clone();
                if !self.accepted_updates.remove(&name) {
                    self.accepted_updates.insert(name);
                }
            }
            _ => {}
        }
    }

    fn cycle_kind_filter(&mut self) {
        self.kind_filter = match self.kind_filter {
            None => Some(PkgKind::ALL[0]),
            Some(kind) => PkgKind::ALL
                .iter()
                .position(|candidate| *candidate == kind)
                .and_then(|index| PkgKind::ALL.get(index + 1).copied()),
        };
        self.rebuild_filter();
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> bool {
        if self.search_active {
            match key {
                KeyEvent {
                    code: KeyCode::Esc | KeyCode::Enter,
                    modifiers: KeyModifiers::NONE,
                } => self.search_active = false,
                KeyEvent {
                    code: KeyCode::Backspace,
                    modifiers: KeyModifiers::NONE,
                } => {
                    self.search_query.pop();
                    self.rebuild_filter();
                }
                KeyEvent {
                    code: KeyCode::Char('u'),
                    modifiers: KeyModifiers::CONTROL,
                } => {
                    self.search_query.clear();
                    self.rebuild_filter();
                }
                KeyEvent {
                    code: KeyCode::Char(ch),
                    modifiers,
                } if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                    self.search_query.push(ch);
                    self.rebuild_filter();
                }
                _ => {}
            }
            return true;
        }
        false
    }

    fn binding_for_key(key: &KeyEvent) -> Option<&'static PkgBinding> {
        PKG_BINDINGS.iter().find(|binding| binding.key.matches(key))
    }

    fn help_entries(&self) -> Vec<(&'static str, &'static str)> {
        let mut entries = PKG_BINDINGS
            .iter()
            .filter_map(|binding| binding.hint)
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| b.2.cmp(&a.2));
        entries
            .into_iter()
            .map(|(key, label, _)| (key, label))
            .collect()
    }

    fn info(&self) -> helix_view::info::Info {
        helix_view::info::Info::new("Package manager", &self.help_entries())
    }

    fn toggle_help(&mut self, editor: &mut Editor) {
        if self.shown_help {
            self.shown_help = false;
            editor.autoinfo = None;
        } else {
            editor.autoinfo = Some(self.info());
            self.shown_help = true;
        }
    }

    fn clear_help(&mut self, editor: &mut Editor) {
        if self.shown_help {
            self.shown_help = false;
            editor.autoinfo = None;
        }
    }

    fn render_surface(
        &mut self,
        area: Rect,
        surface: &mut crate::render::CellSurface,
        cx: &RenderContext,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let theme = cx.theme();
        let styles = PkgManagerStyles::from_theme(theme);
        let layout = Self::layout(area, self.detail_zoom);
        let panel_style =
            crate::widgets::PanelStyle::new(styles.background, styles.border, styles.title);
        crate::widgets::Panel::framed(panel_style, cx.config().rounded_corners)
            .title(" Package Manager ")
            .render(surface, area);

        self.render_tabs(surface, layout.tabs, styles);
        self.render_search(surface, layout.search, layout.search_input, styles);
        self.render_list(surface, layout.list, styles);
        self.render_detail(surface, layout.detail, styles);
        self.render_status(surface, layout.status, styles);
    }

    fn render_tabs(
        &mut self,
        surface: &mut crate::render::CellSurface,
        area: Rect,
        styles: PkgManagerStyles,
    ) {
        let update_count = self
            .updates
            .iter()
            .filter(|change| change.needs_apply())
            .count();
        let tabs = PkgManagerTab::ALL
            .iter()
            .copied()
            .map(|tab| {
                let tab = crate::widgets::Tab::new(tab.label());
                if tab.label.as_ref() == "Updates" {
                    tab.badge(update_count.to_string())
                } else {
                    tab
                }
            })
            .collect::<Vec<_>>();
        let state = crate::widgets::tabs_with_options(
            surface,
            area,
            &tabs,
            crate::widgets::TabsOptions::new(self.tab.index())
                .scroll(self.tab_scroll)
                .separator(" "),
            crate::widgets::TabsStyle {
                background: styles.background,
                active: styles.title,
                inactive: styles.inactive,
                badge: styles.warning,
                separator: styles.background,
                overflow: styles.inactive,
                hover: styles.selection,
            },
        );
        self.tab_scroll = state.scroll;
        self.tab_state = state;
        self.tab_area = area;
    }

    fn render_search(
        &self,
        surface: &mut crate::render::CellSurface,
        area: Rect,
        input_area: Rect,
        styles: PkgManagerStyles,
    ) {
        surface.set_style(
            tui::ratatui::to_ratatui_rect(area),
            tui::ratatui::to_ratatui_style(styles.background),
        );
        let marker_style = if self.search_active {
            styles.title
        } else {
            styles.inactive
        };
        if area.width > 1 {
            surface.set_stringn(
                area.x.saturating_add(1),
                area.y,
                "/",
                1,
                tui::ratatui::to_ratatui_style(marker_style),
            );
        }
        let placeholder = self
            .kind_filter
            .map_or("search packages".to_owned(), |kind| format!("kind:{kind}"));
        if self.search_query.is_empty() && !self.search_active {
            surface.set_stringn(
                input_area.x,
                input_area.y,
                &placeholder,
                input_area.width as usize,
                tui::ratatui::to_ratatui_style(styles.inactive),
            );
        } else {
            crate::widgets::text_input(
                surface,
                input_area,
                &self.search_query,
                self.search_query.len(),
                styles.text,
                styles.selection,
            );
        }
    }

    fn render_list(
        &mut self,
        surface: &mut crate::render::CellSurface,
        area: Rect,
        styles: PkgManagerStyles,
    ) {
        let list_styles = crate::widgets::ListStyles {
            normal: styles.background,
            selected: styles.selection,
            scrollbar_thumb: styles.inactive,
            scrollbar_track: styles.background,
        };
        let marks = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(row_index, row)| match row {
                PkgRow::Package(index) if self.marked.contains(&self.entries[*index].name) => {
                    Some(row_index)
                }
                PkgRow::Update(index)
                    if self.accepted_updates.contains(&self.updates[*index].name) =>
                {
                    Some(row_index)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let sticky_rows = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| matches!(row, PkgRow::Section(_)).then_some(index))
            .collect::<Vec<_>>();
        let state = crate::widgets::item_list_with_marks_and_sticky(
            surface,
            area,
            self.rows.len(),
            (!self.rows.is_empty()).then_some(self.selection),
            self.scroll,
            Some(crate::widgets::MarkedItems::new(&marks, "✓")),
            Some(crate::widgets::StickyRows::new(&sticky_rows)),
            &list_styles,
            |row_index, row_area, surface, selected, _marked| {
                self.render_row(surface, row_area, row_index, selected, styles);
            },
        );
        self.scroll = state.scroll;
        if self.rows.is_empty() {
            render_empty_state(surface, area, self.empty_state(), styles);
        }
    }

    fn render_row(
        &self,
        surface: &mut crate::render::CellSurface,
        area: Rect,
        row_index: usize,
        selected: bool,
        styles: PkgManagerStyles,
    ) {
        let Some(row) = self.rows.get(row_index) else {
            return;
        };
        match row {
            PkgRow::Section(kind) => render_section_row(surface, area, *kind, styles),
            PkgRow::Package(index) => {
                let item = &self.entries[*index];
                let progress = progress_snapshot().active.get(&item.name).cloned();
                render_package_row(surface, area, item, progress.as_ref(), selected, styles);
            }
            PkgRow::Update(index) => {
                let change = &self.updates[*index];
                render_update_row(surface, area, change, selected, styles);
            }
            PkgRow::Registry(index) => {
                let registry = &self.registries[*index];
                render_registry_row(surface, area, registry, selected, styles);
            }
        }
    }

    fn empty_state(&self) -> &'static str {
        match self.tab {
            PkgManagerTab::Browse => "no packages match the filter",
            PkgManagerTab::Installed => "no installed packages",
            PkgManagerTab::Updates => "no updates - u to refresh",
            PkgManagerTab::Registries => "no registry sources configured",
        }
    }

    fn render_detail(
        &self,
        surface: &mut crate::render::CellSurface,
        area: Rect,
        styles: PkgManagerStyles,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        surface.set_style(
            tui::ratatui::to_ratatui_rect(area),
            tui::ratatui::to_ratatui_style(styles.background),
        );
        match self.selected_row() {
            Some(PkgRow::Package(index)) => {
                render_package_detail(surface, area, &self.entries[*index], styles)
            }
            Some(PkgRow::Update(index)) => render_update_detail(
                surface,
                area,
                &self.updates[*index],
                self.accepted_updates.contains(&self.updates[*index].name),
                styles,
            ),
            Some(PkgRow::Registry(index)) => {
                render_registry_detail(surface, area, &self.registries[*index], styles)
            }
            _ => render_empty_state(surface, area, self.empty_state(), styles),
        }
    }

    fn render_status(
        &self,
        surface: &mut crate::render::CellSurface,
        area: Rect,
        styles: PkgManagerStyles,
    ) {
        surface.set_style(
            tui::ratatui::to_ratatui_rect(area),
            tui::ratatui::to_ratatui_style(styles.title),
        );
        let progress = progress_snapshot();
        let Some(status) = progress.statusline() else {
            surface.set_stringn(
                area.x,
                area.y,
                " pkg idle ",
                area.width as usize,
                tui::ratatui::to_ratatui_style(styles.inactive),
            );
            return;
        };
        let queue = progress.active_count().saturating_sub(1);
        let label = if queue > 0 {
            format!("{} ({queue} queued)", status.label)
        } else {
            status.label
        };
        let ratio = status.percent.map_or(0.0, |percent| percent as f32 / 100.0);
        crate::widgets::progress_bar(
            surface,
            area,
            ratio,
            Some(&label),
            crate::widgets::ProgressStyle {
                track: styles.title,
                fill: styles.warning,
                label: styles.text,
            },
        );
    }

    pub(crate) fn storybook_sample(ingress: crate::runtime::RuntimeIngress) -> Self {
        let receipt = Receipt {
            name: "rust-analyzer".to_owned(),
            kind: PkgKind::Lsp,
            version: "2026-06-30".to_owned(),
            source: "github-release".to_owned(),
            url: "https://github.com/rust-lang/rust-analyzer".to_owned(),
            archive_sha256: "fixture".to_owned(),
            bin: "rust-analyzer.exe".to_owned(),
            shim: "rust-analyzer.exe".to_owned(),
            previous_version: None,
            files: BTreeMap::new(),
            installed_at: "2026-06-30T00:00:00Z".to_owned(),
            native_manager: None,
            native_id: None,
        };
        let data = PkgManagerData {
            entries: vec![
                story_item("rust-analyzer", PkgKind::Lsp, Some(receipt), "rust"),
                story_item("lua-language-server", PkgKind::Lsp, None, "lua"),
                story_item("codelldb", PkgKind::Dap, None, "rust c cpp"),
                story_item("tree-sitter-rust", PkgKind::Grammar, None, "rust"),
            ],
            updates: Vec::new(),
            registries: Vec::new(),
        };
        Self::new(data, ingress)
    }
}

fn story_item(
    name: &str,
    kind: PkgKind,
    receipt: Option<Receipt>,
    languages: &str,
) -> PkgManagerItem {
    let installed = receipt.as_ref().map(|receipt| receipt.version.clone());
    PkgManagerItem {
        name: name.to_owned(),
        kind,
        installed,
        latest: "registry".to_owned(),
        description: format!("{name} package for {languages} development."),
        homepage: Some(format!("https://example.invalid/{name}")),
        aliases: String::new(),
        categories: kind.default_category().to_owned(),
        languages: languages.to_owned(),
        schemas: String::new(),
        source: "github-release".to_owned(),
        receipt,
        doctor: "ok".to_owned(),
        problem: None,
        search_blob: format!("{name}\n{kind}\n{languages}"),
    }
}

fn rows_from_groups(
    grouped: BTreeMap<PkgKind, Vec<usize>>,
    row: impl Fn(usize) -> PkgRow,
) -> Vec<PkgRow> {
    let mut rows = Vec::new();
    for (kind, indices) in grouped {
        rows.push(PkgRow::Section(kind));
        rows.extend(indices.into_iter().map(&row));
    }
    rows
}

fn row_identity<'a>(
    row: &PkgRow,
    entries: &'a [PkgManagerItem],
    updates: &'a [PackageChange],
    registries: &'a [RegistryItem],
) -> Option<String> {
    match row {
        PkgRow::Package(index) => entries.get(*index).map(|item| format!("pkg:{}", item.name)),
        PkgRow::Update(index) => updates
            .get(*index)
            .map(|change| format!("upd:{}", change.name)),
        PkgRow::Registry(index) => registries
            .get(*index)
            .map(|registry| format!("reg:{}", registry.name)),
        PkgRow::Section(kind) => Some(format!("section:{kind}")),
    }
}

fn filter_item(item: &PkgManagerItem, query: &FilterQuery, kind_filter: Option<PkgKind>) -> bool {
    if kind_filter.is_some_and(|kind| kind != item.kind) {
        return false;
    }
    if query.kind.is_some_and(|kind| kind != item.kind) {
        return false;
    }
    if let Some(lang) = &query.lang {
        let lang = lang.as_str();
        if !item.languages.split_whitespace().any(|value| value == lang)
            && !item.languages.split(',').any(|value| value.trim() == lang)
        {
            return false;
        }
    }
    if let Some(category) = &query.category {
        let category = category.as_str();
        if !item
            .categories
            .split(',')
            .any(|value| value.trim() == category)
        {
            return false;
        }
    }
    true
}

fn parse_filter_query(input: &str) -> FilterQuery {
    let mut query = FilterQuery::default();
    let mut text = Vec::new();
    for token in input.split_whitespace() {
        if let Some(value) = token.strip_prefix("lang:") {
            query.lang = Some(value.to_ascii_lowercase());
        } else if let Some(value) = token.strip_prefix("kind:") {
            query.kind = value.parse().ok();
        } else if let Some(value) = token.strip_prefix("cat:") {
            query.category = Some(value.to_ascii_lowercase());
        } else {
            text.push(token);
        }
    }
    query.text = text.join(" ");
    query
}

fn row_state(
    item: &PkgManagerItem,
    progress: Option<&PkgStatusline>,
    problem: Option<&str>,
) -> RowState {
    if problem.is_some() {
        RowState::Problem
    } else if progress.is_some() {
        RowState::Working
    } else if item.installed.is_some() {
        RowState::Installed
    } else {
        RowState::Available
    }
}

fn update_row_state(change: &PackageChange) -> RowState {
    if change.error.is_some() {
        RowState::Problem
    } else if change.needs_apply() {
        RowState::Update
    } else {
        RowState::Installed
    }
}

fn render_section_row(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    kind: PkgKind,
    styles: PkgManagerStyles,
) {
    let label = section_label(kind);
    surface.set_stringn(
        area.x,
        area.y,
        label,
        area.width as usize,
        tui::ratatui::to_ratatui_style(styles.inactive),
    );
}

fn section_label(kind: PkgKind) -> &'static str {
    match kind {
        PkgKind::Lsp => "Language servers",
        PkgKind::Dap => "Debug adapters",
        PkgKind::Formatter => "Formatters",
        PkgKind::Linter => "Linters",
        PkgKind::Grammar => "Grammars",
        PkgKind::Plugin => "Plugins",
    }
}

fn render_package_row(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    item: &PkgManagerItem,
    progress: Option<&PkgStatusline>,
    selected: bool,
    styles: PkgManagerStyles,
) {
    let state = row_state(item, progress, item.problem.as_deref());
    let status = progress
        .and_then(|progress| {
            progress
                .percent
                .map(|percent| format!("installing {percent}%"))
        })
        .unwrap_or_else(|| status_label(item).to_owned());
    let version = item.installed.as_deref().unwrap_or(&item.latest);
    render_columns(
        surface,
        area,
        state,
        &item.name,
        version,
        item.problem.as_deref().unwrap_or(&status),
        &item.languages,
        selected,
        styles,
    );
}

fn render_update_row(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    change: &PackageChange,
    selected: bool,
    styles: PkgManagerStyles,
) {
    let state = update_row_state(change);
    let version = match (change.installed.as_deref(), change.candidate_version()) {
        (Some(installed), Some(candidate)) => format!("{installed}->{candidate}"),
        (Some(installed), None) => installed.to_owned(),
        (None, Some(candidate)) => candidate.to_owned(),
        (None, None) => "-".to_owned(),
    };
    let status = change
        .error
        .as_deref()
        .or_else(|| change.needs_apply().then_some("update"))
        .unwrap_or("current");
    render_columns(
        surface,
        area,
        state,
        &change.name,
        &version,
        status,
        change.kind.default_category(),
        selected,
        styles,
    );
}

fn render_registry_row(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    registry: &RegistryItem,
    selected: bool,
    styles: PkgManagerStyles,
) {
    let state = if registry.problem.is_some() {
        RowState::Problem
    } else {
        RowState::Installed
    };
    render_columns(
        surface,
        area,
        state,
        &registry.name,
        &registry.cache_age,
        &registry.doctor,
        &registry.source,
        selected,
        styles,
    );
}

#[allow(
    clippy::too_many_arguments,
    reason = "row renderer is a compact column painter"
)]
fn render_columns(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    state: RowState,
    name: &str,
    version: &str,
    status: &str,
    chips: &str,
    selected: bool,
    styles: PkgManagerStyles,
) {
    if area.width == 0 {
        return;
    }
    let base = if selected {
        styles.selection
    } else {
        styles.background
    };
    surface.set_style(
        tui::ratatui::to_ratatui_rect(area),
        tui::ratatui::to_ratatui_style(base),
    );
    let state_style = styles.row_state(state, selected);
    surface.set_stringn(
        area.x,
        area.y,
        state.glyph(),
        1,
        tui::ratatui::to_ratatui_style(state_style),
    );

    let width = area.width as usize;
    let version_width = 16usize.min(width / 4);
    let status_width = 18usize.min(width / 4);
    let chip_width = 18usize.min(width / 4);
    let fixed = 2 + version_width + status_width + chip_width;
    let name_width = width.saturating_sub(fixed).max(8);
    let mut x = area.x.saturating_add(2);
    write_cell(surface, x, area.y, name_width, name, styles.text);
    x = x.saturating_add(name_width as u16);
    write_cell(surface, x, area.y, version_width, version, styles.inactive);
    x = x.saturating_add(version_width as u16);
    write_cell(surface, x, area.y, status_width, status, state_style);
    x = x.saturating_add(status_width as u16);
    write_cell(surface, x, area.y, chip_width, chips, styles.inactive);
}

fn write_cell(
    surface: &mut crate::render::CellSurface,
    x: u16,
    y: u16,
    width: usize,
    text: &str,
    style: Style,
) {
    if width == 0 {
        return;
    }
    let text =
        crate::ui::text_layout::truncate(text, width, crate::ui::text_layout::TruncateAt::End);
    surface.set_stringn(x, y, &text, width, tui::ratatui::to_ratatui_style(style));
}

fn status_label(item: &PkgManagerItem) -> &'static str {
    if item.installed.is_some() {
        "installed"
    } else {
        "available"
    }
}

fn empty_dash(value: &str) -> &str {
    if value.is_empty() {
        "-"
    } else {
        value
    }
}

fn render_package_detail(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    item: &PkgManagerItem,
    styles: PkgManagerStyles,
) {
    let mut y = area.y;
    write_line(
        surface,
        area,
        &mut y,
        &format!("{} - {}  {}", item.name, item.description, item.kind),
        styles.title,
    );
    let receipt = item.receipt.as_ref();
    let receipt_line = receipt.map_or_else(
        || "not installed".to_owned(),
        |receipt| {
            let verified = if receipt.archive_sha256.is_empty() {
                "sha256 -"
            } else {
                "sha256 ok"
            };
            format!(
                "installed {} ({}, {})",
                receipt.version, receipt.source, verified
            )
        },
    );
    write_line(
        surface,
        area,
        &mut y,
        &format!("{receipt_line}  latest {}", item.latest),
        styles.text,
    );
    write_line(
        surface,
        area,
        &mut y,
        &format!("doctor: {}", item.doctor),
        if item.problem.is_some() {
            styles.error
        } else {
            styles.inactive
        },
    );
    write_text_layout(
        surface,
        area,
        &mut y,
        &format!("source: {}", item.source),
        styles.text,
    );
    write_line(
        surface,
        area,
        &mut y,
        &format!(
            "languages: {}   aliases: {}",
            empty_dash(&item.languages),
            empty_dash(&item.aliases)
        ),
        styles.inactive,
    );
    write_line(
        surface,
        area,
        &mut y,
        &format!(
            "categories: {}   schemas: {}",
            empty_dash(&item.categories),
            empty_dash(&item.schemas)
        ),
        styles.inactive,
    );
    if let Some(homepage) = &item.homepage {
        write_line(
            surface,
            area,
            &mut y,
            &format!("homepage: {homepage}"),
            styles.inactive,
        );
    }
}

fn render_update_detail(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    change: &PackageChange,
    accepted: bool,
    styles: PkgManagerStyles,
) {
    let mut y = area.y;
    let candidate = change.candidate_version().unwrap_or("-");
    let current = change.installed.as_deref().unwrap_or("-");
    write_line(
        surface,
        area,
        &mut y,
        &format!(
            "{}  {} -> {}  {}",
            change.name, current, candidate, change.kind
        ),
        styles.title,
    );
    write_line(
        surface,
        area,
        &mut y,
        if accepted {
            "accepted for U apply"
        } else {
            "not accepted"
        },
        styles.text,
    );
    if let Some(candidate) = &change.candidate {
        let release = release_age_label(
            candidate.published_at.as_deref(),
            &candidate.version,
            SystemTime::now(),
        );
        write_line(
            surface,
            area,
            &mut y,
            &format!("source: {}  release: {}", candidate.source, release),
            styles.inactive,
        );
        if let Some(url) = candidate.source_url() {
            write_text_layout(surface, area, &mut y, url, styles.inactive);
        }
    }
    for warning in &change.warnings {
        write_text_layout(
            surface,
            area,
            &mut y,
            &format!("warning: {warning}"),
            styles.warning,
        );
    }
    if let Some(error) = &change.error {
        write_text_layout(
            surface,
            area,
            &mut y,
            &format!("error: {error}"),
            styles.error,
        );
    }
}

fn render_registry_detail(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    registry: &RegistryItem,
    styles: PkgManagerStyles,
) {
    let mut y = area.y;
    write_line(
        surface,
        area,
        &mut y,
        &format!("{}  registry source", registry.name),
        styles.title,
    );
    write_text_layout(
        surface,
        area,
        &mut y,
        &format!("source: {}", registry.source),
        styles.text,
    );
    write_line(
        surface,
        area,
        &mut y,
        &format!(
            "cache age: {}  doctor: {}",
            registry.cache_age, registry.doctor
        ),
        styles.inactive,
    );
    if let Some(problem) = &registry.problem {
        write_text_layout(surface, area, &mut y, problem, styles.error);
    }
}

fn render_empty_state(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    message: &str,
    styles: PkgManagerStyles,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    surface.set_stringn(
        area.x,
        area.y,
        message,
        area.width as usize,
        tui::ratatui::to_ratatui_style(styles.inactive),
    );
}

fn write_line(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    y: &mut u16,
    text: &str,
    style: Style,
) {
    if *y >= area.bottom() {
        return;
    }
    let text = crate::ui::text_layout::truncate(
        text,
        area.width as usize,
        crate::ui::text_layout::TruncateAt::End,
    );
    surface.set_stringn(
        area.x,
        *y,
        &text,
        area.width as usize,
        tui::ratatui::to_ratatui_style(style),
    );
    *y = y.saturating_add(1);
}

fn write_text_layout(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    y: &mut u16,
    text: &str,
    style: Style,
) {
    for line in crate::ui::text_layout::wrap_to_width(text, area.width as usize) {
        write_line(surface, area, y, &line, style);
        if *y >= area.bottom() {
            break;
        }
    }
}

impl Component for PkgManager {
    fn render(&mut self, area: Rect, surface: &mut crate::render::CellSurface, cx: &RenderContext) {
        self.render_surface(area, surface, cx);
    }

    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        let key = match event {
            Event::Key(key) => *key,
            Event::Resize(..) => return EventResult::Consumed(None),
            Event::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left)
                        if mouse.row == self.tab_area.y
                            && mouse.column >= self.tab_area.x
                            && mouse.column < self.tab_area.right() =>
                    {
                        let x = mouse.column.saturating_sub(self.tab_area.x);
                        if let Some(index) = self.tab_state.tab_at(x) {
                            self.set_tab_index(index);
                        }
                    }
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        let delta = match mouse.kind {
                            MouseEventKind::ScrollUp => -3,
                            MouseEventKind::ScrollDown => 3,
                            _ => 0,
                        };
                        self.move_selection(delta);
                    }
                    _ => {}
                }
                return EventResult::Consumed(None);
            }
            Event::Paste(_) => return EventResult::Consumed(None),
            Event::IdleTimeout | Event::FocusGained | Event::FocusLost => {
                return EventResult::Ignored(None);
            }
        };

        if self.handle_search_key(key) {
            return EventResult::Consumed(None);
        }

        if self.g_pending {
            self.g_pending = false;
            if matches!(
                key,
                KeyEvent {
                    code: KeyCode::Char('g'),
                    modifiers: KeyModifiers::NONE
                }
            ) {
                self.move_first();
                return EventResult::Consumed(None);
            }
        }
        if matches!(
            key,
            KeyEvent {
                code: KeyCode::Char('g'),
                modifiers: KeyModifiers::NONE
            }
        ) {
            self.g_pending = true;
            return EventResult::Consumed(None);
        }

        let Some(binding) = Self::binding_for_key(&key) else {
            return EventResult::Consumed(None);
        };

        match binding.action {
            PkgAction::MoveDown => self.move_selection(1),
            PkgAction::MoveUp => self.move_selection(-1),
            PkgAction::Last => self.move_last(),
            PkgAction::Search => self.search_active = true,
            PkgAction::ToggleMark => self.toggle_mark(),
            PkgAction::Tab1 => self.set_tab_index(0),
            PkgAction::Tab2 => self.set_tab_index(1),
            PkgAction::Tab3 => self.set_tab_index(2),
            PkgAction::Tab4 => self.set_tab_index(3),
            PkgAction::NextTab => self.cycle_tab(1),
            PkgAction::PreviousTab => self.cycle_tab(-1),
            PkgAction::CycleKind => self.cycle_kind_filter(),
            PkgAction::ToggleDetail => self.detail_zoom = !self.detail_zoom,
            PkgAction::Install => self.install_selected(cx),
            PkgAction::Remove => return self.remove_selected(cx),
            PkgAction::UpdateSelected => self.update_selected(cx),
            PkgAction::ApplyUpdates => self.apply_updates(cx),
            PkgAction::Rollback => self.rollback_selected(cx),
            PkgAction::Doctor => self.doctor_selected(cx),
            PkgAction::RegistryUpdate => self.registry_update_selected(cx),
            PkgAction::Help => self.toggle_help(cx.editor),
            PkgAction::Escape => {
                self.clear_help(cx.editor);
                if self.search_active {
                    self.search_active = false;
                } else if !self.search_query.is_empty() {
                    self.search_query.clear();
                    self.rebuild_filter();
                } else if !self.marked.is_empty() {
                    self.marked.clear();
                } else {
                    return EventResult::Consumed(Some(PostAction::PopLayer {
                        model_layer: None,
                        remember_picker: false,
                    }));
                }
            }
        }
        EventResult::Consumed(None)
    }

    fn cursor(&self, area: Rect, _editor: &Editor) -> (Option<helix_core::Position>, CursorKind) {
        if !self.search_active {
            return (None, CursorKind::Hidden);
        }
        let layout = Self::layout(area, self.detail_zoom);
        let state = helix_view::layout::text_input_layout(
            layout.search_input,
            &self.search_query,
            self.search_query.len(),
        );
        if state.cursor_in_area {
            (
                Some(helix_core::Position::new(
                    state.cursor_y as usize,
                    state.cursor_x as usize,
                )),
                CursorKind::Bar,
            )
        } else {
            (None, CursorKind::Hidden)
        }
    }

    fn id(&self) -> Option<&str> {
        Some(ID)
    }
}

pub fn render_statusline<'a>(
    status: &PkgStatusline,
    theme: &helix_view::theme::Theme,
    width: u16,
) -> tui::ratatui::text::Span<'a> {
    let style = theme
        .try_get("ui.statusline.progress")
        .or_else(|| theme.try_get("info"))
        .unwrap_or_else(|| theme.get("ui.statusline"));
    let label = if let Some(percent) = status.percent {
        format!(" {} {percent:>3}% ", status.label)
    } else {
        let dots = match width % 4 {
            0 => "",
            1 => ".",
            2 => "..",
            _ => "...",
        };
        format!(" {}{dots} ", status.label)
    };
    tui::ratatui::text::Span::styled(label, tui::ratatui::to_ratatui_style(style))
}

#[allow(dead_code)]
pub fn statusline_rect(view: &helix_view::View) -> Rect {
    view.area.clip_top(view.area.height.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkg_manager_constructs_headlessly() {
        let tokio_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio_runtime.block_on(async {
            let runtime = helix_runtime::Runtime::new(tokio::runtime::Handle::current());
            let editor = helix_view::editor::EditorBuilder::new(
                helix_view::graphics::Rect::new(0, 0, 80, 24),
                runtime.clone(),
            )
            .build();
            let (ingress, _rx) = crate::runtime::RuntimeIngress::channel(runtime.work().clone());

            let _manager = manager(&editor, ingress).expect("pkg manager opens");
        });
    }

    #[test]
    fn tab_model_uses_update_badge_count_from_plan() {
        let runtime = helix_runtime::test::runtime();
        let (ingress, _rx) = crate::runtime::RuntimeIngress::channel(runtime.work().clone());
        let mut manager = PkgManager::new(
            PkgManagerData {
                entries: vec![test_item("rust-analyzer", PkgKind::Lsp, Some("1"), "rust")],
                updates: vec![test_change(
                    "rust-analyzer",
                    PkgKind::Lsp,
                    Some("1"),
                    Some("2"),
                    None,
                )],
                registries: Vec::new(),
            },
            ingress,
        );

        assert_eq!(PkgManagerTab::from_index(2), Some(PkgManagerTab::Updates));
        manager.set_tab(PkgManagerTab::Updates);
        assert!(matches!(manager.rows[0], PkgRow::Section(PkgKind::Lsp)));
        assert_eq!(
            manager
                .updates
                .iter()
                .filter(|change| change.needs_apply())
                .count(),
            1
        );
    }

    #[test]
    fn row_state_maps_installed_available_working_problem() {
        let installed = test_item("installed", PkgKind::Lsp, Some("1"), "rust");
        let available = test_item("available", PkgKind::Lsp, None, "rust");
        let progress = PkgStatusline {
            label: "pkg available".to_owned(),
            percent: Some(42),
        };
        assert_eq!(row_state(&installed, None, None), RowState::Installed);
        assert_eq!(row_state(&available, None, None), RowState::Available);
        assert_eq!(
            row_state(&available, Some(&progress), None),
            RowState::Working
        );
        assert_eq!(row_state(&available, None, Some("bad")), RowState::Problem);
        assert_eq!(
            update_row_state(&test_change(
                "demo",
                PkgKind::Dap,
                Some("1"),
                Some("2"),
                None
            )),
            RowState::Update
        );
    }

    #[test]
    fn binding_help_entries_have_dispatch_bindings() {
        let runtime = helix_runtime::test::runtime();
        let (ingress, _rx) = crate::runtime::RuntimeIngress::channel(runtime.work().clone());
        let manager = PkgManager::new(
            PkgManagerData {
                entries: Vec::new(),
                updates: Vec::new(),
                registries: Vec::new(),
            },
            ingress,
        );
        for (key, _, _) in PKG_BINDINGS.iter().filter_map(|binding| binding.hint) {
            assert!(
                PKG_BINDINGS
                    .iter()
                    .any(|binding| binding.hint.is_some_and(|hint| hint.0 == key)),
                "{key} missing from binding table"
            );
        }
        for binding in PKG_BINDINGS {
            assert!(SelfCheck::dispatches(binding.action));
        }
        assert!(manager.help_entries().iter().any(|(key, _)| *key == "?"));
    }

    struct SelfCheck;

    impl SelfCheck {
        fn dispatches(action: PkgAction) -> bool {
            matches!(
                action,
                PkgAction::MoveDown
                    | PkgAction::MoveUp
                    | PkgAction::Last
                    | PkgAction::Search
                    | PkgAction::ToggleMark
                    | PkgAction::Tab1
                    | PkgAction::Tab2
                    | PkgAction::Tab3
                    | PkgAction::Tab4
                    | PkgAction::NextTab
                    | PkgAction::PreviousTab
                    | PkgAction::CycleKind
                    | PkgAction::ToggleDetail
                    | PkgAction::Install
                    | PkgAction::Remove
                    | PkgAction::UpdateSelected
                    | PkgAction::ApplyUpdates
                    | PkgAction::Rollback
                    | PkgAction::Doctor
                    | PkgAction::RegistryUpdate
                    | PkgAction::Help
                    | PkgAction::Escape
            )
        }
    }

    #[test]
    fn filter_parse_supports_prefixes() {
        let query = parse_filter_query("lang:rust kind:dap cat:formatter debug");
        assert_eq!(query.lang.as_deref(), Some("rust"));
        assert_eq!(query.kind, Some(PkgKind::Dap));
        assert_eq!(query.category.as_deref(), Some("formatter"));
        assert_eq!(query.text, "debug");
    }

    #[test]
    fn pkg_progress_events_update_statusline_state() {
        let mut state = PkgProgressState::default();
        state.apply(&OpEvent::Started {
            name: "rust-analyzer".into(),
        });
        assert_eq!(
            state.statusline(),
            Some(PkgStatusline {
                label: "pkg rust-analyzer".into(),
                percent: None,
            })
        );

        state.apply(&OpEvent::Progress {
            name: "rust-analyzer".into(),
            message: "download".into(),
            percent: Some(42),
        });
        assert_eq!(state.statusline().unwrap().percent, Some(42));

        state.apply(&OpEvent::Done {
            name: "rust-analyzer".into(),
        });
        assert_eq!(state.statusline(), None);
    }

    fn test_item(
        name: &str,
        kind: PkgKind,
        installed: Option<&str>,
        language: &str,
    ) -> PkgManagerItem {
        PkgManagerItem {
            name: name.to_owned(),
            kind,
            installed: installed.map(str::to_owned),
            latest: "registry".to_owned(),
            description: format!("{name} package"),
            homepage: None,
            aliases: String::new(),
            categories: kind.default_category().to_owned(),
            languages: language.to_owned(),
            schemas: String::new(),
            source: "test".to_owned(),
            receipt: None,
            doctor: installed.map_or_else(|| "not installed".to_owned(), |_| "ok".to_owned()),
            problem: None,
            search_blob: format!("{name}\n{kind}\n{language}"),
        }
    }

    fn test_change(
        name: &str,
        kind: PkgKind,
        installed: Option<&str>,
        candidate: Option<&str>,
        error: Option<&str>,
    ) -> PackageChange {
        PackageChange {
            name: name.to_owned(),
            kind,
            installed: installed.map(str::to_owned),
            candidate: candidate.map(|version| helix_pkg::ResolvedPackage {
                version: version.to_owned(),
                url: "https://example.invalid/pkg".to_owned(),
                sha256: Some("abc".to_owned()),
                source: "test".to_owned(),
                published_at: Some("2026-07-01".to_owned()),
            }),
            warnings: Vec::new(),
            error: error.map(str::to_owned),
        }
    }
}
